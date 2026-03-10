use std::path::{Path, PathBuf};
use std::sync::Arc;

use ad_core::error::{ADError, ADResult};
use ad_core::ndarray::{NDArray, NDDataType, NDDimension};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::file_base::{NDFileMode, NDFileWriter, NDPluginFileBase};
use ad_core::plugin::runtime::NDPluginProcess;

/// TIFF file writer.
/// Production version would use the `image` crate's TiffEncoder.
/// This implementation writes raw data with a minimal TIFF header.
pub struct TiffWriter {
    current_path: Option<PathBuf>,
}

impl TiffWriter {
    pub fn new() -> Self {
        Self { current_path: None }
    }
}

impl NDFileWriter for TiffWriter {
    fn open_file(&mut self, path: &Path, _mode: NDFileMode, _array: &NDArray) -> ADResult<()> {
        self.current_path = Some(path.to_path_buf());
        Ok(())
    }

    fn write_file(&mut self, array: &NDArray) -> ADResult<()> {
        let path = self.current_path.as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let info = array.info();
        let raw_data = array.data.as_u8_slice();

        // Write minimal TIFF (for testing; production would use image crate)
        write_minimal_tiff(path, raw_data, info.x_size, info.y_size, array.data.data_type())?;
        Ok(())
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        let path = self.current_path.as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let data = std::fs::read(path)?;
        // Simple: just read raw data back (skip header for testing)
        let arr = NDArray::new(vec![NDDimension::new(data.len())], NDDataType::UInt8);
        Ok(arr)
    }

    fn close_file(&mut self) -> ADResult<()> {
        self.current_path = None;
        Ok(())
    }

    fn supports_multiple_arrays(&self) -> bool {
        false
    }
}

fn write_minimal_tiff(path: &Path, data: &[u8], width: usize, height: usize, _dtype: NDDataType) -> ADResult<()> {
    use std::io::Write;

    let mut file = std::fs::File::create(path)?;

    // Minimal TIFF header (little-endian)
    let ifd_offset: u32 = 8;
    file.write_all(&[0x49, 0x49])?; // Little-endian
    file.write_all(&42u16.to_le_bytes())?; // Magic
    file.write_all(&ifd_offset.to_le_bytes())?;

    // IFD with minimal entries
    let num_entries: u16 = 6;
    let data_offset: u32 = 8 + 2 + num_entries as u32 * 12 + 4;

    file.write_all(&num_entries.to_le_bytes())?;

    // ImageWidth (tag 256)
    write_ifd_entry(&mut file, 256, 3, 1, width as u32)?;
    // ImageLength (tag 257)
    write_ifd_entry(&mut file, 257, 3, 1, height as u32)?;
    // BitsPerSample (tag 258)
    write_ifd_entry(&mut file, 258, 3, 1, 8)?;
    // Compression (tag 259) - none
    write_ifd_entry(&mut file, 259, 3, 1, 1)?;
    // StripOffsets (tag 273)
    write_ifd_entry(&mut file, 273, 4, 1, data_offset)?;
    // StripByteCounts (tag 279)
    write_ifd_entry(&mut file, 279, 4, 1, data.len() as u32)?;

    // Next IFD = 0 (none)
    file.write_all(&0u32.to_le_bytes())?;

    // Image data
    file.write_all(data)?;

    Ok(())
}

fn write_ifd_entry(file: &mut std::fs::File, tag: u16, dtype: u16, count: u32, value: u32) -> ADResult<()> {
    use std::io::Write;
    file.write_all(&tag.to_le_bytes())?;
    file.write_all(&dtype.to_le_bytes())?;
    file.write_all(&count.to_le_bytes())?;
    file.write_all(&value.to_le_bytes())?;
    Ok(())
}

/// TIFF file processor wrapping NDPluginFileBase + TiffWriter.
pub struct TiffFileProcessor {
    file_base: NDPluginFileBase,
    writer: TiffWriter,
}

impl TiffFileProcessor {
    pub fn new() -> Self {
        Self {
            file_base: NDPluginFileBase::new(),
            writer: TiffWriter::new(),
        }
    }

    pub fn file_base_mut(&mut self) -> &mut NDPluginFileBase {
        &mut self.file_base
    }
}

impl Default for TiffFileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for TiffFileProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        let _ = self
            .file_base
            .process_array(Arc::new(array.clone()), &mut self.writer);
        vec![] // file plugins are sinks
    }

    fn plugin_type(&self) -> &str {
        "NDFileTIFF"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core::ndarray::NDDataBuffer;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(prefix: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("adcore_test_{}_{}.tif", prefix, n))
    }

    #[test]
    fn test_write_u8_mono() {
        let path = temp_path("tiff_u8");
        let mut writer = TiffWriter::new();

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

        // Verify file exists and has content
        let data = std::fs::read(&path).unwrap();
        assert!(data.len() > 16); // header + data
        // Check TIFF magic
        assert_eq!(&data[0..2], &[0x49, 0x49]);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_u16() {
        let path = temp_path("tiff_u16");
        let mut writer = TiffWriter::new();

        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let data = std::fs::read(&path).unwrap();
        assert!(data.len() > 32); // 16 elements * 2 bytes + header

        std::fs::remove_file(&path).ok();
    }
}
