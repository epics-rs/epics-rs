// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — all 1 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p ad-plugins-rs
// --all-features`, 556/556). ad-plugins-rs became a census subject when
// its `build.rs` began deriving `tokio_backend`; nothing here builds a
// CA server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.
use std::sync::Arc;

// Not gated on `parallel`: `should_parallelize` is the whole decision now
// and this file asks it on both arms.
use crate::par_util;
#[cfg(feature = "parallel")]
use rayon::prelude::*;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamUpdate, PluginParamSnapshot, PluginRuntimeHandle, ProcessResult,
};
use ad_core_rs::plugin::wiring::WiringRegistry;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;
use parking_lot::Mutex;

/// Parameter indices for NDStats plugin-specific params.
#[derive(Clone, Copy, Default)]
pub struct NDStatsParams {
    pub compute_statistics: usize,
    pub bgd_width: usize,
    pub min_value: usize,
    pub max_value: usize,
    pub mean_value: usize,
    pub sigma_value: usize,
    pub total: usize,
    pub net: usize,
    pub min_x: usize,
    pub min_y: usize,
    pub max_x: usize,
    pub max_y: usize,
    pub compute_centroid: usize,
    pub centroid_threshold: usize,
    pub centroid_total: usize,
    pub centroid_x: usize,
    pub centroid_y: usize,
    pub sigma_x: usize,
    pub sigma_y: usize,
    pub sigma_xy: usize,
    pub skewness_x: usize,
    pub skewness_y: usize,
    pub kurtosis_x: usize,
    pub kurtosis_y: usize,
    pub eccentricity: usize,
    pub orientation: usize,
    pub compute_histogram: usize,
    pub hist_size: usize,
    pub hist_min: usize,
    pub hist_max: usize,
    pub hist_below: usize,
    pub hist_above: usize,
    pub hist_entropy: usize,
    pub compute_profiles: usize,
    pub cursor_x: usize,
    pub cursor_y: usize,
    pub cursor_val: usize,
    pub profile_size_x: usize,
    pub profile_size_y: usize,
    pub skewx_value: usize,
    pub skewy_value: usize,
    pub profile_average_x: usize,
    pub profile_average_y: usize,
    pub profile_threshold_x: usize,
    pub profile_threshold_y: usize,
    pub profile_centroid_x: usize,
    pub profile_centroid_y: usize,
    pub profile_cursor_x: usize,
    pub profile_cursor_y: usize,
    pub hist_array: usize,
    pub hist_x_array: usize,
}

/// Statistics computed from an NDArray.
#[derive(Debug, Clone, Default)]
pub struct StatsResult {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub sigma: f64,
    pub total: f64,
    pub net: f64,
    pub num_elements: usize,
    pub min_x: usize,
    pub min_y: usize,
    pub max_x: usize,
    pub max_y: usize,
    pub histogram: Vec<f64>,
    pub hist_below: f64,
    pub hist_above: f64,
    pub hist_entropy: f64,
    pub profile_avg_x: Vec<f64>,
    pub profile_avg_y: Vec<f64>,
    pub profile_threshold_x: Vec<f64>,
    pub profile_threshold_y: Vec<f64>,
    pub profile_centroid_x: Vec<f64>,
    pub profile_centroid_y: Vec<f64>,
    pub profile_cursor_x: Vec<f64>,
    pub profile_cursor_y: Vec<f64>,
    pub cursor_value: f64,
}

/// Centroid and higher-order moment results.
#[derive(Debug, Clone, Default)]
pub struct CentroidResult {
    pub centroid_x: f64,
    pub centroid_y: f64,
    pub sigma_x: f64,
    pub sigma_y: f64,
    pub sigma_xy: f64,
    pub centroid_total: f64,
    pub skewness_x: f64,
    pub skewness_y: f64,
    pub kurtosis_x: f64,
    pub kurtosis_y: f64,
    pub eccentricity: f64,
    pub orientation: f64,
}

/// Profile computation results.
#[derive(Debug, Clone, Default)]
pub struct ProfileResult {
    pub avg_x: Vec<f64>,
    pub avg_y: Vec<f64>,
    pub threshold_x: Vec<f64>,
    pub threshold_y: Vec<f64>,
    pub centroid_x: Vec<f64>,
    pub centroid_y: Vec<f64>,
    pub cursor_x: Vec<f64>,
    pub cursor_y: Vec<f64>,
}

/// Compute min/max/mean/sigma/total from an NDDataBuffer, with min/max positions
/// and optional background subtraction.
///
/// When `bgd_width > 0`, the background is computed as N-dimensional edge
/// strips (per dimension a low-edge and a high-edge strip, each spanning the
/// full extent of the other dimensions) exactly as C++ `NDPluginStats`
/// `doComputeStatistics` does — corner pixels are counted twice. The
/// per-pixel average background is subtracted: `net = total - bgd_avg *
/// num_elements`, where `bgd_avg = bgd_counts / bgd_pixels`. This works for
/// any dimensionality (1-D, 2-D, 3-D+). When `bgd_width == 0`, `net = total`.
pub fn compute_stats(
    data: &NDDataBuffer,
    dims: &[ad_core_rs::ndarray::NDDimension],
    bgd_width: usize,
) -> StatsResult {
    macro_rules! stats_for {
        ($vec:expr) => {{
            let v = $vec;
            if v.is_empty() {
                return StatsResult::default();
            }

            let (min, max, min_idx, max_idx, total, variance);

            #[cfg(feature = "parallel")]
            {
                if par_util::should_parallelize(v.len()) {
                    // Parallel: fold+reduce for min/max/total
                    let (pmin, pmax, pmin_idx, pmax_idx, ptotal) =
                        par_util::thread_pool().install(|| {
                            v.par_iter()
                                .enumerate()
                                .fold(
                                    || (f64::MAX, f64::MIN, 0usize, 0usize, 0.0f64),
                                    |(mn, mx, mn_i, mx_i, s), (i, &elem)| {
                                        let f = elem as f64;
                                        let (new_mn, new_mn_i) =
                                            if f < mn { (f, i) } else { (mn, mn_i) };
                                        let (new_mx, new_mx_i) =
                                            if f > mx { (f, i) } else { (mx, mx_i) };
                                        (new_mn, new_mx, new_mn_i, new_mx_i, s + f)
                                    },
                                )
                                .reduce(
                                    || (f64::MAX, f64::MIN, 0, 0, 0.0),
                                    |(mn1, mx1, mn_i1, mx_i1, s1), (mn2, mx2, mn_i2, mx_i2, s2)| {
                                        let (rmn, rmn_i) = if mn1 <= mn2 {
                                            (mn1, mn_i1)
                                        } else {
                                            (mn2, mn_i2)
                                        };
                                        let (rmx, rmx_i) = if mx1 >= mx2 {
                                            (mx1, mx_i1)
                                        } else {
                                            (mx2, mx_i2)
                                        };
                                        (rmn, rmx, rmn_i, rmx_i, s1 + s2)
                                    },
                                )
                        });
                    min = pmin;
                    max = pmax;
                    min_idx = pmin_idx;
                    max_idx = pmax_idx;
                    total = ptotal;
                    let mean_tmp = total / v.len() as f64;
                    variance = par_util::thread_pool().install(|| {
                        v.par_iter()
                            .map(|&elem| {
                                let d = elem as f64 - mean_tmp;
                                d * d
                            })
                            .sum::<f64>()
                    });
                } else {
                    let mut lmin = v[0] as f64;
                    let mut lmax = v[0] as f64;
                    let mut lmin_idx: usize = 0;
                    let mut lmax_idx: usize = 0;
                    let mut ltotal = 0.0f64;
                    for (i, &elem) in v.iter().enumerate() {
                        let f = elem as f64;
                        if f < lmin {
                            lmin = f;
                            lmin_idx = i;
                        }
                        if f > lmax {
                            lmax = f;
                            lmax_idx = i;
                        }
                        ltotal += f;
                    }
                    min = lmin;
                    max = lmax;
                    min_idx = lmin_idx;
                    max_idx = lmax_idx;
                    total = ltotal;
                    let mean_tmp = total / v.len() as f64;
                    let mut lvar = 0.0f64;
                    for &elem in v.iter() {
                        let d = elem as f64 - mean_tmp;
                        lvar += d * d;
                    }
                    variance = lvar;
                }
            }

            #[cfg(not(feature = "parallel"))]
            {
                let mut lmin = v[0] as f64;
                let mut lmax = v[0] as f64;
                let mut lmin_idx: usize = 0;
                let mut lmax_idx: usize = 0;
                let mut ltotal = 0.0f64;
                for (i, &elem) in v.iter().enumerate() {
                    let f = elem as f64;
                    if f < lmin {
                        lmin = f;
                        lmin_idx = i;
                    }
                    if f > lmax {
                        lmax = f;
                        lmax_idx = i;
                    }
                    ltotal += f;
                }
                min = lmin;
                max = lmax;
                min_idx = lmin_idx;
                max_idx = lmax_idx;
                total = ltotal;
                let mean_tmp = total / v.len() as f64;
                let mut lvar = 0.0f64;
                for &elem in v.iter() {
                    let d = elem as f64 - mean_tmp;
                    lvar += d * d;
                }
                variance = lvar;
            }

            let mean = total / v.len() as f64;
            let sigma = (variance / v.len() as f64).sqrt();
            let x_size = dims.first().map_or(v.len(), |d| d.size);

            // Background subtraction.
            //
            // C parity: NDPluginStats.cpp:488-530 `doComputeStatistics` background
            // section. The background is the union of, per dimension, a low-edge
            // strip and a high-edge strip (each spanning the full extent of every
            // other dimension). Strip totals/pixel-counts are SUMMED, so pixels in
            // the corner of multiple strips are counted twice in both `bgdCounts`
            // and `bgdPixels` — the C++ source documents this as intentional
            // (NDPluginStats.cpp:484-487). Works for any dimensionality (1-D,
            // 2-D, 3-D+).
            let net = if bgd_width > 0 && !dims.is_empty() {
                let sizes: Vec<usize> = dims.iter().map(|d| d.size).collect();
                // Row-major strides: dim 0 varies fastest (matches the x_size /
                // y_size index math used above).
                let ndims = sizes.len();
                let mut strides = vec![1usize; ndims];
                for i in 1..ndims {
                    strides[i] = strides[i - 1] * sizes[i - 1];
                }

                // Sum a strip: dimension `sd` restricted to [s_off, s_off+s_len),
                // every other dimension spanning its full extent. Returns
                // (sum, pixel_count).
                let strip = |sd: usize, s_off: usize, s_len: usize| -> (f64, usize) {
                    if s_len == 0 {
                        return (0.0, 0);
                    }
                    // Number of pixels in the strip = s_len * product of other dims.
                    let mut count = s_len;
                    for (d, &sz) in sizes.iter().enumerate() {
                        if d != sd {
                            count *= sz;
                        }
                    }
                    let mut sum = 0.0f64;
                    // Iterate over every flat coordinate in the strip by counting
                    // through per-dimension coordinates.
                    let mut coords = vec![0usize; ndims];
                    for _ in 0..count {
                        let mut flat = 0usize;
                        for d in 0..ndims {
                            let c = if d == sd {
                                coords[d] + s_off
                            } else {
                                coords[d]
                            };
                            flat += c * strides[d];
                        }
                        if flat < v.len() {
                            sum += v[flat] as f64;
                        }
                        // Increment the mixed-radix coordinate counter. The radix
                        // for the strip dimension is `s_len`; for others it is the
                        // full dimension size.
                        for d in 0..ndims {
                            let radix = if d == sd { s_len } else { sizes[d] };
                            coords[d] += 1;
                            if coords[d] < radix {
                                break;
                            }
                            coords[d] = 0;
                        }
                    }
                    (sum, count)
                };

                let mut bgd_counts = 0.0f64;
                let mut bgd_pixels = 0usize;
                for (d, &dim_size) in sizes.iter().enumerate() {
                    // Low-edge strip: offset 0, size min(bgd_width, dim_size).
                    let low_len = bgd_width.min(dim_size);
                    let (low_sum, low_n) = strip(d, 0, low_len);
                    bgd_counts += low_sum;
                    bgd_pixels += low_n;
                    // High-edge strip: offset max(0, dim_size - bgd_width),
                    // size min(bgd_width, dim_size - offset).
                    let high_off = dim_size.saturating_sub(bgd_width);
                    let high_len = bgd_width.min(dim_size - high_off);
                    let (high_sum, high_n) = strip(d, high_off, high_len);
                    bgd_counts += high_sum;
                    bgd_pixels += high_n;
                }
                // C parity: NDPluginStats.cpp:527 — `if (bgdPixels < 1) bgdPixels = 1`.
                let bgd_avg = bgd_counts / bgd_pixels.max(1) as f64;
                total - bgd_avg * v.len() as f64
            } else {
                total
            };

            StatsResult {
                min,
                max,
                mean,
                sigma,
                total,
                net,
                num_elements: v.len(),
                min_x: if x_size > 0 { min_idx % x_size } else { 0 },
                min_y: if x_size > 0 { min_idx / x_size } else { 0 },
                max_x: if x_size > 0 { max_idx % x_size } else { 0 },
                max_y: if x_size > 0 { max_idx / x_size } else { 0 },
                ..StatsResult::default()
            }
        }};
    }

    match data {
        NDDataBuffer::I8(v) => stats_for!(v),
        NDDataBuffer::U8(v) => stats_for!(v),
        NDDataBuffer::I16(v) => stats_for!(v),
        NDDataBuffer::U16(v) => stats_for!(v),
        NDDataBuffer::I32(v) => stats_for!(v),
        NDDataBuffer::U32(v) => stats_for!(v),
        NDDataBuffer::I64(v) => stats_for!(v),
        NDDataBuffer::U64(v) => stats_for!(v),
        NDDataBuffer::F32(v) => stats_for!(v),
        NDDataBuffer::F64(v) => stats_for!(v),
    }
}

