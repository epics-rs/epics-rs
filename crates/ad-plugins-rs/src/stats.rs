// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — all 1 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p ad-plugins-rs
// --all-features`, 556/556). ad-plugins-rs became a census subject when
// its `build.rs` began deriving `tokio_backend`; nothing here builds a
// CA server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.
use std::sync::Arc;

#[cfg(feature = "parallel")]
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
    match data {
        NDDataBuffer::I8(v) => stats_of(v, dims, bgd_width),
        NDDataBuffer::U8(v) => stats_of(v, dims, bgd_width),
        NDDataBuffer::I16(v) => stats_of(v, dims, bgd_width),
        NDDataBuffer::U16(v) => stats_of(v, dims, bgd_width),
        NDDataBuffer::I32(v) => stats_of(v, dims, bgd_width),
        NDDataBuffer::U32(v) => stats_of(v, dims, bgd_width),
        NDDataBuffer::I64(v) => stats_of(v, dims, bgd_width),
        NDDataBuffer::U64(v) => stats_of(v, dims, bgd_width),
        NDDataBuffer::F32(v) => stats_of(v, dims, bgd_width),
        NDDataBuffer::F64(v) => stats_of(v, dims, bgd_width),
    }
}

/// An element type [`compute_stats`] reduces over, with the accumulator its
/// running total is kept in: `i64`/`u64` for the integer types up to 32 bits
/// (exact, and a plain integer add the compiler vectorizes), `f64` for the
/// 64-bit integers and the floats (C sums every type in `double`,
/// NDPluginStats.cpp:137).
pub(crate) trait StatsElem: Copy + PartialOrd + 'static {
    /// The total's type.
    type Acc: Copy + Default + std::ops::Add<Output = Self::Acc>;
    /// The per-lane running sum, flushed into `Acc` every [`FLUSH`] chunks:
    /// `u32`/`i32` for the 8- and 16-bit types, whose widening to 64 bits
    /// per element would cost more than the add, and `Acc` itself otherwise.
    /// `FLUSH * LANES` elements of the widest 16-bit value fit a 32-bit lane.
    type Lane: Copy + Default + std::ops::Add<Output = Self::Lane>;
    fn to_lane(self) -> Self::Lane;
    fn lane_to_acc(lane: Self::Lane) -> Self::Acc;
    fn to_acc(self) -> Self::Acc;
    fn acc_to_f64(acc: Self::Acc) -> f64;
    fn to_f64(self) -> f64;
    /// The running extreme after seeing `e`: strict `<`/`>` against the
    /// current value, as C `doComputeStatisticsT` (NDPluginStats.cpp:130-136),
    /// so a NaN never becomes an extreme and a NaN already held is never
    /// displaced. The integer types spell it `Ord::min`/`max`, the form the
    /// compiler turns into a packed min/max; the two agree wherever `Ord`
    /// exists.
    fn lower(cur: Self, e: Self) -> Self;
    fn upper(cur: Self, e: Self) -> Self;
    /// The two reductions, as this type runs them: the lane loops below, or,
    /// with the `simd` feature, [`simd_kernels`] — `range` for every type,
    /// `variance` for the types up to 32 bits.
    fn range(v: &[Self]) -> Range<Self> {
        range_pass(v)
    }
    fn variance(v: &[Self], mean: f64) -> f64 {
        variance_pass(v, mean)
    }
    /// One row of a [`Projection`]: `col_sum` and `col_thr` gain the row's
    /// values and threshold values column by column, and the row's own
    /// Σvalue, Σthreshold value and Σthreshold value·ix come back. The lane
    /// loop below, or, with the `simd` feature, [`simd_kernels`].
    fn project_row(
        row: &[Self],
        threshold: f64,
        col_sum: &mut [f64],
        col_thr: &mut [f64],
    ) -> [f64; 3] {
        project_row_pass(row, threshold, col_sum, col_thr)
    }
    /// `Some(n)` when the type has only `n` values, few enough that a
    /// histogram maps them through a table of slots built once per frame;
    /// `None` for the wider types, which run the bin formula per element.
    const TABLE_LEN: Option<usize> = None;
    /// This value's index into that table, and the value at an index; both
    /// unused by a type without a table.
    fn table_index(self) -> usize {
        0
    }
    fn table_value(_index: usize) -> f64 {
        0.0
    }
    /// The formula path of a histogram: every value of `v` counted into
    /// `slots` (the bins, then the below and above slots). The element loop
    /// below, or, with the `simd` feature, [`simd_kernels`] for the 32-bit
    /// integers and the floats.
    fn formula_count(v: &[Self], f: &Formula, slots: &mut [u64]) {
        formula_count_pass(v, f, slots)
    }
}

macro_rules! stats_elem {
    (int: $($t:ty => $lane:ty => $acc:ty [$range:ident, $variance:ident, $project:ident $(, hist $hist:ident)?] $table:expr),* $(,)?) => {$(
        impl StatsElem for $t {
            type Acc = $acc;
            type Lane = $lane;
            const TABLE_LEN: Option<usize> = $table;
            #[inline(always)]
            fn table_index(self) -> usize {
                (self as i64 - <$t>::MIN as i64) as usize
            }
            #[inline(always)]
            fn table_value(index: usize) -> f64 {
                (index as i64 + <$t>::MIN as i64) as f64
            }
            #[cfg(feature = "simd")]
            fn range(v: &[Self]) -> Range<Self> {
                fearless_simd::dispatch!(ad_core_rs::simd::level(), s => simd_kernels::$range(s, v))
            }
            #[cfg(feature = "simd")]
            fn variance(v: &[Self], mean: f64) -> f64 {
                fearless_simd::dispatch!(ad_core_rs::simd::level(), s => simd_kernels::$variance(s, v, mean))
            }
            #[cfg(feature = "simd")]
            fn project_row(row: &[Self], threshold: f64, col_sum: &mut [f64], col_thr: &mut [f64]) -> [f64; 3] {
                fearless_simd::dispatch!(ad_core_rs::simd::level(), s => simd_kernels::$project(s, row, threshold, col_sum, col_thr))
            }
            $(
            #[cfg(feature = "simd")]
            fn formula_count(v: &[Self], f: &Formula, slots: &mut [u64]) {
                fearless_simd::dispatch!(ad_core_rs::simd::level(), s => simd_kernels::$hist(s, v, f, slots))
            }
            )?
            #[inline(always)]
            fn to_lane(self) -> $lane {
                self as $lane
            }
            #[inline(always)]
            fn lane_to_acc(lane: $lane) -> $acc {
                lane as $acc
            }
            #[inline(always)]
            fn to_acc(self) -> $acc {
                self as $acc
            }
            #[inline(always)]
            fn acc_to_f64(acc: $acc) -> f64 {
                acc as f64
            }
            #[inline(always)]
            fn to_f64(self) -> f64 {
                self as f64
            }
            #[inline(always)]
            fn lower(cur: Self, e: Self) -> Self {
                cur.min(e)
            }
            #[inline(always)]
            fn upper(cur: Self, e: Self) -> Self {
                cur.max(e)
            }
        }
    )*};
    (wide: $($t:ty => $lane:ty => $acc:ty [$range:ident, $project:ident]),* $(,)?) => {$(
        impl StatsElem for $t {
            type Acc = $acc;
            type Lane = $lane;
            #[cfg(feature = "simd")]
            fn range(v: &[Self]) -> Range<Self> {
                fearless_simd::dispatch!(ad_core_rs::simd::level(), s => simd_kernels::$range(s, v))
            }
            #[cfg(feature = "simd")]
            fn project_row(row: &[Self], threshold: f64, col_sum: &mut [f64], col_thr: &mut [f64]) -> [f64; 3] {
                fearless_simd::dispatch!(ad_core_rs::simd::level(), s => simd_kernels::$project(s, row, threshold, col_sum, col_thr))
            }
            #[inline(always)]
            fn to_lane(self) -> $lane {
                self as $lane
            }
            #[inline(always)]
            fn lane_to_acc(lane: $lane) -> $acc {
                lane as $acc
            }
            #[inline(always)]
            fn to_acc(self) -> $acc {
                self as $acc
            }
            #[inline(always)]
            fn acc_to_f64(acc: $acc) -> f64 {
                acc as f64
            }
            #[inline(always)]
            fn to_f64(self) -> f64 {
                self as f64
            }
            #[inline(always)]
            fn lower(cur: Self, e: Self) -> Self {
                cur.min(e)
            }
            #[inline(always)]
            fn upper(cur: Self, e: Self) -> Self {
                cur.max(e)
            }
        }
    )*};
    (float: $($t:ty [$range:ident, $project:ident, $hist:ident]),* $(,)?) => {$(
        impl StatsElem for $t {
            type Acc = f64;
            type Lane = f64;
            #[cfg(feature = "simd")]
            fn range(v: &[Self]) -> Range<Self> {
                fearless_simd::dispatch!(ad_core_rs::simd::level(), s => simd_kernels::$range(s, v))
            }
            #[cfg(feature = "simd")]
            fn project_row(row: &[Self], threshold: f64, col_sum: &mut [f64], col_thr: &mut [f64]) -> [f64; 3] {
                fearless_simd::dispatch!(ad_core_rs::simd::level(), s => simd_kernels::$project(s, row, threshold, col_sum, col_thr))
            }
            #[cfg(feature = "simd")]
            fn formula_count(v: &[Self], f: &Formula, slots: &mut [u64]) {
                fearless_simd::dispatch!(ad_core_rs::simd::level(), s => simd_kernels::$hist(s, v, f, slots))
            }
            #[inline(always)]
            fn to_lane(self) -> f64 {
                self as f64
            }
            #[inline(always)]
            fn lane_to_acc(lane: f64) -> f64 {
                lane
            }
            #[inline(always)]
            fn to_acc(self) -> f64 {
                self as f64
            }
            #[inline(always)]
            fn acc_to_f64(acc: f64) -> f64 {
                acc
            }
            #[inline(always)]
            fn to_f64(self) -> f64 {
                self as f64
            }
            #[inline(always)]
            fn lower(cur: Self, e: Self) -> Self {
                if e < cur { e } else { cur }
            }
            #[inline(always)]
            fn upper(cur: Self, e: Self) -> Self {
                if e > cur { e } else { cur }
            }
        }
    )*};
}
stats_elem! {
    int: i8 => i32 => i64 [range_i8, variance_i8, project_i8] Some(1 << 8),
    i16 => i32 => i64 [range_i16, variance_i16, project_i16] Some(1 << 16),
    i32 => i64 => i64 [range_i32, variance_i32, project_i32, hist hist_i32] None,
    u8 => u32 => u64 [range_u8, variance_u8, project_u8] Some(1 << 8),
    u16 => u32 => u64 [range_u16, variance_u16, project_u16] Some(1 << 16),
    u32 => u64 => u64 [range_u32, variance_u32, project_u32, hist hist_u32] None,
}
stats_elem!(wide: i64 => f64 => f64 [range_i64, project_i64], u64 => f64 => f64 [range_u64, project_u64]);
stats_elem!(float: f32 [range_f32, project_f32, hist_f32], f64 [range_f64, project_f64, hist_f64]);

