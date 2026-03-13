use std::path::{Path, PathBuf};
use std::sync::Arc;

use ad_core::error::{ADError, ADResult};
use ad_core::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::file_base::{NDFileMode, NDFileWriter, NDPluginFileBase};
use ad_core::plugin::runtime::{NDPluginProcess, ProcessResult};

// ============================================================
// Real HDF5 writer (feature-gated)
// ============================================================

#[cfg(feature = "hdf5")]
mod hdf5_real {
    use super::*;
    use hdf5_metno::File as H5File;

    /// HDF5 file writer using the hdf5 crate.
    pub struct Hdf5RealWriter {
        current_path: Option<PathBuf>,
        file: Option<H5File>,
        frame_count: usize,
        dataset_name: String,
    }

    impl Hdf5RealWriter {
        pub fn new() -> Self {
            Self {
                current_path: None,
                file: None,
                frame_count: 0,
                dataset_name: "data".to_string(),
            }
        }

        pub fn set_dataset_name(&mut self, name: &str) {
            self.dataset_name = name.to_string();
        }
    }

    impl NDFileWriter for Hdf5RealWriter {
        fn open_file(&mut self, path: &Path, _mode: NDFileMode, _array: &NDArray) -> ADResult<()> {
            self.current_path = Some(path.to_path_buf());
            self.frame_count = 0;

            let h5file = H5File::create(path)
                .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 create error: {}", e)))?;
            self.file = Some(h5file);
            Ok(())
        }

        fn write_file(&mut self, array: &NDArray) -> ADResult<()> {
            let h5file = self.file.as_ref()
                .ok_or_else(|| ADError::UnsupportedConversion("no HDF5 file open".into()))?;

            let dataset_name = if self.frame_count == 0 {
                self.dataset_name.clone()
            } else {
                format!("{}_{}", self.dataset_name, self.frame_count)
            };

            // Write based on data type
            let shape = array.dims.iter().rev().map(|d| d.size).collect::<Vec<_>>();

            match &array.data {
                NDDataBuffer::U8(v) => {
                    let ds = h5file.new_dataset::<u8>()
                        .shape(&shape[..])
                        .create(dataset_name.as_str())
                        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 dataset error: {}", e)))?;
                    ds.write_raw(v)
                        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 write error: {}", e)))?;
                }
                NDDataBuffer::U16(v) => {
                    let ds = h5file.new_dataset::<u16>()
                        .shape(&shape[..])
                        .create(dataset_name.as_str())
                        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 dataset error: {}", e)))?;
                    ds.write_raw(v)
                        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 write error: {}", e)))?;
                }
                NDDataBuffer::I32(v) => {
                    let ds = h5file.new_dataset::<i32>()
                        .shape(&shape[..])
                        .create(dataset_name.as_str())
                        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 dataset error: {}", e)))?;
                    ds.write_raw(v)
                        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 write error: {}", e)))?;
                }
                NDDataBuffer::F32(v) => {
                    let ds = h5file.new_dataset::<f32>()
                        .shape(&shape[..])
                        .create(dataset_name.as_str())
                        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 dataset error: {}", e)))?;
                    ds.write_raw(v)
                        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 write error: {}", e)))?;
                }
                NDDataBuffer::F64(v) => {
                    let ds = h5file.new_dataset::<f64>()
                        .shape(&shape[..])
                        .create(dataset_name.as_str())
                        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 dataset error: {}", e)))?;
                    ds.write_raw(v)
                        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 write error: {}", e)))?;
                }
                _ => {
                    // Fallback: write as raw bytes
                    let raw = array.data.as_u8_slice();
                    let ds = h5file.new_dataset::<u8>()
                        .shape([raw.len()])
                        .create(dataset_name.as_str())
                        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 dataset error: {}", e)))?;
                    ds.write_raw(raw)
                        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 write error: {}", e)))?;
                }
            }

            // Write attributes
            for attr in array.attributes.iter() {
                let val_str = attr.value.as_string();
                // HDF5 string attributes on the dataset
                if let Ok(ds) = h5file.dataset(&dataset_name) {
                    let _ = ds.new_attr::<hdf5_metno::types::VarLenUnicode>()
                        .shape(())
                        .create(attr.name.as_str())
                        .and_then(|a| {
                            let s: hdf5_metno::types::VarLenUnicode = val_str.parse().unwrap_or_default();
                            a.write_scalar(&s)
                        });
                }
            }

            self.frame_count += 1;
            Ok(())
        }