/// Compute centroid, sigma, and higher-order moments for a 2D array.
///
/// Pixels with value < `threshold` are excluded from all moment accumulation.
pub fn compute_centroid(
    data: &NDDataBuffer,
    x_size: usize,
    y_size: usize,
    threshold: f64,
) -> CentroidResult {
    let n = x_size * y_size;
    if n == 0 || data.len() < n {
        return CentroidResult::default();
    }

    // Collect values into a flat f64 vec for potential parallel access
    let vals: Vec<f64> = (0..n).map(|i| data.get_as_f64(i).unwrap_or(0.0)).collect();

    // Pass 1: compute M00 (total), M10, M01 for centroid
    let (m00, m10, m01);

    #[cfg(feature = "parallel")]
    {
        if par_util::should_parallelize(n) {
            let xs = x_size;
            let thr = threshold;
            let (pm00, pm10, pm01) = par_util::thread_pool().install(|| {
                vals.par_iter()
                    .enumerate()
                    .fold(
                        || (0.0f64, 0.0f64, 0.0f64),
                        |(s00, s10, s01), (i, &val)| {
                            if val < thr {
                                return (s00, s10, s01);
                            }
                            let ix = i % xs;
                            let iy = i / xs;
                            (s00 + val, s10 + val * ix as f64, s01 + val * iy as f64)
                        },
                    )
                    .reduce(
                        || (0.0, 0.0, 0.0),
                        |(a0, a1, a2), (b0, b1, b2)| (a0 + b0, a1 + b1, a2 + b2),
                    )
            });
            m00 = pm00;
            m10 = pm10;
            m01 = pm01;
        } else {
            let mut lm00 = 0.0f64;
            let mut lm10 = 0.0f64;
            let mut lm01 = 0.0f64;
            for iy in 0..y_size {
                for ix in 0..x_size {
                    let val = vals[iy * x_size + ix];
                    if val < threshold {
                        continue;
                    }
                    lm00 += val;
                    lm10 += val * ix as f64;
                    lm01 += val * iy as f64;
                }
            }
            m00 = lm00;
            m10 = lm10;
            m01 = lm01;
        }
    }

    #[cfg(not(feature = "parallel"))]
    {
        let mut lm00 = 0.0f64;
        let mut lm10 = 0.0f64;
        let mut lm01 = 0.0f64;
        for iy in 0..y_size {
            for ix in 0..x_size {
                let val = vals[iy * x_size + ix];
                if val < threshold {
                    continue;
                }
                lm00 += val;
                lm10 += val * ix as f64;
                lm01 += val * iy as f64;
            }
        }
        m00 = lm00;
        m10 = lm10;
        m01 = lm01;
    }

    if m00 == 0.0 {
        return CentroidResult::default();
    }

    let cx = m10 / m00;
    let cy = m01 / m00;

    // Pass 2: compute central moments up to 4th order
    let (mu20, mu02, mu11, m30_central, m03_central, m40_central, m04_central);

    #[cfg(feature = "parallel")]
    {
        if par_util::should_parallelize(n) {
            let xs = x_size;
            let thr = threshold;
            let (p20, p02, p11, p30, p03, p40, p04) = par_util::thread_pool().install(|| {
                vals.par_iter()
                    .enumerate()
                    .fold(
                        || (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64),
                        |(s20, s02, s11, s30, s03, s40, s04), (i, &val)| {
                            if val < thr {
                                return (s20, s02, s11, s30, s03, s40, s04);
                            }
                            let ix = i % xs;
                            let iy = i / xs;
                            let dx = ix as f64 - cx;
                            let dy = iy as f64 - cy;
                            let dx2 = dx * dx;
                            let dy2 = dy * dy;
                            (
                                s20 + val * dx2,
                                s02 + val * dy2,
                                s11 + val * dx * dy,
                                s30 + val * dx2 * dx,
                                s03 + val * dy2 * dy,
                                s40 + val * dx2 * dx2,
                                s04 + val * dy2 * dy2,
                            )
                        },
                    )
                    .reduce(
                        || (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
                        |(a0, a1, a2, a3, a4, a5, a6), (b0, b1, b2, b3, b4, b5, b6)| {
                            (
                                a0 + b0,
                                a1 + b1,
                                a2 + b2,
                                a3 + b3,
                                a4 + b4,
                                a5 + b5,
                                a6 + b6,
                            )
                        },
                    )
            });
            mu20 = p20;
            mu02 = p02;
            mu11 = p11;
            m30_central = p30;
            m03_central = p03;
            m40_central = p40;
            m04_central = p04;
        } else {
            let mut l20 = 0.0f64;
            let mut l02 = 0.0f64;
            let mut l11 = 0.0f64;
            let mut l30 = 0.0f64;
            let mut l03 = 0.0f64;
            let mut l40 = 0.0f64;
            let mut l04 = 0.0f64;
            for iy in 0..y_size {
                for ix in 0..x_size {
                    let val = vals[iy * x_size + ix];
                    if val < threshold {
                        continue;
                    }
                    let dx = ix as f64 - cx;
                    let dy = iy as f64 - cy;
                    let dx2 = dx * dx;
                    let dy2 = dy * dy;
                    l20 += val * dx2;
                    l02 += val * dy2;
                    l11 += val * dx * dy;
                    l30 += val * dx2 * dx;
                    l03 += val * dy2 * dy;
                    l40 += val * dx2 * dx2;
                    l04 += val * dy2 * dy2;
                }
            }
            mu20 = l20;
            mu02 = l02;
            mu11 = l11;
            m30_central = l30;
            m03_central = l03;
            m40_central = l40;
            m04_central = l04;
        }
    }

    #[cfg(not(feature = "parallel"))]
    {
        let mut l20 = 0.0f64;
        let mut l02 = 0.0f64;
        let mut l11 = 0.0f64;
        let mut l30 = 0.0f64;
        let mut l03 = 0.0f64;
        let mut l40 = 0.0f64;
        let mut l04 = 0.0f64;
        for iy in 0..y_size {
            for ix in 0..x_size {
                let val = vals[iy * x_size + ix];
                if val < threshold {
                    continue;
                }
                let dx = ix as f64 - cx;
                let dy = iy as f64 - cy;
                let dx2 = dx * dx;
                let dy2 = dy * dy;
                l20 += val * dx2;
                l02 += val * dy2;
                l11 += val * dx * dy;
                l30 += val * dx2 * dx;
                l03 += val * dy2 * dy;
                l40 += val * dx2 * dx2;
                l04 += val * dy2 * dy2;
            }
        }
        mu20 = l20;
        mu02 = l02;
        mu11 = l11;
        m30_central = l30;
        m03_central = l03;
        m40_central = l40;
        m04_central = l04;
    }

    let sigma_x = (mu20 / m00).sqrt();
    let sigma_y = (mu02 / m00).sqrt();
    let sigma_xy = if sigma_x > 0.0 && sigma_y > 0.0 {
        (mu11 / m00) / (sigma_x * sigma_y)
    } else {
        0.0
    };

    // Skewness: M30_central / (M00 * sigma_x^3)
    let skewness_x = if sigma_x > 0.0 {
        m30_central / (m00 * sigma_x.powi(3))
    } else {
        0.0
    };
    let skewness_y = if sigma_y > 0.0 {
        m03_central / (m00 * sigma_y.powi(3))
    } else {
        0.0
    };

    // Excess kurtosis: M40_central / (M00 * sigma_x^4) - 3
    let kurtosis_x = if sigma_x > 0.0 {
        m40_central / (m00 * sigma_x.powi(4)) - 3.0
    } else {
        0.0
    };
    let kurtosis_y = if sigma_y > 0.0 {
        m04_central / (m00 * sigma_y.powi(4)) - 3.0
    } else {
        0.0
    };

    // Eccentricity: ((mu20 - mu02)^2 - 4*mu11^2) / (mu20 + mu02)^2
    // Uses un-normalized central moments (normalization cancels in the ratio)
    let denom = mu20 + mu02;
    let eccentricity = if denom > 0.0 {
        ((mu20 - mu02).powi(2) - 4.0 * mu11.powi(2)) / denom.powi(2)
    } else {
        0.0
    };

    // Orientation: 0.5 * atan2(2*mu11, mu20 - mu02) in degrees
    let orientation = 0.5 * (2.0 * mu11).atan2(mu20 - mu02) * 180.0 / std::f64::consts::PI;

    CentroidResult {
        centroid_x: cx,
        centroid_y: cy,
        sigma_x,
        sigma_y,
        sigma_xy,
        centroid_total: m00,
        skewness_x,
        skewness_y,
        kurtosis_x,
        kurtosis_y,
        eccentricity,
        orientation,
    }
}

/// Compute histogram of pixel values.
///
/// Returns (histogram, below_count, above_count, entropy).
/// - `hist_size`: number of bins
/// - `hist_min` / `hist_max`: value range for binning
/// - bin index = `((val - hist_min) * (hist_size - 1) / (hist_max - hist_min) + 0.5) as usize`
/// - Values below `hist_min` increment `below_count`; above `hist_max` increment `above_count`
/// - Entropy = `-sum(p * ln(p))` for non-zero bins where `p = count / total_count`
pub fn compute_histogram(
    data: &NDDataBuffer,
    hist_size: usize,
    hist_min: f64,
    hist_max: f64,
) -> (Vec<f64>, f64, f64, f64) {
    if hist_size == 0 || hist_max <= hist_min {
        return (vec![], 0.0, 0.0, 0.0);
    }

    let mut histogram = vec![0.0f64; hist_size];
    let mut below = 0.0f64;
    let mut above = 0.0f64;
    let range = hist_max - hist_min;
    let n = data.len();

    let use_parallel = par_util::should_parallelize(n);

    if use_parallel {
        #[cfg(feature = "parallel")]
        {
            let vals: Vec<f64> = (0..n).map(|i| data.get_as_f64(i).unwrap_or(0.0)).collect();
            let chunk_size = (n / rayon::current_num_threads().max(1)).max(1024);
            let hs = hist_size;
            let hmin = hist_min;
            let hmax = hist_max;
            let rng = range;
            let chunk_results: Vec<(Vec<f64>, f64, f64)> = par_util::thread_pool().install(|| {
                vals.par_chunks(chunk_size)
                    .map(|chunk| {
                        let mut local_hist = vec![0.0f64; hs];
                        let mut local_below = 0.0f64;
                        let mut local_above = 0.0f64;
                        for &val in chunk {
                            if val < hmin {
                                local_below += 1.0;
                            } else if val > hmax {
                                local_above += 1.0;
                            } else {
                                let bin = ((val - hmin) * (hs - 1) as f64 / rng + 0.5) as usize;
                                let bin = bin.min(hs - 1);
                                local_hist[bin] += 1.0;
                            }
                        }
                        (local_hist, local_below, local_above)
                    })
                    .collect()
            });
            for (local_hist, local_below, local_above) in chunk_results {
                below += local_below;
                above += local_above;
                for (i, &count) in local_hist.iter().enumerate() {
                    histogram[i] += count;
                }
            }
        }
    } else {
        for i in 0..n {
            let val = data.get_as_f64(i).unwrap_or(0.0);
            if val < hist_min {
                below += 1.0;
            } else if val > hist_max {
                above += 1.0;
            } else {
                let bin = ((val - hist_min) * (hist_size - 1) as f64 / range + 0.5) as usize;
                let bin = bin.min(hist_size - 1);
                histogram[bin] += 1.0;
            }
        }
    }

    // Compute entropy matching C++: -sum(count * ln(count)) / nElements
    // Zero-count bins are treated as count=1 (so ln(1)=0, effectively skipped)
    let n_elements = data.len() as f64;
    let entropy = if n_elements > 0.0 {
        let mut ent = 0.0f64;
        for &count in &histogram {
            let c = if count <= 0.0 { 1.0 } else { count };
            ent += c * c.ln();
        }
        -ent / n_elements
    } else {
        0.0
    };

    (histogram, below, above, entropy)
}

/// Compute profile projections for a 2D image.
///
/// - Average X/Y: column/row averages over the full image
/// - Threshold X/Y: column/row averages using only pixels >= threshold
/// - Centroid X/Y: single row/column at the centroid position (rounded)
/// - Cursor X/Y: single row/column at cursor position
pub fn compute_profiles(
    data: &NDDataBuffer,
    x_size: usize,
    y_size: usize,
    threshold: f64,
    centroid_x: f64,
    centroid_y: f64,
    cursor_x: usize,
    cursor_y: usize,
) -> ProfileResult {
    if x_size == 0 || y_size == 0 || data.len() < x_size * y_size {
        return ProfileResult::default();
    }

    let mut avg_x = vec![0.0f64; x_size];
    let mut avg_y = vec![0.0f64; y_size];
    let mut thresh_x_sum = vec![0.0f64; x_size];
    let mut thresh_x_cnt = vec![0usize; x_size];
    let mut thresh_y_sum = vec![0.0f64; y_size];
    let mut thresh_y_cnt = vec![0usize; y_size];

    // Accumulate sums for average and threshold profiles
    for iy in 0..y_size {
        for ix in 0..x_size {
            let val = data.get_as_f64(iy * x_size + ix).unwrap_or(0.0);
            avg_x[ix] += val;
            avg_y[iy] += val;
            if val >= threshold {
                thresh_x_sum[ix] += val;
                thresh_x_cnt[ix] += 1;
                thresh_y_sum[iy] += val;
                thresh_y_cnt[iy] += 1;
            }
        }
    }

    // Average profiles: divide column sums by y_size, row sums by x_size
    for ix in 0..x_size {
        avg_x[ix] /= y_size as f64;
    }
    for iy in 0..y_size {
        avg_y[iy] /= x_size as f64;
    }

    // Threshold profiles: divide by count of pixels above threshold
    let threshold_x: Vec<f64> = thresh_x_sum
        .iter()
        .zip(thresh_x_cnt.iter())
        .map(|(&s, &c)| if c > 0 { s / c as f64 } else { 0.0 })
        .collect();
    let threshold_y: Vec<f64> = thresh_y_sum
        .iter()
        .zip(thresh_y_cnt.iter())
        .map(|(&s, &c)| if c > 0 { s / c as f64 } else { 0.0 })
        .collect();

    // Centroid/cursor profiles: extract a single row/column at the requested
    // position. C clamps the index to the valid range (NDPluginStats.cpp:341-360,
    // `MAX(.,0)` then `MIN(.,size-1)`): an out-of-range centroid or user cursor
    // collapses to the edge row/column, never a zero-filled profile. (x_size and
    // y_size are both > 0 here — the function early-returns on a zero dimension.)
    let cy_row = ((centroid_y + 0.5).max(0.0) as usize).min(y_size - 1);
    let cx_col = ((centroid_x + 0.5).max(0.0) as usize).min(x_size - 1);
    let cur_y = cursor_y.min(y_size - 1);
    let cur_x = cursor_x.min(x_size - 1);

    let centroid_x_profile: Vec<f64> = (0..x_size)
        .map(|ix| data.get_as_f64(cy_row * x_size + ix).unwrap_or(0.0))
        .collect();
    let centroid_y_profile: Vec<f64> = (0..y_size)
        .map(|iy| data.get_as_f64(iy * x_size + cx_col).unwrap_or(0.0))
        .collect();
    let cursor_x_profile: Vec<f64> = (0..x_size)
        .map(|ix| data.get_as_f64(cur_y * x_size + ix).unwrap_or(0.0))
        .collect();
    let cursor_y_profile: Vec<f64> = (0..y_size)
        .map(|iy| data.get_as_f64(iy * x_size + cur_x).unwrap_or(0.0))
        .collect();

    ProfileResult {
        avg_x,
        avg_y,
        threshold_x,
        threshold_y,
        centroid_x: centroid_x_profile,
        centroid_y: centroid_y_profile,
        cursor_x: cursor_x_profile,
        cursor_y: cursor_y_profile,
    }
}

/// Pure processing logic for statistics computation.
/// The compute-enable flags and their tuning values, all written by
/// `on_param_change` and read as one set per frame.
#[derive(Debug, Clone, Copy)]
struct StatsConfig {
    do_compute_statistics: bool,
    do_compute_centroid: bool,
    do_compute_histogram: bool,
    do_compute_profiles: bool,
    bgd_width: usize,
    centroid_threshold: f64,
    cursor_x: usize,
    cursor_y: usize,
    hist_size: usize,
    hist_min: f64,
    hist_max: f64,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            do_compute_statistics: true,
            do_compute_centroid: true,
            do_compute_histogram: false,
            do_compute_profiles: false,
            bgd_width: 0,
            centroid_threshold: 0.0,
            cursor_x: 0,
            cursor_y: 0,
            hist_size: 256,
            hist_min: 0.0,
            hist_max: 255.0,
        }
    }
}

