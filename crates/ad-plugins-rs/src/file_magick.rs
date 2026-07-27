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

use image::codecs::png::{CompressionType as PngCompression, FilterType as PngFilter};
use image::{DynamicImage, ImageEncoder, ImageFormat};

/// GraphicsMagick `CompressionType` ordinals as used by C++ NDFileMagick.cpp:20
/// (`compressionTypes[]`). The `MAGICK_COMPRESS_TYPE` param indexes this list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MagickCompression {
    None = 0,
    BZip = 1,
    Fax = 2,
    Group4 = 3,
    Jpeg = 4,
    Lzw = 5,
    Rle = 6,
    Zip = 7,
}

impl MagickCompression {
    fn from_index(idx: i32) -> Self {
        match idx {
            1 => Self::BZip,
            2 => Self::Fax,
            3 => Self::Group4,
            4 => Self::Jpeg,
            5 => Self::Lzw,
            6 => Self::Rle,
            7 => Self::Zip,
            _ => Self::None,
        }
    }
}

/// NDFileMagick: file writer using the `image` crate.
///
/// Format is determined by the file extension (PNG, BMP, GIF, TIFF, etc.).
/// Supports UInt8 and UInt16 data in mono and RGB color modes.
pub struct MagickWriter {
    current_path: Option<PathBuf>,
    quality: u8,
    bit_depth: u32,
    compress_type: MagickCompression,
}

impl MagickWriter {
    pub fn new() -> Self {
        Self {
            current_path: None,
            quality: 100,
            // 0 = keep the native depth of the NDArray data type. GraphicsMagick
            // `image.depth(0)` is likewise a no-op; an explicit 8/16/32 forces
            // the output sample depth.
            bit_depth: 0,
            compress_type: MagickCompression::None,
        }
    }

    pub fn set_quality(&mut self, q: u8) {
        self.quality = q;
    }

    pub fn set_bit_depth(&mut self, depth: u32) {
        self.bit_depth = depth;
    }

    pub fn set_compress_type(&mut self, idx: i32) {
        self.compress_type = MagickCompression::from_index(idx);
    }

    /// C's `openFile` structure chain (NDFileMagick.cpp:71-95).
    ///
    /// The ColorMode *attribute* is the only source of truth, defaulting to Mono
    /// when it is absent — C `this->colorMode = NDColorModeMono;` (:41) overwritten
    /// only by the attribute (:44-45); `info().color_mode` is that rule's single
    /// owner. Each 3-D branch then requires the attribute to name the layout, so a
    /// 3-D array with no ColorMode attribute matches nothing and C returns
    /// asynError (:90-95). Inferring the layout from the dimensions instead made
    /// such an array look like RGB1 and write a file.
    ///
    /// C takes the mode from the branch it took, not from the attribute: the 2-D
    /// branch is grayscale whatever the attribute says (:71-75). And unlike
    /// NDFileTIFF (:180) there is no ndims == 1 branch — a 1-D array is an error.
    fn color_mode(array: &NDArray) -> ADResult<NDColorMode> {
        let attr_mode = array.info().color_mode;
        Ok(match array.dims.as_slice() {
            [_, _] => NDColorMode::Mono,
            [c, _, _] if c.size == 3 && attr_mode == NDColorMode::RGB1 => NDColorMode::RGB1,
            [_, c, _] if c.size == 3 && attr_mode == NDColorMode::RGB2 => NDColorMode::RGB2,
            [_, _, c] if c.size == 3 && attr_mode == NDColorMode::RGB3 => NDColorMode::RGB3,
            _ => {
                return Err(ADError::InvalidDimensions(
                    "unsupported array structure".into(),
                ));
            }
        })
    }

    /// Convert NDArray to DynamicImage for encoding.
    ///
    /// `bit_depth` selects the output sample depth (C++ `image.depth(depth)`):
    /// `0` keeps the native NDArray depth, `<= 8` produces an 8-bit image,
    /// anything larger a 16-bit image.
    fn array_to_image(array: &NDArray, bit_depth: u32) -> ADResult<DynamicImage> {
        let img = Self::array_to_image_native(array)?;
        Ok(Self::apply_bit_depth(img, bit_depth))
    }