/// Independent accumulators per reduction. Fixing the association this way is
/// what lets the compiler vectorize a floating-point sum at all — an IEEE sum
/// is not reassociable, so a single running total stays a serial chain — and
/// it makes the rounding the same on every CPU, since the association no
/// longer depends on the vector width the compiler picked.
const LANES: usize = 16;

/// Chunks between flushes of the narrow lane sums into the total:
/// `FLUSH * u16::MAX` stays below `u32::MAX`.
const FLUSH: usize = 4096;

/// Elements per parallel chunk: large enough that the per-chunk reduce is
/// noise, small enough to split a frame across the pool.
#[cfg(feature = "parallel")]
const PAR_CHUNK: usize = 1 << 16;

/// Extremes and total of one slice, by value; the positions come later from
/// [`first_index`] so this pass is a pure reduction.
pub(crate) struct Range<T> {
    pub(crate) min: T,
    pub(crate) max: T,
    pub(crate) total: f64,
}

/// One pass: per-lane min, max and sum, then a lane fold. Strict `<`/`>`
/// against the first element, as C `doComputeStatisticsT`
/// (NDPluginStats.cpp:121-136): a NaN never becomes an extreme, and a NaN in
/// the first slot is never displaced.
fn range_pass<T: StatsElem>(v: &[T]) -> Range<T> {
    let mut mins = [v[0]; LANES];
    let mut maxs = [v[0]; LANES];
    let mut sums = [T::Acc::default(); LANES];
    let mut chunks = v.chunks_exact(LANES);
    loop {
        let mut lanes = [T::Lane::default(); LANES];
        let mut seen = 0;
        for c in chunks.by_ref().take(FLUSH) {
            // A fixed-size view: with the length known, no bounds check sits
            // in the loop, which is what keeps it vectorizable.
            let c: &[T; LANES] = c.try_into().expect("chunks_exact yields LANES elements");
            for l in 0..LANES {
                let e = c[l];
                mins[l] = T::lower(mins[l], e);
                maxs[l] = T::upper(maxs[l], e);
                lanes[l] = lanes[l] + e.to_lane();
            }
            seen += 1;
        }
        for l in 0..LANES {
            sums[l] = sums[l] + T::lane_to_acc(lanes[l]);
        }
        if seen < FLUSH {
            break;
        }
    }
    let mut min = v[0];
    let mut max = v[0];
    let mut total = T::Acc::default();
    for l in 0..LANES {
        min = T::lower(min, mins[l]);
        max = T::upper(max, maxs[l]);
        total = total + sums[l];
    }
    for &e in chunks.remainder() {
        min = T::lower(min, e);
        max = T::upper(max, e);
        total = total + e.to_acc();
    }
    Range {
        min,
        max,
        total: T::acc_to_f64(total),
    }
}

/// Sum of squared deviations from `mean`.
fn variance_pass<T: StatsElem>(v: &[T], mean: f64) -> f64 {
    let mut lanes = [0.0f64; LANES];
    let mut chunks = v.chunks_exact(LANES);
    for c in &mut chunks {
        let c: &[T; LANES] = c.try_into().expect("chunks_exact yields LANES elements");
        for l in 0..LANES {
            let d = c[l].to_f64() - mean;
            lanes[l] += d * d;
        }
    }
    let mut acc: f64 = lanes.iter().sum();
    for &e in chunks.remainder() {
        let d = e.to_f64() - mean;
        acc += d * d;
    }
    acc
}

/// The first position holding `x` — C's `imin`/`imax`, which only a strictly
/// smaller/larger value moves. Absent (a NaN extreme) is index 0, where C's
/// counter started. Blocks are tested with a branch-free `any` the compiler
/// vectorizes; only the block that hits is walked element by element.
fn first_index<T: PartialEq + Copy>(v: &[T], x: T) -> usize {
    const BLOCK: usize = 64;
    let mut chunks = v.chunks_exact(BLOCK);
    let mut base = 0;
    for c in &mut chunks {
        if c.iter().fold(false, |hit, &e| hit | (e == x)) {
            return base + c.iter().position(|&e| e == x).unwrap_or(0);
        }
        base += BLOCK;
    }
    chunks
        .remainder()
        .iter()
        .position(|&e| e == x)
        .map_or(0, |i| base + i)
}

/// [`range_pass`], [`variance_pass`] and [`project_row_pass`] on explicit
/// vectors, one level per CPU the binary may run on. The lane loops above
/// only reach the baseline the binary was compiled for (SSE2 on x86_64,
/// where an unsigned 16-bit min does not even exist); `fearless_simd` picks
/// AVX2/AVX-512/NEON at run time, and its `Fallback` level is the lane loop
/// again on anything else. The range kernels are for the integer types
/// only: their extremes are exact under any lane order, and their sums are
/// integer adds. The floats and the 64-bit integers keep the lane loops,
/// whose float min/max the compiler already lowers to packed compares with
/// C's strict semantics. The projection is `f64` arithmetic whatever the
/// element, so every type that widens to `f64` vectors gets a kernel.
#[cfg(feature = "simd")]
mod simd_kernels {
    use super::{FLUSH, Formula, Range, StatsElem};
    use fearless_simd::{Simd, prelude::*};
    use fearless_simd_macros::simd;

    /// [`super::project_row_pass`] on vectors: `$to_f64` splits a chunk of
    /// `$vec` into its `f64` vectors in element order, and each of those
    /// updates one stretch of the column vectors.
    macro_rules! project_kernel {
        ($t:ty, $vec:ident, $project:ident, |$y:ident| $to_f64:expr) => {
            #[simd]
            pub(super) fn $project<S: Simd>(
                simd: S,
                row: &[$t],
                threshold: f64,
                col_sum: &mut [f64],
                col_thr: &mut [f64],
            ) -> [f64; 3] {
                let n = S::f64s::LEN;
                let thr = S::f64s::splat(simd, threshold);
                let zero = S::f64s::splat(simd, 0.0);
                let step = S::f64s::splat(simd, n as f64);
                let mut idx = zero;
                for (l, x) in idx.as_mut_slice().iter_mut().enumerate() {
                    *x = l as f64;
                }
                let (mut sum, mut thr_sum, mut m10) = (zero, zero, zero);
                let mut chunks = row.chunks_exact(S::$vec::LEN);
                let mut base = 0;
                for c in &mut chunks {
                    let $y = S::$vec::from_slice(simd, c);
                    for val in $to_f64 {
                        let masked = val.simd_ge(thr).select(val, zero);
                        let cs = &mut col_sum[base..base + n];
                        cs.copy_from_slice((S::f64s::from_slice(simd, cs) + val).as_slice());
                        let ct = &mut col_thr[base..base + n];
                        ct.copy_from_slice((S::f64s::from_slice(simd, ct) + masked).as_slice());
                        sum += val;
                        thr_sum += masked;
                        m10 = masked.mul_add(idx, m10);
                        idx += step;
                        base += n;
                    }
                }
                let mut out = [sum.reduce_sum(), thr_sum.reduce_sum(), m10.reduce_sum()];
                super::project_tail(
                    chunks.remainder(),
                    base,
                    threshold,
                    col_sum,
                    col_thr,
                    &mut out,
                );
                out
            }
        };
    }

    project_kernel!(f32, f32s, project_f32, |y| {
        let (a, b) = y.widen();
        [a, b]
    });
    project_kernel!(f64, f64s, project_f64, |y| [y]);
    project_kernel!(i64, i64s, project_i64, |y| [S::f64s::float_from(y)]);
    project_kernel!(u64, u64s, project_u64, |y| [S::f64s::float_from(y)]);

