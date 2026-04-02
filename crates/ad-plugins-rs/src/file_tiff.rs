use std::path::{Path, PathBuf};

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
use ad_core_rs::color::{NDColorMode, convert_rgb_layout};
use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{NDFileMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::FilePluginController;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamChangeResult, PluginParamSnapshot, ProcessResult,
};

use tiff::ColorType;
use tiff::decoder::Decoder;
use tiff::encoder::TiffEncoder;
use tiff::encoder::colortype;

/// TIFF file writer using the `tiff` crate for proper encoding/decoding.
pub struct TiffWriter {
    current_path: Option<PathBuf>,
}

impl TiffWriter {
    pub fn new() -> Self {
        Self { current_path: None }
    }

    fn array_color_mode(array: &NDArray) -> NDColorMode {
        array
            .attributes
            .get("ColorMode")
            .and_then(|attr| attr.value.as_i64())
            .map(|v| NDColorMode::from_i32(v as i32))
            .unwrap_or_else(|| match array.dims.as_slice() {
                [a, _, _] if a.size == 3 => NDColorMode::RGB1,
                [_, b, _] if b.size == 3 => NDColorMode::RGB2,
                [_, _, c] if c.size == 3 => NDColorMode::RGB3,
                _ => NDColorMode::Mono,
            })
    }

    fn normalize_for_write(array: &NDArray) -> ADResult<(NDArray, u32, u32, bool)> {
        match array.dims.as_slice() {
            [x] => {
                let mut normalized = NDArray::new(
                    vec![NDDimension::new(x.size), NDDimension::new(1)],
                    array.data.data_type(),
                );
                normalized.data = array.data.clone();
                normalized.unique_id = array.unique_id;
                normalized.timestamp = array.timestamp;
                normalized.attributes = array.attributes.clone();
                normalized.codec = array.codec.clone();
                Ok((normalized, x.size as u32, 1, false))
            }
            [x, y] => Ok((array.clone(), x.size as u32, y.size as u32, false)),
            [_, _, _] => {
                let color_mode = Self::array_color_mode(array);
                let rgb1 = match color_mode {
                    NDColorMode::RGB1 => array.clone(),
                    NDColorMode::RGB2 | NDColorMode::RGB3 => {
                        convert_rgb_layout(array, color_mode, NDColorMode::RGB1)?
                    }
                    other => {
                        return Err(ADError::UnsupportedConversion(format!(
                            "unsupported TIFF color mode: {:?}",
                            other
                        )));
                    }
                };
                Ok((
                    rgb1.clone(),
                    rgb1.dims[1].size as u32,
                    rgb1.dims[2].size as u32,
                    true,
                ))
            }
            _ => Err(ADError::InvalidDimensions(
                "unsupported TIFF array dimensions".into(),
            )),
        }
    }

    fn attach_color_mode(array: &mut NDArray, color_mode: NDColorMode) {
        array.attributes.add(NDAttribute {
            name: "ColorMode".into(),
            description: "Color mode".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Int32(color_mode as i32),
        });
    }
}

impl NDFileWriter for TiffWriter {
    fn open_file(&mut self, path: &Path, _mode: NDFileMode, _array: &NDArray) -> ADResult<()> {
        self.current_path = Some(path.to_path_buf());
        Ok(())
    }

    fn write_file(&mut self, array: &NDArray) -> ADResult<()> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;
        let (array, width, height, is_rgb) = Self::normalize_for_write(array)?;

        let file = std::fs::File::create(path)?;
        let mut encoder = TiffEncoder::new(file)
            .map_err(|e| ADError::UnsupportedConversion(format!("TIFF encoder error: {}", e)))?;

