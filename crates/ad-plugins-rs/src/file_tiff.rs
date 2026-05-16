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
use tiff::tags::Tag;

// Custom TIFF tag numbers — must match C++ NDFileTIFF.cpp:37-41 exactly.
const TIFFTAG_NDTIMESTAMP: u16 = 65000;
const TIFFTAG_UNIQUEID: u16 = 65001;
const TIFFTAG_EPICSTSSEC: u16 = 65002;
const TIFFTAG_EPICSTSNSEC: u16 = 65003;
const TIFFTAG_FIRST_ATTRIBUTE: u16 = 65010;

/// Signed-RGB color types. The `tiff` crate only ships unsigned RGB
/// (`colortype::RGB8/16/32`); C++ libtiff handles signed RGB by setting
/// `SAMPLEFORMAT_INT` on an otherwise identical RGB layout. These local
/// `ColorType` impls do exactly that — same bit layout, `SampleFormat::Int`.
mod signed_rgb {
    use tiff::encoder::colortype::ColorType;
    use tiff::tags::{PhotometricInterpretation, SampleFormat};

    macro_rules! signed_rgb {
        ($name:ident, $inner:ty, $bits:expr) => {
            pub struct $name;
            impl ColorType for $name {
                type Inner = $inner;
                const TIFF_VALUE: PhotometricInterpretation = PhotometricInterpretation::RGB;
                const BITS_PER_SAMPLE: &'static [u16] = &[$bits, $bits, $bits];
                const SAMPLE_FORMAT: &'static [SampleFormat] =
                    &[SampleFormat::Int, SampleFormat::Int, SampleFormat::Int];
            }
        };
    }

    signed_rgb!(RGBI8, i8, 8);
    signed_rgb!(RGBI16, i16, 16);
    signed_rgb!(RGBI32, i32, 32);
    signed_rgb!(RGBI64, i64, 64);
}