    /// [`super::formula_count_pass`] on vectors: C's bin arithmetic on `f64`
    /// lanes, each lane classified before the truncation so that it lands
    /// where [`super::slot`] puts it. `bin < 0` is `t <= -1`, `bin > last`
    /// is `t >= last + 1` (a saturated cast falls on the same side), and a
    /// NaN truncates to 0 as `as i64` does. The increments stay scalar; a
    /// histogram scatter has no vector form.
    macro_rules! hist_kernel {
        ($t:ty, $vec:ident, $hist:ident, |$y:ident| $to_f64:expr) => {
            #[simd]
            pub(super) fn $hist<S: Simd>(simd: S, v: &[$t], f: &Formula, slots: &mut [u64]) {
                let hist_size = slots.len() - 2;
                let min = S::f64s::splat(simd, f.hist_min);
                let max = S::f64s::splat(simd, f.hist_max);
                let scale = S::f64s::splat(simd, f.scale);
                let half = S::f64s::splat(simd, 0.5);
                let zero = S::f64s::splat(simd, 0.0);
                let neg_one = S::f64s::splat(simd, -1.0);
                let end = S::f64s::splat(simd, (f.last + 1) as f64);
                let below_slot = S::i64s::splat(simd, hist_size as i64);
                let above_slot = S::i64s::splat(simd, hist_size as i64 + 1);
                let mut chunks = v.chunks_exact(S::$vec::LEN);
                for c in &mut chunks {
                    let $y = S::$vec::from_slice(simd, c);
                    for val in $to_f64 {
                        let t = (val - min) * scale + half;
                        let below = t.simd_le(neg_one) | val.simd_lt(min);
                        let above = t.simd_ge(end) | val.simd_gt(max);
                        let bin = S::i64s::truncate_from(t.simd_eq(t).select(t, zero));
                        let slot = below.select(below_slot, above.select(above_slot, bin));
                        for &i in slot.as_slice() {
                            slots[i as usize] += 1;
                        }
                    }
                }
                super::formula_count_pass(chunks.remainder(), f, slots);
            }
        };
    }

    hist_kernel!(f32, f32s, hist_f32, |y| {
        let (a, b) = y.widen();
        [a, b]
    });
    hist_kernel!(f64, f64s, hist_f64, |y| [y]);
    hist_kernel!(i32, i32s, hist_i32, |y| {
        let (p0, p1) = y.widen();
        [S::f64s::float_from(p0), S::f64s::float_from(p1)]
    });
    hist_kernel!(u32, u32s, hist_u32, |y| {
        let (p0, p1) = y.widen();
        [S::f64s::float_from(p0), S::f64s::float_from(p1)]
    });

    /// One kernel pair per element type. `$vec` is the element's native
    /// vector, `$lanes` the accumulator vector its chunk sum widens into
    /// (`$widen` does the widening and adds the halves), `$flush` folds that
    /// accumulator into the `$acc` total without overflowing a 32-bit lane
    /// sum, and `$to_f64` splits a chunk into its `f64` vectors.
    macro_rules! int_kernels {
        ($t:ty, $vec:ident, $lanes:ident, $acc:ty, $range:ident, $variance:ident, $project:ident,
         |$x:ident| $widen:expr, |$a:ident| $flush:expr, |$y:ident| $to_f64:expr) => {
            project_kernel!($t, $vec, $project, |$y| $to_f64);

            #[simd]
            pub(super) fn $range<S: Simd>(simd: S, v: &[$t]) -> Range<$t> {
                let mut chunks = v.chunks_exact(S::$vec::LEN);
                let mut mins = S::$vec::splat(simd, v[0]);
                let mut maxs = S::$vec::splat(simd, v[0]);
                let mut total: $acc = 0;
                loop {
                    let mut lanes = S::$lanes::splat(simd, 0);
                    let mut seen = 0;
                    for c in chunks.by_ref().take(FLUSH) {
                        let $x = S::$vec::from_slice(simd, c);
                        mins = mins.min($x);
                        maxs = maxs.max($x);
                        lanes += $widen;
                        seen += 1;
                    }
                    let $a = lanes;
                    total += $flush;
                    if seen < FLUSH {
                        break;
                    }
                }
                let mut min = mins.reduce_min();
                let mut max = maxs.reduce_max();
                for &e in chunks.remainder() {
                    min = min.min(e);
                    max = max.max(e);
                    total += e as $acc;
                }
                Range {
                    min,
                    max,
                    total: total as f64,
                }
            }

            #[simd]
            pub(super) fn $variance<S: Simd>(simd: S, v: &[$t], mean: f64) -> f64 {
                let mut chunks = v.chunks_exact(S::$vec::LEN);
                let m = S::f64s::splat(simd, mean);
                let mut acc = S::f64s::splat(simd, 0.0);
                for c in &mut chunks {
                    let $y = S::$vec::from_slice(simd, c);
                    for f in $to_f64 {
                        let d = f - m;
                        acc = d.mul_add(d, acc);
                    }
                }
                let mut sum = acc.reduce_sum();
                for &e in chunks.remainder() {
                    let d = e as f64 - mean;
                    sum += d * d;
                }
                sum
            }
        };
    }

    int_kernels!(
        u8,
        u8s,
        u32s,
        u64,
        range_u8,
        variance_u8,
        project_u8,
        |x| {
            let (a, b) = x.widen();
            let (a0, a1) = a.widen();
            let (b0, b1) = b.widen();
            a0 + a1 + b0 + b1
        },
        |a| {
            let (lo, hi) = a.widen();
            lo.reduce_sum() + hi.reduce_sum()
        },
        |y| {
            let (a, b) = y.widen();
            let (a0, a1) = a.widen();
            let (b0, b1) = b.widen();
            let (p0, p1) = a0.widen();
            let (p2, p3) = a1.widen();
            let (p4, p5) = b0.widen();
            let (p6, p7) = b1.widen();
            [
                S::f64s::float_from(p0),
                S::f64s::float_from(p1),
                S::f64s::float_from(p2),
                S::f64s::float_from(p3),
                S::f64s::float_from(p4),
                S::f64s::float_from(p5),
                S::f64s::float_from(p6),
                S::f64s::float_from(p7),
            ]
        }
    );
    int_kernels!(
        i8,
        i8s,
        i32s,
        i64,
        range_i8,
        variance_i8,
        project_i8,
        |x| {
            let (a, b) = x.widen();
            let (a0, a1) = a.widen();
            let (b0, b1) = b.widen();
            a0 + a1 + b0 + b1
        },
        |a| {
            let (lo, hi) = a.widen();
            lo.reduce_sum() + hi.reduce_sum()
        },
        |y| {
            let (a, b) = y.widen();
            let (a0, a1) = a.widen();
            let (b0, b1) = b.widen();
            let (p0, p1) = a0.widen();
            let (p2, p3) = a1.widen();
            let (p4, p5) = b0.widen();
            let (p6, p7) = b1.widen();
            [
                S::f64s::float_from(p0),
                S::f64s::float_from(p1),
                S::f64s::float_from(p2),
                S::f64s::float_from(p3),
                S::f64s::float_from(p4),
                S::f64s::float_from(p5),
                S::f64s::float_from(p6),
                S::f64s::float_from(p7),
            ]
        }
    );
    int_kernels!(
        u16,
        u16s,
        u32s,
        u64,
        range_u16,
        variance_u16,
        project_u16,
        |x| {
            let (a, b) = x.widen();
            a + b
        },
        |a| {
            let (lo, hi) = a.widen();
            lo.reduce_sum() + hi.reduce_sum()
        },
        |y| {
            let (a, b) = y.widen();
            let (p0, p1) = a.widen();
            let (p2, p3) = b.widen();
            [
                S::f64s::float_from(p0),
                S::f64s::float_from(p1),
                S::f64s::float_from(p2),
                S::f64s::float_from(p3),
            ]
        }
    );
    int_kernels!(
        i16,
        i16s,
        i32s,
        i64,
        range_i16,
        variance_i16,
        project_i16,
        |x| {
            let (a, b) = x.widen();
            a + b
        },
        |a| {
            let (lo, hi) = a.widen();
            lo.reduce_sum() + hi.reduce_sum()
        },
        |y| {
            let (a, b) = y.widen();
            let (p0, p1) = a.widen();
            let (p2, p3) = b.widen();
            [
                S::f64s::float_from(p0),
                S::f64s::float_from(p1),
                S::f64s::float_from(p2),
                S::f64s::float_from(p3),
            ]
        }
    );
    int_kernels!(
        u32,
        u32s,
        u64s,
        u64,
        range_u32,
        variance_u32,
        project_u32,
        |x| {
            let (a, b) = x.widen();
            a + b
        },
        |a| a.reduce_sum(),
        |y| {
            let (p0, p1) = y.widen();
            [S::f64s::float_from(p0), S::f64s::float_from(p1)]
        }
    );
    int_kernels!(
        i32,
        i32s,
        i64s,
        i64,
        range_i32,
        variance_i32,
        project_i32,
        |x| {
            let (a, b) = x.widen();
            a + b
        },
        |a| a.reduce_sum(),
        |y| {
            let (p0, p1) = y.widen();
            [S::f64s::float_from(p0), S::f64s::float_from(p1)]
        }
    );

    /// [`super::range_pass`] for a 64-bit integer type: the extremes on the
    /// native lanes, the total as `f64` lane sums, the association the
    /// scalar pass uses too.
    macro_rules! wide_range_kernel {
        ($t:ty, $vec:ident, $name:ident) => {
            #[simd]
            pub(super) fn $name<S: Simd>(simd: S, v: &[$t]) -> Range<$t> {
                let mut chunks = v.chunks_exact(S::$vec::LEN);
                let mut mins = S::$vec::splat(simd, v[0]);
                let mut maxs = S::$vec::splat(simd, v[0]);
                let mut sums = S::f64s::splat(simd, 0.0);
                for c in &mut chunks {
                    let x = S::$vec::from_slice(simd, c);
                    mins = mins.min(x);
                    maxs = maxs.max(x);
                    sums += S::f64s::float_from(x);
                }
                let mut min = mins.reduce_min();
                let mut max = maxs.reduce_max();
                let mut total = sums.reduce_sum();
                for &e in chunks.remainder() {
                    min = min.min(e);
                    max = max.max(e);
                    total += e as f64;
                }
                Range { min, max, total }
            }
        };
    }

    wide_range_kernel!(i64, i64s, range_i64);
    wide_range_kernel!(u64, u64s, range_u64);

