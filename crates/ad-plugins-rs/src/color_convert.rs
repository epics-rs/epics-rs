use std::sync::Arc;

// Not gated on `parallel`: `should_parallelize` is the whole decision now
// and this file asks it on both arms.
use crate::par_util;
use parking_lot::Mutex;
#[cfg(feature = "parallel")]
use rayon::prelude::*;

use ad_core_rs::color::{self, NDBayerPattern, NDColorMode};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};

/// Simple Bayer demosaic using bilinear interpolation.
pub fn bayer_to_rgb1(src: &NDArray, pattern: NDBayerPattern) -> Option<NDArray> {
    if src.dims.len() != 2 {
        return None;
    }
    let w = src.dims[0].size;
    let h = src.dims[1].size;

    // Read dimension offsets to adjust the bayer phase when offset is odd
    let offset_x = src.dims[0].offset;
    let offset_y = src.dims[1].offset;

    // Pre-compute source values into a flat f64 vec for efficient random access
    let n = w * h;
    let src_vals: Vec<f64> = (0..n)
        .map(|i| src.data.get_as_f64(i).unwrap_or(0.0))
        .collect();
    let get_val = |x: usize, y: usize| -> f64 { src_vals[y * w + x] };

    let mut r = vec![0.0f64; n];
    let mut g = vec![0.0f64; n];
    let mut b = vec![0.0f64; n];

    // Determine which color each pixel position has, flipping phase for odd offsets
    let (mut r_row_even, mut r_col_even) = match pattern {
        NDBayerPattern::RGGB => (true, true),
        NDBayerPattern::GBRG => (true, false),
        NDBayerPattern::GRBG => (false, true),
        NDBayerPattern::BGGR => (false, false),
    };
    if offset_x % 2 != 0 {
        r_col_even = !r_col_even;
    }
    if offset_y % 2 != 0 {
        r_row_even = !r_row_even;
    }

    // Helper to demosaic a single row into (r, g, b) slices.
    //
    // C interpolates only pixels not touching a border
    // (NDPluginColorConvert.cpp:305): a pixel with x in 0/rowSize-1 or y in
    // 0/numRows-1 keeps only its native Bayer channel, the other two stay 0.
    // Interior pixels always have all 8 neighbours, so C's fixed divisors
    // (/4 for the red/blue arms, /2 for green) apply directly.
    let demosaic_row = |y: usize, r_row: &mut [f64], g_row: &mut [f64], b_row: &mut [f64]| {
        let even_row = (y % 2 == 0) == r_row_even;
        for x in 0..w {
            let val = get_val(x, y);
            let even_col = (x % 2 == 0) == r_col_even;
            let interior = x > 0 && x + 1 < w && y > 0 && y + 1 < h;

            match (even_row, even_col) {
                (true, true) => {
                    // Red pixel: green = orthogonal mean, blue = diagonal mean
                    // (NDPluginColorConvert.cpp:308-309).
                    r_row[x] = val;
                    if interior {
                        g_row[x] = (get_val(x - 1, y)
                            + get_val(x + 1, y)
                            + get_val(x, y - 1)
                            + get_val(x, y + 1))
                            / 4.0;
                        b_row[x] = (get_val(x - 1, y - 1)
                            + get_val(x + 1, y - 1)
                            + get_val(x - 1, y + 1)
                            + get_val(x + 1, y + 1))
                            / 4.0;
                    }
                }
                (true, false) | (false, true) => {
                    // Green pixel.
                    g_row[x] = val;
                    if interior {
                        if even_row {
                            // Green next to red: red horizontal, blue vertical
                            // (NDPluginColorConvert.cpp:313-314).
                            r_row[x] = (get_val(x - 1, y) + get_val(x + 1, y)) / 2.0;
                            b_row[x] = (get_val(x, y - 1) + get_val(x, y + 1)) / 2.0;
                        } else {
                            // Green next to blue: blue horizontal, red vertical
                            // (NDPluginColorConvert.cpp:318-319).
                            b_row[x] = (get_val(x - 1, y) + get_val(x + 1, y)) / 2.0;
                            r_row[x] = (get_val(x, y - 1) + get_val(x, y + 1)) / 2.0;
                        }
                    }
                }
                (false, false) => {
                    // Blue pixel: green = orthogonal mean, red = diagonal mean
                    // (NDPluginColorConvert.cpp:308-309, blue branch).
                    b_row[x] = val;
                    if interior {
                        g_row[x] = (get_val(x - 1, y)
                            + get_val(x + 1, y)
                            + get_val(x, y - 1)
                            + get_val(x, y + 1))
                            / 4.0;
                        r_row[x] = (get_val(x - 1, y - 1)
                            + get_val(x + 1, y - 1)
                            + get_val(x - 1, y + 1)
                            + get_val(x + 1, y + 1))
                            / 4.0;
                    }
                }
            }
        }
    };

    let use_parallel = par_util::should_parallelize(n);

    if use_parallel {
        #[cfg(feature = "parallel")]
        {
            // Split r, g, b into per-row mutable slices and process in parallel
            let r_rows: Vec<&mut [f64]> = r.chunks_mut(w).collect();
            let g_rows: Vec<&mut [f64]> = g.chunks_mut(w).collect();
            let b_rows: Vec<&mut [f64]> = b.chunks_mut(w).collect();

            par_util::thread_pool().install(|| {
                r_rows
                    .into_par_iter()
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

    let dims = vec![
        NDDimension::new(3),
        NDDimension::new(w),
        NDDimension::new(h),
    ];
    let mut arr = NDArray::new(dims, src.data.data_type());
    arr.data = out_data;
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    Some(arr)
}

/// Rainbow false-color lookup table (`falseColor == 1`), 256 RGB entries.
///
/// Ported verbatim from ADCore `colorMaps.h` (RainbowColorR/G/B). C selects
/// this table when the FalseColor parameter is 1
/// (NDPluginColorConvert.cpp:62-66).
const RAINBOW_COLOR_MAP: [[u8; 3]; 256] = [
    [0, 0, 0],
    [0, 0, 4],
    [0, 0, 8],
    [0, 0, 12],
    [0, 0, 16],
    [0, 0, 20],
    [0, 0, 24],
    [0, 0, 28],
    [0, 0, 32],
    [0, 0, 36],
    [0, 0, 40],
    [0, 0, 45],
    [0, 0, 49],
    [0, 0, 53],
    [0, 0, 57],
    [0, 0, 61],
    [0, 0, 65],
    [0, 0, 69],
    [0, 0, 73],
    [0, 0, 77],
    [0, 0, 81],
    [0, 0, 85],
    [0, 0, 89],
    [0, 0, 93],
    [0, 0, 97],
    [0, 0, 101],
    [0, 0, 105],
    [0, 0, 109],
    [0, 0, 113],
    [0, 0, 117],
    [0, 0, 121],
    [0, 0, 125],
    [0, 0, 130],
    [0, 0, 134],
    [0, 0, 138],
    [0, 0, 142],
    [0, 0, 146],
    [0, 0, 150],
    [0, 0, 154],
    [0, 0, 158],
    [0, 0, 162],
    [0, 0, 166],
    [0, 0, 170],
    [0, 0, 174],
    [0, 0, 178],
    [0, 0, 182],
    [0, 0, 186],
    [0, 0, 190],
    [0, 0, 194],
    [0, 0, 198],
    [0, 0, 202],
    [0, 0, 206],
    [0, 0, 210],
    [0, 0, 215],
    [0, 0, 219],
    [0, 0, 223],
    [0, 0, 227],
    [0, 0, 231],
    [0, 0, 235],
    [0, 0, 239],
    [0, 0, 243],
    [0, 0, 247],
    [0, 0, 251],
    [0, 0, 255],
    [0, 4, 255],
    [0, 8, 255],
    [0, 12, 255],
    [0, 16, 255],
    [0, 20, 255],
    [0, 24, 255],
    [0, 28, 255],
    [0, 32, 255],
    [0, 36, 255],
    [0, 40, 255],
    [0, 44, 255],
    [0, 48, 255],
    [0, 52, 255],
    [0, 56, 255],
    [0, 60, 255],
    [0, 64, 255],
    [0, 68, 255],
    [0, 72, 255],
    [0, 76, 255],
    [0, 80, 255],
    [0, 84, 255],
    [0, 88, 255],
    [0, 92, 255],
    [0, 96, 255],
    [0, 100, 255],
    [0, 104, 255],
    [0, 108, 255],
    [0, 112, 255],
    [0, 116, 255],
    [0, 120, 255],
    [0, 124, 255],
    [0, 128, 255],
    [0, 131, 255],
    [0, 135, 255],
    [0, 139, 255],
    [0, 143, 255],
    [0, 147, 255],
    [0, 151, 255],
    [0, 155, 255],
    [0, 159, 255],
    [0, 163, 255],
    [0, 167, 255],
    [0, 171, 255],
    [0, 175, 255],
    [0, 179, 255],
    [0, 183, 255],
    [0, 187, 255],
    [0, 191, 255],
    [0, 195, 255],
    [0, 199, 255],
    [0, 203, 255],
    [0, 207, 255],
    [0, 211, 255],
    [0, 215, 255],
    [0, 219, 255],
    [0, 223, 255],
    [0, 227, 255],
    [0, 231, 255],
    [0, 235, 255],
    [0, 239, 255],
    [0, 243, 255],
    [0, 247, 255],
    [0, 251, 255],
    [0, 255, 255],
    [4, 255, 251],
    [8, 255, 247],
    [12, 255, 243],
    [16, 255, 239],
    [20, 255, 235],
    [24, 255, 231],
    [28, 255, 227],
    [32, 255, 223],
    [36, 255, 219],
    [40, 255, 215],
    [44, 255, 211],
    [48, 255, 207],
    [52, 255, 203],
    [56, 255, 199],
    [60, 255, 195],
    [64, 255, 191],
    [68, 255, 187],
    [72, 255, 183],
    [76, 255, 179],
    [80, 255, 175],
    [84, 255, 171],
    [88, 255, 167],
    [92, 255, 163],
    [96, 255, 159],
    [100, 255, 155],
    [104, 255, 151],
    [108, 255, 147],
    [112, 255, 143],
    [116, 255, 139],
    [120, 255, 135],
    [124, 255, 131],
    [128, 255, 128],
    [131, 255, 124],
    [135, 255, 120],
    [139, 255, 116],
    [143, 255, 112],
    [147, 255, 108],
    [151, 255, 104],
    [155, 255, 100],
    [159, 255, 96],
    [163, 255, 92],
    [167, 255, 88],
    [171, 255, 84],
    [175, 255, 80],
    [179, 255, 76],
    [183, 255, 72],
    [187, 255, 68],
    [191, 255, 64],
    [195, 255, 60],
    [199, 255, 56],
    [203, 255, 52],
    [207, 255, 48],
    [211, 255, 44],
    [215, 255, 40],
    [219, 255, 36],
    [223, 255, 32],
    [227, 255, 28],
    [231, 255, 24],
    [235, 255, 20],
    [239, 255, 16],
    [243, 255, 12],
    [247, 255, 8],
    [251, 255, 4],
    [255, 255, 0],
    [255, 251, 0],
    [255, 247, 0],
    [255, 243, 0],
    [255, 239, 0],
    [255, 235, 0],
    [255, 231, 0],
    [255, 227, 0],
    [255, 223, 0],
    [255, 219, 0],
    [255, 215, 0],
    [255, 211, 0],
    [255, 207, 0],
    [255, 203, 0],
    [255, 199, 0],
    [255, 195, 0],
    [255, 191, 0],
    [255, 187, 0],
    [255, 183, 0],
    [255, 179, 0],
    [255, 175, 0],
    [255, 171, 0],
    [255, 167, 0],
    [255, 163, 0],
    [255, 159, 0],
    [255, 155, 0],
    [255, 151, 0],
    [255, 147, 0],
    [255, 143, 0],
    [255, 139, 0],
    [255, 135, 0],
    [255, 131, 0],
    [255, 128, 0],
    [255, 124, 0],
    [255, 120, 0],
    [255, 116, 0],
    [255, 112, 0],
    [255, 108, 0],
    [255, 104, 0],
    [255, 100, 0],
    [255, 96, 0],
    [255, 92, 0],
    [255, 88, 0],
    [255, 84, 0],
    [255, 80, 0],
    [255, 76, 0],
    [255, 72, 0],
    [255, 68, 0],
    [255, 64, 0],
    [255, 60, 0],
    [255, 56, 0],
    [255, 52, 0],
    [255, 48, 0],
    [255, 44, 0],
    [255, 40, 0],
    [255, 36, 0],
    [255, 32, 0],
    [255, 28, 0],
    [255, 24, 0],
    [255, 20, 0],
    [255, 16, 0],
    [255, 12, 0],
    [255, 8, 0],
    [255, 4, 0],
    [255, 255, 255],
];

/// Iron false-color lookup table (`falseColor == 2`), 256 RGB entries.
///
/// Ported verbatim from ADCore `colorMaps.h` (IronColorR/G/B). C selects this
/// table when the FalseColor parameter is 2
/// (NDPluginColorConvert.cpp:68-72).
const IRON_COLOR_MAP: [[u8; 3]; 256] = [
    [0, 0, 0],
    [1, 0, 1],
    [3, 1, 3],
    [4, 1, 5],
    [6, 2, 6],
    [7, 2, 8],
    [9, 3, 10],
    [10, 3, 11],
    [12, 4, 13],
    [13, 4, 15],
    [15, 5, 17],
    [16, 5, 18],
    [18, 6, 20],
    [19, 6, 22],
    [21, 7, 23],
    [22, 7, 25],
    [24, 8, 27],
    [25, 8, 28],
    [27, 9, 30],
    [28, 9, 32],
    [30, 10, 34],
    [31, 10, 35],
    [33, 11, 37],
    [34, 11, 39],
    [36, 12, 40],
    [37, 12, 42],
    [39, 13, 44],
    [40, 13, 45],
    [42, 14, 47],
    [43, 14, 49],
    [45, 15, 51],
    [46, 15, 52],
    [48, 16, 54],
    [49, 16, 56],
    [51, 17, 57],
    [52, 17, 59],
    [54, 18, 61],
    [55, 18, 62],
    [57, 19, 64],
    [58, 19, 66],
    [60, 20, 68],
    [61, 20, 69],
    [63, 21, 71],
    [64, 21, 73],
    [66, 22, 74],
    [67, 22, 76],
    [69, 23, 78],
    [70, 23, 79],
    [72, 24, 81],
    [73, 24, 83],
    [75, 25, 85],
    [76, 25, 86],
    [78, 26, 88],
    [79, 26, 90],
    [81, 27, 91],
    [82, 27, 93],
    [84, 28, 95],
    [85, 28, 96],
    [87, 29, 98],
    [88, 29, 100],
    [90, 30, 102],
    [91, 30, 103],
    [93, 31, 105],
    [94, 31, 107],
    [96, 32, 108],
    [97, 32, 110],
    [99, 33, 112],
    [100, 33, 113],
    [102, 34, 115],
    [103, 34, 117],
    [105, 35, 119],
    [106, 35, 120],
    [108, 36, 120],
    [109, 36, 119],
    [111, 37, 118],
    [112, 37, 117],
    [114, 38, 116],
    [115, 38, 115],
    [117, 39, 114],
    [118, 39, 113],
    [120, 40, 112],
    [121, 40, 111],
    [123, 41, 110],
    [124, 41, 109],
    [126, 42, 108],
    [127, 42, 107],
    [129, 43, 106],
    [130, 43, 105],
    [132, 44, 104],
    [133, 44, 103],
    [135, 45, 102],
    [136, 45, 101],
    [138, 46, 100],
    [139, 46, 99],
    [141, 47, 98],
    [142, 47, 97],
    [144, 48, 96],
    [145, 48, 95],
    [147, 49, 94],
    [148, 49, 93],
    [150, 50, 92],
    [151, 50, 91],
    [153, 51, 90],
    [154, 51, 89],
    [156, 52, 88],
    [157, 52, 87],
    [159, 53, 86],
    [160, 53, 85],
    [162, 54, 84],
    [163, 54, 83],
    [165, 55, 82],
    [166, 55, 81],
    [168, 56, 80],
    [169, 56, 79],
    [171, 57, 78],
    [172, 57, 77],
    [174, 58, 76],
    [175, 58, 75],
    [177, 59, 74],
    [178, 59, 73],
    [180, 60, 72],
    [181, 60, 71],
    [183, 61, 70],
    [184, 61, 69],
    [186, 62, 68],
    [187, 62, 67],
    [189, 63, 66],
    [190, 63, 65],
    [192, 64, 64],
    [192, 65, 64],
    [192, 67, 64],
    [192, 68, 64],
    [192, 70, 64],
    [192, 71, 64],
    [192, 73, 64],
    [192, 74, 64],
    [192, 76, 64],
    [192, 77, 64],
    [192, 79, 64],
    [192, 80, 64],
    [192, 82, 64],
    [192, 83, 64],
    [192, 85, 64],
    [192, 86, 64],
    [192, 88, 64],
    [192, 89, 64],
    [192, 91, 64],
    [192, 92, 64],
    [192, 94, 64],
    [192, 95, 64],
    [192, 97, 64],
    [192, 98, 64],
    [192, 100, 64],
    [192, 101, 64],
    [192, 103, 64],
    [192, 104, 64],
    [192, 106, 64],
    [192, 107, 64],
    [192, 109, 64],
    [192, 110, 64],
    [192, 112, 64],
    [192, 113, 64],
    [192, 115, 64],
    [192, 116, 64],
    [192, 118, 64],
    [192, 119, 64],
    [192, 121, 64],
    [192, 122, 64],
    [192, 124, 64],
    [192, 125, 64],
    [192, 127, 64],
    [192, 128, 64],
    [192, 130, 64],
    [192, 131, 64],
    [192, 133, 64],
    [192, 134, 64],
    [192, 136, 64],
    [192, 137, 64],
    [192, 139, 64],
    [192, 140, 64],
    [192, 142, 64],
    [192, 143, 64],
    [192, 145, 64],
    [192, 146, 64],
    [192, 148, 64],
    [192, 149, 64],
    [192, 151, 64],
    [192, 152, 64],
    [192, 154, 64],
    [192, 155, 64],
    [192, 157, 64],
    [192, 158, 64],
    [192, 160, 64],
    [192, 161, 64],
    [192, 163, 64],
    [192, 164, 64],
    [192, 166, 64],
    [192, 167, 64],
    [192, 169, 64],
    [192, 170, 64],
    [192, 172, 64],
    [192, 173, 64],
    [192, 175, 64],
    [192, 176, 64],
    [192, 178, 64],
    [192, 179, 64],
    [192, 181, 64],
    [192, 182, 64],
    [192, 184, 64],
    [192, 185, 64],
    [192, 187, 64],
    [192, 188, 64],
    [192, 190, 64],
    [192, 191, 64],
    [192, 193, 64],
    [192, 194, 64],
    [192, 196, 64],
    [192, 197, 64],
    [192, 199, 64],
    [192, 200, 64],
    [192, 202, 64],
    [192, 203, 64],
    [192, 205, 64],
    [192, 206, 64],
    [192, 208, 64],
    [192, 209, 64],
    [192, 211, 64],
    [192, 212, 64],
    [192, 214, 64],
    [192, 215, 64],
    [192, 217, 64],
    [192, 218, 64],
    [192, 220, 64],
    [192, 221, 64],
    [192, 223, 64],
    [192, 224, 64],
    [192, 226, 64],
    [192, 227, 64],
    [192, 229, 64],
    [192, 230, 64],
    [192, 232, 64],
    [192, 233, 64],
    [192, 235, 64],
    [192, 236, 64],
    [192, 238, 64],
    [192, 239, 64],
    [192, 241, 64],
    [192, 242, 64],
    [192, 244, 64],
    [192, 245, 64],
    [192, 247, 64],
    [192, 248, 64],
    [192, 250, 64],
    [192, 251, 64],
    [192, 253, 64],
    [255, 255, 255],
];

/// Select the false-color LUT for a `falseColor` parameter value.
///
/// Matches NDPluginColorConvert.cpp:61-78: 1 => Rainbow, 2 => Iron, any other
/// value => no false color (C resets `falseColor` to 0 in the `default` arm).
fn false_color_lut(false_color: i32) -> Option<&'static [[u8; 3]; 256]> {
    match false_color {
        1 => Some(&RAINBOW_COLOR_MAP),
        2 => Some(&IRON_COLOR_MAP),
        _ => None,
    }
}

/// Convert a mono 8-bit image to RGB1 using a false-color LUT.
///
/// C reads the `FalseColor` parameter **only** for 8-bit arrays
/// (NDPluginColorConvert.cpp:59-60: `if (pArray->dataType == NDInt8 ||
/// pArray->dataType == NDUInt8)`); for every wider type the local `falseColor`
/// stays at its `0` initializer (`:45`) and the plain grayscale replication runs.
/// So both `NDInt8` and `NDUInt8` are colormapped, and nothing else is.
///
/// The LUT index is the *low byte* of the sample — C writes
/// `colorMapRGB + 3 * ((unsigned char)*pIn)` (`:108`), so a negative `epicsInt8`
/// indexes 128..=255. The output keeps the input data type, and for `NDInt8`
/// C `memcpy`s the LUT's raw bytes into `epicsInt8` cells, so a LUT entry above
/// 127 lands as a negative sample.
///
/// Returns `None` for a non-8-bit or non-2-D input, or a `false_color` value
/// with no table.
fn false_color_mono_to_rgb1(src: &NDArray, false_color: i32) -> Option<NDArray> {
    if src.dims.len() != 2 {
        return None;
    }
    let lut = false_color_lut(false_color)?;

    let w = src.dims[0].size;
    let h = src.dims[1].size;
    let n = w * h;

    // Map each sample through the LUT by its low byte, keeping the input type.
    let (data, data_type) = match &src.data {
        NDDataBuffer::U8(src_slice) => {
            let mut out = vec![0u8; n * 3];
            for i in 0..n {
                let [r, g, b] = lut[src_slice[i] as usize];
                out[i * 3] = r;
                out[i * 3 + 1] = g;
                out[i * 3 + 2] = b;
            }
            (NDDataBuffer::U8(out), NDDataType::UInt8)
        }
        NDDataBuffer::I8(src_slice) => {
            let mut out = vec![0i8; n * 3];
            for i in 0..n {
                let [r, g, b] = lut[src_slice[i] as u8 as usize];
                out[i * 3] = r as i8;
                out[i * 3 + 1] = g as i8;
                out[i * 3 + 2] = b as i8;
            }
            (NDDataBuffer::I8(out), NDDataType::Int8)
        }
        _ => return None,
    };

    let dims = vec![
        NDDimension::new(3),
        NDDimension::new(w),
        NDDimension::new(h),
    ];
    let mut arr = NDArray::new(dims, data_type);
    arr.data = data;
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
    /// False color mode: 0=off, 1=Rainbow, 2=Iron. Nonzero is treated as enabled.
    pub false_color: i32,
}

/// Pure color conversion processing logic.
pub struct ColorConvertProcessor {
    config: Mutex<ColorConvertConfig>,
    color_mode_out_idx: Option<usize>,
    false_color_idx: Option<usize>,
}

impl ColorConvertProcessor {
    pub fn new(config: ColorConvertConfig) -> Self {
        Self {
            config: Mutex::new(config),
            color_mode_out_idx: None,
            false_color_idx: None,
        }
    }
}

impl NDPluginProcess for ColorConvertProcessor {
    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        base.create_param("COLOR_MODE_OUT", ParamType::Int32)?;
        base.create_param("FALSE_COLOR", ParamType::Int32)?;
        self.color_mode_out_idx = base.find_param("COLOR_MODE_OUT");
        self.false_color_idx = base.find_param("FALSE_COLOR");
        Ok(())
    }

    fn on_param_change(
        &self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        if Some(reason) == self.color_mode_out_idx {
            self.config.lock().target_mode = NDColorMode::from_i32(params.value.as_i32());
        } else if Some(reason) == self.false_color_idx {
            self.config.lock().false_color = params.value.as_i32();
        }
        ad_core_rs::plugin::runtime::ParamChangeResult::updates(vec![])
    }

    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        // C `convertColor` (NDPluginColorConvert.cpp:44,54-55) starts from
        // `int colorMode = NDColorModeMono` and overwrites it only from the
        // `ColorMode` attribute; it never infers a layout from the dimensions.
        // `NDArray::info()` owns that rule for the whole workspace.
        let src_mode = array.info().color_mode;
        let (target, false_color, bayer_pattern) = {
            let config = self.config.lock();
            (config.target_mode, config.false_color, config.bayer_pattern)
        };

        // C:584 — `if (!pArrayOut) pArrayOut = this->pNDArrayPool->copy(pArray, NULL, 1);`
        // Every arm that does not convert (same mode, unsupported pair, or a shape the
        // arm rejects, e.g. Mono with `ndims != 2` at :84) leaves `pArrayOut` NULL and
        // the untouched input is forwarded — with its own ColorMode, since
        // `changedColorMode` stayed 0 (:589). A frame is never dropped.
        let passthrough = || ProcessResult::arrays(vec![Arc::new(array.clone())]);

        if src_mode == target {
            return passthrough();
        }

        // Step 1: Convert source to RGB1 intermediate
        let rgb1 = match src_mode {
            NDColorMode::RGB1 => Some(array.clone()),
            NDColorMode::Mono => {
                if false_color != 0 {
                    false_color_mono_to_rgb1(array, false_color)
                        .or_else(|| color::mono_to_rgb1(array).ok())
                } else {
                    color::mono_to_rgb1(array).ok()
                }
            }
            NDColorMode::Bayer => bayer_to_rgb1(array, bayer_pattern),
            NDColorMode::RGB2 | NDColorMode::RGB3 => {
                color::convert_rgb_layout(array, src_mode, NDColorMode::RGB1).ok()
            }
            NDColorMode::YUV444 => color::yuv444_to_rgb1(array).ok(),
            NDColorMode::YUV422 => color::yuv422_to_rgb1(array).ok(),
            NDColorMode::YUV411 => color::yuv411_to_rgb1(array).ok(),
        };

        let rgb1 = match rgb1 {
            Some(r) => r,
            None => return passthrough(),
        };

        // Step 2: Convert RGB1 intermediate to target
        let result = match target {
            NDColorMode::RGB1 => Some(rgb1),
            NDColorMode::Mono => color::rgb1_to_mono(&rgb1).ok(),
            NDColorMode::Bayer => None,
            NDColorMode::RGB2 | NDColorMode::RGB3 => {
                color::convert_rgb_layout(&rgb1, NDColorMode::RGB1, target).ok()
            }
            NDColorMode::YUV444 => color::rgb1_to_yuv444(&rgb1).ok(),
            NDColorMode::YUV422 => color::rgb1_to_yuv422(&rgb1).ok(),
            NDColorMode::YUV411 => color::rgb1_to_yuv411(&rgb1).ok(),
        };

        match result {
            Some(mut out) => {
                // C++: set ColorMode attribute on output array
                let color_mode_val = match target {
                    NDColorMode::Mono => 0i32,
                    NDColorMode::Bayer => 1,
                    NDColorMode::RGB1 => 2,
                    NDColorMode::RGB2 => 3,
                    NDColorMode::RGB3 => 4,
                    NDColorMode::YUV444 => 5,
                    NDColorMode::YUV422 => 6,
                    NDColorMode::YUV411 => 7,
                };
                use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
                out.attributes.add(NDAttribute::new_static(
                    "ColorMode",
                    "Color Mode",
                    NDAttrSource::Driver,
                    NDAttrValue::Int32(color_mode_val),
                ));
                ProcessResult::arrays(vec![Arc::new(out)])
            }
            None => passthrough(),
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

    // RGB1 pixel (x,y) channel c lives at out[(y*w + x)*3 + c].
    fn rgb1_pixel(arr: &NDArray, w: usize, x: usize, y: usize) -> [u8; 3] {
        let i = (y * w + x) * 3;
        if let NDDataBuffer::U8(ref v) = arr.data {
            [v[i], v[i + 1], v[i + 2]]
        } else {
            panic!("expected UInt8 RGB1 output");
        }
    }

    #[test]
    fn test_adp4_bayer_border_keeps_native_channel_only() {
        // 4x4 RGGB, all pixels = 100. C interpolates only interior pixels
        // (NDPluginColorConvert.cpp:305); border pixels keep only their native
        // Bayer channel, the other two stay 0. Previously the Rust border was
        // interpolated from available neighbours (e.g. corner -> (100,100,100)).
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v.iter_mut().for_each(|p| *p = 100);
        }
        let rgb = bayer_to_rgb1(&arr, NDBayerPattern::RGGB).unwrap();

        // Border pixels: native channel only.
        assert_eq!(rgb1_pixel(&rgb, 4, 0, 0), [100, 0, 0]); // corner, red
        assert_eq!(rgb1_pixel(&rgb, 4, 1, 0), [0, 100, 0]); // top edge, green
        assert_eq!(rgb1_pixel(&rgb, 4, 0, 1), [0, 100, 0]); // left edge, green
        assert_eq!(rgb1_pixel(&rgb, 4, 3, 3), [0, 0, 100]); // corner, blue

        // Interior pixels: fully interpolated (all neighbours = 100).
        assert_eq!(rgb1_pixel(&rgb, 4, 1, 1), [100, 100, 100]); // blue
        assert_eq!(rgb1_pixel(&rgb, 4, 1, 2), [100, 100, 100]); // green
        assert_eq!(rgb1_pixel(&rgb, 4, 2, 2), [100, 100, 100]); // red
    }

    #[test]
    fn test_adp4_bayer_interior_uses_fixed_quarter_divisor() {
        // 3x3 RGGB: the only interior pixel is the blue centre (1,1). Its red is
        // the diagonal mean /4 and green the orthogonal mean /4
        // (NDPluginColorConvert.cpp:308-309). Distinct neighbour values pin the
        // divisor: diagonals all 200 -> red 200; orthogonals 40,40,80,80 ->
        // green 60.
        let mut arr = NDArray::new(
            vec![NDDimension::new(3), NDDimension::new(3)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            // row0: 200 40 200 / row1: 80 128 80 / row2: 200 40 200
            v.copy_from_slice(&[200, 40, 200, 80, 128, 80, 200, 40, 200]);
        }
        let rgb = bayer_to_rgb1(&arr, NDBayerPattern::RGGB).unwrap();

        // Interior blue centre: red = (200*4)/4 = 200, green = (40+40+80+80)/4 = 60.
        assert_eq!(rgb1_pixel(&rgb, 3, 1, 1), [200, 60, 128]);
        // All eight surrounding pixels are border: native channel only.
        assert_eq!(rgb1_pixel(&rgb, 3, 0, 0), [200, 0, 0]); // red
        assert_eq!(rgb1_pixel(&rgb, 3, 1, 0), [0, 40, 0]); // green
    }

    #[test]
    fn test_color_convert_processor_bayer() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::RGB1,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: 0,
        };
        let proc = ColorConvertProcessor::new(config);
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
            false_color: 1,
        };
        let proc = ColorConvertProcessor::new(config);
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

        // false_color=1 selects the Rainbow LUT (NDPluginColorConvert.cpp:62-66).
        // Pixel 0 (value=0) -> Rainbow[0]=(0,0,0); the old jet LUT gave B=127 here.
        if let NDDataBuffer::U8(ref v) = out.data {
            assert_eq!([v[0], v[1], v[2]], RAINBOW_COLOR_MAP[0]);
            assert_eq!([v[0], v[1], v[2]], [0, 0, 0]);
            // Last pixel (value=255) -> Rainbow[255]=(255,255,255).
            let last = 15 * 3;
            assert_eq!([v[last], v[last + 1], v[last + 2]], RAINBOW_COLOR_MAP[255]);
        } else {
            panic!("expected UInt8 output");
        }
    }

    #[test]
    fn test_false_color_iron_table() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::RGB1,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: 2,
        };
        let proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        // 2x1 mono image with values 0 and 192 to probe distinct Iron entries.
        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(1)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v[0] = 0;
            v[1] = 192;
        }

        let result = proc.process_array(&arr, &pool);
        let out = &result.output_arrays[0];
        if let NDDataBuffer::U8(ref v) = out.data {
            // false_color=2 selects Iron (NDPluginColorConvert.cpp:68-72).
            assert_eq!([v[0], v[1], v[2]], IRON_COLOR_MAP[0]);
            assert_eq!([v[3], v[4], v[5]], IRON_COLOR_MAP[192]);
            // Iron[192]=(192,160,64) differs from the Rainbow table at the same index.
            assert_eq!([v[3], v[4], v[5]], [192, 160, 64]);
            assert_ne!(IRON_COLOR_MAP[192], RAINBOW_COLOR_MAP[192]);
        } else {
            panic!("expected UInt8 output");
        }
    }

    #[test]
    fn test_r6_66_false_color_int8_uses_low_byte_index() {
        // R6-66 / NDPluginColorConvert.cpp:59-60 — C reads the FalseColor
        // parameter for NDInt8 as well as NDUInt8, and indexes the map with
        // `(unsigned char)*pIn` (:108), so a negative epicsInt8 selects entries
        // 128..=255. The LUT bytes are memcpy'd into epicsInt8 cells, so an
        // entry above 127 lands as a negative sample.
        let config = ColorConvertConfig {
            target_mode: NDColorMode::RGB1,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: 2,
        };
        let proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(1)],
            NDDataType::Int8,
        );
        if let NDDataBuffer::I8(ref mut v) = arr.data {
            v[0] = 0;
            v[1] = -64; // (unsigned char)(-64) == 192
        }

        let result = proc.process_array(&arr, &pool);
        let out = &result.output_arrays[0];
        let NDDataBuffer::I8(ref v) = out.data else {
            panic!("Int8 input must stay Int8 (C allocates with pArray->dataType)");
        };
        let e0 = IRON_COLOR_MAP[0];
        let e192 = IRON_COLOR_MAP[192]; // (192, 160, 64)
        assert_eq!(
            [v[0], v[1], v[2]],
            [e0[0] as i8, e0[1] as i8, e0[2] as i8],
            "Int8 mono must be colormapped, not replicated to grayscale"
        );
        assert_eq!(
            [v[3], v[4], v[5]],
            [e192[0] as i8, e192[1] as i8, e192[2] as i8]
        );
        assert_eq!(v[3], -64, "LUT byte 192 reinterpreted as epicsInt8");
    }

    #[test]
    fn test_r6_66_false_color_ignored_for_uint16() {
        // C only reads FalseColor for 8-bit arrays (NDPluginColorConvert.cpp:59);
        // for a UInt16 mono frame the local `falseColor` stays 0 (:45) and the
        // grayscale replication runs. Rust must match — no colormap here.
        let config = ColorConvertConfig {
            target_mode: NDColorMode::RGB1,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: 1,
        };
        let proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(1)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            v[0] = 1000;
            v[1] = 2000;
        }

        let result = proc.process_array(&arr, &pool);
        let out = &result.output_arrays[0];
        let NDDataBuffer::U16(ref v) = out.data else {
            panic!("expected UInt16 output");
        };
        assert_eq!(v[..6], [1000, 1000, 1000, 2000, 2000, 2000]);
    }

    #[test]
    fn test_rgb1_to_rgb2_conversion() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::RGB2,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: 0,
        };
        let proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        // Create RGB1 image: dims [3, 4, 4]
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(4),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = (i % 256) as u8;
            }
        }
        // The layout is declared, not guessed from the size-3 dimension
        // (NDPluginColorConvert.cpp:54-55).
        set_color_mode_attr(&mut arr, NDColorMode::RGB1);

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
            false_color: 0,
        };
        let proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        // Create RGB2 image: dims [4, 3, 4]
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(4),
                NDDimension::new(3),
                NDDimension::new(4),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = 128;
            }
        }
        // The layout is declared, not guessed from the size-3 dimension
        // (NDPluginColorConvert.cpp:54-55).
        set_color_mode_attr(&mut arr, NDColorMode::RGB2);

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        let out = &result.output_arrays[0];
        // Mono output should be 2D
        assert_eq!(out.dims.len(), 2);
    }

    /// A size-3 dimension is not a color mode. C reads the source mode from the
    /// `ColorMode` attribute alone (NDPluginColorConvert.cpp:54-55); with none
    /// present the mode is the initialiser, `NDColorModeMono` (:44), whatever the
    /// dimensions look like.
    #[test]
    fn r9_80_dims_never_imply_a_color_mode() {
        for dims in [
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(4),
            ],
            vec![
                NDDimension::new(4),
                NDDimension::new(3),
                NDDimension::new(4),
            ],
            vec![
                NDDimension::new(4),
                NDDimension::new(4),
                NDDimension::new(3),
            ],
            vec![NDDimension::new(4), NDDimension::new(4)],
        ] {
            let arr = NDArray::new(dims, NDDataType::UInt8);
            assert_eq!(
                arr.info().color_mode,
                NDColorMode::Mono,
                "no ColorMode attribute -> Mono, never an RGB layout guessed from a \
                 size-3 dimension"
            );
        }
    }

    #[test]
    fn test_same_mode_passthrough() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::Mono,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: 0,
        };
        let proc = ColorConvertProcessor::new(config);
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

    fn set_color_mode_attr(arr: &mut NDArray, mode: NDColorMode) {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            String::new(),
            NDAttrSource::Driver,
            NDAttrValue::Int32(mode as i32),
        ));
    }

    #[test]
    fn test_bayer_to_mono_via_rgb1() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::Mono,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: 0,
        };
        let proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        set_color_mode_attr(&mut arr, NDColorMode::Bayer);
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..16 {
                v[i] = 128;
            }
        }

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(result.output_arrays[0].dims.len(), 2);
    }

    #[test]
    fn test_rgb1_to_yuv444_conversion() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::YUV444,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: 0,
        };
        let proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(4),
            ],
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
        assert_eq!(out.dims[0].size, 3);
    }

    #[test]
    fn test_yuv422_to_rgb1_conversion() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::RGB1,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: 0,
        };
        let proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        // packed_x=8 means 4 pixels wide, 2 rows
        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        set_color_mode_attr(&mut arr, NDColorMode::YUV422);
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            // UYVY pattern: U Y0 V Y1
            let uyvy: [u8; 16] = [
                128, 100, 128, 150, 128, 200, 128, 50, 128, 128, 128, 128, 128, 64, 128, 192,
            ];
            v[..16].copy_from_slice(&uyvy);
        }

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        let out = &result.output_arrays[0];
        assert_eq!(out.dims[0].size, 3);
        assert_eq!(out.dims[1].size, 4);
        assert_eq!(out.dims[2].size, 2);
    }

    #[test]
    fn test_mono_to_yuv422_conversion() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::YUV422,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: 0,
        };
        let proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..8 {
                v[i] = (i * 30) as u8;
            }
        }

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        let out = &result.output_arrays[0];
        assert_eq!(out.dims.len(), 2);
        assert_eq!(out.dims[0].size, 8); // packed_x = 4*2
    }

    #[test]
    fn test_yuv444_to_mono_conversion() {
        let config = ColorConvertConfig {
            target_mode: NDColorMode::Mono,
            bayer_pattern: NDBayerPattern::RGGB,
            false_color: 0,
        };
        let proc = ColorConvertProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(4),
            ],
            NDDataType::UInt8,
        );
        set_color_mode_attr(&mut arr, NDColorMode::YUV444);
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = 128;
            }
        }

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        let out = &result.output_arrays[0];
        assert_eq!(out.dims.len(), 2);
        assert_eq!(out.dims[0].size, 4);
        assert_eq!(out.dims[1].size, 4);
    }

    #[test]
    fn test_color_mode_comes_from_the_attribute() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        assert_eq!(arr.info().color_mode, NDColorMode::Mono);

        set_color_mode_attr(&mut arr, NDColorMode::YUV422);
        assert_eq!(arr.info().color_mode, NDColorMode::YUV422);
    }

    #[test]
    fn test_false_color_table_endpoints() {
        // Both ADCore tables start at black and end at white (colorMaps.h).
        // The previous jet generator gave (0,0,127) at index 0 — the divergence
        // this fix closes.
        assert_eq!(RAINBOW_COLOR_MAP[0], [0, 0, 0]);
        assert_eq!(RAINBOW_COLOR_MAP[255], [255, 255, 255]);
        assert_eq!(IRON_COLOR_MAP[0], [0, 0, 0]);
        assert_eq!(IRON_COLOR_MAP[255], [255, 255, 255]);

        // Mid-table sample pins the exact ported bytes (RainbowColor at 128,
        // IronColor at 128 per colorMaps.h).
        assert_eq!(RAINBOW_COLOR_MAP[128], [4, 255, 251]);
        assert_eq!(IRON_COLOR_MAP[128], [192, 64, 64]);

        // falseColor selector matches NDPluginColorConvert.cpp:61-78.
        assert_eq!(false_color_lut(1), Some(&RAINBOW_COLOR_MAP));
        assert_eq!(false_color_lut(2), Some(&IRON_COLOR_MAP));
        assert_eq!(false_color_lut(0), None);
        assert_eq!(false_color_lut(3), None);
    }
}
