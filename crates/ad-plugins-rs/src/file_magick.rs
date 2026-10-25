use asyn_rs::port::InterfaceMask;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ad_core_rs::color::{NDColorMode, convert_rgb_layout};
use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::{NDArray, NDDataView};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{NDFileOpenMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::{
    FileControl, FilePluginController, SettingsStore, WriterSettings,
};
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};

use image::codecs::png::{CompressionType as PngCompression, FilterType as PngFilter};
use image::{DynamicImage, ImageEncoder, ImageFormat};
use parking_lot::Mutex;

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
    cfg: WriterSettings<MagickSettings>,
}

/// The settings C `writeFile` reads from the param library
/// (NDFileMagick.cpp:118-121).
#[derive(Clone)]
struct MagickSettings {
    quality: u8,
    bit_depth: u32,
    compress_type: MagickCompression,
}

impl MagickWriter {
    pub fn new() -> Self {
        Self {
            current_path: None,
            cfg: WriterSettings::new(MagickSettings {
                quality: 100,
                // 0 = keep the native depth of the NDArray data type. GraphicsMagick
                // `image.depth(0)` is likewise a no-op; an explicit 8/16/32 forces
                // the output sample depth.
                bit_depth: 0,
                compress_type: MagickCompression::None,
            }),
        }
    }

    pub fn set_quality(&mut self, q: u8) {
        self.cfg.store().update(|s| s.quality = q);
    }

    pub fn set_bit_depth(&mut self, depth: u32) {
        self.cfg.store().update(|s| s.bit_depth = depth);
    }

    pub fn set_compress_type(&mut self, idx: i32) {
        self.cfg
            .store()
            .update(|s| s.compress_type = MagickCompression::from_index(idx));
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
    /// NDFileTIFF.cpp:180 there is no ndims == 1 branch — a 1-D array is an error.
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

        // Convert to RGB1 layout if needed (image crate expects interleaved RGB).
        // The scratch comes from the frame's own pool (C `pArray->pNDArrayPool`),
        // or a throwaway one for a frame that has none.
        let src = if is_rgb && color != NDColorMode::RGB1 {
            let pool = array.pool().unwrap_or_else(|| NDArrayPool::new(0));
            &convert_rgb_layout(&pool, array, color, NDColorMode::RGB1)?
        } else {
            array
        };

        match src.data().view() {
            NDDataView::U8(v) => {
                if is_rgb {
                    image::RgbImage::from_raw(width, height, v.to_vec())
                        .map(DynamicImage::ImageRgb8)
                        .ok_or_else(|| {
                            ADError::UnsupportedConversion("RGB8 buffer size mismatch".into())
                        })
                } else {
                    image::GrayImage::from_raw(width, height, v.to_vec())
                        .map(DynamicImage::ImageLuma8)
                        .ok_or_else(|| {
                            ADError::UnsupportedConversion("Gray8 buffer size mismatch".into())
                        })
                }
            }
            NDDataView::I8(v) => {
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
            NDDataView::U16(v) => {
                if is_rgb {
                    image::ImageBuffer::<image::Rgb<u16>, Vec<u16>>::from_raw(
                        width,
                        height,
                        v.to_vec(),
                    )
                    .map(DynamicImage::ImageRgb16)
                    .ok_or_else(|| {
                        ADError::UnsupportedConversion("RGB16 buffer size mismatch".into())
                    })
                } else {
                    image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(
                        width,
                        height,
                        v.to_vec(),
                    )
                    .map(DynamicImage::ImageLuma16)
                    .ok_or_else(|| {
                        ADError::UnsupportedConversion("Gray16 buffer size mismatch".into())
                    })
                }
            }
            NDDataView::I16(v) => {
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
            NDDataView::F32(v) => {
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
                src.data().data_type()
            ))),
        }
    }
}

impl NDFileWriter for MagickWriter {
    fn open_file(&mut self, path: &Path, mode: NDFileOpenMode, _array: &NDArray) -> ADResult<()> {
        // NDFileMagick.cpp:35-38
        mode.refuse_read_or_append("NDFileMagick")?;
        self.current_path = Some(path.to_path_buf());
        Ok(())
    }

    fn write_file(&mut self, array: &Arc<NDArray>) -> ADResult<()> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;
        self.cfg.refresh();

        let img = Self::array_to_image(array, self.cfg.bit_depth)?;

        // Determine format from extension, default to PNG
        let format = ImageFormat::from_path(path).unwrap_or(ImageFormat::Png);