        fn read_file(&mut self) -> ADResult<NDArray> {
            let path = self.current_path.as_ref()
                .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

            let h5file = H5File::open(path)
                .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 open error: {}", e)))?;

            let ds = h5file.dataset(&self.dataset_name)
                .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 dataset error: {}", e)))?;

            let shape = ds.shape();
            let dims: Vec<NDDimension> = shape.iter().rev().map(|&s| NDDimension::new(s)).collect();

            // Try reading as different types
            if let Ok(data) = ds.read_raw::<u8>() {
                let mut arr = NDArray::new(dims, NDDataType::UInt8);
                arr.data = NDDataBuffer::U8(data);
                return Ok(arr);
            }
            if let Ok(data) = ds.read_raw::<u16>() {
                let mut arr = NDArray::new(dims, NDDataType::UInt16);
                arr.data = NDDataBuffer::U16(data);
                return Ok(arr);
            }
            if let Ok(data) = ds.read_raw::<f64>() {
                let mut arr = NDArray::new(dims, NDDataType::Float64);
                arr.data = NDDataBuffer::F64(data);
                return Ok(arr);
            }

            Err(ADError::UnsupportedConversion("unsupported HDF5 data type".into()))
        }

        fn close_file(&mut self) -> ADResult<()> {
            self.file = None;
            self.current_path = None;
            Ok(())
        }

        fn supports_multiple_arrays(&self) -> bool {
            true
        }
    }
}

// ============================================================
// Binary format writer (fallback when hdf5 feature is not enabled)
// ============================================================

/// HDF5-compatible binary file writer.
/// When the `hdf5` feature is enabled, use `Hdf5RealWriter` for proper HDF5 I/O.
/// This fallback writes binary data in a simple custom format with HDF5 magic header.
pub struct Hdf5Writer {
    current_path: Option<PathBuf>,
    frame_count: usize,
    file: Option<std::fs::File>,
}

impl Hdf5Writer {
    pub fn new() -> Self {
        Self {
            current_path: None,
            frame_count: 0,
            file: None,
        }
    }

    pub fn frame_count(&self) -> usize {
        self.frame_count
    }
}

impl Default for Hdf5Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl NDFileWriter for Hdf5Writer {
    fn open_file(&mut self, path: &Path, _mode: NDFileMode, _array: &NDArray) -> ADResult<()> {
        use std::io::Write;

        self.current_path = Some(path.to_path_buf());
        self.frame_count = 0;

        let mut file = std::fs::File::create(path)?;
        // Write a simple header (placeholder for HDF5 superblock)
        file.write_all(b"\x89HDF\r\n\x1a\n")?; // HDF5 magic
        self.file = Some(file);
        Ok(())
    }

    fn write_file(&mut self, array: &NDArray) -> ADResult<()> {
        use std::io::Write;

        let file = self.file.as_mut()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let info = array.info();

        // Write frame header: ndims, dims, dtype, data_size
        let ndims = array.dims.len() as u32;
        file.write_all(&ndims.to_le_bytes())?;
        for dim in &array.dims {
            file.write_all(&(dim.size as u32).to_le_bytes())?;
        }
        file.write_all(&(array.data.data_type() as u8).to_le_bytes())?;
        let data_size = info.total_bytes as u32;
        file.write_all(&data_size.to_le_bytes())?;

        // Write raw data
        file.write_all(array.data.as_u8_slice())?;

        // Write attributes
        let num_attrs = array.attributes.len() as u32;
        file.write_all(&num_attrs.to_le_bytes())?;
        for attr in array.attributes.iter() {
            let name_bytes = attr.name.as_bytes();
            file.write_all(&(name_bytes.len() as u16).to_le_bytes())?;
            file.write_all(name_bytes)?;
            let val_str = attr.value.as_string();
            let val_bytes = val_str.as_bytes();
            file.write_all(&(val_bytes.len() as u16).to_le_bytes())?;
            file.write_all(val_bytes)?;
        }

        self.frame_count += 1;
        Ok(())
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        let path = self.current_path.as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let data = std::fs::read(path)?;
        if data.len() < 8 || &data[0..8] != b"\x89HDF\r\n\x1a\n" {
            return Err(ADError::UnsupportedConversion("not an HDF5 file".into()));
        }

        // Read first frame
        let mut pos = 8;
        if pos + 4 > data.len() {
            return Err(ADError::UnsupportedConversion("truncated file".into()));
        }
        let ndims = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;

        let mut dims = Vec::with_capacity(ndims);
        for _ in 0..ndims {
            if pos + 4 > data.len() {
                return Err(ADError::UnsupportedConversion("truncated file".into()));
            }
            let size = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            dims.push(NDDimension::new(size));
            pos += 4;
        }

        if pos + 5 > data.len() {
            return Err(ADError::UnsupportedConversion("truncated file".into()));
        }
        let dtype_ord = data[pos];
        pos += 1;
        let data_size = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;

        let dtype = NDDataType::from_ordinal(dtype_ord)
            .ok_or_else(|| ADError::UnsupportedConversion("invalid data type".into()))?;

        if pos + data_size > data.len() {
            return Err(ADError::UnsupportedConversion("truncated file".into()));
        }
        let raw = &data[pos..pos + data_size];

        let buf = reconstruct_buffer(dtype, raw);

        let mut arr = NDArray::new(dims, dtype);
        arr.data = buf;
        Ok(arr)
    }