        match &array.data {
            NDDataBuffer::U8(v) => {
                if is_rgb {
                    encoder.write_image::<colortype::RGB8>(width, height, v)
                } else {
                    encoder.write_image::<colortype::Gray8>(width, height, v)
                }
            }
            NDDataBuffer::I8(v) => {
                if is_rgb {
                    return Err(ADError::UnsupportedConversion(
                        "TIFF crate does not support signed RGB8".into(),
                    ));
                }
                encoder.write_image::<colortype::GrayI8>(width, height, v)
            }
            NDDataBuffer::U16(v) => {
                if is_rgb {
                    encoder.write_image::<colortype::RGB16>(width, height, v)
                } else {
                    encoder.write_image::<colortype::Gray16>(width, height, v)
                }
            }
            NDDataBuffer::I16(v) => {
                if is_rgb {
                    return Err(ADError::UnsupportedConversion(
                        "TIFF crate does not support signed RGB16".into(),
                    ));
                }
                encoder.write_image::<colortype::GrayI16>(width, height, v)
            }
            NDDataBuffer::U32(v) => {
                if is_rgb {
                    encoder.write_image::<colortype::RGB32>(width, height, v)
                } else {
                    encoder.write_image::<colortype::Gray32>(width, height, v)
                }
            }
            NDDataBuffer::I32(v) => {
                if is_rgb {
                    return Err(ADError::UnsupportedConversion(
                        "TIFF crate does not support signed RGB32".into(),
                    ));
                }
                encoder.write_image::<colortype::GrayI32>(width, height, v)
            }
            NDDataBuffer::I64(v) => {
                if is_rgb {
                    return Err(ADError::UnsupportedConversion(
                        "TIFF crate does not support signed RGB64".into(),
                    ));
                }
                encoder.write_image::<colortype::GrayI64>(width, height, v)
            }
            NDDataBuffer::U64(v) => {
                if is_rgb {
                    encoder.write_image::<colortype::RGB64>(width, height, v)
                } else {
                    encoder.write_image::<colortype::Gray64>(width, height, v)
                }
            }
            NDDataBuffer::F32(v) => {
                if is_rgb {
                    encoder.write_image::<colortype::RGB32Float>(width, height, v)
                } else {
                    encoder.write_image::<colortype::Gray32Float>(width, height, v)
                }
            }
            NDDataBuffer::F64(v) => {
                if is_rgb {
                    encoder.write_image::<colortype::RGB64Float>(width, height, v)
                } else {
                    encoder.write_image::<colortype::Gray64Float>(width, height, v)
                }
            }
        }
        .map_err(|e| ADError::UnsupportedConversion(format!("TIFF write error: {}", e)))?;

        Ok(())
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let file = std::fs::File::open(path)?;
        let mut decoder = Decoder::new(file)
            .map_err(|e| ADError::UnsupportedConversion(format!("TIFF decode error: {}", e)))?;

        let (width, height) = decoder
            .dimensions()
            .map_err(|e| ADError::UnsupportedConversion(format!("TIFF dimensions error: {}", e)))?;
        let color_type = decoder
            .colortype()
            .map_err(|e| ADError::UnsupportedConversion(format!("TIFF colortype error: {}", e)))?;

        let result = decoder
            .read_image()
            .map_err(|e| ADError::UnsupportedConversion(format!("TIFF read error: {}", e)))?;

        let (dims, color_mode) = match color_type {
            ColorType::Gray(_) => (
                vec![
                    NDDimension::new(width as usize),
                    NDDimension::new(height as usize),
                ],
                NDColorMode::Mono,
            ),
            ColorType::RGB(_) => (
                vec![
                    NDDimension::new(3),
                    NDDimension::new(width as usize),
                    NDDimension::new(height as usize),
                ],
                NDColorMode::RGB1,
            ),
            other => {
                return Err(ADError::UnsupportedConversion(format!(
                    "unsupported TIFF color type: {:?}",
                    other
                )));
            }
        };

