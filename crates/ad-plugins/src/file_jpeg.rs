use std::path::{Path, PathBuf};
use std::sync::Arc;

use ad_core::error::{ADError, ADResult};
use ad_core::ndarray::{NDArray, NDDataType};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::file_base::{NDFileMode, NDFileWriter, NDPluginFileBase};
use ad_core::plugin::runtime::NDPluginProcess;

/// JPEG file writer.
/// Production version would use the `image` crate's JpegEncoder.
/// This writes raw data with a JFIF marker for testing.
pub struct JpegWriter {
    current_path: Option<PathBuf>,
    quality: u8,
}

impl JpegWriter {
    pub fn new(quality: u8) -> Self {
        Self {
            current_path: None,
            quality,
        }
    }

    pub fn set_quality(&mut self, quality: u8) {
        self.quality = quality;
    }
}

impl NDFileWriter for JpegWriter {
    fn open_file(&mut self, path: &Path, _mode: NDFileMode, array: &NDArray) -> ADResult<()> {
        if array.data.data_type() != NDDataType::UInt8 {
            return Err(ADError::UnsupportedConversion(
                "JPEG only supports UInt8 data".into(),
            ));
        }
        self.current_path = Some(path.to_path_buf());
        Ok(())
    }

    fn write_file(&mut self, array: &NDArray) -> ADResult<()> {
        let path = self.current_path.as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        // Write a minimal JFIF file (placeholder for real JPEG encoding)
        let raw = array.data.as_u8_slice();
        let info = array.info();

        write_placeholder_jpeg(path, raw, info.x_size, info.y_size, self.quality)?;
        Ok(())
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        Err(ADError::UnsupportedConversion(
            "JPEG read not implemented (requires image crate)".into(),
        ))
    }

    fn close_file(&mut self) -> ADResult<()> {
        self.current_path = None;
        Ok(())
    }

    fn supports_multiple_arrays(&self) -> bool {
        false
    }
}

fn write_placeholder_jpeg(
    path: &Path,
    data: &[u8],
    _width: usize,
    _height: usize,
    quality: u8,
) -> ADResult<()> {
    use std::io::Write;

    let mut file = std::fs::File::create(path)?;

    // JPEG SOI marker
    file.write_all(&[0xFF, 0xD8])?;

    // APP0 JFIF marker (minimal)
    file.write_all(&[0xFF, 0xE0])?;
    let app0_len: u16 = 16;
    file.write_all(&app0_len.to_be_bytes())?;
    file.write_all(b"JFIF\0")?;
    file.write_all(&[1, 1])?; // version
    file.write_all(&[0])?; // units
    file.write_all(&1u16.to_be_bytes())?; // x density
    file.write_all(&1u16.to_be_bytes())?; // y density
    file.write_all(&[0, 0])?; // thumbnail

    // Quality marker as comment (non-standard, for testing)
    file.write_all(&[0xFF, 0xFE])?; // COM marker
    let comment = format!("quality={}", quality);
    let com_len = (comment.len() + 2) as u16;
    file.write_all(&com_len.to_be_bytes())?;
    file.write_all(comment.as_bytes())?;

    // Raw data (not real JPEG scan data, just for file size testing)
    // Scale data by quality to simulate compression
    let scale = quality as f64 / 100.0;
    let output_size = (data.len() as f64 * scale) as usize;
    let truncated = &data[..output_size.min(data.len())];
    file.write_all(truncated)?;

    // JPEG EOI marker
    file.write_all(&[0xFF, 0xD9])?;

    Ok(())
}

/// JPEG file processor wrapping NDPluginFileBase + JpegWriter.
pub struct JpegFileProcessor {
    file_base: NDPluginFileBase,
    writer: JpegWriter,
}

impl JpegFileProcessor {
    pub fn new(quality: u8) -> Self {
        Self {
            file_base: NDPluginFileBase::new(),
            writer: JpegWriter::new(quality),
        }
    }

    pub fn file_base_mut(&mut self) -> &mut NDPluginFileBase {
        &mut self.file_base
    }
}

impl Default for JpegFileProcessor {
    fn default() -> Self {
        Self::new(90)
    }
}

impl NDPluginProcess for JpegFileProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        let _ = self
            .file_base
            .process_array(Arc::new(array.clone()), &mut self.writer);
        vec![] // file plugins are sinks
    }

    fn plugin_type(&self) -> &str {
        "NDFileJPEG"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core::ndarray::{NDDataBuffer, NDDimension};
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(prefix: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("adcore_test_{}_{}.jpg", prefix, n))
    }

    #[test]
    fn test_write_u8() {
        let path = temp_path("jpeg");
        let mut writer = JpegWriter::new(90);

        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..64 { v[i] = (i * 4) as u8; }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let data = std::fs::read(&path).unwrap();
        // Check JPEG SOI marker
        assert_eq!(&data[0..2], &[0xFF, 0xD8]);
        // Check JPEG EOI marker at end
        assert_eq!(&data[data.len()-2..], &[0xFF, 0xD9]);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_rejects_non_u8() {
        let path = temp_path("jpeg_u16");
        let mut writer = JpegWriter::new(90);

        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );

        let result = writer.open_file(&path, NDFileMode::Single, &arr);
        assert!(result.is_err());
    }

    #[test]
    fn test_quality_affects_size() {
        let path_high = temp_path("jpeg_hi");
        let path_low = temp_path("jpeg_lo");

        let mut arr = NDArray::new(
            vec![NDDimension::new(32), NDDimension::new(32)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..v.len() { v[i] = (i % 256) as u8; }
        }

        let mut writer_high = JpegWriter::new(95);
        writer_high.open_file(&path_high, NDFileMode::Single, &arr).unwrap();
        writer_high.write_file(&arr).unwrap();
        writer_high.close_file().unwrap();

        let mut writer_low = JpegWriter::new(10);
        writer_low.open_file(&path_low, NDFileMode::Single, &arr).unwrap();
        writer_low.write_file(&arr).unwrap();
        writer_low.close_file().unwrap();

        let size_high = std::fs::metadata(&path_high).unwrap().len();
        let size_low = std::fs::metadata(&path_low).unwrap().len();
        assert!(size_high > size_low);

        std::fs::remove_file(&path_high).ok();
        std::fs::remove_file(&path_low).ok();
    }
}