    /// [`super::range_pass`] for a float type: strict `<`/`>` compares with a
    /// select, so a NaN never wins a lane and the NaN a lane holds from
    /// `v[0]` is never displaced; the lane fold and the tail keep the same
    /// rule through [`StatsElem::lower`]/[`upper`]. `$to_f64` splits a chunk
    /// into its `f64` vectors for the sum.
    macro_rules! float_range_kernel {
        ($t:ty, $vec:ident, $name:ident, |$y:ident| $to_f64:expr) => {
            #[simd]
            pub(super) fn $name<S: Simd>(simd: S, v: &[$t]) -> Range<$t> {
                let mut chunks = v.chunks_exact(S::$vec::LEN);
                let mut mins = S::$vec::splat(simd, v[0]);
                let mut maxs = S::$vec::splat(simd, v[0]);
                let mut sums = S::f64s::splat(simd, 0.0);
                for c in &mut chunks {
                    let $y = S::$vec::from_slice(simd, c);
                    mins = $y.simd_lt(mins).select($y, mins);
                    maxs = $y.simd_gt(maxs).select($y, maxs);
                    for f in $to_f64 {
                        sums += f;
                    }
                }
                let mut min = v[0];
                let mut max = v[0];
                for (&lo, &hi) in mins.as_slice().iter().zip(maxs.as_slice()) {
                    min = <$t as StatsElem>::lower(min, lo);
                    max = <$t as StatsElem>::upper(max, hi);
                }
                let mut total = sums.reduce_sum();
                for &e in chunks.remainder() {
                    min = <$t as StatsElem>::lower(min, e);
                    max = <$t as StatsElem>::upper(max, e);
                    total += e as f64;
                }
                Range { min, max, total }
            }
        };
    }

    float_range_kernel!(f32, f32s, range_f32, |y| {
        let (a, b) = y.widen();
        [a, b]
    });
    float_range_kernel!(f64, f64s, range_f64, |y| [y]);
}

/// Merge two partial ranges; ties keep `a`, the earlier slice.
pub(crate) fn merge_range<T: StatsElem>(a: Range<T>, b: Range<T>) -> Range<T> {
    Range {
        min: T::lower(a.min, b.min),
        max: T::upper(a.max, b.max),
        total: a.total + b.total,
    }
}

fn stats_of<T: StatsElem + Send + Sync>(
    v: &[T],
    dims: &[ad_core_rs::ndarray::NDDimension],
    bgd_width: usize,
) -> StatsResult {
    if v.is_empty() {
        return StatsResult::default();
    }

    let n = v.len() as f64;
    let (range, variance);
    #[cfg(feature = "parallel")]
    if par_util::should_parallelize(v.len()) {
        range = par_util::thread_pool().install(|| {
            v.par_chunks(PAR_CHUNK)
                .map(T::range)
                .reduce_with(merge_range)
                .expect("a non-empty slice has at least one chunk")
        });
        let mean = range.total / n;
        variance = par_util::thread_pool().install(|| {
            v.par_chunks(PAR_CHUNK)
                .map(|c| T::variance(c, mean))
                .sum::<f64>()
        });
    } else {
        range = T::range(v);
        variance = T::variance(v, range.total / n);
    }
    #[cfg(not(feature = "parallel"))]
    {
        range = T::range(v);
        variance = T::variance(v, range.total / n);
    }

    let total = range.total;
    let mean = total / n;
    let sigma = (variance / n).sqrt();
    let min_idx = first_index(v, range.min);
    let max_idx = first_index(v, range.max);
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
                    sum += v[flat].to_f64();
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
        min: range.min.to_f64(),
        max: range.max.to_f64(),
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
}

/// The column and row sums of a 2-D frame, taken in one pass: what both
/// [`compute_centroid`] and [`compute_profiles`] are read off. C takes the
/// same pass in `doComputeCentroidT` (NDPluginStats.cpp:207-218), filling the
/// average and threshold profiles and then forming the moments from those
/// 1-D vectors, and its `doComputeProfilesT` only extracts rows and columns.
///
/// Every sum is per column (`col_*`, `x_size` long) or per row (`row_*`,
/// `y_size` long). `*_thr` covers the pixels at or above the threshold —
/// C's `value >= centroidThreshold` (NDPluginStats.cpp:212), so a NaN is
/// never one of them. `row_m10` is Σ value·ix over a row's threshold
/// pixels: the one moment (`mu11`) that needs both coordinates at once, kept
/// per row so it can be centred once the centroid is known.
struct Projection {
    col_sum: Vec<f64>,
    col_thr: Vec<f64>,
    row_sum: Vec<f64>,
    row_thr: Vec<f64>,
    row_m10: Vec<f64>,
}

impl Projection {
    fn zeroed(x_size: usize, y_size: usize) -> Self {
        Self {
            col_sum: vec![0.0; x_size],
            col_thr: vec![0.0; x_size],
            row_sum: vec![0.0; y_size],
            row_thr: vec![0.0; y_size],
            row_m10: vec![0.0; y_size],
        }
    }

    /// Fold the band that follows this one in the frame into it.
    #[cfg(feature = "parallel")]
    fn append(mut self, next: Self) -> Self {
        for (a, b) in self.col_sum.iter_mut().zip(&next.col_sum) {
            *a += b;
        }
        for (a, b) in self.col_thr.iter_mut().zip(&next.col_thr) {
            *a += b;
        }
        self.row_sum.extend(next.row_sum);
        self.row_thr.extend(next.row_thr);
        self.row_m10.extend(next.row_m10);
        self
    }
}

/// Project a band of whole rows (`v.len()` a multiple of `x_size`).
fn project_band<T: StatsElem>(v: &[T], x_size: usize, threshold: f64) -> Projection {
    let mut p = Projection::zeroed(x_size, v.len() / x_size);
    for (iy, row) in v.chunks_exact(x_size).enumerate() {
        let [sum, thr, m10] = T::project_row(row, threshold, &mut p.col_sum, &mut p.col_thr);
        p.row_sum[iy] = sum;
        p.row_thr[iy] = thr;
        p.row_m10[iy] = m10;
    }
    p
}

/// [`StatsElem::project_row`] as a lane loop: the row is walked `LANES`
/// columns at a time so that the column vectors are updated as vectors and
/// the row's own sums are kept in per-lane accumulators, folded once per
/// row; the threshold test is a select, not a branch, so the whole body
/// vectorizes.
fn project_row_pass<T: StatsElem>(
    row: &[T],
    threshold: f64,
    col_sum: &mut [f64],
    col_thr: &mut [f64],
) -> [f64; 3] {
    let mut sum = [0.0f64; LANES];
    let mut thr = [0.0f64; LANES];
    let mut m10 = [0.0f64; LANES];
    let mut cols = row.chunks_exact(LANES);
    let mut base = 0;
    // The column index as a vector of f64, stepped by LANES per chunk:
    // converting `base + l` in the loop would need a packed usize->f64,
    // which the baseline target lacks, and the loop would scalarize.
    let mut idx = [0.0f64; LANES];
    for (l, x) in idx.iter_mut().enumerate() {
        *x = l as f64;
    }
    for c in cols.by_ref() {
        let c: &[T; LANES] = c.try_into().expect("chunks_exact yields LANES elements");
        let cs: &mut [f64; LANES] = (&mut col_sum[base..base + LANES])
            .try_into()
            .expect("LANES columns");
        let ct: &mut [f64; LANES] = (&mut col_thr[base..base + LANES])
            .try_into()
            .expect("LANES columns");
        for l in 0..LANES {
            let val = c[l].to_f64();
            let masked = if val >= threshold { val } else { 0.0 };
            cs[l] += val;
            ct[l] += masked;
            sum[l] += val;
            thr[l] += masked;
            m10[l] += masked * idx[l];
        }
        for x in idx.iter_mut() {
            *x += LANES as f64;
        }
        base += LANES;
    }
    let mut out = [sum.iter().sum(), thr.iter().sum(), m10.iter().sum()];
    project_tail(
        cols.remainder(),
        base,
        threshold,
        col_sum,
        col_thr,
        &mut out,
    );
    out
}

/// The columns from `base` on that no vector covered, one at a time.
fn project_tail<T: StatsElem>(
    rest: &[T],
    base: usize,
    threshold: f64,
    col_sum: &mut [f64],
    col_thr: &mut [f64],
    [sum, thr, m10]: &mut [f64; 3],
) {
    for (l, &e) in rest.iter().enumerate() {
        let ix = base + l;
        let val = e.to_f64();
        let masked = if val >= threshold { val } else { 0.0 };
        col_sum[ix] += val;
        col_thr[ix] += masked;
        *sum += val;
        *thr += masked;
        *m10 += masked * ix as f64;
    }
}

/// Project the first `x_size * y_size` elements of `v` as a 2-D frame, in
/// bands of whole rows across the pool when the frame is large enough.
fn project_of<T: StatsElem + Sync>(
    v: &[T],
    x_size: usize,
    y_size: usize,
    threshold: f64,
) -> Projection {
    let v = &v[..x_size * y_size];
    #[cfg(feature = "parallel")]
    if par_util::should_parallelize(v.len()) {
        let band = (PAR_CHUNK / x_size).max(1) * x_size;
        return par_util::thread_pool().install(|| {
            v.par_chunks(band)
                .map(|band| project_band(band, x_size, threshold))
                .collect::<Vec<_>>()
                .into_iter()
                .reduce(Projection::append)
                .unwrap_or_else(|| Projection::zeroed(x_size, y_size))
        });
    }
    project_band(v, x_size, threshold)
}