pub struct StatsProcessor {
    latest_stats: Arc<Mutex<StatsResult>>,
    config: Mutex<StatsConfig>,
    params: NDStatsParams,
    /// Shared cell to export params after register_params is called.
    params_out: Arc<Mutex<NDStatsParams>>,
    /// Optional sender to push time series data to the TS port driver.
    ts_sender: Option<crate::time_series::TimeSeriesSender>,
}

impl StatsProcessor {
    pub fn new() -> Self {
        Self {
            latest_stats: Arc::new(Mutex::new(StatsResult::default())),
            config: Mutex::new(StatsConfig::default()),
            params: NDStatsParams::default(),
            params_out: Arc::new(Mutex::new(NDStatsParams::default())),
            ts_sender: None,
        }
    }

    /// Get a cloneable handle to the latest stats.
    pub fn stats_handle(&self) -> Arc<Mutex<StatsResult>> {
        self.latest_stats.clone()
    }

    /// Get a shared handle to the params (populated after register_params is called).
    pub fn params_handle(&self) -> Arc<Mutex<NDStatsParams>> {
        self.params_out.clone()
    }

    /// Set the time series sender for pushing data to the TS port driver.
    pub fn set_ts_sender(&mut self, sender: crate::time_series::TimeSeriesSender) {
        self.ts_sender = Some(sender);
    }
}