/// Format an NDAttribute value as the C++ `epicsSnprintf` "name:value" tag
/// string (NDFileTIFF.cpp:303-327). Numeric values keep their type, not a
/// generic stringification: signed `%lld`, unsigned `%llu`, float `%f`.
fn attribute_tag_string(attr: &NDAttribute) -> String {
    let value = match &attr.value {
        NDAttrValue::Int8(v) => format!("{}", v),
        NDAttrValue::Int16(v) => format!("{}", v),
        NDAttrValue::Int32(v) => format!("{}", v),
        NDAttrValue::Int64(v) => format!("{}", v),
        NDAttrValue::UInt8(v) => format!("{}", v),
        NDAttrValue::UInt16(v) => format!("{}", v),
        NDAttrValue::UInt32(v) => format!("{}", v),
        NDAttrValue::UInt64(v) => format!("{}", v),
        // C++ uses "%f" which is 6 fractional digits.
        NDAttrValue::Float32(v) => format!("{:.6}", v),
        NDAttrValue::Float64(v) => format!("{:.6}", v),
        NDAttrValue::String(s) => s.clone(),
        NDAttrValue::Undefined => String::new(),
    };
    format!("{}:{}", attr.name, value)
}

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
        array.attributes.add(NDAttribute::new_static(
                "ColorMode",
                "Color mode",
                NDAttrSource::Driver,
                NDAttrValue::Int32(color_mode as i32),
            ));
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

        // Collect attribute tag data before borrowing encoder mutably.
        // C++ writes NDArray attributes as custom TIFF tags starting at tag
        // 65010 (TIFFTAG_FIRST_ATTRIBUTE), value format "name:value" (colon).
        let attr_tags: Vec<(u16, String)> = array
            .attributes
            .iter()
            .enumerate()
            .map(|(i, attr)| {
                let tag_num = TIFFTAG_FIRST_ATTRIBUTE.saturating_add(i as u16);
                (tag_num, attribute_tag_string(attr))
            })
            .collect();

        // Standard tags derived from well-known attributes (NDFileTIFF.cpp:243-271).
        let model = array
            .attributes
            .get("Model")
            .map(|a| a.value.as_string())
            .unwrap_or_else(|| "Unknown".to_string());
        let make = array
            .attributes
            .get("Manufacturer")
            .map(|a| a.value.as_string())
            .unwrap_or_else(|| "Unknown".to_string());
        let image_description = array
            .attributes
            .get("TIFFImageDescription")
            .map(|a| a.value.as_string());

        let unique_id = array.unique_id;
        let time_stamp = array.time_stamp;
        let ts_sec = array.timestamp.sec;
        let ts_nsec = array.timestamp.nsec;

        // Macro to reduce repetition: create image encoder, write custom tags, write data.
        macro_rules! write_with_tags {
            ($ct:ty, $data:expr) => {{
                let mut image = encoder.new_image::<$ct>(width, height).map_err(|e| {
                    ADError::UnsupportedConversion(format!("TIFF encoder error: {}", e))
                })?;

                macro_rules! tag {
                    ($tag:expr, $val:expr) => {
                        image.encoder().write_tag($tag, $val).map_err(|e| {
                            ADError::UnsupportedConversion(format!(
                                "TIFF tag write error: {}",
                                e
                            ))
                        })?;
                    };
                }

                // EPICS metadata tags 65000-65003 — typed values matching C++.
                tag!(Tag::Unknown(TIFFTAG_NDTIMESTAMP), time_stamp);
                tag!(Tag::Unknown(TIFFTAG_UNIQUEID), unique_id as u32);
                tag!(Tag::Unknown(TIFFTAG_EPICSTSSEC), ts_sec);
                tag!(Tag::Unknown(TIFFTAG_EPICSTSNSEC), ts_nsec);

                // Standard tags (NDFileTIFF.cpp:243-271).
                tag!(Tag::Software, "EPICS areaDetector");
                tag!(Tag::Model, &*model);
                tag!(Tag::Make, &*make);
                if let Some(desc) = &image_description {
                    tag!(Tag::ImageDescription, &**desc);
                }

                // NDArray attributes as custom tags starting at 65010.
                for (tag_num, tag_val) in &attr_tags {
                    tag!(Tag::Unknown(*tag_num), &**tag_val);
                }

                image
                    .write_data($data)
                    .map_err(|e| ADError::UnsupportedConversion(format!("TIFF write error: {}", e)))
            }};
        }

        match &array.data {
            NDDataBuffer::U8(v) => {
                if is_rgb {
                    write_with_tags!(colortype::RGB8, v)
                } else {
                    write_with_tags!(colortype::Gray8, v)
                }
            }
            NDDataBuffer::I8(v) => {
                if is_rgb {
                    write_with_tags!(signed_rgb::RGBI8, v)
                } else {
                    write_with_tags!(colortype::GrayI8, v)
                }
            }
            NDDataBuffer::U16(v) => {
                if is_rgb {
                    write_with_tags!(colortype::RGB16, v)
                } else {
                    write_with_tags!(colortype::Gray16, v)
                }
            }
            NDDataBuffer::I16(v) => {
                if is_rgb {
                    write_with_tags!(signed_rgb::RGBI16, v)
                } else {
                    write_with_tags!(colortype::GrayI16, v)
                }
            }
            NDDataBuffer::U32(v) => {
                if is_rgb {
                    write_with_tags!(colortype::RGB32, v)
                } else {
                    write_with_tags!(colortype::Gray32, v)
                }
            }
            NDDataBuffer::I32(v) => {
                if is_rgb {
                    write_with_tags!(signed_rgb::RGBI32, v)
                } else {
                    write_with_tags!(colortype::GrayI32, v)
                }
            }
            NDDataBuffer::I64(v) => {
                if is_rgb {
                    write_with_tags!(signed_rgb::RGBI64, v)
                } else {
                    write_with_tags!(colortype::GrayI64, v)
                }
            }
            NDDataBuffer::U64(v) => {
                if is_rgb {
                    write_with_tags!(colortype::RGB64, v)
                } else {
                    write_with_tags!(colortype::Gray64, v)
                }
            }
            NDDataBuffer::F32(v) => {
                if is_rgb {
                    write_with_tags!(colortype::RGB32Float, v)
                } else {
                    write_with_tags!(colortype::Gray32Float, v)
                }
            }
            NDDataBuffer::F64(v) => {
                if is_rgb {
                    write_with_tags!(colortype::RGB64Float, v)
                } else {
                    write_with_tags!(colortype::Gray64Float, v)
                }
            }
        }?;

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
    fn test_metadata_tags_match_cpp_numbers_and_types() {
        let path = temp_path("tiff_meta_tags");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.unique_id = 4242;
        arr.time_stamp = 1234.5;
        arr.timestamp.sec = 1_000_000;
        arr.timestamp.nsec = 500;

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut decoder = Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
        // 65000 = NDTimeStamp (TIFF_DOUBLE).
        assert_eq!(decoder.get_tag_f64(Tag::Unknown(65000)).unwrap(), 1234.5);
        // 65001 = NDUniqueId (TIFF_LONG).
        assert_eq!(decoder.get_tag_u32(Tag::Unknown(65001)).unwrap(), 4242);
        // 65002 = EPICSTSSec, 65003 = EPICSTSNsec.
        assert_eq!(decoder.get_tag_u32(Tag::Unknown(65002)).unwrap(), 1_000_000);
        assert_eq!(decoder.get_tag_u32(Tag::Unknown(65003)).unwrap(), 500);
        // Standard Software tag.
        assert_eq!(
            decoder.get_tag(Tag::Software).unwrap().into_string().unwrap(),
            "EPICS areaDetector"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_standard_tags_from_attributes() {
        let path = temp_path("tiff_std_tags");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute::new_static(
            "Model",
            "",
            NDAttrSource::Driver,
            NDAttrValue::String("SimDetector".into()),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "Manufacturer",
            "",
            NDAttrSource::Driver,
            NDAttrValue::String("EPICS".into()),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "TIFFImageDescription",
            "",
            NDAttrSource::Driver,
            NDAttrValue::String("test frame".into()),
        ));

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut decoder = Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(
            decoder.get_tag(Tag::Model).unwrap().into_string().unwrap(),
            "SimDetector"
        );
        assert_eq!(
            decoder.get_tag(Tag::Make).unwrap().into_string().unwrap(),
            "EPICS"
        );
        assert_eq!(
            decoder
                .get_tag(Tag::ImageDescription)
                .unwrap()
                .into_string()
                .unwrap(),
            "test frame"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_attribute_tag_format_uses_colon_and_type() {
        // C++ uses "name:value" with typed numeric formatting.
        let mut a = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        a.attributes.add(NDAttribute::new_static(
            "Gain",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(-7),
        ));

        let path = temp_path("tiff_attr_fmt");
        let mut writer = TiffWriter::new();
        writer.open_file(&path, NDFileMode::Single, &a).unwrap();
        writer.write_file(&a).unwrap();
        writer.close_file().unwrap();

        let mut decoder = Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
        // First attribute tag is 65010.
        let s = decoder
            .get_tag(Tag::Unknown(65010))
            .unwrap()
            .into_string()
            .unwrap();
        assert_eq!(s, "Gain:-7");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_signed_rgb_writes_instead_of_erroring() {
        let path = temp_path("tiff_signed_rgb");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(2),
                NDDimension::new(2),
            ],
            NDDataType::Int16,
        );
        TiffWriter::attach_color_mode(&mut arr, NDColorMode::RGB1);
        if let NDDataBuffer::I16(v) = &mut arr.data {
            for (i, item) in v.iter_mut().enumerate() {
                *item = (i as i16) - 6;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        // Previously a hard error; must now succeed.
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut decoder = Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
        let sf = decoder.get_tag_u16_vec(Tag::SampleFormat).unwrap();
        // SampleFormat 2 = SAMPLEFORMAT_INT.
        assert!(sf.iter().all(|&s| s == 2), "expected signed sample format");

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