        match format {
            ImageFormat::Jpeg => {
                // JPEG: use the quality setting.
                let mut buf = Vec::new();
                let encoder =
                    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, self.cfg.quality);
                img.write_with_encoder(encoder).map_err(|e| {
                    ADError::UnsupportedConversion(format!("Magick encode error: {e}"))
                })?;
                std::fs::write(path, &buf)?;
            }
            ImageFormat::Png => {
                // PNG: map the GraphicsMagick compression type onto the PNG
                // deflate compression level. Zip/BZip → best, None → uncompressed,
                // everything else → the encoder default.
                let compression = match self.cfg.compress_type {
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

    /// C `readFile` returns asynError (NDFileMagick.cpp:163-168).
    fn read_file(&mut self, _pool: &NDArrayPool) -> ADResult<NDArray> {
        Err(ADError::UnsupportedConversion(
            "NDFileMagick cannot read".into(),
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

/// Magick file processor wrapping `FilePluginController<MagickWriter>`.
pub struct MagickFileProcessor {
    ctrl: Mutex<FilePluginController<MagickWriter>>,
    settings: SettingsStore<MagickSettings>,
    quality_idx: Option<usize>,
    bit_depth_idx: Option<usize>,
    compress_type_idx: Option<usize>,
    /// The plugin's driver, for the posts a capture flush makes.
    attributes: Option<ad_core_rs::plugin::runtime::PluginAttributes>,
}

impl MagickFileProcessor {
    pub fn new() -> Self {
        let writer = MagickWriter::new();
        Self {
            settings: writer.cfg.store(),
            ctrl: Mutex::new(FilePluginController::new(writer)),
            quality_idx: None,
            bit_depth_idx: None,
            compress_type_idx: None,
            attributes: None,
        }
    }
}

impl NDPluginProcess for MagickFileProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_CANBLOCK;
    // NDFileMagick.cpp:202, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::GENERIC_POINTER;
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;

    fn posts_before_write(&self, reason: usize, value: i32) -> bool {
        self.ctrl.lock().posts_before_write(reason, value)
    }

    fn processing_required(&self, array: &NDArray) -> bool {
        self.ctrl.lock().processing_required(array)
    }

    /// C NDPluginFile::writeInt32 (NDPluginFile.cpp:799-870). C reads the
    /// Magick settings from the param library at open (NDFileMagick.cpp:
    /// 118-121), so the writer takes them inside the write, before any file
    /// I/O after it.
    fn validate_int32(
        &self,
        reason: usize,
        _addr: i32,
        value: i32,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<i32>,
        params: &mut ad_core_rs::plugin::runtime::WritePort<'_>,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<i32> {
        let mut ctrl = self.ctrl.lock();
        let out = ctrl.write_int32(reason, value, verdict, params);
        let kept = out.verdict.kept();
        if Some(reason) == self.quality_idx {
            let q = kept.clamp(1, 100) as u8;
            self.settings.update(|s| s.quality = q);
        } else if Some(reason) == self.bit_depth_idx {
            self.settings.update(|s| s.bit_depth = kept as u32);
        } else if Some(reason) == self.compress_type_idx {
            let c = MagickCompression::from_index(kept);
            self.settings.update(|s| s.compress_type = c);
        }
        out
    }

    /// C NDPluginFile::writeOctet: the setting is stored inside the write.
    fn validate_octet(
        &self,
        reason: usize,
        _addr: i32,
        _value: &str,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<String>,
        _params: &asyn_rs::port::PortDriverBase,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<String> {
        self.ctrl.lock().write_octet(reason, verdict)
    }
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        // Made before the controller lock: its reads wait on the actor,
        // which takes that lock in its writes.
        let mut post = ad_core_rs::plugin::runtime::PortPost::of(self.attributes.as_ref());
        self.ctrl.lock().process_array(array, &mut post)
    }

    fn bind_attributes(&mut self, attributes: ad_core_rs::plugin::runtime::PluginAttributes) {
        self.attributes = Some(attributes);
    }

    fn bind_pool(&mut self, pool: ad_core_rs::plugin::runtime::PluginPool) {
        self.ctrl.get_mut().bind_pool(pool);
    }

    fn plugin_type(&self) -> &str {
        "NDFileMagick"
    }

    /// C `NDPluginFile.cpp:948` (base of every file writer) sets
    /// `NDArrayCallbacks = 0`: file plugins write to disk, not downstream.
    fn array_callbacks_seed(&self) -> Option<bool> {
        Some(false)
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        self.ctrl.lock().register_params(base)?;
        use asyn_rs::param::ParamType;
        self.quality_idx = Some(base.create_param("MAGICK_QUALITY", ParamType::Int32)?);
        self.bit_depth_idx = Some(base.create_param("MAGICK_BIT_DEPTH", ParamType::Int32)?);
        self.compress_type_idx = Some(base.create_param("MAGICK_COMPRESS_TYPE", ParamType::Int32)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDataViewMut, NDDimension};
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    #[test]
    fn test_register_params_leaves_the_magick_controls_undefined() {
        // NDFileMagick.cpp's constructor sets only PluginType.
        use asyn_rs::port::InterfaceMask;
        use asyn_rs::port::{PortDriverBase, PortFlags};
        let mut base = PortDriverBase::new(
            "MAGICK_INIT",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let mut proc = MagickFileProcessor::new();
        proc.register_params(&mut base).unwrap();
        for name in ["MAGICK_QUALITY", "MAGICK_BIT_DEPTH", "MAGICK_COMPRESS_TYPE"] {
            let reason = base.find_param(name).unwrap();
            assert!(base.get_int32_param(reason, 0).is_err(), "{name}");
        }
    }

    fn temp_path(ext: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "adcore_test_magick_{}_{n}.{ext}",
            std::process::id()
        ))
    }

    /// C `openFile` refuses NDFileModeRead (NDFileMagick.cpp:35), so ReadFile
    /// fails at the open.
    #[test]
    fn read_file_is_refused_at_the_open() {
        let path = temp_path("png");
        let mut writer = MagickWriter::new();
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        let mut proc = MagickFileProcessor::new();
        let (out, _base) = crate::test_util::read_file(&mut proc, &path);
        assert_eq!(
            out.trace,
            crate::test_util::read_refused_lines(&path, &_base)
        );
        assert_eq!(out.publish, ad_core_rs::plugin::runtime::Published::Nothing);
        std::fs::remove_file(&path).ok();
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
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .expect("open");
        let err = writer.write_file(&Arc::new(arr)).unwrap_err();
        assert!(
            matches!(err, ADError::InvalidDimensions(_)),
            "3-D without ColorMode must be rejected, got {err:?}"
        );
        // ADP-95(b): C fails this array in `openFile` (NDFileMagick.cpp:90-95),
        // before `image.write()` ever runs, so no file exists on disk. The port
        // reaches the same decision one call later, in `write_file`, but must
        // leave the same absence — a caller that branches on the error code
        // must not then find a file the error says was not written.
        assert!(
            !path.exists(),
            "a rejected array must leave no file: {}",
            path.display()
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
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .expect("open");
        writer
            .write_file(&Arc::new(arr))
            .expect("3-D WITH ColorMode=RGB1 must still write");
        assert!(path.exists());
        std::fs::remove_file(&path).ok();
    }

    /// C reads the Magick settings per frame (NDFileMagick.cpp:118-121), so a
    /// setting write during a file write returns at once and the next frame
    /// takes it.
    #[test]
    fn a_setting_write_during_a_busy_write_returns_at_once() {
        use ad_core_rs::plugin::runtime::WriteVerdict;
        use asyn_rs::port::InterfaceMask;
        use asyn_rs::port::{PortDriverBase, PortFlags};
        let mut base = PortDriverBase::new(
            "magick_settings_write",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let mut proc = MagickFileProcessor::new();
        proc.register_params(&mut base).unwrap();
        let mut w = |idx: Option<usize>, v| {
            proc.validate_int32(
                idx.unwrap(),
                0,
                v,
                WriteVerdict::Accept(v),
                &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
            );
        };
        let writer = Arc::clone(&proc.ctrl.lock().writer);
        assert!(
            crate::test_util::returns_while_busy(&writer, || {
                w(proc.quality_idx, 70);
                w(proc.bit_depth_idx, 16);
            }),
            "the setting write waited for the file write"
        );
        let mut w = writer.lock();
        assert_eq!(w.cfg.quality, 100, "not read before the next frame");
        w.cfg.refresh();
        assert_eq!((w.cfg.quality, w.cfg.bit_depth), (70, 16));
    }

    #[test]
    fn test_write_read_png_u8() {
        let path = temp_path("png");
        let mut writer = MagickWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt8,
        );
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
            for i in 0..64 {
                v[i] = (i * 4) as u8;
            }
        }

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.close_file().unwrap();

        let read_back = image::open(&path).unwrap();
        assert_eq!(read_back.color(), image::ColorType::L8);
        let NDDataView::U8(orig) = arr.data().view() else {
            unreachable!()
        };
        assert_eq!(read_back.as_bytes(), orig);
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
        if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
            for i in 0..64 {
                v[i] = (i * 1000) as u16;
            }
        }

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap();
        writer.close_file().unwrap();

        let read_back = image::open(&path).unwrap();
        assert_eq!(read_back.color(), image::ColorType::L16);
        let NDDataView::U16(orig) = arr.data().view() else {
            unreachable!()
        };
        assert_eq!(read_back.into_luma16().into_raw(), *orig);
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
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
            for i in 0..48 {
                v[i] = (i * 5) as u8;
            }
        }

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let read_back = image::open(&path).unwrap();
        assert_eq!(read_back.color(), image::ColorType::Rgb8);
        assert_eq!((read_back.width(), read_back.height()), (4, 4));
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
        if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
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
        if let NDDataViewMut::F32(v) = arr.data_mut().view_mut() {
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
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
            for x in v.iter_mut() {
                *x = 128; // uniform → highly compressible
            }
        }

        let path_none = temp_path("png");
        let mut w_none = MagickWriter::new();
        w_none.set_compress_type(0); // None
        w_none
            .open_file(&path_none, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        w_none.write_file(&Arc::new(arr.clone())).unwrap();
        w_none.close_file().unwrap();

        let path_zip = temp_path("png");
        let mut w_zip = MagickWriter::new();
        w_zip.set_compress_type(7); // Zip
        w_zip
            .open_file(&path_zip, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        w_zip.write_file(&Arc::new(arr)).unwrap();
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