impl Default for StatsProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for StatsProcessor {
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let p = &self.params;
        let info = array.info();
        let cfg = *self.config.lock();

        let mut result = if cfg.do_compute_statistics {
            compute_stats(&array.data, &array.dims, cfg.bgd_width)
        } else {
            StatsResult::default()
        };

        // Centroid computation
        let mut centroid = CentroidResult::default();
        if cfg.do_compute_centroid {
            // C rejects ndims>2 (NDPluginStats.cpp:205 `if (ndims>2) return
            // asynError`): centroid is computed only for a true 2-D image, never
            // by treating the first two dims of a 4-D (or [x,y,1]) array as a
            // slice. The `>= 2` lower bound is unchanged (1-D is handled
            // elsewhere); adding `== 2` only removes the ndims>2 path.
            if info.color_size <= 1 && array.dims.len() == 2 {
                centroid = compute_centroid(
                    &array.data,
                    info.x_size,
                    info.y_size,
                    cfg.centroid_threshold,
                );
            }
        }

        // Histogram computation
        if cfg.do_compute_histogram {
            let (histogram, below, above, entropy) =
                compute_histogram(&array.data, cfg.hist_size, cfg.hist_min, cfg.hist_max);
            result.histogram = histogram;
            result.hist_below = below;
            result.hist_above = above;
            result.hist_entropy = entropy;
        }

        // Profile computation. C also rejects ndims>2 here (NDPluginStats.cpp:338),
        // so profiles are computed only for a true 2-D image.
        if cfg.do_compute_profiles && info.color_size <= 1 && array.dims.len() == 2 {
            let profiles = compute_profiles(
                &array.data,
                info.x_size,
                info.y_size,
                cfg.centroid_threshold,
                centroid.centroid_x,
                centroid.centroid_y,
                cfg.cursor_x,
                cfg.cursor_y,
            );
            result.profile_avg_x = profiles.avg_x;
            result.profile_avg_y = profiles.avg_y;
            result.profile_threshold_x = profiles.threshold_x;
            result.profile_threshold_y = profiles.threshold_y;
            result.profile_centroid_x = profiles.centroid_x;
            result.profile_centroid_y = profiles.centroid_y;
            result.profile_cursor_x = profiles.cursor_x;
            result.profile_cursor_y = profiles.cursor_y;
        }

        // Compute cursor value: pixel at (cursor_x, cursor_y). C clamps the
        // cursor to the last valid pixel (NDPluginStats.cpp:357-362) and always
        // reads it — an out-of-range cursor yields the edge pixel, never 0.
        if info.color_size <= 1 && array.dims.len() == 2 && info.x_size > 0 && info.y_size > 0 {
            let cx = cfg.cursor_x.min(info.x_size - 1);
            let cy = cfg.cursor_y.min(info.y_size - 1);
            result.cursor_value = array.data.get_as_f64(cy * info.x_size + cx).unwrap_or(0.0);
        }

        let mut updates = vec![
            ParamUpdate::float64(p.min_value, result.min),
            ParamUpdate::float64(p.max_value, result.max),
            ParamUpdate::float64(p.mean_value, result.mean),
            ParamUpdate::float64(p.sigma_value, result.sigma),
            ParamUpdate::float64(p.total, result.total),
            ParamUpdate::float64(p.net, result.net),
            ParamUpdate::float64(p.min_x, result.min_x as f64),
            ParamUpdate::float64(p.min_y, result.min_y as f64),
            ParamUpdate::float64(p.max_x, result.max_x as f64),
            ParamUpdate::float64(p.max_y, result.max_y as f64),
            ParamUpdate::float64(p.centroid_x, centroid.centroid_x),
            ParamUpdate::float64(p.centroid_y, centroid.centroid_y),
            ParamUpdate::float64(p.sigma_x, centroid.sigma_x),
            ParamUpdate::float64(p.sigma_y, centroid.sigma_y),
            ParamUpdate::float64(p.sigma_xy, centroid.sigma_xy),
            ParamUpdate::float64(p.centroid_total, centroid.centroid_total),
            ParamUpdate::float64(p.skewness_x, centroid.skewness_x),
            ParamUpdate::float64(p.skewness_y, centroid.skewness_y),
            ParamUpdate::float64(p.kurtosis_x, centroid.kurtosis_x),
            ParamUpdate::float64(p.kurtosis_y, centroid.kurtosis_y),
            ParamUpdate::float64(p.eccentricity, centroid.eccentricity),
            ParamUpdate::float64(p.orientation, centroid.orientation),
            ParamUpdate::int32(p.hist_below, result.hist_below as i32),
            ParamUpdate::int32(p.hist_above, result.hist_above as i32),
            ParamUpdate::float64(p.hist_entropy, result.hist_entropy),
            ParamUpdate::float64(p.cursor_val, result.cursor_value),
            ParamUpdate::int32(p.profile_size_x, info.x_size as i32),
            ParamUpdate::int32(p.profile_size_y, info.y_size as i32),
        ];

        // Histogram waveforms: the counts (HIST_ARRAY) and the bin X axis
        // (HIST_X_ARRAY). C++ NDPluginStats::computeHistX fills the X axis
        // with bin left edges: `scale = (histMax - histMin) / histSize` and
        // `histX[i] = histMin + i*scale` for i in 0..histSize. The divisor is
        // the bin count (histSize), not histSize-1, so the last bin's X is
        // histMin + (histSize-1)*scale, strictly below histMax.
        if cfg.do_compute_histogram && !result.histogram.is_empty() {
            updates.push(ParamUpdate::float64_array(
                p.hist_array,
                result.histogram.clone(),
            ));
            let n = result.histogram.len();
            let step = (cfg.hist_max - cfg.hist_min) / n as f64;
            let hist_x: Vec<f64> = (0..n).map(|i| cfg.hist_min + i as f64 * step).collect();
            updates.push(ParamUpdate::float64_array(p.hist_x_array, hist_x));
        }

        // Profile waveforms: emit the computed X/Y projections to asyn
        // clients (C++ doCallbacksFloat64Array for each PROFILE_* waveform).
        if cfg.do_compute_profiles && !result.profile_avg_x.is_empty() {
            updates.push(ParamUpdate::float64_array(
                p.profile_average_x,
                result.profile_avg_x.clone(),
            ));
            updates.push(ParamUpdate::float64_array(
                p.profile_average_y,
                result.profile_avg_y.clone(),
            ));
            updates.push(ParamUpdate::float64_array(
                p.profile_threshold_x,
                result.profile_threshold_x.clone(),
            ));
            updates.push(ParamUpdate::float64_array(
                p.profile_threshold_y,
                result.profile_threshold_y.clone(),
            ));
            updates.push(ParamUpdate::float64_array(
                p.profile_centroid_x,
                result.profile_centroid_x.clone(),
            ));
            updates.push(ParamUpdate::float64_array(
                p.profile_centroid_y,
                result.profile_centroid_y.clone(),
            ));
            updates.push(ParamUpdate::float64_array(
                p.profile_cursor_x,
                result.profile_cursor_x.clone(),
            ));
            updates.push(ParamUpdate::float64_array(
                p.profile_cursor_y,
                result.profile_cursor_y.clone(),
            ));
        }

        // Send time series data to TS port driver (if configured)
        if let Some(ref sender) = self.ts_sender {
            let ts_data = crate::time_series::TimeSeriesData {
                values: vec![
                    result.min,
                    result.min_x as f64,
                    result.min_y as f64,
                    result.max,
                    result.max_x as f64,
                    result.max_y as f64,
                    result.mean,
                    result.sigma,
                    result.total,
                    result.net,
                    centroid.centroid_total,
                    centroid.centroid_x,
                    centroid.centroid_y,
                    centroid.sigma_x,
                    centroid.sigma_y,
                    centroid.sigma_xy,
                    centroid.skewness_x,
                    centroid.skewness_y,
                    centroid.kurtosis_x,
                    centroid.kurtosis_y,
                    centroid.eccentricity,
                    centroid.orientation,
                    // C `timeSeries[TSTimestamp] = pArray->timeStamp`
                    // (NDPluginStats.cpp:577) — the standalone double.
                    array.time_stamp,
                ],
            };
            let _ = sender.try_send(ts_data);
        }

        *self.latest_stats.lock() = result;
        // C++ Stats forwards the input array to downstream plugins
        ProcessResult {
            output_arrays: vec![Arc::new(array.clone())],
            param_updates: updates,
            scatter: false,
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginStats"
    }

