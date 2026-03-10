use std::sync::Arc;

use ad_core::color::{self, NDColorMode, NDBayerPattern};
use ad_core::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::NDPluginProcess;

/// Simple Bayer demosaic using bilinear interpolation.
pub fn bayer_to_rgb1(src: &NDArray, pattern: NDBayerPattern) -> Option<NDArray> {
    if src.dims.len() != 2 {
        return None;
    }
    let w = src.dims[0].size;
    let h = src.dims[1].size;

    let get_val = |x: usize, y: usize| -> f64 {
        let idx = y * w + x;
        src.data.get_as_f64(idx).unwrap_or(0.0)
    };

    let n = w * h;
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

    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            let val = get_val(x, y);
            let even_row = (y % 2 == 0) == r_row_even;
            let even_col = (x % 2 == 0) == r_col_even;

            match (even_row, even_col) {
                (true, true) => {
                    // Red pixel
                    r[idx] = val;
                    // Interpolate G from neighbors
                    let mut gsum = 0.0;
                    let mut gc = 0;
                    if x > 0 { gsum += get_val(x - 1, y); gc += 1; }
                    if x < w - 1 { gsum += get_val(x + 1, y); gc += 1; }
                    if y > 0 { gsum += get_val(x, y - 1); gc += 1; }
                    if y < h - 1 { gsum += get_val(x, y + 1); gc += 1; }
                    g[idx] = if gc > 0 { gsum / gc as f64 } else { 0.0 };
                    // Interpolate B from diagonal neighbors
                    let mut bsum = 0.0;
                    let mut bc = 0;
                    if x > 0 && y > 0 { bsum += get_val(x - 1, y - 1); bc += 1; }
                    if x < w - 1 && y > 0 { bsum += get_val(x + 1, y - 1); bc += 1; }
                    if x > 0 && y < h - 1 { bsum += get_val(x - 1, y + 1); bc += 1; }
                    if x < w - 1 && y < h - 1 { bsum += get_val(x + 1, y + 1); bc += 1; }
                    b[idx] = if bc > 0 { bsum / bc as f64 } else { 0.0 };
                }
                (true, false) | (false, true) => {
                    // Green pixel
                    g[idx] = val;
                    if even_row {
                        // Green in red row
                        let mut rsum = 0.0;
                        let mut rc = 0;
                        if x > 0 { rsum += get_val(x - 1, y); rc += 1; }
                        if x < w - 1 { rsum += get_val(x + 1, y); rc += 1; }
                        r[idx] = if rc > 0 { rsum / rc as f64 } else { 0.0 };
                        let mut bsum = 0.0;
                        let mut bc = 0;
                        if y > 0 { bsum += get_val(x, y - 1); bc += 1; }
                        if y < h - 1 { bsum += get_val(x, y + 1); bc += 1; }
                        b[idx] = if bc > 0 { bsum / bc as f64 } else { 0.0 };
                    } else {
                        // Green in blue row
                        let mut bsum = 0.0;
                        let mut bc = 0;
                        if x > 0 { bsum += get_val(x - 1, y); bc += 1; }
                        if x < w - 1 { bsum += get_val(x + 1, y); bc += 1; }
                        b[idx] = if bc > 0 { bsum / bc as f64 } else { 0.0 };
                        let mut rsum = 0.0;
                        let mut rc = 0;
                        if y > 0 { rsum += get_val(x, y - 1); rc += 1; }
                        if y < h - 1 { rsum += get_val(x, y + 1); rc += 1; }
                        r[idx] = if rc > 0 { rsum / rc as f64 } else { 0.0 };
                    }
                }
                (false, false) => {
                    // Blue pixel
                    b[idx] = val;
                    let mut gsum = 0.0;
                    let mut gc = 0;
                    if x > 0 { gsum += get_val(x - 1, y); gc += 1; }
                    if x < w - 1 { gsum += get_val(x + 1, y); gc += 1; }
                    if y > 0 { gsum += get_val(x, y - 1); gc += 1; }
                    if y < h - 1 { gsum += get_val(x, y + 1); gc += 1; }
                    g[idx] = if gc > 0 { gsum / gc as f64 } else { 0.0 };
                    let mut rsum = 0.0;
                    let mut rc = 0;
                    if x > 0 && y > 0 { rsum += get_val(x - 1, y - 1); rc += 1; }
                    if x < w - 1 && y > 0 { rsum += get_val(x + 1, y - 1); rc += 1; }
                    if x > 0 && y < h - 1 { rsum += get_val(x - 1, y + 1); rc += 1; }
                    if x < w - 1 && y < h - 1 { rsum += get_val(x + 1, y + 1); rc += 1; }
                    r[idx] = if rc > 0 { rsum / rc as f64 } else { 0.0 };
                }
            }
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

/// Color convert plugin configuration.
#[derive(Debug, Clone)]
pub struct ColorConvertConfig {
    pub target_mode: NDColorMode,
    pub bayer_pattern: NDBayerPattern,
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
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        let result = match self.config.target_mode {
            NDColorMode::Mono => color::rgb1_to_mono(array).ok(),
            NDColorMode::RGB1 => {
                if array.dims.len() == 2 {
                    bayer_to_rgb1(array, self.config.bayer_pattern)
                        .or_else(|| color::mono_to_rgb1(array).ok())
                } else {
                    color::convert_rgb_layout(
                        array, NDColorMode::RGB2, NDColorMode::RGB1,
                    ).ok()
                }
            }
            _ => None,
        };
        match result {
            Some(out) => vec![Arc::new(out)],
            None => vec![],
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
            for i in 0..16 { v[i] = 128; }
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
        };
        let mut proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..16 { v[i] = 128; }
        }

        let outputs = proc.process_array(&arr, &pool);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].dims[0].size, 3); // RGB color dim
    }
}