    /// Apply the requested output bit depth by converting the DynamicImage.
    fn apply_bit_depth(img: DynamicImage, bit_depth: u32) -> DynamicImage {
        if bit_depth == 0 {
            // Keep native depth.
            return img;
        }
        let is_rgb = matches!(
            img,
            DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgb16(_)
        );
        if bit_depth <= 8 {
            if is_rgb {
                DynamicImage::ImageRgb8(img.to_rgb8())
            } else {
                DynamicImage::ImageLuma8(img.to_luma8())
            }
        } else {
            if is_rgb {
                DynamicImage::ImageRgb16(img.to_rgb16())
            } else {
                DynamicImage::ImageLuma16(img.to_luma16())
            }
        }
    }

    /// Convert NDArray to DynamicImage at the native depth of the data type.
    fn array_to_image_native(array: &NDArray) -> ADResult<DynamicImage> {
        let info = array.info();
        let width = info.x_size as u32;
        let height = info.y_size as u32;
        let color = Self::color_mode(array)?;
        let is_rgb = matches!(
            color,
            NDColorMode::RGB1 | NDColorMode::RGB2 | NDColorMode::RGB3
        );

        // Convert to RGB1 layout if needed (image crate expects interleaved RGB)
        let src = if is_rgb && color != NDColorMode::RGB1 {
            &convert_rgb_layout(array, color, NDColorMode::RGB1)?
        } else {
            array
        };

        match &src.data {
            NDDataBuffer::U8(v) => {
                if is_rgb {
                    image::RgbImage::from_raw(width, height, v.clone())
                        .map(DynamicImage::ImageRgb8)
                        .ok_or_else(|| {
                            ADError::UnsupportedConversion("RGB8 buffer size mismatch".into())
                        })
                } else {
                    image::GrayImage::from_raw(width, height, v.clone())
                        .map(DynamicImage::ImageLuma8)
                        .ok_or_else(|| {
                            ADError::UnsupportedConversion("Gray8 buffer size mismatch".into())
                        })
                }
            }
            NDDataBuffer::I8(v) => {
                let u8_data: Vec<u8> = v.iter().map(|&b| b as u8).collect();
                if is_rgb {
                    image::RgbImage::from_raw(width, height, u8_data)
                        .map(DynamicImage::ImageRgb8)
                        .ok_or_else(|| {
                            ADError::UnsupportedConversion("RGB8 buffer size mismatch".into())
                        })
                } else {
                    image::GrayImage::from_raw(width, height, u8_data)
                        .map(DynamicImage::ImageLuma8)
                        .ok_or_else(|| {
                            ADError::UnsupportedConversion("Gray8 buffer size mismatch".into())
                        })
                }
            }
            NDDataBuffer::U16(v) => {
                if is_rgb {
                    image::ImageBuffer::<image::Rgb<u16>, Vec<u16>>::from_raw(
                        width,
                        height,
                        v.clone(),
                    )
                    .map(DynamicImage::ImageRgb16)
                    .ok_or_else(|| {
                        ADError::UnsupportedConversion("RGB16 buffer size mismatch".into())
                    })
                } else {
                    image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(
                        width,
                        height,
                        v.clone(),
                    )
                    .map(DynamicImage::ImageLuma16)
                    .ok_or_else(|| {
                        ADError::UnsupportedConversion("Gray16 buffer size mismatch".into())
                    })
                }
            }
            NDDataBuffer::I16(v) => {
                let u16_data: Vec<u16> = v.iter().map(|&b| b as u16).collect();
                if is_rgb {
                    image::ImageBuffer::<image::Rgb<u16>, Vec<u16>>::from_raw(
                        width, height, u16_data,
                    )
                    .map(DynamicImage::ImageRgb16)
                    .ok_or_else(|| {
                        ADError::UnsupportedConversion("RGB16 buffer size mismatch".into())
                    })
                } else {
                    image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(
                        width, height, u16_data,
                    )
                    .map(DynamicImage::ImageLuma16)
                    .ok_or_else(|| {
                        ADError::UnsupportedConversion("Gray16 buffer size mismatch".into())
                    })
                }
            }
            NDDataBuffer::F32(v) => {
                // Scale by the actual data range, not a fixed [0,1] clamp
                // (C++ NDFileMagick scales by the image's min/max range).
                let mut min = f32::INFINITY;
                let mut max = f32::NEG_INFINITY;
                for &f in v {
                    if f.is_finite() {
                        min = min.min(f);
                        max = max.max(f);
                    }
                }
                let range = if min.is_finite() && max > min {
                    max - min
                } else {
                    1.0
                };
                let offset = if min.is_finite() { min } else { 0.0 };
                let u16_data: Vec<u16> = v
                    .iter()
                    .map(|&f| {
                        let norm = ((f - offset) / range).clamp(0.0, 1.0);
                        (norm * 65535.0).round() as u16
                    })
                    .collect();
                if is_rgb {
                    image::ImageBuffer::<image::Rgb<u16>, Vec<u16>>::from_raw(
                        width, height, u16_data,
                    )
                    .map(DynamicImage::ImageRgb16)
                    .ok_or_else(|| {
                        ADError::UnsupportedConversion("RGB16 buffer size mismatch".into())
                    })
                } else {
                    image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(
                        width, height, u16_data,
                    )
                    .map(DynamicImage::ImageLuma16)
                    .ok_or_else(|| {
                        ADError::UnsupportedConversion("Gray16 buffer size mismatch".into())
                    })
                }
            }
            _ => Err(ADError::UnsupportedConversion(format!(
                "NDFileMagick: unsupported data type {:?}, use UInt8, Int8, UInt16, Int16, or Float32",
                src.data.data_type()
            ))),
        }
    }
}