        let mut array = match result {
            tiff::decoder::DecodingResult::U8(data) => {
                let mut arr = NDArray::new(dims.clone(), NDDataType::UInt8);
                arr.data = NDDataBuffer::U8(data);
                arr
            }
            tiff::decoder::DecodingResult::U16(data) => {
                let mut arr = NDArray::new(dims.clone(), NDDataType::UInt16);
                arr.data = NDDataBuffer::U16(data);
                arr
            }
            tiff::decoder::DecodingResult::U32(data) => {
                let mut arr = NDArray::new(dims.clone(), NDDataType::UInt32);
                arr.data = NDDataBuffer::U32(data);
                arr
            }
            tiff::decoder::DecodingResult::U64(data) => {
                let mut arr = NDArray::new(dims.clone(), NDDataType::UInt64);
                arr.data = NDDataBuffer::U64(data);
                arr
            }
            tiff::decoder::DecodingResult::I8(data) => {
                let mut arr = NDArray::new(dims.clone(), NDDataType::Int8);
                arr.data = NDDataBuffer::I8(data);
                arr
            }
            tiff::decoder::DecodingResult::I16(data) => {
                let mut arr = NDArray::new(dims.clone(), NDDataType::Int16);
                arr.data = NDDataBuffer::I16(data);
                arr
            }
            tiff::decoder::DecodingResult::I32(data) => {
                let mut arr = NDArray::new(dims.clone(), NDDataType::Int32);
                arr.data = NDDataBuffer::I32(data);
                arr
            }
            tiff::decoder::DecodingResult::I64(data) => {
                let mut arr = NDArray::new(dims.clone(), NDDataType::Int64);
                arr.data = NDDataBuffer::I64(data);
                arr
            }
            tiff::decoder::DecodingResult::F32(data) => {
                let mut arr = NDArray::new(dims.clone(), NDDataType::Float32);
                arr.data = NDDataBuffer::F32(data);
                arr
            }
            tiff::decoder::DecodingResult::F64(data) => {
                let mut arr = NDArray::new(dims.clone(), NDDataType::Float64);
                arr.data = NDDataBuffer::F64(data);
                arr
            }
        };
        Self::attach_color_mode(&mut array, color_mode);
        Ok(array)
    }

    fn close_file(&mut self) -> ADResult<()> {
        self.current_path = None;
        Ok(())
    }

    fn supports_multiple_arrays(&self) -> bool {
        false
    }
}

/// TIFF file processor wrapping FilePluginController<TiffWriter>.
pub struct TiffFileProcessor {
    pub ctrl: FilePluginController<TiffWriter>,
}

impl TiffFileProcessor {
    pub fn new() -> Self {
        Self {
            ctrl: FilePluginController::new(TiffWriter::new()),
        }
    }
}