/// One projection of the frame, or `None` when it is not a full 2-D image.
fn project(
    data: &NDDataBuffer,
    x_size: usize,
    y_size: usize,
    threshold: f64,
) -> Option<Projection> {
    let n = x_size * y_size;
    if n == 0 || data.len() < n {
        return None;
    }
    Some(ad_core_rs::with_buffer!(data, |v| project_of(
        v, x_size, y_size, threshold
    )))
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
    match project(data, x_size, y_size, threshold) {
        Some(p) => centroid_from(&p),
        None => CentroidResult::default(),
    }
}

/// The moments, from the projection. The central moments of each axis are
/// sums over that axis's threshold profile — `dx` depends on the column
/// alone, so Σ value·dx^k over the pixels is Σ `col_thr[ix]`·dx^k — which is
/// the direct central form, not C's raw-moment expansion (NDPluginStats.cpp:
/// 245-252) with its cancellation at the fourth order.
fn centroid_from(p: &Projection) -> CentroidResult {
    let m00: f64 = p.col_thr.iter().sum();
    if m00 == 0.0 {
        return CentroidResult::default();
    }
    let m10: f64 = p
        .col_thr
        .iter()
        .enumerate()
        .map(|(ix, &t)| t * ix as f64)
        .sum();
    let m01: f64 = p
        .row_thr
        .iter()
        .enumerate()
        .map(|(iy, &t)| t * iy as f64)
        .sum();
    let cx = m10 / m00;
    let cy = m01 / m00;

    let (mut mu20, mut m30_central, mut m40_central) = (0.0f64, 0.0f64, 0.0f64);
    for (ix, &t) in p.col_thr.iter().enumerate() {
        let dx = ix as f64 - cx;
        let dx2 = dx * dx;
        mu20 += t * dx2;
        m30_central += t * dx2 * dx;
        m40_central += t * dx2 * dx2;
    }
    let (mut mu02, mut m03_central, mut m04_central, mut mu11) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for (iy, (&t, &m10_row)) in p.row_thr.iter().zip(&p.row_m10).enumerate() {
        let dy = iy as f64 - cy;
        let dy2 = dy * dy;
        mu02 += t * dy2;
        m03_central += t * dy2 * dy;
        m04_central += t * dy2 * dy2;
        // Σ value·dx over the row is Σ value·ix − cx·Σ value.
        mu11 += dy * (m10_row - cx * t);
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
/// Returns (histogram, below_count, above_count, entropy), binned as C
/// `doComputeHistogramT` (NDPluginStats.cpp:42-56):
/// - `hist_size`: number of bins
/// - `hist_min` / `hist_max`: value range for binning
/// - `scale = (hist_size - 1) / (hist_max - hist_min)`,
///   `bin = ((val - hist_min) * scale + 0.5) as i64`
/// - `bin < 0` or `val < hist_min` counts in `below`; `bin > hist_size - 1`
///   or `val > hist_max` in `above`
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

    let counts = ad_core_rs::with_buffer!(data, |v| histogram_of(v, hist_size, hist_min, hist_max));
    let histogram: Vec<f64> = counts.bins().iter().map(|&c| c as f64).collect();

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

    (
        histogram,
        counts.below() as f64,
        counts.above() as f64,
        entropy,
    )
}

/// Slot counts of one histogram pass, or of several merged: the bins, then
/// the count below the range and the count above it.
struct HistCounts {
    slots: Vec<u64>,
}

impl HistCounts {
    fn zeroed(hist_size: usize) -> Self {
        Self {
            slots: vec![0; hist_size + 2],
        }
    }

    fn bins(&self) -> &[u64] {
        &self.slots[..self.slots.len() - 2]
    }

    fn below(&self) -> u64 {
        self.slots[self.slots.len() - 2]
    }

    fn above(&self) -> u64 {
        self.slots[self.slots.len() - 1]
    }

    #[cfg(feature = "parallel")]
    fn merge(mut self, other: Self) -> Self {
        for (a, b) in self.slots.iter_mut().zip(&other.slots) {
            *a += b;
        }
        self
    }
}

/// How the values of `T` map onto the slots of one histogram: through the
/// bin formula per element, or, for a type with few enough values, through
/// a table of every value's slot.
enum SlotMap {
    Formula(Formula),
    Table(Vec<u32>),
}

/// The bin formula's constants for one histogram.
#[derive(Clone, Copy)]
pub(crate) struct Formula {
    pub(crate) hist_min: f64,
    pub(crate) hist_max: f64,
    pub(crate) scale: f64,
    pub(crate) last: i64,
}

/// One pass of the formula path: `slots` holds the bins, then the below
/// slot, then the above slot.
fn formula_count_pass<T: StatsElem>(v: &[T], f: &Formula, slots: &mut [u64]) {
    // The two out-of-range counts stay in registers: routing them through
    // the slot vector like the bins costs a fifth of the pass.
    let (mut below, mut above) = (0u64, 0u64);
    for &e in v {
        match slot(e.to_f64(), f.hist_min, f.hist_max, f.scale, f.last) {
            Slot::Below => below += 1,
            Slot::Above => above += 1,
            Slot::Bin(bin) => slots[bin] += 1,
        }
    }
    let n = slots.len();
    slots[n - 2] += below;
    slots[n - 1] += above;
}

impl SlotMap {
    fn new<T: StatsElem>(hist_size: usize, hist_min: f64, hist_max: f64) -> Self {
        let last = hist_size as i64 - 1;
        let scale = last as f64 / (hist_max - hist_min);
        match T::TABLE_LEN {
            Some(len) => Self::Table(
                (0..len)
                    .map(
                        |i| match slot(T::table_value(i), hist_min, hist_max, scale, last) {
                            Slot::Below => hist_size as u32,
                            Slot::Above => hist_size as u32 + 1,
                            Slot::Bin(bin) => bin as u32,
                        },
                    )
                    .collect(),
            ),
            None => Self::Formula(Formula {
                hist_min,
                hist_max,
                scale,
                last,
            }),
        }
    }

    /// Count `v` into `acc`.
    fn count<T: StatsElem>(&self, acc: &mut HistCounts, v: &[T]) {
        match self {
            Self::Formula(f) => T::formula_count(v, f, &mut acc.slots),
            Self::Table(table) => {
                for &e in v {
                    acc.slots[table[e.table_index()] as usize] += 1;
                }
            }
        }
    }
}

/// Where one value counts.
enum Slot {
    Below,
    Above,
    Bin(usize),
}

/// The slot of `value`, its bin formed exactly as C `doComputeHistogramT`
/// forms it (NDPluginStats.cpp:46-54), so the two agree bin for bin,
/// including which side of an edge a value falls on.
#[inline(always)]
fn slot(value: f64, hist_min: f64, hist_max: f64, scale: f64, last: i64) -> Slot {
    let bin = ((value - hist_min) * scale + 0.5) as i64;
    if bin < 0 || value < hist_min {
        Slot::Below
    } else if bin > last || value > hist_max {
        Slot::Above
    } else {
        Slot::Bin(bin as usize)
    }
}

/// The histogram of `v`, counted across the pool when it is large enough:
/// one set of slots per worker, merged at the end.
fn histogram_of<T: StatsElem + Sync>(
    v: &[T],
    hist_size: usize,
    hist_min: f64,
    hist_max: f64,
) -> HistCounts {
    let map = SlotMap::new::<T>(hist_size, hist_min, hist_max);
    #[cfg(feature = "parallel")]
    if par_util::should_parallelize(v.len()) {
        return par_util::thread_pool().install(|| {
            v.par_chunks(PAR_CHUNK)
                .fold(
                    || HistCounts::zeroed(hist_size),
                    |mut acc, chunk| {
                        map.count(&mut acc, chunk);
                        acc
                    },
                )
                .reduce(|| HistCounts::zeroed(hist_size), HistCounts::merge)
        });
    }
    let mut acc = HistCounts::zeroed(hist_size);
    map.count(&mut acc, v);
    acc
}

/// Compute profile projections for a 2D image.
///
/// - Average X/Y: column/row averages over the full image
/// - Threshold X/Y: column/row averages with the pixels under the
///   threshold taken as zero
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
    match project(data, x_size, y_size, threshold) {
        Some(p) => profiles_from(&p, data, centroid_x, centroid_y, cursor_x, cursor_y),
        None => ProfileResult::default(),
    }
}

