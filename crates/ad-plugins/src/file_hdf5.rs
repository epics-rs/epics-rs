use std::path::{Path, PathBuf};
use std::sync::Arc;

use ad_core::error::{ADError, ADResult};
use ad_core::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::file_base::{NDFileMode, NDFileWriter, NDPluginFileBase};
use ad_core::plugin::runtime::NDPluginProcess;

/// HDF5 file writer.
/// This is a stub implementation that writes binary data in a simple format.
/// Production version would use the `hdf5` crate for proper HDF5 I/O.
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
            let size = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            dims.push(NDDimension::new(size));
            pos += 4;
        }

        let dtype_ord = data[pos];
        pos += 1;
        let data_size = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;

        let dtype = NDDataType::from_ordinal(dtype_ord)
            .ok_or_else(|| ADError::UnsupportedConversion("invalid data type".into()))?;

        let raw = &data[pos..pos + data_size];

        let buf = match dtype {
            NDDataType::UInt8 => NDDataBuffer::U8(raw.to_vec()),
            _ => NDDataBuffer::U8(raw.to_vec()), // simplified
        };

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

/// HDF5 file processor wrapping NDPluginFileBase + Hdf5Writer.
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
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        let _ = self
            .file_base
            .process_array(Arc::new(array.clone()), &mut self.writer);
        vec![] // file plugins are sinks
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
}