    fn close_file(&mut self) -> ADResult<()> {
        self.file = None;
        self.current_path = None;
        Ok(())
    }

    fn supports_multiple_arrays(&self) -> bool {
        true
    }
}

/// Reconstruct a typed NDDataBuffer from raw bytes.
fn reconstruct_buffer(dtype: NDDataType, raw: &[u8]) -> NDDataBuffer {
    match dtype {
        NDDataType::Int8 => {
            NDDataBuffer::I8(raw.iter().map(|&b| b as i8).collect())
        }
        NDDataType::UInt8 => {
            NDDataBuffer::U8(raw.to_vec())
        }
        NDDataType::Int16 => {
            let v: Vec<i16> = raw.chunks_exact(2)
                .map(|c| i16::from_ne_bytes([c[0], c[1]]))
                .collect();
            NDDataBuffer::I16(v)
        }
        NDDataType::UInt16 => {
            let v: Vec<u16> = raw.chunks_exact(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .collect();
            NDDataBuffer::U16(v)
        }
        NDDataType::Int32 => {
            let v: Vec<i32> = raw.chunks_exact(4)
                .map(|c| i32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            NDDataBuffer::I32(v)
        }
        NDDataType::UInt32 => {
            let v: Vec<u32> = raw.chunks_exact(4)
                .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            NDDataBuffer::U32(v)
        }
        NDDataType::Int64 => {
            let v: Vec<i64> = raw.chunks_exact(8)
                .map(|c| i64::from_ne_bytes(c.try_into().unwrap()))
                .collect();
            NDDataBuffer::I64(v)
        }
        NDDataType::UInt64 => {
            let v: Vec<u64> = raw.chunks_exact(8)
                .map(|c| u64::from_ne_bytes(c.try_into().unwrap()))
                .collect();
            NDDataBuffer::U64(v)
        }
        NDDataType::Float32 => {
            let v: Vec<f32> = raw.chunks_exact(4)
                .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            NDDataBuffer::F32(v)
        }
        NDDataType::Float64 => {
            let v: Vec<f64> = raw.chunks_exact(8)
                .map(|c| f64::from_ne_bytes(c.try_into().unwrap()))
                .collect();
            NDDataBuffer::F64(v)
        }
    }
}

// ============================================================
// Processor (wraps either real HDF5 or binary writer)
// ============================================================

/// HDF5 file processor wrapping NDPluginFileBase + Hdf5Writer.
/// When the `hdf5` feature is not enabled, uses a binary format fallback.
pub struct Hdf5FileProcessor {
    file_base: NDPluginFileBase,
    writer: Hdf5Writer,
}

impl Hdf5FileProcessor {
    pub fn new() -> Self {
        Self {
            file_base: NDPluginFileBase::new(),
            writer: Hdf5Writer::new(),
        }
    }

    pub fn file_base_mut(&mut self) -> &mut NDPluginFileBase {
        &mut self.file_base
    }
}

impl Default for Hdf5FileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for Hdf5FileProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let _ = self
            .file_base
            .process_array(Arc::new(array.clone()), &mut self.writer);
        ProcessResult::empty() // file plugins are sinks
    }

    fn plugin_type(&self) -> &str {
        "NDFileHDF5"
    }
}