    fn register_params(
        &mut self,
        base: &mut PortDriverBase,
    ) -> Result<(), asyn_rs::error::AsynError> {
        self.params.compute_statistics =
            base.create_param("COMPUTE_STATISTICS", ParamType::Int32)?;
        base.set_int32_param(self.params.compute_statistics, 0, 1)?;

        self.params.bgd_width = base.create_param("BGD_WIDTH", ParamType::Int32)?;
        self.params.min_value = base.create_param("MIN_VALUE", ParamType::Float64)?;
        self.params.max_value = base.create_param("MAX_VALUE", ParamType::Float64)?;
        self.params.mean_value = base.create_param("MEAN_VALUE", ParamType::Float64)?;
        self.params.sigma_value = base.create_param("SIGMA_VALUE", ParamType::Float64)?;
        self.params.total = base.create_param("TOTAL", ParamType::Float64)?;
        self.params.net = base.create_param("NET", ParamType::Float64)?;
        self.params.min_x = base.create_param("MIN_X", ParamType::Float64)?;
        self.params.min_y = base.create_param("MIN_Y", ParamType::Float64)?;
        self.params.max_x = base.create_param("MAX_X", ParamType::Float64)?;
        self.params.max_y = base.create_param("MAX_Y", ParamType::Float64)?;

        self.params.compute_centroid = base.create_param("COMPUTE_CENTROID", ParamType::Int32)?;
        base.set_int32_param(self.params.compute_centroid, 0, 1)?;

        self.params.centroid_threshold =
            base.create_param("CENTROID_THRESHOLD", ParamType::Float64)?;
        self.params.centroid_total = base.create_param("CENTROID_TOTAL", ParamType::Float64)?;
        self.params.centroid_x = base.create_param("CENTROIDX_VALUE", ParamType::Float64)?;
        self.params.centroid_y = base.create_param("CENTROIDY_VALUE", ParamType::Float64)?;
        self.params.sigma_x = base.create_param("SIGMAX_VALUE", ParamType::Float64)?;
        self.params.sigma_y = base.create_param("SIGMAY_VALUE", ParamType::Float64)?;
        self.params.sigma_xy = base.create_param("SIGMAXY_VALUE", ParamType::Float64)?;
        self.params.skewness_x = base.create_param("SKEWNESSX_VALUE", ParamType::Float64)?;
        self.params.skewness_y = base.create_param("SKEWNESSY_VALUE", ParamType::Float64)?;
        self.params.kurtosis_x = base.create_param("KURTOSISX_VALUE", ParamType::Float64)?;
        self.params.kurtosis_y = base.create_param("KURTOSISY_VALUE", ParamType::Float64)?;
        self.params.eccentricity = base.create_param("ECCENTRICITY_VALUE", ParamType::Float64)?;
        self.params.orientation = base.create_param("ORIENTATION_VALUE", ParamType::Float64)?;

        self.params.compute_histogram = base.create_param("COMPUTE_HISTOGRAM", ParamType::Int32)?;
        self.params.hist_size = base.create_param("HIST_SIZE", ParamType::Int32)?;
        base.set_int32_param(self.params.hist_size, 0, 256)?;
        self.params.hist_min = base.create_param("HIST_MIN", ParamType::Float64)?;
        self.params.hist_max = base.create_param("HIST_MAX", ParamType::Float64)?;
        base.set_float64_param(self.params.hist_max, 0, 255.0)?;
        // HIST_BELOW/HIST_ABOVE are integer pixel counts: C registers them as
        // asynInt32 and pushes via setIntegerParam (NDPluginStats.cpp:827-828,
        // 627-628; epicsInt32 fields NDPluginStats.h:86-87). A client reading
        // these RBVs must see DBR_LONG, not DBR_DOUBLE.
        self.params.hist_below = base.create_param("HIST_BELOW", ParamType::Int32)?;
        self.params.hist_above = base.create_param("HIST_ABOVE", ParamType::Int32)?;
        self.params.hist_entropy = base.create_param("HIST_ENTROPY", ParamType::Float64)?;

        self.params.compute_profiles = base.create_param("COMPUTE_PROFILES", ParamType::Int32)?;
        self.params.cursor_x = base.create_param("CURSOR_X", ParamType::Int32)?;
        base.set_int32_param(self.params.cursor_x, 0, 0)?;
        self.params.cursor_y = base.create_param("CURSOR_Y", ParamType::Int32)?;
        base.set_int32_param(self.params.cursor_y, 0, 0)?;

        self.params.cursor_val = base.create_param("CURSOR_VAL", ParamType::Float64)?;
        self.params.profile_size_x = base.create_param("PROFILE_SIZE_X", ParamType::Int32)?;
        self.params.profile_size_y = base.create_param("PROFILE_SIZE_Y", ParamType::Int32)?;

        self.params.skewx_value = base.create_param("SKEWX_VALUE", ParamType::Float64)?;
        self.params.skewy_value = base.create_param("SKEWY_VALUE", ParamType::Float64)?;
        self.params.profile_average_x =
            base.create_param("PROFILE_AVERAGE_X", ParamType::Float64Array)?;
        self.params.profile_average_y =
            base.create_param("PROFILE_AVERAGE_Y", ParamType::Float64Array)?;
        self.params.profile_threshold_x =
            base.create_param("PROFILE_THRESHOLD_X", ParamType::Float64Array)?;
        self.params.profile_threshold_y =
            base.create_param("PROFILE_THRESHOLD_Y", ParamType::Float64Array)?;
        self.params.profile_centroid_x =
            base.create_param("PROFILE_CENTROID_X", ParamType::Float64Array)?;
        self.params.profile_centroid_y =
            base.create_param("PROFILE_CENTROID_Y", ParamType::Float64Array)?;
        self.params.profile_cursor_x =
            base.create_param("PROFILE_CURSOR_X", ParamType::Float64Array)?;
        self.params.profile_cursor_y =
            base.create_param("PROFILE_CURSOR_Y", ParamType::Float64Array)?;
        self.params.hist_array = base.create_param("HIST_ARRAY", ParamType::Float64Array)?;
        self.params.hist_x_array = base.create_param("HIST_X_ARRAY", ParamType::Float64Array)?;

        // Export params so create_stats_runtime can retrieve them after the move
        *self.params_out.lock() = self.params;

        Ok(())
    }