impl NDFileWriter for MagickWriter {
    fn open_file(&mut self, path: &Path, _mode: NDFileMode, _array: &NDArray) -> ADResult<()> {
        self.current_path = Some(path.to_path_buf());
        Ok(())
    }

    fn write_file(&mut self, array: &NDArray) -> ADResult<()> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let img = Self::array_to_image(array, self.bit_depth)?;

        // Determine format from extension, default to PNG
        let format = ImageFormat::from_path(path).unwrap_or(ImageFormat::Png);

        match format {
            ImageFormat::Jpeg => {
                // JPEG: use the quality setting.
                let mut buf = Vec::new();
                let encoder =
                    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, self.quality);
                img.write_with_encoder(encoder).map_err(|e| {
                    ADError::UnsupportedConversion(format!("Magick encode error: {e}"))
                })?;
                std::fs::write(path, &buf)?;
            }
            ImageFormat::Png => {
                // PNG: map the GraphicsMagick compression type onto the PNG
                // deflate compression level. Zip/BZip → best, None → uncompressed,
                // everything else → the encoder default.
                let compression = match self.compress_type {
                    MagickCompression::None => PngCompression::Uncompressed,
                    MagickCompression::Zip | MagickCompression::BZip => PngCompression::Best,
                    _ => PngCompression::default(),
                };
                let mut buf = Vec::new();
                let encoder = image::codecs::png::PngEncoder::new_with_quality(
                    &mut buf,
                    compression,
                    PngFilter::Adaptive,
                );
                let rgb = img.color();
                encoder
                    .write_image(img.as_bytes(), img.width(), img.height(), rgb.into())
                    .map_err(|e| {
                        ADError::UnsupportedConversion(format!("Magick PNG encode error: {e}"))
                    })?;
                std::fs::write(path, &buf)?;
            }
            _ => {
                // Other formats: the `image` crate's high-level save() has no
                // compression knob; GraphicsMagick's CompressionType does not
                // map onto these codecs, so the compress-type param has no
                // effect for them (matches `image` crate capability).
                img.save(path).map_err(|e| {
                    ADError::UnsupportedConversion(format!("Magick save error: {e}"))
                })?;
            }
        }

        Ok(())
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let img = image::open(path)
            .map_err(|e| ADError::UnsupportedConversion(format!("Magick read error: {e}")))?;

        let width = img.width() as usize;
        let height = img.height() as usize;

        match img {
            DynamicImage::ImageLuma8(buf) => {
                let mut arr = NDArray::new(
                    vec![NDDimension::new(width), NDDimension::new(height)],
                    NDDataType::UInt8,
                );
                arr.data = NDDataBuffer::U8(buf.into_raw());
                Ok(arr)
            }
            DynamicImage::ImageRgb8(buf) => {
                let mut arr = NDArray::new(
                    vec![
                        NDDimension::new(3),
                        NDDimension::new(width),
                        NDDimension::new(height),
                    ],
                    NDDataType::UInt8,
                );
                arr.data = NDDataBuffer::U8(buf.into_raw());
                Ok(arr)
            }
            DynamicImage::ImageLuma16(buf) => {
                let mut arr = NDArray::new(
                    vec![NDDimension::new(width), NDDimension::new(height)],
                    NDDataType::UInt16,
                );
                arr.data = NDDataBuffer::U16(buf.into_raw());
                Ok(arr)
            }
            DynamicImage::ImageRgb16(buf) => {
                let mut arr = NDArray::new(
                    vec![
                        NDDimension::new(3),
                        NDDimension::new(width),
                        NDDimension::new(height),
                    ],
                    NDDataType::UInt16,
                );
                arr.data = NDDataBuffer::U16(buf.into_raw());
                Ok(arr)
            }
            other => {
                // Convert anything else to RGB8
                let rgb = other.to_rgb8();
                let mut arr = NDArray::new(
                    vec![
                        NDDimension::new(3),
                        NDDimension::new(width),
                        NDDimension::new(height),
                    ],
                    NDDataType::UInt8,
                );
                arr.data = NDDataBuffer::U8(rgb.into_raw());
                Ok(arr)
            }
        }
    }

    fn close_file(&mut self) -> ADResult<()> {
        self.current_path = None;
        Ok(())
    }

    fn supports_multiple_arrays(&self) -> bool {
        false
    }
}

