use std::path::{Path, PathBuf};

use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{NDFileMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::FilePluginController;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamChangeResult, PluginParamSnapshot, ProcessResult,
};

use jpeg_encoder::{ColorType as JpegColorType, Encoder as JpegEncoder};

/// JPEG file writer using `jpeg-encoder` for encoding and `jpeg-decoder` for decoding.
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
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let info = array.info();
        let width = info.x_size;
        let height = info.y_size;

        let data = match &array.data {
            NDDataBuffer::U8(v) => v.as_slice(),
            _ => {
                return Err(ADError::UnsupportedConversion(
                    "JPEG only supports UInt8".into(),
                ));
            }
        };

        let color_type = if info.color_size == 3 {
            JpegColorType::Rgb
        } else {
            JpegColorType::Luma
        };

        let mut buf = Vec::new();
        let encoder = JpegEncoder::new(&mut buf, self.quality);
        encoder
            .encode(data, width as u16, height as u16, color_type)
            .map_err(|e| ADError::UnsupportedConversion(format!("JPEG encode error: {}", e)))?;

        std::fs::write(path, &buf)?;
        Ok(())
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let file_data = std::fs::read(path)?;
        let mut decoder = jpeg_decoder::Decoder::new(&file_data[..]);
        let pixels = decoder
            .decode()
            .map_err(|e| ADError::UnsupportedConversion(format!("JPEG decode error: {}", e)))?;
        let info = decoder.info().unwrap();

        let (width, height) = (info.width as usize, info.height as usize);

        let dims = match info.pixel_format {
            jpeg_decoder::PixelFormat::L8 => {
                vec![NDDimension::new(width), NDDimension::new(height)]
            }
            jpeg_decoder::PixelFormat::RGB24 => {
                vec![
                    NDDimension::new(3),
                    NDDimension::new(width),
                    NDDimension::new(height),
                ]
            }
            _ => {
                return Err(ADError::UnsupportedConversion(
                    "unsupported JPEG pixel format".into(),
                ));
            }
        };

        let mut arr = NDArray::new(dims, NDDataType::UInt8);
        arr.data = NDDataBuffer::U8(pixels);
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

/// JPEG file processor wrapping FilePluginController<JpegWriter>.
pub struct JpegFileProcessor {
    ctrl: FilePluginController<JpegWriter>,
    jpeg_quality_idx: Option<usize>,
}

impl JpegFileProcessor {
    pub fn new(quality: u8) -> Self {
        Self {
            ctrl: FilePluginController::new(JpegWriter::new(quality)),
            jpeg_quality_idx: None,
        }
    }
}

impl Default for JpegFileProcessor {
    fn default() -> Self {
        Self::new(90)
    }
}

impl NDPluginProcess for JpegFileProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        self.ctrl.process_array(array)
    }

    fn plugin_type(&self) -> &str {
        "NDFileJPEG"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        self.ctrl.register_params(base)?;
        use asyn_rs::param::ParamType;
        self.jpeg_quality_idx = Some(base.create_param("JPEG_QUALITY", ParamType::Int32)?);
        Ok(())
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        params: &PluginParamSnapshot,
    ) -> ParamChangeResult {
        // JPEG-specific: quality change
        if Some(reason) == self.jpeg_quality_idx {
            let q = params.value.as_i32().clamp(1, 100) as u8;
            self.ctrl.writer.set_quality(q);
            return ParamChangeResult::empty();
        }
        self.ctrl.on_param_change(reason, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataBuffer, NDDimension};
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
            for i in 0..64 {
                v[i] = (i * 4) as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let data = std::fs::read(&path).unwrap();
        // Check JPEG SOI marker
        assert_eq!(&data[0..2], &[0xFF, 0xD8]);
        // Check JPEG EOI marker at end
        assert_eq!(&data[data.len() - 2..], &[0xFF, 0xD9]);

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
            for i in 0..v.len() {
                v[i] = (i % 256) as u8;
            }
        }

        let mut writer_high = JpegWriter::new(95);
        writer_high
            .open_file(&path_high, NDFileMode::Single, &arr)
            .unwrap();
        writer_high.write_file(&arr).unwrap();
        writer_high.close_file().unwrap();

        let mut writer_low = JpegWriter::new(10);
        writer_low
            .open_file(&path_low, NDFileMode::Single, &arr)
            .unwrap();
        writer_low.write_file(&arr).unwrap();
        writer_low.close_file().unwrap();

        let size_high = std::fs::metadata(&path_high).unwrap().len();
        let size_low = std::fs::metadata(&path_low).unwrap().len();
        assert!(
            size_high > size_low,
            "high quality ({}) should be larger than low quality ({})",
            size_high,
            size_low
        );

        std::fs::remove_file(&path_high).ok();
        std::fs::remove_file(&path_low).ok();
    }

    #[test]
    fn test_roundtrip_luma() {
        let path = temp_path("jpeg_rt");
        let mut writer = JpegWriter::new(100);

        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            // Use uniform value so JPEG compression is lossless at quality 100
            for i in 0..64 {
                v[i] = 128;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();

        let read_back = writer.read_file().unwrap();
        assert_eq!(read_back.data.data_type(), NDDataType::UInt8);
        if let NDDataBuffer::U8(ref v) = read_back.data {
            // With uniform input at max quality, decoded values should be close
            for &px in v.iter() {
                assert!(
                    (px as i16 - 128).unsigned_abs() < 5,
                    "pixel {} too far from 128",
                    px
                );
            }
        }

        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }
}