impl Default for TiffFileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for TiffFileProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        self.ctrl.process_array(array)
    }

    fn plugin_type(&self) -> &str {
        "NDFileTIFF"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        self.ctrl.register_params(base)
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        params: &PluginParamSnapshot,
    ) -> ParamChangeResult {
        self.ctrl.on_param_change(reason, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::NDDataBuffer;
    use ad_core_rs::params::ndarray_driver::NDArrayDriverParams;
    use ad_core_rs::plugin::runtime::{ParamChangeValue, ParamUpdate, PluginParamSnapshot};
    use asyn_rs::port::{PortDriverBase, PortFlags};
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
        if let NDDataBuffer::U8(v) = &mut arr.data {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let data = std::fs::read(&path).unwrap();
        assert!(data.len() > 16);
        assert!(
            &data[0..2] == &[0x49, 0x49] || &data[0..2] == &[0x4D, 0x4D],
            "Expected TIFF magic bytes"
        );

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
        assert!(data.len() > 32);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_roundtrip_u8() {
        let path = temp_path("tiff_rt_u8");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(v) = &mut arr.data {
            for i in 0..16 {
                v[i] = (i * 10) as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();

        let read_back = writer.read_file().unwrap();
        if let (NDDataBuffer::U8(orig), NDDataBuffer::U8(read)) = (&arr.data, &read_back.data) {
            assert_eq!(orig, read);
        } else {
            panic!("data type mismatch on roundtrip");
        }

        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_roundtrip_u16() {
        let path = temp_path("tiff_rt_u16");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(v) = &mut arr.data {
            for i in 0..16 {
                v[i] = (i * 1000) as u16;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();

        let read_back = writer.read_file().unwrap();
        if let (NDDataBuffer::U16(orig), NDDataBuffer::U16(read)) = (&arr.data, &read_back.data) {
            assert_eq!(orig, read);
        } else {
            panic!("data type mismatch on roundtrip");
        }

        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_on_param_change_read_file_emits_array_and_resets_busy() {
        let path = temp_path("tiff_read_param");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(3)],
            NDDataType::UInt8,
        );
        arr.unique_id = 77;
        if let NDDataBuffer::U8(v) = &mut arr.data {
            for (i, item) in v.iter_mut().enumerate() {
                *item = i as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut base = PortDriverBase::new("TIFFTEST", 1, PortFlags::default());
        let _nd_params = NDArrayDriverParams::create(&mut base).unwrap();

        let mut proc = TiffFileProcessor::new();
        proc.register_params(&mut base).unwrap();

        let reason_path = base.find_param("FILE_PATH").unwrap();
        let reason_name = base.find_param("FILE_NAME").unwrap();
        let reason_template = base.find_param("FILE_TEMPLATE").unwrap();
        let reason_read = base.find_param("READ_FILE").unwrap();

        let _ = proc.on_param_change(
            reason_path,
            &PluginParamSnapshot {
                enable_callbacks: true,
                reason: reason_path,
                addr: 0,
                value: ParamChangeValue::Octet(
                    path.parent().unwrap().to_str().unwrap().to_string(),
                ),
            },
        );
        let _ = proc.on_param_change(
            reason_name,
            &PluginParamSnapshot {
                enable_callbacks: true,
                reason: reason_name,
                addr: 0,
                value: ParamChangeValue::Octet(
                    path.file_name().unwrap().to_str().unwrap().to_string(),
                ),
            },
        );
        let _ = proc.on_param_change(
            reason_template,
            &PluginParamSnapshot {
                enable_callbacks: true,
                reason: reason_template,
                addr: 0,
                value: ParamChangeValue::Octet("%s%s".into()),
            },
        );

        let result = proc.on_param_change(
            reason_read,
            &PluginParamSnapshot {
                enable_callbacks: true,
                reason: reason_read,
                addr: 0,
                value: ParamChangeValue::Int32(1),
            },
        );

        assert_eq!(result.output_arrays.len(), 1);
        assert!(result.param_updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Int32 { reason, value: 0, .. } if *reason == reason_read
        )));
        match &result.output_arrays[0].data {
            NDDataBuffer::U8(v) => assert_eq!(v.len(), 12),
            other => panic!("unexpected data buffer: {other:?}"),
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_single_mode_requires_auto_save_for_automatic_write() {
        let path = temp_path("tiff_autosave_single");
        let full_name = path.to_string_lossy().to_string();
        let file_path = path.parent().unwrap().to_str().unwrap().to_string();
        let file_name = path.file_name().unwrap().to_str().unwrap().to_string();

        let mut proc = TiffFileProcessor::new();
        proc.ctrl.file_base.file_path = file_path.clone() + "/";
        proc.ctrl.file_base.file_name = file_name;
        proc.ctrl.file_base.file_template = "%s%s".into();
        proc.ctrl.file_base.set_mode(NDFileMode::Single);

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(v) = &mut arr.data {
            for (i, item) in v.iter_mut().enumerate() {
                *item = i as u8;
            }
        }

        proc.ctrl.auto_save = false;
        let _ = proc.process_array(&arr, &NDArrayPool::new(1024));
        assert!(!std::path::Path::new(&full_name).exists());

        proc.ctrl.auto_save = true;
        let _ = proc.process_array(&arr, &NDArrayPool::new(1024));
        assert!(std::path::Path::new(&full_name).exists());

        std::fs::remove_file(&path).ok();
    }
}