/// Re-export the real HDF5 writer when the feature is enabled.
#[cfg(feature = "hdf5")]
pub use hdf5_real::Hdf5RealWriter;

/// HDF5 file processor using the real hdf5 crate.
#[cfg(feature = "hdf5")]
pub struct Hdf5RealFileProcessor {
    file_base: NDPluginFileBase,
    writer: Hdf5RealWriter,
}

#[cfg(feature = "hdf5")]
impl Hdf5RealFileProcessor {
    pub fn new() -> Self {
        Self {
            file_base: NDPluginFileBase::new(),
            writer: Hdf5RealWriter::new(),
        }
    }

    pub fn file_base_mut(&mut self) -> &mut NDPluginFileBase {
        &mut self.file_base
    }

    pub fn set_dataset_name(&mut self, name: &str) {
        self.writer.set_dataset_name(name);
    }
}

#[cfg(feature = "hdf5")]
impl Default for Hdf5RealFileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "hdf5")]
impl NDPluginProcess for Hdf5RealFileProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let _ = self
            .file_base
            .process_array(Arc::new(array.clone()), &mut self.writer);
        ProcessResult::empty()
    }

    fn plugin_type(&self) -> &str {
        "NDFileHDF5"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core::attributes::{NDAttribute, NDAttrSource, NDAttrValue};
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(prefix: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("adcore_test_{}_{}.h5", prefix, n))
    }

    #[test]
    fn test_write_single_frame() {
        let path = temp_path("hdf5_single");
        let mut writer = Hdf5Writer::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..16 { v[i] = i as u8; }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        // Read back
        let mut reader = Hdf5Writer::new();
        reader.current_path = Some(path.clone());
        let read_arr = reader.read_file().unwrap();
        assert_eq!(read_arr.dims.len(), 2);
        assert_eq!(read_arr.dims[0].size, 4);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_multiple_frames() {
        let path = temp_path("hdf5_multi");
        let mut writer = Hdf5Writer::new();

        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );

        writer.open_file(&path, NDFileMode::Stream, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        assert!(writer.supports_multiple_arrays());
        assert_eq!(writer.frame_count(), 3);

        let meta = std::fs::metadata(&path).unwrap();
        assert!(meta.len() > 16 * 3); // 3 frames of data

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_attributes_stored() {
        let path = temp_path("hdf5_attrs");
        let mut writer = Hdf5Writer::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute {
            name: "exposure".into(),
            description: "".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Float64(0.5),
        });

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        // File should contain "exposure" and "0.5"
        let data = std::fs::read(&path).unwrap();
        let content = String::from_utf8_lossy(&data);
        assert!(content.contains("exposure"));
        assert!(content.contains("0.5"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_roundtrip_u16() {
        let path = temp_path("hdf5_u16");
        let mut writer = Hdf5Writer::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for i in 0..16 { v[i] = (i * 100) as u16; }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut reader = Hdf5Writer::new();
        reader.current_path = Some(path.clone());
        let read_arr = reader.read_file().unwrap();
        assert_eq!(read_arr.data.data_type(), NDDataType::UInt16);
        if let NDDataBuffer::U16(ref v) = read_arr.data {
            assert_eq!(v[0], 0);
            assert_eq!(v[1], 100);
            assert_eq!(v[15], 1500);
        } else {
            panic!("expected U16 data");
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_roundtrip_f64() {
        let path = temp_path("hdf5_f64");
        let mut writer = Hdf5Writer::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4)],
            NDDataType::Float64,
        );
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v[0] = 1.5; v[1] = 2.5; v[2] = 3.5; v[3] = 4.5;
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut reader = Hdf5Writer::new();
        reader.current_path = Some(path.clone());
        let read_arr = reader.read_file().unwrap();
        assert_eq!(read_arr.data.data_type(), NDDataType::Float64);
        if let NDDataBuffer::F64(ref v) = read_arr.data {
            assert!((v[0] - 1.5).abs() < 1e-10);
            assert!((v[3] - 4.5).abs() < 1e-10);
        } else {
            panic!("expected F64 data");
        }

        std::fs::remove_file(&path).ok();
    }
}