    fn on_param_change(
        &self,
        reason: usize,
        snapshot: &PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        let p = &self.params;
        let mut cfg = self.config.lock();
        if reason == p.compute_statistics {
            cfg.do_compute_statistics = snapshot.value.as_i32() != 0;
        } else if reason == p.compute_centroid {
            cfg.do_compute_centroid = snapshot.value.as_i32() != 0;
        } else if reason == p.compute_histogram {
            cfg.do_compute_histogram = snapshot.value.as_i32() != 0;
        } else if reason == p.compute_profiles {
            cfg.do_compute_profiles = snapshot.value.as_i32() != 0;
        } else if reason == p.bgd_width {
            cfg.bgd_width = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.centroid_threshold {
            cfg.centroid_threshold = snapshot.value.as_f64();
        } else if reason == p.cursor_x {
            cfg.cursor_x = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.cursor_y {
            cfg.cursor_y = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.hist_size {
            cfg.hist_size = (snapshot.value.as_i32().max(1)) as usize;
        } else if reason == p.hist_min {
            cfg.hist_min = snapshot.value.as_f64();
        } else if reason == p.hist_max {
            cfg.hist_max = snapshot.value.as_f64();
        }
        ad_core_rs::plugin::runtime::ParamChangeResult::empty()
    }
}

/// Create a stats plugin runtime with an integrated time series port.
///
/// Returns:
/// Create a stats plugin runtime. The TS receiver is stored in the registry
/// for later pickup by `NDTimeSeriesConfigure`.
pub fn create_stats_runtime(
    port_name: &str,
    pool: Arc<NDArrayPool>,
    queue_size: usize,
    ndarray_port: &str,
    wiring: Arc<WiringRegistry>,
    ts_registry: &crate::time_series::TsReceiverRegistry,
) -> (
    PluginRuntimeHandle,
    Arc<Mutex<StatsResult>>,
    NDStatsParams,
    std::thread::JoinHandle<()>,
) {
    let (ts_tx, ts_rx) = tokio::sync::mpsc::channel(256);

    let mut processor = StatsProcessor::new();
    processor.set_ts_sender(ts_tx);
    let stats_handle = processor.stats_handle();
    let params_handle = processor.params_handle();

    let (plugin_handle, data_jh) = ad_core_rs::plugin::runtime::create_plugin_runtime(
        port_name,
        processor,
        pool,
        queue_size,
        ndarray_port,
        wiring,
    );

    let stats_params = *params_handle.lock();

    // Store the TS receiver for NDTimeSeriesConfigure to pick up
    let channel_names: Vec<String> = crate::time_series::STATS_TS_CHANNEL_NAMES
        .iter()
        .map(|s| s.to_string())
        .collect();
    ts_registry.store(port_name, ts_rx, channel_names);

    (plugin_handle, stats_handle, stats_params, data_jh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    #[test]
    fn test_ts_timestamp_channel_is_the_standalone_double() {
        // R8-66 family: C `timeSeries[TSTimestamp] = pArray->timeStamp`
        // (NDPluginStats.cpp:577) — the standalone double a driver sets from its
        // own clock, not a value derived from epicsTS. The port sent
        // `timestamp.as_f64()`.
        use crate::time_series::{NUM_STATS_TS_CHANNELS, STATS_TS_CHANNEL_NAMES};
        use ad_core_rs::ndarray::NDArray;
        use ad_core_rs::ndarray_pool::NDArrayPool;
        use ad_core_rs::plugin::runtime::NDPluginProcess;

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut processor = StatsProcessor::new();
        processor.set_ts_sender(tx);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.data = NDDataBuffer::U8(vec![1, 2, 3, 4]);
        arr.timestamp = ad_core_rs::timestamp::EpicsTimestamp {
            sec: 100,
            nsec: 500_000_000,
        };
        arr.time_stamp = 7.25; // hardware clock, unrelated to epicsTS

        processor.process_array(&arr, &NDArrayPool::new(1_000_000));

        let ts = rx.try_recv().expect("stats pushes a TS sample per frame");
        assert_eq!(ts.values.len(), NUM_STATS_TS_CHANNELS);
        let idx = STATS_TS_CHANNEL_NAMES
            .iter()
            .position(|n| *n == "TSTimestamp")
            .unwrap();
        assert!(
            (ts.values[idx] - 7.25).abs() < 1e-9,
            "TSTimestamp carries pArray->timeStamp, got {}",
            ts.values[idx]
        );
    }

    #[test]
    fn test_compute_stats_u8() {
        let dims = vec![NDDimension::new(5)];
        let data = NDDataBuffer::U8(vec![10, 20, 30, 40, 50]);
        let stats = compute_stats(&data, &dims, 0);
        assert_eq!(stats.min, 10.0);
        assert_eq!(stats.max, 50.0);
        assert_eq!(stats.mean, 30.0);
        assert_eq!(stats.total, 150.0);
        assert_eq!(stats.num_elements, 5);
    }

    #[test]
    fn test_compute_stats_sigma() {
        let dims = vec![NDDimension::new(8)];
        let data = NDDataBuffer::F64(vec![2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]);
        let stats = compute_stats(&data, &dims, 0);
        assert!((stats.mean - 5.0).abs() < 1e-10);
        assert!((stats.sigma - 2.0).abs() < 1e-10);
    }

    #[test]
    fn test_compute_stats_u16() {
        let dims = vec![NDDimension::new(3)];
        let data = NDDataBuffer::U16(vec![100, 200, 300]);
        let stats = compute_stats(&data, &dims, 0);
        assert_eq!(stats.min, 100.0);
        assert_eq!(stats.max, 300.0);
        assert_eq!(stats.mean, 200.0);
    }

    #[test]
    fn test_compute_stats_f64() {
        let dims = vec![NDDimension::new(3)];
        let data = NDDataBuffer::F64(vec![1.5, 2.5, 3.5]);
        let stats = compute_stats(&data, &dims, 0);
        assert!((stats.min - 1.5).abs() < 1e-10);
        assert!((stats.max - 3.5).abs() < 1e-10);
        assert!((stats.mean - 2.5).abs() < 1e-10);
    }

    #[test]
    fn test_compute_stats_single_element() {
        let dims = vec![NDDimension::new(1)];
        let data = NDDataBuffer::I32(vec![42]);
        let stats = compute_stats(&data, &dims, 0);
        assert_eq!(stats.min, 42.0);
        assert_eq!(stats.max, 42.0);
        assert_eq!(stats.mean, 42.0);
        assert_eq!(stats.sigma, 0.0);
        assert_eq!(stats.num_elements, 1);
    }

    #[test]
    fn test_compute_stats_empty() {
        let data = NDDataBuffer::U8(vec![]);
        let stats = compute_stats(&data, &[], 0);
        assert_eq!(stats.num_elements, 0);
    }

    #[test]
    fn test_compute_stats_min_max_position() {
        let dims = vec![NDDimension::new(4), NDDimension::new(4)];
        // 4x4 array: min at [0], max at [15]
        let data = NDDataBuffer::U8((1..=16).collect());
        let stats = compute_stats(&data, &dims, 0);
        assert_eq!(stats.min_x, 0); // index 0 -> x=0, y=0
        assert_eq!(stats.min_y, 0);
        assert_eq!(stats.max_x, 3); // index 15 -> x=3, y=3
        assert_eq!(stats.max_y, 3);
    }

    #[test]
    fn test_compute_stats_net_no_bgd() {
        let dims = vec![NDDimension::new(4), NDDimension::new(4)];
        let data = NDDataBuffer::U8((1..=16).collect());
        let stats = compute_stats(&data, &dims, 0);
        // With bgd_width=0, net should equal total
        assert_eq!(stats.net, stats.total);
    }

    #[test]
    fn test_compute_stats_bgd_subtraction() {
        // 4x4 image with uniform value 10, plus a bright center pixel
        let dims = vec![NDDimension::new(4), NDDimension::new(4)];
        let mut pixels = vec![10u16; 16];
        // Put a bright spot at (2,2) = index 10
        pixels[2 * 4 + 2] = 110;
        let data = NDDataBuffer::U16(pixels);
        let stats = compute_stats(&data, &dims, 1);

        // With bgd_width=1, all edge pixels (1 pixel from each edge) are used for background.
        // In a 4x4 image with bgd_width=1, only pixels at (1,1), (2,1), (1,2), (2,2) are interior.
        // Edge pixels are the 12 remaining pixels. 11 of them are 10, one at (2,2) might be edge or not.
        // Actually (2,2) is interior (ix=2 is not <1 and not >=3, iy=2 is not <1 and not >=3).
        // So edge pixels: 12 pixels all with value 10. bgd_avg = 10.0
        // net = total - bgd_avg * num_elements
        // total = 15*10 + 110 = 260
        // net = 260 - 10.0 * 16 = 260 - 160 = 100
        assert!((stats.net - 100.0).abs() < 1e-10);
    }

    /// BUG 2 regression: 2-D background matches C++ `NDPluginStats`
    /// edge-strip computation, including corner double-counting.
    ///
    /// 4x4 image, bgd_width=1. C++ strips (dim 0 = x fastest):
    ///   dim x: low strip ix=0 (4 px), high strip ix=3 (4 px)
    ///   dim y: low strip iy=0 (4 px), high strip iy=3 (4 px)
    /// bgd_pixels = 16 (the 4 corners are counted twice; the 4 interior
    /// pixels are never counted). bgd_counts is the sum over those 16
    /// strip slots, corners contributing twice.
    #[test]
    fn test_compute_stats_bgd_2d_corner_double_count() {
        // 4x4, row-major, dim0=x fastest. Asymmetric data so that corner
        // double-counting demonstrably changes the result:
        //   corners      = 100   (idx 0, 3, 12, 15)
        //   other edges  = 10    (idx 1, 2, 4, 7, 8, 11, 13, 14)
        //   interior     = 1     (idx 5, 6, 9, 10)
        // Rows (y):
        //   y0: [100,  10,  10, 100]
        //   y1: [ 10,   1,   1,  10]
        //   y2: [ 10,   1,   1,  10]
        //   y3: [100,  10,  10, 100]
        let dims = vec![NDDimension::new(4), NDDimension::new(4)];
        let mut pixels = vec![1u16; 16];
        for &i in &[1usize, 2, 4, 7, 8, 11, 13, 14] {
            pixels[i] = 10;
        }
        for &i in &[0usize, 3, 12, 15] {
            pixels[i] = 100;
        }
        let total_expected: f64 = pixels.iter().map(|&p| p as f64).sum();
        let data = NDDataBuffer::U16(pixels);
        let stats = compute_stats(&data, &dims, 1);

        // C++ strip sum (bgd_width=1):
        //   x low strip  (ix=0): idx 0,4,8,12  -> 100,10,10,100 = 220
        //   x high strip (ix=3): idx 3,7,11,15 -> 100,10,10,100 = 220
        //   y low strip  (iy=0): idx 0,1,2,3   -> 100,10,10,100 = 220
        //   y high strip (iy=3): idx 12,13,14,15 -> 100,10,10,100 = 220
        // Each corner (100) appears in two strips => double-counted.
        let bgd_counts = 220 + 220 + 220 + 220; // 880
        let bgd_pixels = 16; // 4 strips * 4 px each
        let bgd_avg = bgd_counts as f64 / bgd_pixels as f64; // 55.0
        let expected_net = total_expected - bgd_avg * 16.0;
        assert!(
            (stats.net - expected_net).abs() < 1e-9,
            "net {} != expected {}",
            stats.net,
            expected_net
        );

        // A once-each perimeter (12 distinct pixels) would average
        // (4*100 + 8*10)/12 = 40.0, NOT 55.0 — proving the corners are
        // double-counted exactly as C++ documents.
        let perimeter_avg = (4.0 * 100.0 + 8.0 * 10.0) / 12.0;
        assert!(
            (bgd_avg - perimeter_avg).abs() > 1e-9,
            "corner double-count must change bgd_avg vs a once-each perimeter"
        );
        assert!((bgd_avg - 55.0).abs() < 1e-9);
    }

    /// BUG 2 regression: background works for 1-D arrays (C++ runs the
    /// strip algorithm for any `ndims`, not just >= 2).
    #[test]
    fn test_compute_stats_bgd_1d() {
        // 1-D, 8 elements: [10, 20, 30, 40, 50, 60, 70, 80], bgd_width=2.
        let dims = vec![NDDimension::new(8)];
        let data = NDDataBuffer::U16(vec![10, 20, 30, 40, 50, 60, 70, 80]);
        let stats = compute_stats(&data, &dims, 2);

        // Single dimension: low strip indices 0,1 -> 10,20;
        // high strip offset 8-2=6, indices 6,7 -> 70,80.
        // No corner overlap in 1-D (strips disjoint here).
        let bgd_counts = 10 + 20 + 70 + 80;
        let bgd_pixels = 4;
        let bgd_avg = bgd_counts as f64 / bgd_pixels as f64; // 45.0
        let total = (10 + 20 + 30 + 40 + 50 + 60 + 70 + 80) as f64;
        let expected_net = total - bgd_avg * 8.0;
        assert!(
            (stats.net - expected_net).abs() < 1e-9,
            "1-D net {} != expected {}",
            stats.net,
            expected_net
        );
    }

    /// BUG 2 regression: background works for 3-D arrays.
    #[test]
    fn test_compute_stats_bgd_3d() {
        // 2x2x2 array, every element = 1, bgd_width = 1.
        // With bgd_width >= every dim size, every strip covers the whole
        // array; corners counted many times. bgd_avg must still be 1.0
        // (uniform data), so net = total - 1.0 * 8 = 0.
        let dims = vec![
            NDDimension::new(2),
            NDDimension::new(2),
            NDDimension::new(2),
        ];
        let data = NDDataBuffer::U8(vec![1u8; 8]);
        let stats = compute_stats(&data, &dims, 1);
        assert!(
            stats.net.abs() < 1e-9,
            "3-D uniform net should be 0, got {}",
            stats.net
        );
        assert_eq!(stats.total, 8.0);
    }

    #[test]
    fn test_centroid_uniform() {
        let data = NDDataBuffer::U8(vec![1; 16]);
        let c = compute_centroid(&data, 4, 4, 0.0);
        assert!((c.centroid_x - 1.5).abs() < 1e-10);
        assert!((c.centroid_y - 1.5).abs() < 1e-10);
    }

    #[test]
    fn test_centroid_corner() {
        let mut d = vec![0u8; 16];
        d[0] = 255;
        let data = NDDataBuffer::U8(d);
        let c = compute_centroid(&data, 4, 4, 0.0);
        assert!((c.centroid_x - 0.0).abs() < 1e-10);
        assert!((c.centroid_y - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_centroid_threshold() {
        // 4x4 image: background of 5, bright spot of 100 at (2,2)
        let mut pixels = vec![5u8; 16];
        pixels[2 * 4 + 2] = 100;
        let data = NDDataBuffer::U8(pixels);

        // With threshold=50, only the bright pixel should be counted
        let c = compute_centroid(&data, 4, 4, 50.0);
        assert!((c.centroid_x - 2.0).abs() < 1e-10);
        assert!((c.centroid_y - 2.0).abs() < 1e-10);
        assert!((c.centroid_total - 100.0).abs() < 1e-10);
    }

    #[test]
    fn test_centroid_higher_moments_symmetric() {
        // Symmetric distribution: skewness should be ~0, eccentricity ~0 for uniform
        let data = NDDataBuffer::U8(vec![1; 16]);
        let c = compute_centroid(&data, 4, 4, 0.0);
        // Symmetric -> skewness ~0
        assert!(c.skewness_x.abs() < 1e-10);
        assert!(c.skewness_y.abs() < 1e-10);
        // Uniform 4x4 -> sigma_x == sigma_y -> eccentricity ~0
        assert!(c.eccentricity.abs() < 1e-10);
    }

    #[test]
    fn test_histogram_basic() {
        // 10 values: 0..9, hist range [0, 9], 10 bins
        let data = NDDataBuffer::F64((0..10).map(|x| x as f64).collect());
        let (hist, below, above, entropy) = compute_histogram(&data, 10, 0.0, 9.0);
        assert_eq!(hist.len(), 10);
        assert_eq!(below, 0.0);
        assert_eq!(above, 0.0);
        // Each bin should have ~1 count (uniform distribution)
        let total: f64 = hist.iter().sum();
        assert!((total - 10.0).abs() < 1e-10);
        // C++ entropy: -sum(count * ln(count)) / nElements
        // Uniform: each bin has 1, so sum(1*ln(1)) = 0, entropy = 0
        assert!(entropy.abs() < 1e-10);
    }

    #[test]
    fn test_histogram_below_above() {
        let data = NDDataBuffer::F64(vec![-1.0, 0.5, 1.5, 3.0]);
        let (hist, below, above, _entropy) = compute_histogram(&data, 2, 0.0, 2.0);
        assert_eq!(below, 1.0); // -1.0 is below
        assert_eq!(above, 1.0); // 3.0 is above
        let total_in_bins: f64 = hist.iter().sum();
        assert!((total_in_bins - 2.0).abs() < 1e-10); // 0.5 and 1.5
    }

    #[test]
    fn test_histogram_single_value() {
        let data = NDDataBuffer::F64(vec![5.0; 100]);
        let (hist, below, above, entropy) = compute_histogram(&data, 10, 0.0, 10.0);
        assert_eq!(below, 0.0);
        assert_eq!(above, 0.0);
        // C++ entropy: one bin has 100, 9 bins have 0→1
        // sum = 100*ln(100) + 9*(1*ln(1)) = 100*ln(100)
        // entropy = -100*ln(100)/100 = -ln(100)
        let expected = -(100.0f64.ln());
        assert!((entropy - expected).abs() < 1e-10);
        let total: f64 = hist.iter().sum();
        assert!((total - 100.0).abs() < 1e-10);
    }

    #[test]
    fn test_profiles_8x8() {
        // 8x8 image with value = row index (0..7 repeated across columns)
        let mut pixels = vec![0.0f64; 64];
        for iy in 0..8 {
            for ix in 0..8 {
                pixels[iy * 8 + ix] = iy as f64;
            }
        }
        let data = NDDataBuffer::F64(pixels);

        let profiles = compute_profiles(
            &data, 8, 8, 0.0, // threshold
            3.5, // centroid_x (center)
            3.5, // centroid_y (center)
            0,   // cursor_x
            7,   // cursor_y
        );

        // Average X profile: each column has the same values (0..7), avg = 3.5
        assert_eq!(profiles.avg_x.len(), 8);
        for &v in &profiles.avg_x {
            assert!((v - 3.5).abs() < 1e-10, "avg_x should be 3.5, got {v}");
        }

        // Average Y profile: each row has uniform value = row index, avg = row index
        assert_eq!(profiles.avg_y.len(), 8);
        for (iy, &v) in profiles.avg_y.iter().enumerate() {
            assert!(
                (v - iy as f64).abs() < 1e-10,
                "avg_y[{iy}] should be {iy}, got {v}"
            );
        }

        // Cursor X profile: row at cursor_y=7 -> all pixels are 7.0
        assert_eq!(profiles.cursor_x.len(), 8);
        for &v in &profiles.cursor_x {
            assert!((v - 7.0).abs() < 1e-10);
        }

        // Cursor Y profile: column at cursor_x=0 -> values are 0,1,2,...,7
        assert_eq!(profiles.cursor_y.len(), 8);
        for (iy, &v) in profiles.cursor_y.iter().enumerate() {
            assert!((v - iy as f64).abs() < 1e-10);
        }

        // Centroid X profile: row at round(centroid_y=3.5+0.5)=4 -> all pixels are 4.0
        assert_eq!(profiles.centroid_x.len(), 8);
        for &v in &profiles.centroid_x {
            assert!((v - 4.0).abs() < 1e-10);
        }

        // Centroid Y profile: column at round(centroid_x=3.5+0.5)=4 -> values are 0,1,...,7
        assert_eq!(profiles.centroid_y.len(), 8);
        for (iy, &v) in profiles.centroid_y.iter().enumerate() {
            assert!((v - iy as f64).abs() < 1e-10);
        }
    }

    #[test]
    fn test_adp14_out_of_range_cursor_clamps_to_edge_not_zeros() {
        // C clamps an out-of-range cursor/centroid to the last valid line
        // (NDPluginStats.cpp:341-360), never returning a zero-filled profile.
        // 8x8 image with value = row index.
        let mut pixels = vec![0.0f64; 64];
        for iy in 0..8 {
            for ix in 0..8 {
                pixels[iy * 8 + ix] = iy as f64;
            }
        }
        let data = NDDataBuffer::F64(pixels);

        let profiles = compute_profiles(
            &data, 8, 8, 0.0,   // threshold
            100.0, // centroid_x out of range -> clamp to col 7
            100.0, // centroid_y out of range -> clamp to row 7
            50,    // cursor_x out of range -> clamp to col 7
            50,    // cursor_y out of range -> clamp to row 7
        );

        // Cursor X profile: clamped row 7 -> all 7.0 (NOT zeros).
        assert_eq!(profiles.cursor_x.len(), 8);
        for &v in &profiles.cursor_x {
            assert!((v - 7.0).abs() < 1e-10, "cursor_x should clamp to row 7");
        }
        // Cursor Y profile: clamped col 7 -> values 0..7 (NOT zeros).
        for (iy, &v) in profiles.cursor_y.iter().enumerate() {
            assert!(
                (v - iy as f64).abs() < 1e-10,
                "cursor_y should clamp to col 7"
            );
        }
        // Centroid X profile: clamped row 7 -> all 7.0.
        for &v in &profiles.centroid_x {
            assert!((v - 7.0).abs() < 1e-10, "centroid_x should clamp to row 7");
        }
        // Centroid Y profile: clamped col 7 -> values 0..7.
        for (iy, &v) in profiles.centroid_y.iter().enumerate() {
            assert!(
                (v - iy as f64).abs() < 1e-10,
                "centroid_y should clamp to col 7"
            );
        }
    }

    #[test]
    fn test_profiles_threshold() {
        // 4x4 image: all 1.0 except one bright pixel at (2,1) = 10.0
        let mut pixels = vec![1.0f64; 16];
        pixels[1 * 4 + 2] = 10.0;
        let data = NDDataBuffer::F64(pixels);

        let profiles = compute_profiles(
            &data, 4, 4, 5.0, // threshold
            2.0, 1.0, 0, 0,
        );

        // Threshold X profile: only column 2 has a pixel >= 5.0 (at row 1)
        assert_eq!(profiles.threshold_x.len(), 4);
        assert!((profiles.threshold_x[2] - 10.0).abs() < 1e-10);
        // Other columns: no pixels above threshold
        assert!((profiles.threshold_x[0] - 0.0).abs() < 1e-10);
        assert!((profiles.threshold_x[1] - 0.0).abs() < 1e-10);
        assert!((profiles.threshold_x[3] - 0.0).abs() < 1e-10);

        // Threshold Y profile: only row 1 has a pixel >= 5.0
        assert_eq!(profiles.threshold_y.len(), 4);
        assert!((profiles.threshold_y[1] - 10.0).abs() < 1e-10);
        assert!((profiles.threshold_y[0] - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_stats_processor_direct() {
        let proc = StatsProcessor::new();
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(5)], NDDataType::UInt8);
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v[0] = 10;
            v[1] = 20;
            v[2] = 30;
            v[3] = 40;
            v[4] = 50;
        }

        let result = proc.process_array(&arr, &pool);
        // C++ Stats forwards the input array to downstream plugins
        assert_eq!(result.output_arrays.len(), 1, "stats forwards the array");

        let stats = proc.stats_handle().lock().clone();
        assert_eq!(stats.min, 10.0);
        assert_eq!(stats.max, 50.0);
        assert_eq!(stats.mean, 30.0);
    }

    #[test]
    fn test_stats_emits_histogram_and_profile_arrays() {
        use ad_core_rs::plugin::runtime::ParamUpdate;
        let mut proc = StatsProcessor::new();
        // Register params so the array reasons are distinct, non-zero indices.
        let mut base = asyn_rs::port::PortDriverBase::new(
            "_stats_scratch_",
            1,
            asyn_rs::port::PortFlags::default(),
        );
        let _ = ad_core_rs::params::ndarray_driver::NDArrayDriverParams::create(&mut base);
        let _ = ad_core_rs::plugin::params::PluginBaseParams::create(&mut base);
        proc.register_params(&mut base).unwrap();

        proc.config.lock().do_compute_histogram = true;
        proc.config.lock().do_compute_profiles = true;
        proc.config.lock().hist_size = 8;
        proc.config.lock().hist_min = 0.0;
        proc.config.lock().hist_max = 7.0;
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for (i, val) in v.iter_mut().enumerate() {
                *val = (i % 8) as u8;
            }
        }

        let result = proc.process_array(&arr, &pool);
        let p = proc.params;
        // HIST_ARRAY, HIST_X_ARRAY and the 8 PROFILE_* waveforms must be
        // pushed as float64 array updates.
        let array_reasons: Vec<usize> = result
            .param_updates
            .iter()
            .filter_map(|u| match u {
                ParamUpdate::Float64Array { reason, value, .. } => {
                    assert!(!value.is_empty(), "array waveform must not be empty");
                    Some(*reason)
                }
                _ => None,
            })
            .collect();
        for reason in [
            p.hist_array,
            p.hist_x_array,
            p.profile_average_x,
            p.profile_average_y,
            p.profile_threshold_x,
            p.profile_threshold_y,
            p.profile_centroid_x,
            p.profile_centroid_y,
            p.profile_cursor_x,
            p.profile_cursor_y,
        ] {
            assert!(
                array_reasons.contains(&reason),
                "missing array update for reason {reason}"
            );
        }
    }

    #[test]
    fn test_adp13_ndims_gt_2_skips_centroid_and_profiles() {
        use ad_core_rs::plugin::runtime::ParamUpdate;
        // C rejects ndims>2 for centroid/profiles (NDPluginStats.cpp:205,338):
        // a 4-D array must NOT have its centroid/profiles computed on the first
        // two dims as if it were a 2-D image.
        let mut proc = StatsProcessor::new();
        let mut base = asyn_rs::port::PortDriverBase::new(
            "_stats_adp13_",
            1,
            asyn_rs::port::PortFlags::default(),
        );
        let _ = ad_core_rs::params::ndarray_driver::NDArrayDriverParams::create(&mut base);
        let _ = ad_core_rs::plugin::params::PluginBaseParams::create(&mut base);
        proc.register_params(&mut base).unwrap();
        proc.config.lock().do_compute_centroid = true;
        proc.config.lock().do_compute_profiles = true;
        let pool = NDArrayPool::new(1_000_000);

        // 4-D mono array [x=4, y=4, z=2, w=2]. The first 4x4 plane has a bright
        // column at x=3, so a (wrong) 2-D centroid would be clearly nonzero.
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(4),
                NDDimension::new(4),
                NDDimension::new(2),
                NDDimension::new(2),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for (i, val) in v.iter_mut().enumerate() {
                *val = if i % 4 == 3 { 100 } else { 0 };
            }
        }

        let result = proc.process_array(&arr, &pool);
        let p = proc.params;

        // Centroid left at 0 (not computed on a slice).
        let centroid_x = result.param_updates.iter().find_map(|u| match u {
            ParamUpdate::Float64 { reason, value, .. } if *reason == p.centroid_x => Some(*value),
            _ => None,
        });
        assert_eq!(
            centroid_x,
            Some(0.0),
            "centroid_x must not be computed for ndims>2"
        );

        // No profile waveforms emitted for a >2-D array.
        let profile_reasons = [
            p.profile_average_x,
            p.profile_average_y,
            p.profile_threshold_x,
            p.profile_threshold_y,
            p.profile_centroid_x,
            p.profile_centroid_y,
            p.profile_cursor_x,
            p.profile_cursor_y,
        ];
        for u in &result.param_updates {
            if let ParamUpdate::Float64Array { reason, .. } = u {
                assert!(
                    !profile_reasons.contains(reason),
                    "no profile waveform may be emitted for ndims>2"
                );
            }
        }
    }

    #[test]
    fn test_adp30_hist_below_above_emitted_as_int32() {
        use ad_core_rs::plugin::runtime::ParamUpdate;
        // C registers HIST_BELOW/HIST_ABOVE as asynParamInt32 and writes them
        // via setIntegerParam (NDPluginStats.cpp:827-828,627-628); a client
        // reading the RBVs must get DBR_LONG (Int32), not DBR_DOUBLE.
        let mut proc = StatsProcessor::new();
        let mut base = asyn_rs::port::PortDriverBase::new(
            "_stats_adp30_",
            1,
            asyn_rs::port::PortFlags::default(),
        );
        let _ = ad_core_rs::params::ndarray_driver::NDArrayDriverParams::create(&mut base);
        let _ = ad_core_rs::plugin::params::PluginBaseParams::create(&mut base);
        proc.register_params(&mut base).unwrap();
        proc.config.lock().do_compute_histogram = true;
        proc.config.lock().hist_size = 4;
        proc.config.lock().hist_min = 2.0;
        proc.config.lock().hist_max = 5.0;
        let pool = NDArrayPool::new(1_000_000);

        // 8 pixels: 0,1 below min(2) → below=2; 9,9,9 above max(5) → above=3;
        // 3,3,4 in range.
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v.copy_from_slice(&[0, 1, 3, 3, 9, 9, 9, 4]);
        }

        let result = proc.process_array(&arr, &pool);
        let p = proc.params;

        let below = result.param_updates.iter().find_map(|u| match u {
            ParamUpdate::Int32 { reason, value, .. } if *reason == p.hist_below => Some(*value),
            _ => None,
        });
        let above = result.param_updates.iter().find_map(|u| match u {
            ParamUpdate::Int32 { reason, value, .. } if *reason == p.hist_above => Some(*value),
            _ => None,
        });
        assert_eq!(
            below,
            Some(2),
            "HIST_BELOW must be emitted as an Int32 count"
        );
        assert_eq!(
            above,
            Some(3),
            "HIST_ABOVE must be emitted as an Int32 count"
        );

        // And never as Float64 — the param type must be Int32 end to end.
        for u in &result.param_updates {
            if let ParamUpdate::Float64 { reason, .. } = u {
                assert!(
                    *reason != p.hist_below && *reason != p.hist_above,
                    "HIST_BELOW/HIST_ABOVE must not be emitted as Float64"
                );
            }
        }
    }

    #[test]
    fn test_hist_x_array_uses_bin_count_divisor() {
        use ad_core_rs::plugin::runtime::ParamUpdate;
        // C++ NDPluginStats::computeHistX: scale = (histMax-histMin)/histSize,
        // histX[i] = histMin + i*scale. For histSize=256, min=0, max=255 the
        // last bin X must be ~254.0 (= 255*255/256), NOT 255.0.
        let mut proc = StatsProcessor::new();
        let mut base = asyn_rs::port::PortDriverBase::new(
            "_stats_histx_",
            1,
            asyn_rs::port::PortFlags::default(),
        );
        let _ = ad_core_rs::params::ndarray_driver::NDArrayDriverParams::create(&mut base);
        let _ = ad_core_rs::plugin::params::PluginBaseParams::create(&mut base);
        proc.register_params(&mut base).unwrap();

        proc.config.lock().do_compute_histogram = true;
        proc.config.lock().hist_size = 256;
        proc.config.lock().hist_min = 0.0;
        proc.config.lock().hist_max = 255.0;
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for (i, val) in v.iter_mut().enumerate() {
                *val = (i * 16) as u8;
            }
        }

        let result = proc.process_array(&arr, &pool);
        let hist_x = result
            .param_updates
            .iter()
            .find_map(|u| match u {
                ParamUpdate::Float64Array { reason, value, .. }
                    if *reason == proc.params.hist_x_array =>
                {
                    Some(value.clone())
                }
                _ => None,
            })
            .expect("HIST_X_ARRAY must be emitted");

        assert_eq!(hist_x.len(), 256, "256 bins");
        let scale = 255.0 / 256.0;
        assert!(
            (hist_x[0] - 0.0).abs() < 1e-9,
            "bin 0 X must be histMin (0.0), got {}",
            hist_x[0]
        );
        assert!(
            (hist_x[1] - scale).abs() < 1e-9,
            "bin 1 X must be {scale}, got {}",
            hist_x[1]
        );
        assert!(
            (hist_x[255] - 255.0 * scale).abs() < 1e-9,
            "last bin X must be ~254.004 (255*255/256), got {}",
            hist_x[255]
        );
        assert!(
            hist_x[255] < 255.0,
            "last bin X must be strictly below histMax, got {}",
            hist_x[255]
        );
    }

    #[test]
    fn test_stats_runtime_end_to_end() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let wiring = Arc::new(WiringRegistry::new());
        let ts_registry = crate::time_series::TsReceiverRegistry::new();
        let (handle, stats, _params, _jh) =
            create_stats_runtime("STATS_RT", pool, 10, "", wiring, &ts_registry);

        // Plugins default to disabled — enable for test, and fence until the
        // data thread has applied the flip (the write only queues it).
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        assert!(
            handle.wait_params_applied(std::time::Duration::from_secs(10)),
            "data thread did not apply EnableCallbacks"
        );

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for (i, val) in v.iter_mut().enumerate() {
                *val = (i + 1) as u8;
            }
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(handle.array_sender().publish(Arc::new(arr)));

        // Wait on the observable itself — the stats land when the data thread
        // processes the array; a fixed sleep is a race on a loaded machine.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while stats.lock().num_elements != 16 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for stats to be computed"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let result = stats.lock().clone();
        assert_eq!(result.min, 1.0);
        assert_eq!(result.max, 16.0);
        assert_eq!(result.num_elements, 16);
    }
}
