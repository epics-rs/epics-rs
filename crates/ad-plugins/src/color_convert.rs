use std::sync::Arc;

#[cfg(feature = "parallel")]
use rayon::prelude::*;
#[cfg(feature = "parallel")]
use crate::par_util;

use ad_core::color::{self, NDColorMode, NDBayerPattern};
use ad_core::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::{NDPluginProcess, ProcessResult};

/// Simple Bayer demosaic using bilinear interpolation.
pub fn bayer_to_rgb1(src: &NDArray, pattern: NDBayerPattern) -> Option<NDArray> {
    if src.dims.len() != 2 {
        return None;
    }
    let w = src.dims[0].size;
    let h = src.dims[1].size;

    // Pre-compute source values into a flat f64 vec for efficient random access
    let n = w * h;
    let src_vals: Vec<f64> = (0..n).map(|i| src.data.get_as_f64(i).unwrap_or(0.0)).collect();
    let get_val = |x: usize, y: usize| -> f64 { src_vals[y * w + x] };

    let mut r = vec![0.0f64; n];
    let mut g = vec![0.0f64; n];
    let mut b = vec![0.0f64; n];

    // Determine which color each pixel position has
    let (r_row_even, r_col_even) = match pattern {
        NDBayerPattern::RGGB => (true, true),
        NDBayerPattern::GBRG => (true, false),
        NDBayerPattern::GRBG => (false, true),
        NDBayerPattern::BGGR => (false, false),
    };

    // Helper to demosaic a single row into (r, g, b) slices
    let demosaic_row = |y: usize, r_row: &mut [f64], g_row: &mut [f64], b_row: &mut [f64]| {
        let even_row = (y % 2 == 0) == r_row_even;
        for x in 0..w {
            let val = get_val(x, y);
            let even_col = (x % 2 == 0) == r_col_even;

            match (even_row, even_col) {
                (true, true) => {
                    r_row[x] = val;
                    let mut gsum = 0.0; let mut gc = 0;
                    if x > 0 { gsum += get_val(x - 1, y); gc += 1; }
                    if x < w - 1 { gsum += get_val(x + 1, y); gc += 1; }
                    if y > 0 { gsum += get_val(x, y - 1); gc += 1; }
                    if y < h - 1 { gsum += get_val(x, y + 1); gc += 1; }
                    g_row[x] = if gc > 0 { gsum / gc as f64 } else { 0.0 };
                    let mut bsum = 0.0; let mut bc = 0;
                    if x > 0 && y > 0 { bsum += get_val(x - 1, y - 1); bc += 1; }
                    if x < w - 1 && y > 0 { bsum += get_val(x + 1, y - 1); bc += 1; }
                    if x > 0 && y < h - 1 { bsum += get_val(x - 1, y + 1); bc += 1; }
                    if x < w - 1 && y < h - 1 { bsum += get_val(x + 1, y + 1); bc += 1; }
                    b_row[x] = if bc > 0 { bsum / bc as f64 } else { 0.0 };
                }
                (true, false) | (false, true) => {
                    g_row[x] = val;
                    if even_row {
                        let mut rsum = 0.0; let mut rc = 0;
                        if x > 0 { rsum += get_val(x - 1, y); rc += 1; }
                        if x < w - 1 { rsum += get_val(x + 1, y); rc += 1; }
                        r_row[x] = if rc > 0 { rsum / rc as f64 } else { 0.0 };
                        let mut bsum = 0.0; let mut bc = 0;
                        if y > 0 { bsum += get_val(x, y - 1); bc += 1; }
                        if y < h - 1 { bsum += get_val(x, y + 1); bc += 1; }
                        b_row[x] = if bc > 0 { bsum / bc as f64 } else { 0.0 };
                    } else {
                        let mut bsum = 0.0; let mut bc = 0;
                        if x > 0 { bsum += get_val(x - 1, y); bc += 1; }
                        if x < w - 1 { bsum += get_val(x + 1, y); bc += 1; }
                        b_row[x] = if bc > 0 { bsum / bc as f64 } else { 0.0 };
                        let mut rsum = 0.0; let mut rc = 0;
                        if y > 0 { rsum += get_val(x, y - 1); rc += 1; }
                        if y < h - 1 { rsum += get_val(x, y + 1); rc += 1; }
                        r_row[x] = if rc > 0 { rsum / rc as f64 } else { 0.0 };
                    }
                }
                (false, false) => {
                    b_row[x] = val;
                    let mut gsum = 0.0; let mut gc = 0;
                    if x > 0 { gsum += get_val(x - 1, y); gc += 1; }
                    if x < w - 1 { gsum += get_val(x + 1, y); gc += 1; }
                    if y > 0 { gsum += get_val(x, y - 1); gc += 1; }
                    if y < h - 1 { gsum += get_val(x, y + 1); gc += 1; }
                    g_row[x] = if gc > 0 { gsum / gc as f64 } else { 0.0 };
                    let mut rsum = 0.0; let mut rc = 0;
                    if x > 0 && y > 0 { rsum += get_val(x - 1, y - 1); rc += 1; }
                    if x < w - 1 && y > 0 { rsum += get_val(x + 1, y - 1); rc += 1; }
                    if x > 0 && y < h - 1 { rsum += get_val(x - 1, y + 1); rc += 1; }
                    if x < w - 1 && y < h - 1 { rsum += get_val(x + 1, y + 1); rc += 1; }
                    r_row[x] = if rc > 0 { rsum / rc as f64 } else { 0.0 };
                }
            }
        }
    };

    #[cfg(feature = "parallel")]
    let use_parallel = par_util::should_parallelize(n);
    #[cfg(not(feature = "parallel"))]
    let use_parallel = false;

    if use_parallel {
        #[cfg(feature = "parallel")]
        {
            // Split r, g, b into per-row mutable slices and process in parallel
            let r_rows: Vec<&mut [f64]> = r.chunks_mut(w).collect();
            let g_rows: Vec<&mut [f64]> = g.chunks_mut(w).collect();
            let b_rows: Vec<&mut [f64]> = b.chunks_mut(w).collect();

            par_util::thread_pool().install(|| {
                r_rows.into_par_iter()
                    .zip(g_rows.into_par_iter())
                    .zip(b_rows.into_par_iter())
                    .enumerate()
                    .for_each(|(y, ((r_row, g_row), b_row))| {
                        demosaic_row(y, r_row, g_row, b_row);
                    });
            });
        }
    } else {
        for y in 0..h {
            let row_start = y * w;
            let row_end = row_start + w;
            demosaic_row(
                y,
                &mut r[row_start..row_end],
                &mut g[row_start..row_end],
                &mut b[row_start..row_end],
            );
        }
    }

    // Build RGB1 interleaved output
    let out_data = match src.data.data_type() {
        NDDataType::UInt8 => {
            let mut out = vec![0u8; n * 3];
            for i in 0..n {
                out[i * 3] = r[i].clamp(0.0, 255.0) as u8;
                out[i * 3 + 1] = g[i].clamp(0.0, 255.0) as u8;
                out[i * 3 + 2] = b[i].clamp(0.0, 255.0) as u8;
            }
            NDDataBuffer::U8(out)
        }
        NDDataType::UInt16 => {
            let mut out = vec![0u16; n * 3];
            for i in 0..n {
                out[i * 3] = r[i].clamp(0.0, 65535.0) as u16;
                out[i * 3 + 1] = g[i].clamp(0.0, 65535.0) as u16;
                out[i * 3 + 2] = b[i].clamp(0.0, 65535.0) as u16;
            }
            NDDataBuffer::U16(out)
        }
        _ => return None,
    };

    let dims = vec![NDDimension::new(3), NDDimension::new(w), NDDimension::new(h)];
    let mut arr = NDArray::new(dims, src.data.data_type());
    arr.data = out_data;
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    Some(arr)
}

