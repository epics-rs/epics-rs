use std::path::{Path, PathBuf};

use ad_core_rs::color::{NDColorMode, convert_rgb_layout};
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
    pub(crate) quality: u8,
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
        let dt = array.data.data_type();
        if dt != NDDataType::UInt8 && dt != NDDataType::Int8 {
            return Err(ADError::UnsupportedConversion(
                "JPEG only supports UInt8/Int8 data".into(),
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

        // The ColorMode *attribute* is the only source of truth, defaulting to
        // Mono when it is absent — C `int colorMode = NDColorModeMono;`
        // (NDFileJPEG.cpp:29) overwritten only by the attribute (:52-53).
        // `info().color_mode` is that rule's single owner.
        let color_mode = array.info().color_mode;

        // C's `openFile` structure chain (NDFileJPEG.cpp:55-84). Each 3-D branch
        // requires the *attribute* to name the layout, so a 3-D array with no
        // ColorMode attribute (colorMode stays Mono) matches nothing and C returns
        // asynError (:79-84). Inferring the layout from the dimensions instead
        // made such an array look like RGB1 and write a file.
        //
        // C also takes `this->colorMode` from the branch it took, not from the
        // attribute: the 2-D branch forces Mono (:60). And unlike NDFileTIFF
        // (:180) there is no ndims == 1 branch here — a 1-D array is an error.
        let color_mode = match array.dims.as_slice() {
            [_, _] => NDColorMode::Mono,
            [c, _, _] if c.size == 3 && color_mode == NDColorMode::RGB1 => NDColorMode::RGB1,
            [_, c, _] if c.size == 3 && color_mode == NDColorMode::RGB2 => NDColorMode::RGB2,
            [_, _, c] if c.size == 3 && color_mode == NDColorMode::RGB3 => NDColorMode::RGB3,
            _ => {
                return Err(ADError::InvalidDimensions(
                    "unsupported array structure".into(),
                ));
            }
        };

        let is_rgb = matches!(
            color_mode,
            NDColorMode::RGB1 | NDColorMode::RGB2 | NDColorMode::RGB3
        );
        let src = if is_rgb && color_mode != NDColorMode::RGB1 {
            &convert_rgb_layout(array, color_mode, NDColorMode::RGB1)?
        } else {
            array
        };

        let info = src.info();
        let width = info.x_size;
        let height = info.y_size;

        let data: Vec<u8> = match &src.data {
            NDDataBuffer::U8(v) => v.clone(),
            NDDataBuffer::I8(v) => v.iter().map(|&b| b as u8).collect(),
            _ => {
                return Err(ADError::UnsupportedConversion(
                    "JPEG only supports UInt8/Int8".into(),
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
            .encode(&data, width as u16, height as u16, color_type)
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

/// JPEG file processor wrapping `FilePluginController<JpegWriter>`.
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
        Self::new(50)
    }
}

impl NDPluginProcess for JpegFileProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        self.ctrl.process_array(array)
    }

    fn plugin_type(&self) -> &str {
        "NDFileJPEG"
    }

    /// C `NDPluginFile.cpp:948` (base of every file writer) sets
    /// `NDArrayCallbacks = 0`: file plugins write to disk, not downstream.
    fn does_array_callbacks(&self) -> bool {
        false
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        self.ctrl.register_params(base)?;
        use asyn_rs::param::ParamType;
        let idx = base.create_param("JPEG_QUALITY", ParamType::Int32)?;
        // Seed the readback PV with the actual encoder default (C++ NDFileJPEG.cpp:327
        // sets NDFileJPEGQuality default to 50). Without this the PV reads 0 while the
        // encoder uses its constructed default, so PV and effective quality disagree.
        base.set_int32_param(idx, 0, i32::from(self.ctrl.writer.quality))?;
        self.jpeg_quality_idx = Some(idx);
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

    /// R8-75, JPEG writer: same defect as the cited TIFF site. C defaults
    /// `colorMode` to Mono (NDFileJPEG.cpp:29), overwrites it only from the
    /// attribute (:52-53), and each 3-D branch requires the attribute (:61, :67,
    /// :73) — so a 3-D array without ColorMode returns asynError (:79-84). The
    /// port inferred RGB1 from the dims. A 1-D array is an error here too: C has
    /// no ndims == 1 branch in this writer.
    #[test]
    fn test_r8_75_3d_without_colormode_attribute_is_an_error() {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        use ad_core_rs::color::NDColorMode;

        let rgb1_dims = || {
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(4),
            ]
        };

        let arr = NDArray::new(rgb1_dims(), NDDataType::UInt8);
        let path = temp_path("jpeg_3d_no_colormode");
        let mut writer = JpegWriter::new(90);
        writer
            .open_file(&path, NDFileMode::Single, &arr)
            .expect("open");
        let err = writer.write_file(&arr).unwrap_err();
        assert!(
            matches!(err, ADError::InvalidDimensions(_)),
            "3-D without ColorMode must be rejected, got {err:?}"
        );
        std::fs::remove_file(&path).ok();

        // C has no ndims == 1 branch (unlike NDFileTIFF.cpp:180).
        let arr = NDArray::new(vec![NDDimension::new(16)], NDDataType::UInt8);
        let path = temp_path("jpeg_1d");
        let mut writer = JpegWriter::new(90);
        writer
            .open_file(&path, NDFileMode::Single, &arr)
            .expect("open");
        assert!(matches!(
            writer.write_file(&arr).unwrap_err(),
            ADError::InvalidDimensions(_)
        ));
        std::fs::remove_file(&path).ok();

        // Positive control: WITH ColorMode=RGB1 the same array writes.
        let mut arr = NDArray::new(rgb1_dims(), NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(NDColorMode::RGB1 as i32),
        ));
        let path = temp_path("jpeg_3d_rgb1");
        let mut writer = JpegWriter::new(90);
        writer
            .open_file(&path, NDFileMode::Single, &arr)
            .expect("open");
        writer
            .write_file(&arr)
            .expect("3-D WITH ColorMode=RGB1 must still write");
        assert!(path.exists());
        std::fs::remove_file(&path).ok();
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
    fn test_default_quality_is_50() {
        // C++ NDFileJPEG.cpp:327 default quality is 50.
        assert_eq!(JpegFileProcessor::default().ctrl.writer.quality, 50);
        assert_eq!(JpegWriter::new(50).quality, 50);
    }

    #[test]
    fn test_register_params_seeds_quality_pv() {
        use asyn_rs::port::{PortDriverBase, PortFlags};
        let mut base = PortDriverBase::new("jpeg_param_test", 1, PortFlags::default());
        let mut proc = JpegFileProcessor::new(50);
        proc.register_params(&mut base).unwrap();
        let idx = proc.jpeg_quality_idx.unwrap();
        // Readback PV must equal the encoder's effective quality, not 0.
        assert_eq!(base.get_int32_param(idx, 0).unwrap(), 50);
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

    /// Write an RGB-mode array to JPEG and return the decoded array's dims.
    fn jpeg_roundtrip_dims(prefix: &str, mode: NDColorMode, dims: Vec<usize>) -> Vec<usize> {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        let path = temp_path(prefix);
        let mut writer = JpegWriter::new(95);

        let mut arr = NDArray::new(
            dims.iter().map(|&d| NDDimension::new(d)).collect(),
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "Color mode",
            NDAttrSource::Driver,
            NDAttrValue::Int32(mode as i32),
        ));
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i % 256) as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        let read_back = writer.read_file().unwrap();
        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
        read_back.dims.iter().map(|d| d.size).collect()
    }

    #[test]
    fn test_adp12_rgb2_jpeg_written_as_rgb_not_grayscale() {
        // RGB2 [x=5, c=3, y=4]: C writes width=5, height=4, 3 JCS_RGB
        // components (NDFileJPEG.cpp:67-78). The Rust converts to RGB1 first;
        // before the fix the stale ColorMode=RGB2 attribute made info() read
        // the RGB1 dims as width=3, color=5 -> a 3x4 grayscale JPEG. Decoded
        // dims must be RGB24 [3, 5, 4], not grayscale [3, 4].
        let dims = jpeg_roundtrip_dims("jpeg_rgb2", NDColorMode::RGB2, vec![5, 3, 4]);
        assert_eq!(dims, vec![3, 5, 4]);
    }

    #[test]
    fn test_adp12_rgb3_jpeg_written_as_rgb_not_grayscale() {
        // RGB3 [x=5, y=4, c=3]: C writes width=5, height=4, 3 components
        // (NDFileJPEG.cpp:72-78). Decoded dims must be RGB24 [3, 5, 4].
        let dims = jpeg_roundtrip_dims("jpeg_rgb3", NDColorMode::RGB3, vec![5, 4, 3]);
        assert_eq!(dims, vec![3, 5, 4]);
    }
}