/// The eight profiles: the four averages from the projection, the four
/// single rows and columns read from the frame.
fn profiles_from(
    p: &Projection,
    data: &NDDataBuffer,
    centroid_x: f64,
    centroid_y: f64,
    cursor_x: usize,
    cursor_y: usize,
) -> ProfileResult {
    let x_size = p.col_sum.len();
    let y_size = p.row_sum.len();

    // Both the average and the threshold profile of an axis are divided by
    // the other axis's length (NDPluginStats.cpp:230,240): the threshold
    // profile is the per-pixel mean over the whole column or row, with the
    // pixels under the threshold counted as zero.
    let per_row = |s: &f64| s / y_size as f64;
    let per_col = |s: &f64| s / x_size as f64;
    let avg_x: Vec<f64> = p.col_sum.iter().map(per_row).collect();
    let avg_y: Vec<f64> = p.row_sum.iter().map(per_col).collect();
    let threshold_x: Vec<f64> = p.col_thr.iter().map(per_row).collect();
    let threshold_y: Vec<f64> = p.row_thr.iter().map(per_col).collect();

    // Centroid/cursor profiles: extract a single row/column at the requested
    // position. C clamps the index to the valid range (NDPluginStats.cpp:341-360,
    // `MAX(.,0)` then `MIN(.,size-1)`): an out-of-range centroid or user cursor
    // collapses to the edge row/column, never a zero-filled profile. (x_size and
    // y_size are both > 0 here — `project` yields nothing for a zero dimension.)
    let cy_row = ((centroid_y + 0.5).max(0.0) as usize).min(y_size - 1);
    let cx_col = ((centroid_x + 0.5).max(0.0) as usize).min(x_size - 1);
    let cur_y = cursor_y.min(y_size - 1);
    let cur_x = cursor_x.min(x_size - 1);

    let row = |iy: usize| -> Vec<f64> {
        ad_core_rs::with_buffer!(data, |v| v[iy * x_size..(iy + 1) * x_size]
            .iter()
            .map(|&e| StatsElem::to_f64(e))
            .collect())
    };
    let col = |ix: usize| -> Vec<f64> {
        ad_core_rs::with_buffer!(data, |v| v[..x_size * y_size]
            .iter()
            .skip(ix)
            .step_by(x_size)
            .map(|&e| StatsElem::to_f64(e))
            .collect())
    };

    ProfileResult {
        avg_x,
        avg_y,
        threshold_x,
        threshold_y,
        centroid_x: row(cy_row),
        centroid_y: col(cx_col),
        cursor_x: row(cur_y),
        cursor_y: col(cur_x),
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
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        let p = &self.params;
        let info = array.info();
        let cfg = *self.config.lock();

        let mut result = if cfg.do_compute_statistics {
            compute_stats(&array.data, &array.dims, cfg.bgd_width)
        } else {
            StatsResult::default()
        };

        // Centroid and profiles share one projection of the frame (C fills
        // the profile vectors inside `doComputeCentroidT` and reads the
        // moments off them, NDPluginStats.cpp:207-240). C rejects ndims>2 for
        // both (NDPluginStats.cpp:205 and :338, `if (ndims>2) return
        // asynError`): they are computed only for a true 2-D image, never by
        // treating the first two dims of a 4-D (or [x,y,1]) array as a slice.
        let mut centroid = CentroidResult::default();
        let two_d = info.color_size <= 1 && array.dims.len() == 2;
        if two_d && (cfg.do_compute_centroid || cfg.do_compute_profiles) {
            if let Some(p) = project(
                &array.data,
                info.x_size,
                info.y_size,
                cfg.centroid_threshold,
            ) {
                if cfg.do_compute_centroid {
                    centroid = centroid_from(&p);
                }
                if cfg.do_compute_profiles {
                    let profiles = profiles_from(
                        &p,
                        &array.data,
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
        ProcessResult::forward(array, updates)
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

        processor.process_array(&Arc::new(arr), &NDArrayPool::new(1_000_000));

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

    /// The plain serial definition every kernel must agree with, at a
    /// tolerance: C `doComputeStatisticsT` (NDPluginStats.cpp:121-137) for
    /// min/max/positions/total, the two-pass sigma this plugin has always
    /// computed.
    fn reference_stats<T: super::StatsElem>(v: &[T]) -> (f64, f64, usize, usize, f64, f64) {
        let (mut mn, mut mx, mut imin, mut imax, mut total) = (v[0], v[0], 0usize, 0usize, 0.0f64);
        for (i, &e) in v.iter().enumerate() {
            if e < mn {
                mn = e;
                imin = i;
            }
            if e > mx {
                mx = e;
                imax = i;
            }
            total += e.to_f64();
        }
        let mean = total / v.len() as f64;
        let var: f64 = v.iter().map(|&e| (e.to_f64() - mean).powi(2)).sum();
        (
            mn.to_f64(),
            mx.to_f64(),
            imin,
            imax,
            total,
            (var / v.len() as f64).sqrt(),
        )
    }

    fn assert_close(what: &str, got: f64, want: f64, rel: f64) {
        let tol = rel * want.abs().max(1.0);
        assert!(
            (got - want).abs() <= tol,
            "{what}: got {got}, want {want} (tol {tol})"
        );
    }

    /// Lengths that leave every remainder the lane count can leave, straddle
    /// the parallel threshold, sit on both sides of a lane flush
    /// (`FLUSH * LANES` elements), and end inside a parallel chunk.
    const KERNEL_LENGTHS: &[usize] = &[
        1,
        2,
        7,
        8,
        9,
        15,
        4095,
        4096,
        4097,
        FLUSH * LANES - 1,
        FLUSH * LANES,
        FLUSH * LANES + 1,
        70_001,
        2 * FLUSH * LANES + 3,
    ];

    /// The wide and float range kernels against the lane pass on every
    /// level the box offers, with NaNs where the strict-compare rule shows:
    /// in the first slot, mid-vector, and in the tail.
    #[cfg(feature = "simd")]
    #[test]
    fn wide_and_float_range_kernels_match_range_pass_on_every_level() {
        use fearless_simd::{Level, dispatch};
        let top = ad_core_rs::simd::level();
        let mut levels = vec![top, Level::baseline()];
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            levels.extend(top.as_avx2().map(Level::Avx2));
            levels.extend(top.as_sse4_2().map(Level::Sse4_2));
            levels.extend(top.as_sse2().map(Level::Sse2));
        }
        fn same<T: StatsElem + std::fmt::Debug>(what: &str, got: Range<T>, want: Range<T>) {
            let bits = |x: T| format!("{x:?}");
            assert_eq!(bits(got.min), bits(want.min), "{what} min");
            assert_eq!(bits(got.max), bits(want.max), "{what} max");
            if got.total.is_nan() && want.total.is_nan() {
                return;
            }
            assert_close(&format!("{what} total"), got.total, want.total, 1e-12);
        }
        for &n in KERNEL_LENGTHS {
            let ints: Vec<i64> = (0..n).map(|i| seq(i) as i64 - 500_000).collect();
            let uints: Vec<u64> = (0..n).map(|i| seq(i) as u64 * 3).collect();
            let mut floats: Vec<Vec<f64>> =
                vec![(0..n).map(|i| seq(i) as f64 * 0.37 - 100.0).collect()];
            for at in [0, n / 2, n - 1] {
                let mut v = floats[0].clone();
                v[at] = f64::NAN;
                floats.push(v);
            }
            for &level in &levels {
                let what = format!("{level:?} n={n}");
                same(
                    &format!("{what} i64"),
                    dispatch!(level, s => simd_kernels::range_i64(s, &ints)),
                    range_pass(&ints),
                );
                same(
                    &format!("{what} u64"),
                    dispatch!(level, s => simd_kernels::range_u64(s, &uints)),
                    range_pass(&uints),
                );
                for (k, v) in floats.iter().enumerate() {
                    same(
                        &format!("{what} f64 case {k}"),
                        dispatch!(level, s => simd_kernels::range_f64(s, v)),
                        range_pass(v),
                    );
                    let v32: Vec<f32> = v.iter().map(|&x| x as f32).collect();
                    same(
                        &format!("{what} f32 case {k}"),
                        dispatch!(level, s => simd_kernels::range_f32(s, &v32)),
                        range_pass(&v32),
                    );
                }
            }
        }
    }

    fn check_kernel_against_reference<T>(
        name: &str,
        make: impl Fn(usize) -> T,
        to_buf: impl Fn(Vec<T>) -> NDDataBuffer,
        rel: f64,
    ) where
        T: super::StatsElem,
    {
        for &n in KERNEL_LENGTHS {
            let v: Vec<T> = (0..n).map(&make).collect();
            let (mn, mx, imin, imax, total, sigma) = reference_stats(&v);
            let dims = vec![NDDimension::new(n)];
            let s = compute_stats(&to_buf(v), &dims, 0);
            let what = format!("{name} n={n}");
            assert_eq!(s.min, mn, "{what} min");
            assert_eq!(s.max, mx, "{what} max");
            assert_eq!(s.min_x, imin, "{what} min index");
            assert_eq!(s.max_x, imax, "{what} max index");
            assert_close(&format!("{what} total"), s.total, total, rel);
            assert_close(&format!("{what} sigma"), s.sigma, sigma, rel);
            assert_close(&format!("{what} mean"), s.mean, total / n as f64, rel);
        }
    }

    /// A pseudo-random sequence with repeated extremes so the first-occurrence
    /// rule is exercised, scaled into each element type's range.
    fn seq(i: usize) -> u32 {
        (i as u32).wrapping_mul(2_654_435_761) >> 12
    }

    #[test]
    fn kernel_matches_reference_for_every_element_type() {
        check_kernel_against_reference(
            "i8",
            |i| (seq(i) % 256) as u8 as i8,
            NDDataBuffer::I8,
            1e-12,
        );
        check_kernel_against_reference("u8", |i| (seq(i) % 256) as u8, NDDataBuffer::U8, 1e-12);
        check_kernel_against_reference(
            "i16",
            |i| (seq(i) % 65536) as u16 as i16,
            NDDataBuffer::I16,
            1e-12,
        );
        check_kernel_against_reference(
            "u16",
            |i| (seq(i) % 65536) as u16,
            NDDataBuffer::U16,
            1e-12,
        );
        check_kernel_against_reference(
            "i32",
            |i| seq(i) as i32 - 500_000,
            NDDataBuffer::I32,
            1e-12,
        );
        check_kernel_against_reference("u32", seq, NDDataBuffer::U32, 1e-12);
        check_kernel_against_reference(
            "i64",
            |i| seq(i) as i64 - 500_000,
            NDDataBuffer::I64,
            1e-12,
        );
        check_kernel_against_reference("u64", |i| seq(i) as u64, NDDataBuffer::U64, 1e-12);
        check_kernel_against_reference(
            "f32",
            |i| seq(i) as f32 * 0.37 - 100.0,
            NDDataBuffer::F32,
            1e-9,
        );
        check_kernel_against_reference(
            "f64",
            |i| seq(i) as f64 * 0.37 - 100.0,
            NDDataBuffer::F64,
            1e-12,
        );
    }

    /// The position of a repeated extreme is its FIRST occurrence — C's
    /// `imin`/`imax` move only on a strictly smaller/larger value — including
    /// when the repeat sits in a later parallel chunk.
    #[test]
    fn extreme_positions_are_first_occurrences_across_chunks() {
        let n = 3 * PAR_CHUNK_OR_LARGE + 11;
        let mut v = vec![100u16; n];
        v[5] = 1;
        v[n - 1] = 1;
        v[9] = 60000;
        v[2 * PAR_CHUNK_OR_LARGE + 7] = 60000;
        let dims = vec![NDDimension::new(n)];
        let s = compute_stats(&NDDataBuffer::U16(v), &dims, 0);
        assert_eq!((s.min, s.min_x), (1.0, 5));
        assert_eq!((s.max, s.max_x), (60000.0, 9));
    }

    #[cfg(feature = "parallel")]
    const PAR_CHUNK_OR_LARGE: usize = super::PAR_CHUNK;
    #[cfg(not(feature = "parallel"))]
    const PAR_CHUNK_OR_LARGE: usize = 1 << 16;

    /// A NaN is never an extreme (strict compare), and a NaN first element is
    /// never displaced — both as C.
    #[test]
    fn nan_follows_the_strict_compare_rule() {
        let dims = vec![NDDimension::new(4)];
        let s = compute_stats(&NDDataBuffer::F64(vec![3.0, f64::NAN, 1.0, 5.0]), &dims, 0);
        assert_eq!((s.min, s.min_x, s.max, s.max_x), (1.0, 2, 5.0, 3));
        let s = compute_stats(&NDDataBuffer::F64(vec![f64::NAN, 1.0, 5.0]), &dims, 0);
        assert!(s.min.is_nan() && s.max.is_nan());
        assert_eq!((s.min_x, s.max_x), (0, 0));
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

        // Threshold X profile: only column 2 has a pixel >= 5.0 (at row 1);
        // its sum is divided by the 4 rows (NDPluginStats.cpp:230).
        assert_eq!(profiles.threshold_x.len(), 4);
        assert!((profiles.threshold_x[2] - 2.5).abs() < 1e-10);
        // Other columns: no pixels above threshold
        assert!((profiles.threshold_x[0] - 0.0).abs() < 1e-10);
        assert!((profiles.threshold_x[1] - 0.0).abs() < 1e-10);
        assert!((profiles.threshold_x[3] - 0.0).abs() < 1e-10);

        // Threshold Y profile: only row 1 has a pixel >= 5.0
        assert_eq!(profiles.threshold_y.len(), 4);
        assert!((profiles.threshold_y[1] - 2.5).abs() < 1e-10);
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

        let arr = Arc::new(arr);
        let result = proc.process_array(&arr, &pool);
        // C++ Stats forwards the input array to downstream plugins
        assert!(
            Arc::ptr_eq(&result.output_arrays[0], &arr),
            "stats forwards the array"
        );
        assert_eq!(result.output_arrays.len(), 1);

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

        let result = proc.process_array(&Arc::new(arr), &pool);
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

        let result = proc.process_array(&Arc::new(arr), &pool);
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

        let result = proc.process_array(&Arc::new(arr), &pool);
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

        let result = proc.process_array(&Arc::new(arr), &pool);
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
        let pool = NDArrayPool::new(1_000_000);
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

    /// The centroid as the per-pixel loops computed it before the projection:
    /// two passes over every pixel, central moments accumulated directly.
    fn reference_centroid(vals: &[f64], w: usize, h: usize, thr: f64) -> CentroidResult {
        let (mut m00, mut m10, mut m01) = (0.0, 0.0, 0.0);
        for iy in 0..h {
            for ix in 0..w {
                let val = vals[iy * w + ix];
                if val >= thr {
                    m00 += val;
                    m10 += val * ix as f64;
                    m01 += val * iy as f64;
                }
            }
        }
        if m00 == 0.0 {
            return CentroidResult::default();
        }
        let (cx, cy) = (m10 / m00, m01 / m00);
        let mut m = [0.0f64; 7];
        for iy in 0..h {
            for ix in 0..w {
                let val = vals[iy * w + ix];
                if val < thr {
                    continue;
                }
                let dx = ix as f64 - cx;
                let dy = iy as f64 - cy;
                m[0] += val * dx * dx;
                m[1] += val * dy * dy;
                m[2] += val * dx * dy;
                m[3] += val * dx * dx * dx;
                m[4] += val * dy * dy * dy;
                m[5] += val * dx * dx * dx * dx;
                m[6] += val * dy * dy * dy * dy;
            }
        }
        let [mu20, mu02, mu11, mu30, mu03, mu40, mu04] = m;
        let sigma_x = (mu20 / m00).sqrt();
        let sigma_y = (mu02 / m00).sqrt();
        let sigma_xy = if sigma_x > 0.0 && sigma_y > 0.0 {
            (mu11 / m00) / (sigma_x * sigma_y)
        } else {
            0.0
        };
        let denom = mu20 + mu02;
        CentroidResult {
            centroid_x: cx,
            centroid_y: cy,
            sigma_x,
            sigma_y,
            sigma_xy,
            centroid_total: m00,
            skewness_x: if sigma_x > 0.0 {
                mu30 / (m00 * sigma_x.powi(3))
            } else {
                0.0
            },
            skewness_y: if sigma_y > 0.0 {
                mu03 / (m00 * sigma_y.powi(3))
            } else {
                0.0
            },
            kurtosis_x: if sigma_x > 0.0 {
                mu40 / (m00 * sigma_x.powi(4)) - 3.0
            } else {
                0.0
            },
            kurtosis_y: if sigma_y > 0.0 {
                mu04 / (m00 * sigma_y.powi(4)) - 3.0
            } else {
                0.0
            },
            eccentricity: if denom > 0.0 {
                ((mu20 - mu02).powi(2) - 4.0 * mu11.powi(2)) / denom.powi(2)
            } else {
                0.0
            },
            orientation: 0.5 * (2.0 * mu11).atan2(mu20 - mu02) * 180.0 / std::f64::consts::PI,
        }
    }

    /// The four average profiles as per-pixel loops compute them.
    fn reference_profiles(vals: &[f64], w: usize, h: usize, thr: f64) -> [Vec<f64>; 4] {
        let (mut ax, mut ay) = (vec![0.0; w], vec![0.0; h]);
        let (mut tx, mut ty) = (vec![0.0; w], vec![0.0; h]);
        for iy in 0..h {
            for ix in 0..w {
                let val = vals[iy * w + ix];
                ax[ix] += val;
                ay[iy] += val;
                if val >= thr {
                    tx[ix] += val;
                    ty[iy] += val;
                }
            }
        }
        for a in ax.iter_mut().chain(tx.iter_mut()) {
            *a /= h as f64;
        }
        for a in ay.iter_mut().chain(ty.iter_mut()) {
            *a /= w as f64;
        }
        [ax, ay, tx, ty]
    }

    /// Frame shapes: widths on both sides of a LANES boundary, a single row
    /// and column, and frames over the parallel threshold whose band count
    /// leaves a partial last band (300x700 -> bands of 218 rows).
    const PROJECTION_SHAPES: &[(usize, usize)] = &[
        (1, 1),
        (1, 7),
        (7, 1),
        (15, 3),
        (16, 4),
        (17, 5),
        (33, 9),
        (100, 50),
        (2048, 3),
        (300, 700),
        (5000, 40),
    ];

    fn check_projection_against_reference<T: Copy>(
        name: &str,
        make: impl Fn(usize) -> T,
        wrap: fn(Vec<T>) -> NDDataBuffer,
        to_f64: fn(T) -> f64,
        rel: f64,
    ) {
        for &(w, h) in PROJECTION_SHAPES {
            let raw: Vec<T> = (0..w * h).map(&make).collect();
            let vals: Vec<f64> = raw.iter().map(|&e| to_f64(e)).collect();
            let mut sorted = vals.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let thr = sorted[sorted.len() / 2];
            let data = wrap(raw);
            let what = format!("{name} {w}x{h}");

            let got = compute_centroid(&data, w, h, thr);
            let want = reference_centroid(&vals, w, h, thr);
            for (field, g, e) in [
                ("centroid_x", got.centroid_x, want.centroid_x),
                ("centroid_y", got.centroid_y, want.centroid_y),
                ("sigma_x", got.sigma_x, want.sigma_x),
                ("sigma_y", got.sigma_y, want.sigma_y),
                ("sigma_xy", got.sigma_xy, want.sigma_xy),
                ("centroid_total", got.centroid_total, want.centroid_total),
                ("skewness_x", got.skewness_x, want.skewness_x),
                ("skewness_y", got.skewness_y, want.skewness_y),
                ("kurtosis_x", got.kurtosis_x, want.kurtosis_x),
                ("kurtosis_y", got.kurtosis_y, want.kurtosis_y),
                ("eccentricity", got.eccentricity, want.eccentricity),
                ("orientation", got.orientation, want.orientation),
            ] {
                // Signed data with a negative total leaves both sides NaN
                // (sqrt of a negative variance), as the old loops did.
                if !(g.is_nan() && e.is_nan()) {
                    assert_close(&format!("{what} {field}"), g, e, rel);
                }
            }

            let got = compute_profiles(&data, w, h, thr, 1.0, 2.0, w / 2, h / 3);
            let [ax, ay, tx, ty] = reference_profiles(&vals, w, h, thr);
            for (field, g, e) in [
                ("avg_x", &got.avg_x, &ax),
                ("avg_y", &got.avg_y, &ay),
                ("threshold_x", &got.threshold_x, &tx),
                ("threshold_y", &got.threshold_y, &ty),
            ] {
                assert_eq!(g.len(), e.len(), "{what} {field} length");
                for (i, (&g, &e)) in g.iter().zip(e).enumerate() {
                    assert_close(&format!("{what} {field}[{i}]"), g, e, rel);
                }
            }
            let cy_row = ((2.0f64 + 0.5) as usize).min(h - 1);
            let cx_col = ((1.0f64 + 0.5) as usize).min(w - 1);
            let row = |iy: usize| vals[iy * w..(iy + 1) * w].to_vec();
            let col = |ix: usize| (0..h).map(|iy| vals[iy * w + ix]).collect::<Vec<_>>();
            assert_eq!(got.centroid_x, row(cy_row), "{what} centroid row");
            assert_eq!(got.centroid_y, col(cx_col), "{what} centroid column");
            assert_eq!(got.cursor_x, row((h / 3).min(h - 1)), "{what} cursor row");
            assert_eq!(
                got.cursor_y,
                col((w / 2).min(w - 1)),
                "{what} cursor column"
            );
        }
    }

    #[test]
    fn projection_matches_reference_for_every_element_type() {
        check_projection_against_reference(
            "i8",
            |i| (seq(i) % 256) as u8 as i8,
            NDDataBuffer::I8,
            |e| e as f64,
            1e-9,
        );
        check_projection_against_reference(
            "u8",
            |i| (seq(i) % 256) as u8,
            NDDataBuffer::U8,
            |e| e as f64,
            1e-9,
        );
        check_projection_against_reference(
            "i16",
            |i| (seq(i) % 65536) as u16 as i16,
            NDDataBuffer::I16,
            |e| e as f64,
            1e-9,
        );
        check_projection_against_reference(
            "u16",
            |i| (seq(i) % 65536) as u16,
            NDDataBuffer::U16,
            |e| e as f64,
            1e-9,
        );
        check_projection_against_reference(
            "i32",
            |i| seq(i) as i32 - 500_000,
            NDDataBuffer::I32,
            |e| e as f64,
            1e-9,
        );
        check_projection_against_reference("u32", seq, NDDataBuffer::U32, |e| e as f64, 1e-9);
        check_projection_against_reference(
            "i64",
            |i| seq(i) as i64 - 500_000,
            NDDataBuffer::I64,
            |e| e as f64,
            1e-9,
        );
        check_projection_against_reference(
            "u64",
            |i| seq(i) as u64,
            NDDataBuffer::U64,
            |e| e as f64,
            1e-9,
        );
        check_projection_against_reference(
            "f32",
            |i| seq(i) as f32 * 0.37 - 100.0,
            NDDataBuffer::F32,
            |e| e as f64,
            1e-9,
        );
        check_projection_against_reference(
            "f64",
            |i| seq(i) as f64 * 0.37 - 100.0,
            NDDataBuffer::F64,
            |e| e,
            1e-9,
        );
    }

    /// C's `value >= centroidThreshold` (NDPluginStats.cpp:212) is false for
    /// a NaN: it is left out of the threshold sums and out of the centroid,
    /// while the plain average it does take part in becomes NaN.
    #[test]
    fn projection_leaves_nan_out_of_the_threshold_sums() {
        let mut pixels = vec![2.0f32; 4 * 3];
        pixels[1 * 4 + 2] = f32::NAN;
        let data = NDDataBuffer::F32(pixels);
        let c = compute_centroid(&data, 4, 3, 0.0);
        assert_eq!(c.centroid_total, 22.0);
        assert!((c.centroid_x - 16.0 / 11.0).abs() < 1e-12);
        let p = compute_profiles(&data, 4, 3, 0.0, 0.0, 0.0, 0, 0);
        assert_eq!(p.threshold_x[2], 4.0 / 3.0);
        assert_eq!(p.threshold_y[1], 1.5);
        assert!(p.avg_x[2].is_nan());
        assert!(p.avg_y[1].is_nan());
        assert_eq!(p.avg_x[0], 2.0);
    }

    /// The histogram as C bins it (NDPluginStats.cpp:42-56), one element at
    /// a time in f64.
    fn reference_histogram(
        vals: &[f64],
        hist_size: usize,
        lo: f64,
        hi: f64,
    ) -> (Vec<f64>, f64, f64) {
        let scale = (hist_size - 1) as f64 / (hi - lo);
        let (mut bins, mut below, mut above) = (vec![0.0; hist_size], 0.0, 0.0);
        for &value in vals {
            let bin = ((value - lo) * scale + 0.5) as i64;
            if bin < 0 || value < lo {
                below += 1.0;
            } else if bin > hist_size as i64 - 1 || value > hi {
                above += 1.0;
            } else {
                bins[bin as usize] += 1.0;
            }
        }
        (bins, below, above)
    }

    fn check_histogram_against_reference<T: Copy>(
        name: &str,
        make: impl Fn(usize) -> T,
        wrap: fn(Vec<T>) -> NDDataBuffer,
        to_f64: fn(T) -> f64,
    ) {
        for &n in KERNEL_LENGTHS {
            let raw: Vec<T> = (0..n).map(&make).collect();
            let vals: Vec<f64> = raw.iter().map(|&e| to_f64(e)).collect();
            let mut sorted = vals.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            // A range that leaves some values on either side.
            let (lo, hi) = (sorted[n / 8], sorted[n - 1 - n / 8]);
            if hi <= lo {
                continue;
            }
            let data = wrap(raw);
            for hist_size in [1, 2, 7, 256] {
                let what = format!("{name} n={n} bins={hist_size}");
                let (got, below, above, _) = compute_histogram(&data, hist_size, lo, hi);
                let (want, want_below, want_above) = reference_histogram(&vals, hist_size, lo, hi);
                assert_eq!(got, want, "{what} bins");
                assert_eq!(below, want_below, "{what} below");
                assert_eq!(above, want_above, "{what} above");
            }
        }
    }

    /// The formula path on the values the bin arithmetic treats specially:
    /// NaN, the infinities, magnitudes beyond `i64`, and values sitting on
    /// the range limits and on the `.5` rounding edges of the bins.
    #[test]
    fn histogram_formula_edges_match_the_scalar_rule() {
        let (lo, hi, bins) = (-3.0, 5.0, 7usize);
        let scale = (bins - 1) as f64 / (hi - lo);
        let mut vals: Vec<f64> = vec![
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e300,
            -1e300,
            lo,
            hi,
            lo - 1e-9,
            hi + 1e-9,
            lo - 1.0 / scale,
            -0.0,
            0.0,
        ];
        for b in 0..bins {
            let edge = lo + (b as f64 - 0.5) / scale;
            vals.extend([edge, edge - 1e-9, edge + 1e-9]);
        }
        // Enough copies to fill several vectors plus a remainder.
        let vals: Vec<f64> = vals
            .iter()
            .cycle()
            .take(vals.len() * 5 + 3)
            .copied()
            .collect();
        let (want, want_below, want_above) = reference_histogram(&vals, bins, lo, hi);
        assert!(want_below > 0.0 && want_above > 0.0 && want.iter().any(|&c| c > 0.0));

        let (got, below, above, _) =
            compute_histogram(&NDDataBuffer::F64(vals.clone()), bins, lo, hi);
        assert_eq!(
            (got, below, above),
            (want.clone(), want_below, want_above),
            "f64"
        );

        let f32s: Vec<f32> = vals.iter().map(|&v| v as f32).collect();
        let as_f64: Vec<f64> = f32s.iter().map(|&v| v as f64).collect();
        let (want, want_below, want_above) = reference_histogram(&as_f64, bins, lo, hi);
        let (got, below, above, _) = compute_histogram(&NDDataBuffer::F32(f32s), bins, lo, hi);
        assert_eq!((got, below, above), (want, want_below, want_above), "f32");

        let ints: Vec<i32> = vec![i32::MIN, -4, -3, -2, 0, 4, 5, 6, i32::MAX];
        let ints: Vec<i32> = ints
            .iter()
            .cycle()
            .take(ints.len() * 5 + 3)
            .copied()
            .collect();
        let as_f64: Vec<f64> = ints.iter().map(|&v| v as f64).collect();
        let (want, want_below, want_above) = reference_histogram(&as_f64, bins, lo, hi);
        let (got, below, above, _) = compute_histogram(&NDDataBuffer::I32(ints), bins, lo, hi);
        assert_eq!((got, below, above), (want, want_below, want_above), "i32");
    }

    #[test]
    fn histogram_matches_reference_for_every_element_type() {
        check_histogram_against_reference(
            "i8",
            |i| (seq(i) % 256) as u8 as i8,
            NDDataBuffer::I8,
            |e| e as f64,
        );
        check_histogram_against_reference(
            "u8",
            |i| (seq(i) % 256) as u8,
            NDDataBuffer::U8,
            |e| e as f64,
        );
        check_histogram_against_reference(
            "i16",
            |i| (seq(i) % 65536) as u16 as i16,
            NDDataBuffer::I16,
            |e| e as f64,
        );
        check_histogram_against_reference(
            "u16",
            |i| (seq(i) % 65536) as u16,
            NDDataBuffer::U16,
            |e| e as f64,
        );
        check_histogram_against_reference(
            "i32",
            |i| seq(i) as i32 - 500_000,
            NDDataBuffer::I32,
            |e| e as f64,
        );
        check_histogram_against_reference("u32", seq, NDDataBuffer::U32, |e| e as f64);
        check_histogram_against_reference(
            "i64",
            |i| seq(i) as i64 - 500_000,
            NDDataBuffer::I64,
            |e| e as f64,
        );
        check_histogram_against_reference(
            "u64",
            |i| seq(i) as u64,
            NDDataBuffer::U64,
            |e| e as f64,
        );
        check_histogram_against_reference(
            "f32",
            |i| seq(i) as f32 * 0.37 - 100.0,
            NDDataBuffer::F32,
            |e| e as f64,
        );
        check_histogram_against_reference(
            "f64",
            |i| seq(i) as f64 * 0.37 - 100.0,
            NDDataBuffer::F64,
            |e| e,
        );
    }
}