/// Generate a jet colormap lookup table (256 entries, RGB).
///
/// Maps scalar values 0..255 through blue -> cyan -> green -> yellow -> red.
fn jet_colormap() -> [[u8; 3]; 256] {
    let mut lut = [[0u8; 3]; 256];
    for i in 0..256 {
        let v = i as f64 / 255.0;
        // Jet colormap: blue -> cyan -> green -> yellow -> red
        let r = (1.5 - (4.0 * v - 3.0).abs()).clamp(0.0, 1.0);
        let g = (1.5 - (4.0 * v - 2.0).abs()).clamp(0.0, 1.0);
        let b = (1.5 - (4.0 * v - 1.0).abs()).clamp(0.0, 1.0);
        lut[i] = [(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8];
    }
    lut
}

/// Convert a mono UInt8 image to RGB1 using false color (jet colormap).
///
/// Only supports 2D UInt8 arrays. Each pixel value is mapped through the jet
/// colormap LUT to produce a pseudo-color RGB1 output.
fn false_color_mono_to_rgb1(src: &NDArray) -> Option<NDArray> {
    if src.dims.len() != 2 || src.data.data_type() != NDDataType::UInt8 {
        return None;
    }

    let w = src.dims[0].size;
    let h = src.dims[1].size;
    let n = w * h;
    let lut = jet_colormap();

    let src_slice = src.data.as_u8_slice();
    let mut out = vec![0u8; n * 3];
    for i in 0..n {
        let val = src_slice[i] as usize;
        let [r, g, b] = lut[val];
        out[i * 3] = r;
        out[i * 3 + 1] = g;
        out[i * 3 + 2] = b;
    }

    let dims = vec![NDDimension::new(3), NDDimension::new(w), NDDimension::new(h)];
    let mut arr = NDArray::new(dims, NDDataType::UInt8);
    arr.data = NDDataBuffer::U8(out);
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    Some(arr)
}

/// Detect the color mode of an NDArray from its dimensionality.
///
/// 2D arrays are always treated as Mono (Bayer cannot be distinguished from Mono
/// by dimensions alone). For 3D arrays, the dimension with size 3 determines the
/// RGB layout variant.
fn detect_color_mode(array: &NDArray) -> NDColorMode {
    match array.dims.len() {
        0 | 1 => NDColorMode::Mono,
        2 => NDColorMode::Mono, // 2D is always mono (or bayer, but we can't tell)
        3 => {
            // Check color dimension
            if array.dims[0].size == 3 {
                NDColorMode::RGB1
            } else if array.dims[1].size == 3 {
                NDColorMode::RGB2
            } else if array.dims[2].size == 3 {
                NDColorMode::RGB3
            } else {
                NDColorMode::Mono
            }
        }
        _ => NDColorMode::Mono,
    }
}

/// Color convert plugin configuration.
#[derive(Debug, Clone)]
pub struct ColorConvertConfig {
    pub target_mode: NDColorMode,
    pub bayer_pattern: NDBayerPattern,
    pub false_color: bool,
}

/// Pure color conversion processing logic.
pub struct ColorConvertProcessor {
    config: ColorConvertConfig,
}

impl ColorConvertProcessor {
    pub fn new(config: ColorConvertConfig) -> Self {
        Self { config }
    }
}

impl NDPluginProcess for ColorConvertProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let src_mode = detect_color_mode(array);
        let result = match (src_mode, self.config.target_mode) {
            // Same mode - passthrough
            (s, t) if s == t => Some(array.clone()),

            // To Mono
            (NDColorMode::RGB1 | NDColorMode::RGB2 | NDColorMode::RGB3, NDColorMode::Mono) => {
                // If not RGB1, convert to RGB1 first, then to mono
                let rgb1 = if src_mode != NDColorMode::RGB1 {
                    color::convert_rgb_layout(array, src_mode, NDColorMode::RGB1).ok()
                } else {
                    Some(array.clone())
                };
                rgb1.and_then(|a| color::rgb1_to_mono(&a).ok())
            }

            // Mono to RGB with false color
            (NDColorMode::Mono, NDColorMode::RGB1) if self.config.false_color => {
                false_color_mono_to_rgb1(array)
            }

            // Mono to any RGB
            (NDColorMode::Mono, NDColorMode::RGB1) => color::mono_to_rgb1(array).ok(),
            (NDColorMode::Mono, NDColorMode::RGB2 | NDColorMode::RGB3) => {
                color::mono_to_rgb1(array).ok().and_then(|a| {
                    color::convert_rgb_layout(&a, NDColorMode::RGB1, self.config.target_mode).ok()
                })
            }

            // Bayer to any RGB
            (NDColorMode::Bayer, NDColorMode::RGB1) => {
                bayer_to_rgb1(array, self.config.bayer_pattern)
            }
            (NDColorMode::Bayer, NDColorMode::RGB2 | NDColorMode::RGB3) => {
                bayer_to_rgb1(array, self.config.bayer_pattern).and_then(|a| {
                    color::convert_rgb_layout(&a, NDColorMode::RGB1, self.config.target_mode).ok()
                })
            }

            // RGB to RGB (layout conversion)
            (
                NDColorMode::RGB1 | NDColorMode::RGB2 | NDColorMode::RGB3,
                NDColorMode::RGB1 | NDColorMode::RGB2 | NDColorMode::RGB3,
            ) => color::convert_rgb_layout(array, src_mode, self.config.target_mode).ok(),

            _ => None,
        };
        match result {
            Some(out) => ProcessResult::arrays(vec![Arc::new(out)]),
            None => ProcessResult::empty(),
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginColorConvert"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bayer_to_rgb1_basic() {
        // 4x4 RGGB bayer pattern
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            // Simple pattern: all pixels = 128
            for i in 0..16 {
                v[i] = 128;
            }
        }

        let rgb = bayer_to_rgb1(&arr, NDBayerPattern::RGGB).unwrap();
        assert_eq!(rgb.dims.len(), 3);
        assert_eq!(rgb.dims[0].size, 3); // color
        assert_eq!(rgb.dims[1].size, 4); // x
        assert_eq!(rgb.dims[2].size, 4); // y
    }

    #[test]
    fn test_color_convert_processor_bayer() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::RGB1,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: false,
        };
        let mut proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..16 {
                v[i] = 128;
            }
        }

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(result.output_arrays[0].dims[0].size, 3); // RGB color dim
    }

    #[test]
    fn test_false_color_conversion() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::RGB1,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: true,
        };
        let mut proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        // Create a 4x4 mono UInt8 image with a gradient
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..16 {
                v[i] = (i * 17) as u8; // 0, 17, 34, ... 255
            }
        }

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        let out = &result.output_arrays[0];
        assert_eq!(out.dims.len(), 3);
        assert_eq!(out.dims[0].size, 3); // color
        assert_eq!(out.dims[1].size, 4); // x
        assert_eq!(out.dims[2].size, 4); // y

        // Verify false color: pixel 0 (value=0) should be blue, pixel 15 (value=255) should be red
        let lut = jet_colormap();
        if let NDDataBuffer::U8(ref v) = out.data {
            // First pixel (value=0)
            assert_eq!(v[0], lut[0][0]); // R
            assert_eq!(v[1], lut[0][1]); // G
            assert_eq!(v[2], lut[0][2]); // B
            // Last pixel (value=255)
            let last = 15 * 3;
            assert_eq!(v[last], lut[255][0]); // R
            assert_eq!(v[last + 1], lut[255][1]); // G
            assert_eq!(v[last + 2], lut[255][2]); // B
        } else {
            panic!("expected UInt8 output");
        }
    }

    #[test]
    fn test_rgb1_to_rgb2_conversion() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::RGB2,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: false,
        };
        let mut proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        // Create RGB1 image: dims [3, 4, 4]
        let mut arr = NDArray::new(
            vec![NDDimension::new(3), NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = (i % 256) as u8;
            }
        }

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        let out = &result.output_arrays[0];
        assert_eq!(out.dims.len(), 3);
        // RGB2 has color dim in position 1
        assert_eq!(out.dims[1].size, 3);
    }

    #[test]
    fn test_rgb2_to_mono_conversion() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::Mono,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: false,
        };
        let mut proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        // Create RGB2 image: dims [4, 3, 4]
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(3), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = 128;
            }
        }

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        let out = &result.output_arrays[0];
        // Mono output should be 2D
        assert_eq!(out.dims.len(), 2);
    }

    #[test]
    fn test_detect_color_mode() {
        // 2D -> Mono
        let arr2d = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        assert_eq!(detect_color_mode(&arr2d), NDColorMode::Mono);

        // 3D with color dim first -> RGB1
        let arr_rgb1 = NDArray::new(
            vec![NDDimension::new(3), NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        assert_eq!(detect_color_mode(&arr_rgb1), NDColorMode::RGB1);

        // 3D with color dim second -> RGB2
        let arr_rgb2 = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(3), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        assert_eq!(detect_color_mode(&arr_rgb2), NDColorMode::RGB2);

        // 3D with color dim last -> RGB3
        let arr_rgb3 = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4), NDDimension::new(3)],
            NDDataType::UInt8,
        );
        assert_eq!(detect_color_mode(&arr_rgb3), NDColorMode::RGB3);
    }

    #[test]
    fn test_same_mode_passthrough() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::Mono,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: false,
        };
        let mut proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        // 2D mono input with Mono target -> passthrough
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.unique_id = 42;
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(result.output_arrays[0].unique_id, 42);
        assert_eq!(result.output_arrays[0].dims.len(), 2);
    }

    #[test]
    fn test_jet_colormap_endpoints() {
        let lut = jet_colormap();
        // At v=0: r=clamp(1.5-3.0,0,1)=0, g=clamp(1.5-2.0,0,1)=0, b=clamp(1.5-1.0,0,1)=0.5
        assert_eq!(lut[0][0], 0); // R
        assert_eq!(lut[0][1], 0); // G
        assert_eq!(lut[0][2], 127); // B (0.5 * 255 = 127)

        // At v=1: r=clamp(1.5-1.0,0,1)=0.5, g=clamp(1.5-2.0,0,1)=0, b=clamp(1.5-3.0,0,1)=0
        assert_eq!(lut[255][0], 127); // R
        assert_eq!(lut[255][1], 0); // G
        assert_eq!(lut[255][2], 0); // B
    }
}