/// Magick file processor wrapping `FilePluginController<MagickWriter>`.
pub struct MagickFileProcessor {
    ctrl: FilePluginController<MagickWriter>,
    quality_idx: Option<usize>,
    bit_depth_idx: Option<usize>,
    compress_type_idx: Option<usize>,
}

impl MagickFileProcessor {
    pub fn new() -> Self {
        Self {
            ctrl: FilePluginController::new(MagickWriter::new()),
            quality_idx: None,
            bit_depth_idx: None,
            compress_type_idx: None,
        }
    }
}

impl NDPluginProcess for MagickFileProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        self.ctrl.process_array(array)
    }

    fn plugin_type(&self) -> &str {
        "NDFileMagick"
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
        self.quality_idx = Some(base.create_param("MAGICK_QUALITY", ParamType::Int32)?);
        self.bit_depth_idx = Some(base.create_param("MAGICK_BIT_DEPTH", ParamType::Int32)?);
        self.compress_type_idx = Some(base.create_param("MAGICK_COMPRESS_TYPE", ParamType::Int32)?);
        // Set defaults
        base.set_int32_param(self.quality_idx.unwrap(), 0, 100)?;
        base.set_int32_param(self.bit_depth_idx.unwrap(), 0, 8)?;
        base.set_int32_param(self.compress_type_idx.unwrap(), 0, 0)?;
        Ok(())
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        params: &PluginParamSnapshot,
    ) -> ParamChangeResult {
        if Some(reason) == self.quality_idx {
            let q = params.value.as_i32().clamp(1, 100) as u8;
            self.ctrl.writer.set_quality(q);
            return ParamChangeResult::empty();
        }
        if Some(reason) == self.bit_depth_idx {
            let d = params.value.as_i32() as u32;
            self.ctrl.writer.set_bit_depth(d);
            return ParamChangeResult::empty();
        }
        if Some(reason) == self.compress_type_idx {
            self.ctrl.writer.set_compress_type(params.value.as_i32());
            return ParamChangeResult::empty();
        }
        self.ctrl.on_param_change(reason, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(ext: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("adcore_test_magick_{n}.{ext}"))
    }

    /// R8-75, Magick writer: same defect as the cited TIFF site. C sets
    /// `this->colorMode = NDColorModeMono` (NDFileMagick.cpp:41) and overwrites it
    /// only from the attribute (:44-45); each 3-D branch requires the attribute
    /// (:76, :81, :86), so a 3-D array without ColorMode returns asynError
    /// (:90-95). The port inferred RGB1 from the dims.
    #[test]
    fn test_r8_75_3d_without_colormode_attribute_is_an_error() {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};

        let rgb1_dims = || {
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(4),
            ]
        };

        let arr = NDArray::new(rgb1_dims(), NDDataType::UInt8);
        let path = temp_path("png");
        let mut writer = MagickWriter::new();
        writer
            .open_file(&path, NDFileMode::Single, &arr)
            .expect("open");
        let err = writer.write_file(&arr).unwrap_err();
        assert!(
            matches!(err, ADError::InvalidDimensions(_)),
            "3-D without ColorMode must be rejected, got {err:?}"
        );
        std::fs::remove_file(&path).ok();

        // Positive control: WITH ColorMode=RGB1 the same array writes.
        let mut arr = NDArray::new(rgb1_dims(), NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(NDColorMode::RGB1 as i32),
        ));
        let path = temp_path("png");
        let mut writer = MagickWriter::new();
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
    fn test_write_read_png_u8() {
        let path = temp_path("png");
        let mut writer = MagickWriter::new();

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

        let read_back = writer.read_file().unwrap();
        assert_eq!(read_back.data.data_type(), NDDataType::UInt8);
        if let (NDDataBuffer::U8(orig), NDDataBuffer::U8(read)) = (&arr.data, &read_back.data) {
            assert_eq!(orig, read);
        }

        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_read_png_u16() {
        let path = temp_path("png");
        let mut writer = MagickWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for i in 0..64 {
                v[i] = (i * 1000) as u16;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();

        let read_back = writer.read_file().unwrap();
        assert_eq!(read_back.data.data_type(), NDDataType::UInt16);
        if let (NDDataBuffer::U16(orig), NDDataBuffer::U16(read)) = (&arr.data, &read_back.data) {
            assert_eq!(orig, read);
        }

        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_read_bmp_rgb() {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};

        let path = temp_path("bmp");
        let mut writer = MagickWriter::new();

        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(4),
            ],
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "Color Mode",
            NDAttrSource::Driver,
            NDAttrValue::Int32(2), // RGB1
        ));
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..48 {
                v[i] = (i * 5) as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();

        let read_back = writer.read_file().unwrap();
        assert_eq!(read_back.dims.len(), 3);
        assert_eq!(read_back.dims[0].size, 3);

        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_rejects_unsupported_type() {
        // F32 is now supported (normalized to U16). Use Float64 as unsupported.
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::Float64,
        );
        assert!(MagickWriter::array_to_image(&arr, 8).is_err());
    }

    #[test]
    fn test_bit_depth_controls_output_depth() {
        // u16 input with bit_depth 8 → 8-bit output image.
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 4000) as u16;
            }
        }
        let img8 = MagickWriter::array_to_image(&arr, 8).unwrap();
        assert!(matches!(img8, DynamicImage::ImageLuma8(_)));
        let img16 = MagickWriter::array_to_image(&arr, 16).unwrap();
        assert!(matches!(img16, DynamicImage::ImageLuma16(_)));
    }

    #[test]
    fn test_f32_scales_by_actual_range() {
        // Values well outside [0,1] must not all saturate to white.
        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::Float32,
        );
        if let NDDataBuffer::F32(ref mut v) = arr.data {
            v[0] = 100.0;
            v[1] = 200.0;
            v[2] = 300.0;
            v[3] = 400.0;
        }
        let img = MagickWriter::array_to_image(&arr, 16).unwrap();
        if let DynamicImage::ImageLuma16(buf) = img {
            let raw = buf.into_raw();
            // min maps to 0, max maps to 65535, intermediate values spread out.
            assert_eq!(raw[0], 0);
            assert_eq!(raw[3], 65535);
            assert!(raw[1] > 0 && raw[1] < raw[2]);
        } else {
            panic!("expected 16-bit luma image");
        }
    }

    #[test]
    fn test_compress_type_applied_to_png() {
        // None vs Best compression must produce different PNG file sizes for
        // compressible data — proving the param is not discarded.
        let mut arr = NDArray::new(
            vec![NDDimension::new(64), NDDimension::new(64)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for x in v.iter_mut() {
                *x = 128; // uniform → highly compressible
            }
        }

        let path_none = temp_path("png");
        let mut w_none = MagickWriter::new();
        w_none.set_compress_type(0); // None
        w_none
            .open_file(&path_none, NDFileMode::Single, &arr)
            .unwrap();
        w_none.write_file(&arr).unwrap();
        w_none.close_file().unwrap();

        let path_zip = temp_path("png");
        let mut w_zip = MagickWriter::new();
        w_zip.set_compress_type(7); // Zip
        w_zip
            .open_file(&path_zip, NDFileMode::Single, &arr)
            .unwrap();
        w_zip.write_file(&arr).unwrap();
        w_zip.close_file().unwrap();

        let size_none = std::fs::metadata(&path_none).unwrap().len();
        let size_zip = std::fs::metadata(&path_zip).unwrap().len();
        assert!(
            size_zip < size_none,
            "Zip ({size_zip}) should be smaller than None ({size_none})"
        );

        std::fs::remove_file(&path_none).ok();
        std::fs::remove_file(&path_zip).ok();
    }
}
