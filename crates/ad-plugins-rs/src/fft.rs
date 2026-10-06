use std::sync::Arc;

use ad_core_rs::error::ADResult;
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use parking_lot::Mutex;
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// FFT direction (forward or inverse transform).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FFTDirection {
    Forward,
    Inverse,
}

/// Configuration for FFT processing.
///
/// The transform rank (1-D vs 2-D) is NOT configured here: like C
/// `NDPluginFFT::processCallbacks` (NDPluginFFT.cpp:298-315) it is taken from
/// the input array's `ndims` on every frame, so a 1-D input drives a 1-D FFT
/// and a 2-D input a full 2-D FFT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FFTConfig {
    pub direction: FFTDirection,
    /// Zero out DC component (k=0) in the output magnitudes.
    pub suppress_dc: bool,
    /// Average N frames of magnitude. 0 or 1 means no averaging.
    pub num_average: usize,
}

impl Default for FFTConfig {
    fn default() -> Self {
        Self {
            direction: FFTDirection::Forward,
            suppress_dc: false,
            num_average: 0,
        }
    }
}

/// Smallest power of two greater than or equal to `n` (C++ `nextPow2`).
///
/// `next_pow2(0)` and `next_pow2(1)` return 1.
pub fn next_pow2(n: usize) -> usize {
    if n <= 1 {
        return 1;
    }
    let mut p = 1usize;
    while p < n {
        p <<= 1;
    }
    p
}

/// Allocate the Float64 output for `src` from `pool`, as C's
/// `pNDArrayPool->alloc(rank, dims, NDFloat64, 0, 0)` (NDPluginFFT.cpp:212),
/// and copy the frame identity over. The caller writes every element.
fn float64_output(pool: &NDArrayPool, src: &NDArray, dims: Vec<NDDimension>) -> ADResult<NDArray> {
    let mut arr = pool.alloc(dims, NDDataType::Float64)?;
    arr.unique_id = src.unique_id;
    arr.copy_time_stamps_from(src);
    arr.attributes = src.attributes.clone();
    Ok(arr)
}

/// The Float64 payload of an array made by [`float64_output`].
fn f64_slice(arr: &mut NDArray) -> &mut [f64] {
    match &mut arr.data {
        NDDataBuffer::F64(v) => v.as_mut_slice(),
        _ => unreachable!("the output was allocated as Float64"),
    }
}

/// A rustfft plan with its scratch, so a frame's transforms allocate the
/// scratch once instead of on every `Fft::process`.
struct Plan {
    fft: Arc<dyn Fft<f64>>,
    scratch: Vec<Complex<f64>>,
}

impl Plan {
    fn new(fft: Arc<dyn Fft<f64>>) -> Self {
        let scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        Self { fft, scratch }
    }

    fn len(&self) -> usize {
        self.fft.len()
    }

    /// Transform every chunk of [`Plan::len`] elements of `buf` in place.
    fn run(&mut self, buf: &mut [Complex<f64>]) {
        self.fft.process_with_scratch(buf, &mut self.scratch);
    }
}

/// Row `row` of the `width`-wide samples as complex values, zero-extended
/// over the rest of `buf`. Samples past the end of `vals` read as zero.
fn load_row(buf: &mut [Complex<f64>], vals: &[f64], row: usize, width: usize) {
    let start = (row * width).min(vals.len());
    let src = &vals[start..(start + width).min(vals.len())];
    let (head, tail) = buf.split_at_mut(src.len());
    for (c, &v) in head.iter_mut().zip(src) {
        *c = Complex::new(v, 0.0);
    }
    tail.fill(Complex::new(0.0, 0.0));
}

/// Rows `row` and `row + 1` packed as the real and imaginary parts of one
/// complex row, zero-extended over the rest of `buf`.
fn load_row_pair(buf: &mut [Complex<f64>], vals: &[f64], row: usize, width: usize) {
    load_row(buf, vals, row, width);
    let start = ((row + 1) * width).min(vals.len());
    let src = &vals[start..(start + width).min(vals.len())];
    for (c, &v) in buf.iter_mut().zip(src) {
        c.im = v;
    }
}

/// The transforms of the two real rows packed by [`load_row_pair`], bins
/// `0..a.len()` of each: with `Z` the transform of `a + ib` over `N`
/// points, `A[k] = (Z[k] + conj Z[N-k]) / 2` and
/// `B[k] = (Z[k] - conj Z[N-k]) / 2i`. Halving is exact, so the bins carry
/// only the rounding of the shared transform.
fn unpack_pair(z: &[Complex<f64>], a: &mut [Complex<f64>], b: &mut [Complex<f64>]) {
    let n = z.len();
    for (k, (a, b)) in a.iter_mut().zip(b).enumerate() {
        let zk = z[k];
        let zn = z[(n - k) % n].conj();
        *a = (zk + zn) * 0.5;
        let d = zk - zn;
        *b = Complex::new(d.im * 0.5, -d.re * 0.5);
    }
}

/// The magnitude of each bin as C computes it, `sqrt(re*re + im*im) / n`
/// (NDPluginFFT.cpp:134 for 1-D, :163 for 2-D with `n = nTimeX * nTimeY`).
fn magnitudes_into(out: &mut [f64], bins: &[Complex<f64>], n: f64) {
    for (m, c) in out.iter_mut().zip(bins) {
        *m = (c.re * c.re + c.im * c.im).sqrt() / n;
    }
}

/// The column transforms of the row-major `w`-wide complex array `data`,
/// `COLS` columns at a time so every row is read and written in runs of
/// adjacent elements instead of one element per row per column.
fn fft_columns(plan: &mut Plan, data: &mut [Complex<f64>], w: usize) {
    const COLS: usize = 8;
    let h = plan.len();
    let mut cols = vec![Complex::new(0.0, 0.0); COLS * h];
    for c0 in (0..w).step_by(COLS) {
        let nc = COLS.min(w - c0);
        for (row, r) in data.chunks_exact(w).enumerate() {
            for (k, &v) in r[c0..c0 + nc].iter().enumerate() {
                cols[k * h + row] = v;
            }
        }
        plan.run(&mut cols[..nc * h]);
        for (row, r) in data.chunks_exact_mut(w).enumerate() {
            for (k, v) in r[c0..c0 + nc].iter_mut().enumerate() {
                *v = cols[k * h + row];
            }
        }
    }
}

/// Compute 1D FFT magnitude for each row of a 2D array using rustfft.
/// Returns a Float64 array with half the *padded* width (positive frequencies
/// only). Like C++ NDPluginFFT, each row is zero-padded to the next power of
/// two before the transform, and `nFreqX = paddedWidth / 2`.
/// Magnitudes are normalized by the padded length.
pub fn fft_1d_rows(
    pool: &NDArrayPool,
    src: &NDArray,
    suppress_dc: bool,
) -> ADResult<Option<NDArray>> {
    let mut planner = FftPlanner::<f64>::new();
    let vals = src.data.to_f64_vec();
    fft_1d_rows_with(
        |n| planner.plan_fft_forward(n),
        pool,
        src,
        &vals,
        suppress_dc,
    )
}

/// [`fft_1d_rows`] on `vals`, the samples of `src` as `f64`, with the
/// forward plan for the padded width from `plan`.
fn fft_1d_rows_with(
    plan: impl FnOnce(usize) -> Arc<dyn Fft<f64>>,
    pool: &NDArrayPool,
    src: &NDArray,
    vals: &[f64],
    suppress_dc: bool,
) -> ADResult<Option<NDArray>> {
    if src.dims.is_empty() {
        return Ok(None);
    }

    let width = src.dims[0].size;
    let height = if src.dims.len() >= 2 {
        src.dims[1].size
    } else {
        1
    };

    if width == 0 {
        return Ok(None);
    }

    // C++ rounds the time dimension up to the next power of two and zero-pads.
    let padded = next_pow2(width);

    // C++: nFreqX = paddedWidth / 2 (only positive frequencies)
    let n_freq = padded / 2;
    if n_freq == 0 {
        return Ok(None);
    }
    let mut plan = Plan::new(plan(padded));

    let dims = if height > 1 {
        vec![NDDimension::new(n_freq), NDDimension::new(height)]
    } else {
        vec![NDDimension::new(n_freq)]
    };
    let mut arr = float64_output(pool, src, dims)?;
    let magnitudes = f64_slice(&mut arr);

    // The rows are real, so each transform carries two of them.
    let mut row_buf = vec![Complex::new(0.0, 0.0); padded];
    let mut a_bins = vec![Complex::new(0.0, 0.0); n_freq];
    let mut b_bins = vec![Complex::new(0.0, 0.0); n_freq];
    let mut pairs = magnitudes.chunks_exact_mut(2 * n_freq);
    for (pair, mags) in (&mut pairs).enumerate() {
        load_row_pair(&mut row_buf, vals, 2 * pair, width);
        plan.run(&mut row_buf);
        unpack_pair(&row_buf, &mut a_bins, &mut b_bins);
        let (ma, mb) = mags.split_at_mut(n_freq);
        magnitudes_into(ma, &a_bins, padded as f64);
        magnitudes_into(mb, &b_bins, padded as f64);
        if suppress_dc {
            ma[0] = 0.0;
            mb[0] = 0.0;
        }
    }
    let last = pairs.into_remainder();
    if !last.is_empty() {
        load_row(&mut row_buf, vals, height - 1, width);
        plan.run(&mut row_buf);
        magnitudes_into(last, &row_buf[..n_freq], padded as f64);
        if suppress_dc {
            last[0] = 0.0;
        }
    }

    Ok(Some(arr))
}

/// Compute 2D FFT magnitude using separable row-then-column FFT via rustfft.
pub fn fft_2d(pool: &NDArrayPool, src: &NDArray, suppress_dc: bool) -> ADResult<Option<NDArray>> {
    let mut planner = FftPlanner::<f64>::new();
    let vals = src.data.to_f64_vec();
    fft_2d_with(
        |n| planner.plan_fft_forward(n),
        pool,
        src,
        &vals,
        suppress_dc,
    )
}

/// [`fft_2d`] on `vals`, the samples of `src` as `f64`, with the forward
/// plans for the padded width and height from `plan`.
fn fft_2d_with(
    mut plan: impl FnMut(usize) -> Arc<dyn Fft<f64>>,
    pool: &NDArrayPool,
    src: &NDArray,
    vals: &[f64],
    suppress_dc: bool,
) -> ADResult<Option<NDArray>> {
    if src.dims.len() < 2 {
        return Ok(None);
    }

    let src_w = src.dims[0].size;
    let src_h = src.dims[1].size;

    if src_w == 0 || src_h == 0 {
        return Ok(None);
    }

    // C++ zero-pads each dimension to the next power of two.
    let w = next_pow2(src_w);
    let h = next_pow2(src_h);

    // C++: nFreqX = paddedX/2, nFreqY = paddedY/2; normalize by padded N*M
    let n_freq_x = w / 2;
    let n_freq_y = h / 2;
    if n_freq_x == 0 || n_freq_y == 0 {
        return Ok(None);
    }

    let mut rows = Plan::new(plan(w));
    let mut cols = Plan::new(plan(h));

    // The rows are real, so each row transform carries two of them, and
    // only bins below nFreqX reach the output, so the column transforms run
    // over those bins alone: `data` is nFreqX wide and h tall, its padding
    // rows zero as the transform of zeros.
    let mut data = vec![Complex::new(0.0, 0.0); n_freq_x * h];
    let mut row_buf = vec![Complex::new(0.0, 0.0); w];
    let mut pairs = data[..src_h * n_freq_x].chunks_exact_mut(2 * n_freq_x);
    for (pair, bins) in (&mut pairs).enumerate() {
        load_row_pair(&mut row_buf, vals, 2 * pair, src_w);
        rows.run(&mut row_buf);
        let (a, b) = bins.split_at_mut(n_freq_x);
        unpack_pair(&row_buf, a, b);
    }
    let last = pairs.into_remainder();
    if !last.is_empty() {
        load_row(&mut row_buf, vals, src_h - 1, src_w);
        rows.run(&mut row_buf);
        last.copy_from_slice(&row_buf[..n_freq_x]);
    }
    fft_columns(&mut cols, &mut data, n_freq_x);

    let dims = vec![NDDimension::new(n_freq_x), NDDimension::new(n_freq_y)];
    let mut arr = float64_output(pool, src, dims)?;
    let magnitudes = f64_slice(&mut arr);
    for (fy, mags) in magnitudes.chunks_exact_mut(n_freq_x).enumerate() {
        magnitudes_into(
            mags,
            &data[fy * n_freq_x..(fy + 1) * n_freq_x],
            (w * h) as f64,
        );
    }

    if suppress_dc {
        magnitudes[0] = 0.0;
    }

    Ok(Some(arr))
}

/// FFT processing engine with cached planner and optional magnitude averaging.
#[derive(Default)]
struct FFTParamIndices {
    direction: Option<usize>,
    suppress_dc: Option<usize>,
    num_average: Option<usize>,
    num_averaged: Option<usize>,
    reset_average: Option<usize>,
    time_per_point: Option<usize>,
    /// `FFTTimeSeries` waveform — the input time series (nTimeX points).
    time_series: Option<usize>,
    /// `FFTReal` waveform — real part of the spectrum (nFreqX points).
    real: Option<usize>,
    /// `FFTImaginary` waveform — imaginary part of the spectrum.
    imaginary: Option<usize>,
    /// `FFTAbsValue` waveform — magnitude of the spectrum.
    abs_value: Option<usize>,
    /// `FFTTimeAxis` waveform — `i * timePerPoint`.
    time_axis: Option<usize>,
    /// `FFTFreqAxis` waveform — frequency-axis values.
    freq_axis: Option<usize>,
}

/// Everything a frame mutates: the tuning config and the running-average
/// accumulator. One transform advances both, so they live under one lock
/// rather than one lock each.
///
/// The planner is deliberately NOT here. It is a plan cache, not frame
/// state, and keeping it in this mutex would force the transform itself to
/// run under the lock that a param write contends for -- the shape C avoids
/// by releasing at NDPluginFFT.cpp:334.
struct FFTState {
    config: FFTConfig,
    /// Running average magnitude buffer.
    avg_buffer: Option<Vec<f64>>,
    /// Number of frames accumulated so far.
    avg_count: usize,
    /// Cached dimensions to detect changes.
    cached_dims: Vec<usize>,
    /// Seconds per input time point (C++ `timePerPoint_`); scales the time
    /// and frequency axis waveforms.
    time_per_point: f64,
}

pub struct FFTProcessor {
    state: Mutex<FFTState>,
    /// Plan cache, shared across frames and across pool workers. Locked only
    /// for the plan lookup itself (one or two per frame), never across a
    /// transform.
    planner: Mutex<FftPlanner<f64>>,
    params: FFTParamIndices,
}

impl FFTProcessor {
    pub fn new() -> Self {
        Self::with_config(FFTConfig::default())
    }

    pub fn with_config(config: FFTConfig) -> Self {
        Self {
            state: Mutex::new(FFTState {
                config,
                avg_buffer: None,
                avg_count: 0,
                cached_dims: Vec::new(),
                time_per_point: 1.0,
            }),
            planner: Mutex::new(FftPlanner::new()),
            params: FFTParamIndices::default(),
        }
    }
}

impl FFTState {
    /// Check if dimensions changed and reset averaging state if so.
    fn check_dims_changed(&mut self, dims: &[NDDimension]) {
        let current: Vec<usize> = dims.iter().map(|d| d.size).collect();
        if current != self.cached_dims {
            self.cached_dims = current;
            self.avg_buffer = None;
            self.avg_count = 0;
        }
    }

    /// Apply magnitude averaging using exponential moving average (matching C++).
    ///
    /// C++: `FFTAbsValue_[j] = FFTAbsValue_[j] * oldFraction + new[j] * newFraction`
    /// where `oldFraction = 1 - 1/numAveraged`, `newFraction = 1/numAveraged`.
    ///
    /// `magnitudes` is updated in place to the averaged values, so the pooled
    /// output buffer is what goes downstream.
    fn apply_averaging(&mut self, magnitudes: &mut [f64]) {
        let num_avg = self.config.num_average;
        if num_avg <= 1 {
            return;
        }

        let buf = self
            .avg_buffer
            .get_or_insert_with(|| vec![0.0; magnitudes.len()]);

        // Reset if buffer size changed
        if buf.len() != magnitudes.len() {
            *buf = vec![0.0; magnitudes.len()];
            self.avg_count = 0;
        }

        self.avg_count += 1;
        // Cap at num_average for the weighting
        let n = self.avg_count.min(num_avg) as f64;
        let new_fraction = 1.0 / n;
        let old_fraction = 1.0 - new_fraction;

        // C++ exponential moving average
        for (b, m) in buf.iter_mut().zip(magnitudes.iter_mut()) {
            *b = *b * old_fraction + *m * new_fraction;
            *m = *b;
        }
    }
}

/// The per-frame view of the configuration: everything the transform reads
/// and nothing it writes.
///
/// C takes exactly this snapshot under the port lock -- `suppressDC` at
/// NDPluginFFT.cpp:325 and `timePerPoint_` at :330 -- and then releases at
/// :334 ("things below don't access shared memory") before converting the
/// input and running `computeFFT_1D`/`computeFFT_2D`. Splitting the config
/// off `FFTState` is what lets us do the same: the transform borrows this
/// value, so it cannot be holding the state lock while it runs.
struct FFTFrame<'a> {
    config: FFTConfig,
    time_per_point: f64,
    planner: &'a Mutex<FftPlanner<f64>>,
}

impl FFTFrame<'_> {
    /// Look a forward plan up in the shared cache. The lock spans the lookup
    /// only -- never `Fft::process`.
    fn plan_forward(&self, len: usize) -> Arc<dyn Fft<f64>> {
        self.planner.lock().plan_fft_forward(len)
    }

    /// Inverse counterpart of [`FFTFrame::plan_forward`].
    fn plan_inverse(&self, len: usize) -> Arc<dyn Fft<f64>> {
        self.planner.lock().plan_fft_inverse(len)
    }

    /// Compute FFT using cached planner for plan reuse across frames.
    ///
    /// `vals` is the frame's samples as `f64`, converted once per frame and
    /// shared with [`FFTFrame::compute_row_spectrum`].
    ///
    /// The rank is taken from the input array's dimension count, matching C
    /// `NDPluginFFT::processCallbacks` (NDPluginFFT.cpp:298-315): `ndims==1`
    /// drives a 1-D FFT, `ndims==2` a full 2-D FFT, and any other rank is
    /// rejected (C prints an error and returns with no output).
    fn compute_fft(
        &self,
        pool: &NDArrayPool,
        src: &NDArray,
        vals: &[f64],
    ) -> ADResult<Option<NDArray>> {
        let suppress_dc = self.config.suppress_dc;

        match (src.dims.len(), self.config.direction) {
            (1, FFTDirection::Forward) => {
                fft_1d_rows_with(|n| self.plan_forward(n), pool, src, vals, suppress_dc)
            }
            (1, FFTDirection::Inverse) => {
                self.compute_fft_1d_rows_inverse(pool, src, vals, suppress_dc)
            }
            (2, FFTDirection::Forward) => {
                fft_2d_with(|n| self.plan_forward(n), pool, src, vals, suppress_dc)
            }
            (2, FFTDirection::Inverse) => self.compute_fft_2d_inverse(pool, src, vals, suppress_dc),
            _ => Ok(None),
        }
    }

    /// Compute the 1D forward FFT of the first row of `src`, returning the
    /// extracted time series and the half-spectrum complex values.
    ///
    /// This drives the C++ `FFTTimeSeries`/`FFTReal`/`FFTImaginary`/
    /// `FFTAbsValue` waveform records, which in C++ are 1D arrays over the
    /// first time axis. Returns `(time_series, real, imag)` where `time_series`
    /// has `padded` elements (nTimeX) — C posts the zero-extended padded series
    /// (NDPluginFFT.cpp: timeSeries is the nTimeX-long calloc buffer) — and
    /// `real`/`imag` have `padded/2` elements (nFreqX). The DC bin is zeroed in
    /// the two spectral arrays when `suppress_dc` is set (C++ behaviour); the
    /// time series is never DC-suppressed.
    fn compute_row_spectrum(
        &self,
        src: &NDArray,
        vals: &[f64],
        suppress_dc: bool,
    ) -> Option<(Vec<f64>, Vec<f64>, Vec<f64>)> {
        if src.dims.is_empty() {
            return None;
        }
        let width = src.dims[0].size;
        if width == 0 {
            return None;
        }
        let padded = next_pow2(width);
        let n_freq = padded / 2;
        if n_freq == 0 {
            return None;
        }
        let mut plan = Plan::new(self.plan_forward(padded));

        // The first row, zero-extended to the padded length nTimeX. C posts the
        // padded series (calloc'd to nTimeX, the input copied into [0,width)),
        // so FFTTimeSeries and FFTTimeAxis are nTimeX long, not width long.
        let mut row_buf = vec![Complex::new(0.0, 0.0); padded];
        load_row(&mut row_buf, vals, 0, width);
        let time_series: Vec<f64> = row_buf.iter().map(|c| c.re).collect();
        plan.run(&mut row_buf);

        let mut real: Vec<f64> = row_buf[..n_freq].iter().map(|c| c.re).collect();
        let mut imag: Vec<f64> = row_buf[..n_freq].iter().map(|c| c.im).collect();
        if suppress_dc {
            real[0] = 0.0;
            imag[0] = 0.0;
        }
        Some((time_series, real, imag))
    }

    /// Frequency-axis values for `n_freq` bins (C++ `createAxisArrays`):
    /// `freqStep = 0.5 / timePerPoint / (nFreqX - 1)`.
    fn freq_axis(&self, n_freq: usize) -> Vec<f64> {
        if n_freq <= 1 {
            return vec![0.0; n_freq];
        }
        let tpp = if self.time_per_point > 0.0 {
            self.time_per_point
        } else {
            1.0
        };
        let step = 0.5 / tpp / (n_freq - 1) as f64;
        (0..n_freq).map(|i| i as f64 * step).collect()
    }

    /// Time-axis values for `n_time` points: `i * timePerPoint`.
    fn time_axis(&self, n_time: usize) -> Vec<f64> {
        let tpp = if self.time_per_point > 0.0 {
            self.time_per_point
        } else {
            1.0
        };
        (0..n_time).map(|i| i as f64 * tpp).collect()
    }

    fn compute_fft_1d_rows_inverse(
        &self,
        pool: &NDArrayPool,
        src: &NDArray,
        vals: &[f64],
        suppress_dc: bool,
    ) -> ADResult<Option<NDArray>> {
        if src.dims.is_empty() {
            return Ok(None);
        }

        let width = src.dims[0].size;
        let height = if src.dims.len() >= 2 {
            src.dims[1].size
        } else {
            1
        };

        if width == 0 {
            return Ok(None);
        }

        let mut plan = Plan::new(self.plan_inverse(width));
        let scale = 1.0 / width as f64;

        // An inverse transform of a real-valued spectrum yields signed real
        // samples: take the real part, not the modulus, so negative samples
        // survive a forward->inverse round trip.
        let mut arr = float64_output(pool, src, src.dims.clone())?;
        let samples = f64_slice(&mut arr);

        let mut row_buf = vec![Complex::new(0.0, 0.0); width];
        for (row, out) in samples.chunks_exact_mut(width).take(height).enumerate() {
            load_row(&mut row_buf, vals, row, width);
            if suppress_dc {
                row_buf[0] = Complex::new(0.0, 0.0);
            }
            plan.run(&mut row_buf);
            for (s, c) in out.iter_mut().zip(&row_buf) {
                *s = c.re * scale;
            }
        }

        Ok(Some(arr))
    }

    fn compute_fft_2d_inverse(
        &self,
        pool: &NDArrayPool,
        src: &NDArray,
        vals: &[f64],
        suppress_dc: bool,
    ) -> ADResult<Option<NDArray>> {
        if src.dims.len() < 2 {
            return Ok(None);
        }

        let w = src.dims[0].size;
        let h = src.dims[1].size;

        if w == 0 || h == 0 {
            return Ok(None);
        }

        let mut rows = Plan::new(self.plan_inverse(w));
        let mut cols = Plan::new(self.plan_inverse(h));
        let scale = 1.0 / (w * h) as f64;

        let mut data = vec![Complex::new(0.0, 0.0); w * h];
        for (row, r) in data.chunks_exact_mut(w).enumerate() {
            load_row(r, vals, row, w);
        }

        if suppress_dc {
            data[0] = Complex::new(0.0, 0.0);
        }

        fft_columns(&mut cols, &mut data, w);
        rows.run(&mut data);

        // Inverse transform yields signed real samples: keep the real part.
        let dims = vec![NDDimension::new(w), NDDimension::new(h)];
        let mut arr = float64_output(pool, src, dims)?;
        for (s, c) in f64_slice(&mut arr).iter_mut().zip(&data) {
            *s = c.re * scale;
        }
        Ok(Some(arr))
    }
}

impl Default for FFTProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for FFTProcessor {
    fn process_array(&self, array: &Arc<NDArray>, pool: &NDArrayPool) -> ProcessResult {
        use ad_core_rs::plugin::runtime::ParamUpdate;

        // C processes only 1-D and 2-D inputs (NDPluginFFT.cpp:298-315); any
        // other rank prints an error and returns before allocating, computing,
        // or emitting any waveform. Gate the whole frame on the input rank so a
        // 3-D+ array yields no NDArray and no first-row waveforms.
        let rank = array.dims.len();
        if rank != 1 && rank != 2 {
            return ProcessResult::sink(Vec::new());
        }

        // C reads the frame's tuning under the port lock, then releases before
        // the transform (NDPluginFFT.cpp:334). Take the same snapshot here and
        // drop the state lock: everything from `compute_fft` to the axis
        // waveforms below reads only the snapshot, so a param write lands
        // while the transform runs instead of queueing behind it.
        let (frame, avg_count) = {
            let mut state = self.state.lock();
            state.check_dims_changed(&array.dims);
            (
                FFTFrame {
                    config: state.config,
                    time_per_point: state.time_per_point,
                    planner: &self.planner,
                },
                state.avg_count,
            )
        };

        let vals = array.data.to_f64_vec();
        let result = match frame.compute_fft(pool, array, &vals) {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(error = %e, "FFT output allocation failed; dropping frame");
                return ProcessResult::empty();
            }
        };
        let mut updates = Vec::new();
        if let Some(idx) = self.params.num_averaged {
            updates.push(ParamUpdate::int32(idx, avg_count as i32));
        }

        // Emit the C++ NDPluginFFT waveform records. On a forward transform
        // these are the time series, the real/imaginary/abs spectrum, and
        // the time/frequency axes (C++ doFFTCallbacks / createAxisArrays).
        // The inverse transform has no spectrum to publish.
        //
        // `apply_averaging` advances the EMA state, so it must be invoked at
        // most once per frame. The averaged FFTAbsValue waveform and the
        // averaged NDArray output therefore share a single averaging pass.
        if frame.config.direction == FFTDirection::Forward {
            let suppress_dc = frame.config.suppress_dc;
            if let Some((time_series, real, imag)) =
                frame.compute_row_spectrum(array, &vals, suppress_dc)
            {
                let n_time = time_series.len();
                let n_freq = real.len();
                if let Some(idx) = self.params.time_series {
                    updates.push(ParamUpdate::float64_array(idx, time_series));
                }
                if let Some(idx) = self.params.real {
                    updates.push(ParamUpdate::float64_array(idx, real));
                }
                if let Some(idx) = self.params.imaginary {
                    updates.push(ParamUpdate::float64_array(idx, imag));
                }
                if let Some(idx) = self.params.time_axis {
                    updates.push(ParamUpdate::float64_array(idx, frame.time_axis(n_time)));
                }
                if let Some(idx) = self.params.freq_axis {
                    updates.push(ParamUpdate::float64_array(idx, frame.freq_axis(n_freq)));
                }
            }
        }

        match result {
            Some(mut out) => {
                // The EMA accumulator is the one part of the frame that cannot
                // run released: it is shared mutable state advanced once per
                // frame. C re-takes the lock for exactly this
                // (NDPluginFFT.cpp:373) and re-reads NumAverage inside
                // `doArrayCallbacks` (:189-203) rather than trusting the value
                // it snapshotted before the transform, so re-read it here too.
                if let NDDataBuffer::F64(ref mut mags) = out.data {
                    let mut state = self.state.lock();
                    if state.config.num_average > 1 {
                        state.apply_averaging(mags);
                    }
                }
                // FFTAbsValue waveform mirrors the (possibly averaged) NDArray
                // magnitude buffer — for 1D forward this is the half-spectrum
                // magnitude that the NDArray output already carries.
                if frame.config.direction == FFTDirection::Forward {
                    if let (Some(idx), NDDataBuffer::F64(mags)) = (self.params.abs_value, &out.data)
                        && !mags.is_empty()
                    {
                        updates.push(ParamUpdate::float64_array(idx, mags.clone()));
                    }
                }
                let mut r = ProcessResult::arrays(vec![Arc::new(out)]);
                r.param_updates = updates;
                r
            }
            None => ProcessResult::sink(updates),
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginFFT"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        base.create_param("FFT_TIME_PER_POINT", ParamType::Float64)?;
        base.create_param("FFT_TIME_AXIS", ParamType::Float64Array)?;
        base.create_param("FFT_FREQ_AXIS", ParamType::Float64Array)?;
        base.create_param("FFT_DIRECTION", ParamType::Int32)?;
        base.create_param("FFT_SUPPRESS_DC", ParamType::Int32)?;
        base.create_param("FFT_NUM_AVERAGE", ParamType::Int32)?;
        base.create_param("FFT_NUM_AVERAGED", ParamType::Int32)?;
        base.create_param("FFT_RESET_AVERAGE", ParamType::Int32)?;
        base.create_param("FFT_TIME_SERIES", ParamType::Float64Array)?;
        base.create_param("FFT_REAL", ParamType::Float64Array)?;
        base.create_param("FFT_IMAGINARY", ParamType::Float64Array)?;
        base.create_param("FFT_ABS_VALUE", ParamType::Float64Array)?;

        self.params.direction = base.find_param("FFT_DIRECTION");
        self.params.suppress_dc = base.find_param("FFT_SUPPRESS_DC");
        self.params.num_average = base.find_param("FFT_NUM_AVERAGE");
        self.params.num_averaged = base.find_param("FFT_NUM_AVERAGED");
        self.params.reset_average = base.find_param("FFT_RESET_AVERAGE");
        self.params.time_per_point = base.find_param("FFT_TIME_PER_POINT");
        self.params.time_series = base.find_param("FFT_TIME_SERIES");
        self.params.real = base.find_param("FFT_REAL");
        self.params.imaginary = base.find_param("FFT_IMAGINARY");
        self.params.abs_value = base.find_param("FFT_ABS_VALUE");
        self.params.time_axis = base.find_param("FFT_TIME_AXIS");
        self.params.freq_axis = base.find_param("FFT_FREQ_AXIS");
        Ok(())
    }

    fn on_param_change(
        &self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        let mut state = self.state.lock();
        if Some(reason) == self.params.direction {
            state.config.direction = if params.value.as_i32() == 0 {
                FFTDirection::Forward
            } else {
                FFTDirection::Inverse
            };
        } else if Some(reason) == self.params.suppress_dc {
            state.config.suppress_dc = params.value.as_i32() != 0;
        } else if Some(reason) == self.params.num_average {
            state.config.num_average = params.value.as_i32().max(0) as usize;
        } else if Some(reason) == self.params.reset_average {
            if params.value.as_i32() != 0 {
                state.avg_buffer = None;
                state.avg_count = 0;
            }
        } else if Some(reason) == self.params.time_per_point {
            // Scales the FFTTimeAxis / FFTFreqAxis waveforms.
            let v = params.value.as_f64();
            if v > 0.0 {
                state.time_per_point = v;
            }
        }
        ad_core_rs::plugin::runtime::ParamChangeResult::updates(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `float64_output` carries both halves of its source's timestamp pair.
    ///
    /// The two are independent in C — a hardware clock feeds `timeStamp` while
    /// the registered time source feeds `epicsTS` — so the source is stamped
    /// with values that cannot be derived from each other.
    #[test]
    fn the_float64_output_carries_the_whole_timestamp_pair() {
        let mut src = NDArray::with_data(
            vec![NDDimension::new(4), NDDimension::new(2)],
            NDDataBuffer::F64(vec![0.0; 8]),
        );
        src.timestamp = ad_core_rs::timestamp::EpicsTimestamp { sec: 7, nsec: 11 };
        src.time_stamp = 123.5;

        let pool = NDArrayPool::new(0);
        let out = float64_output(&pool, &src, src.dims.clone()).unwrap();

        assert_eq!(out.timestamp, src.timestamp, "epicsTS was dropped");
        assert_eq!(
            out.time_stamp, src.time_stamp,
            "the derived double was dropped"
        );
    }

    /// The paired-row and half-column transforms against one complex
    /// transform per row and per column, on frames with an odd row count
    /// so the unpaired last row runs too.
    #[test]
    fn packed_real_transforms_match_the_direct_transforms() {
        let (w, h) = (13, 7);
        let vals: Vec<f64> = (0..w * h)
            .map(|i| ((i * 7919) % 251) as f64 - 120.0)
            .collect();
        let arr = NDArray::with_data(
            vec![NDDimension::new(w), NDDimension::new(h)],
            NDDataBuffer::F64(vals.clone()),
        );
        let (pw, ph) = (next_pow2(w), next_pow2(h));
        let mut planner = FftPlanner::<f64>::new();
        let row = planner.plan_fft_forward(pw);
        let col = planner.plan_fft_forward(ph);

        let mut data = vec![Complex::new(0.0, 0.0); pw * ph];
        for (r, chunk) in data.chunks_exact_mut(pw).take(h).enumerate() {
            load_row(chunk, &vals, r, w);
            row.process(chunk);
        }
        let want_1d: Vec<f64> = data
            .chunks_exact(pw)
            .take(h)
            .flat_map(|r| r[..pw / 2].iter().map(|c| c.norm() / pw as f64))
            .collect();
        let got = fft_1d_rows(&pool(), &arr, false).unwrap().unwrap();
        let got_1d = match &got.data {
            NDDataBuffer::F64(v) => v.clone(),
            _ => unreachable!(),
        };
        assert_eq!(got_1d.len(), want_1d.len());
        for (g, e) in got_1d.iter().zip(&want_1d) {
            assert!((g - e).abs() <= 1e-12 * e.abs().max(1.0), "1d {g} vs {e}");
        }

        let mut column = vec![Complex::new(0.0, 0.0); ph];
        for c in 0..pw {
            for r in 0..ph {
                column[r] = data[r * pw + c];
            }
            col.process(&mut column);
            for r in 0..ph {
                data[r * pw + c] = column[r];
            }
        }
        let want_2d: Vec<f64> = (0..ph / 2)
            .flat_map(|fy| (0..pw / 2).map(move |fx| (fy, fx)))
            .map(|(fy, fx)| data[fy * pw + fx].norm() / (pw * ph) as f64)
            .collect();
        let got = fft_2d(&pool(), &arr, false).unwrap().unwrap();
        let got_2d = match &got.data {
            NDDataBuffer::F64(v) => v.clone(),
            _ => unreachable!(),
        };
        assert_eq!(got_2d.len(), want_2d.len());
        for (g, e) in got_2d.iter().zip(&want_2d) {
            assert!((g - e).abs() <= 1e-12 * e.abs().max(1.0), "2d {g} vs {e}");
        }
    }

    fn pool() -> Arc<NDArrayPool> {
        NDArrayPool::new(0)
    }

    #[test]
    fn test_fft_1d_dc() {
        // Constant signal: DC component should dominate
        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..8 {
                v[i] = 1.0;
            }
        }

        let result = fft_1d_rows(&pool(), &arr, false).unwrap().unwrap();
        // Output is half spectrum: N/2 = 4 bins
        assert_eq!(result.dims[0].size, 4);
        if let NDDataBuffer::F64(ref v) = result.data {
            // DC component normalized by N: 8/8 = 1.0
            assert!((v[0] - 1.0).abs() < 1e-10);
            // Other components should be ~0
            assert!(v[1].abs() < 1e-10);
        }
    }

    #[test]
    fn test_fft_1d_sine() {
        // Sine wave at frequency 1: peak at k=1 and k=N-1
        let n = 16;
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..n {
                v[i] = (2.0 * std::f64::consts::PI * i as f64 / n as f64).sin();
            }
        }

        let result = fft_1d_rows(&pool(), &arr, false).unwrap().unwrap();
        // Output is N/2 = 8 bins
        assert_eq!(result.dims[0].size, 8);
        if let NDDataBuffer::F64(ref v) = result.data {
            // DC should be ~0
            assert!(v[0].abs() < 1e-10);
            // Peak at k=1, normalized by N: magnitude = N/2 / N = 0.5
            assert!((v[1] - 0.5).abs() < 1e-10);
            // k=2 should be small
            assert!(v[2].abs() < 1e-10);
        }
    }

    #[test]
    fn test_fft_2d_dimensions() {
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        let result = fft_2d(&pool(), &arr, false).unwrap().unwrap();
        // Half spectrum: 4/2 x 4/2 = 2x2
        assert_eq!(result.dims[0].size, 2);
        assert_eq!(result.dims[1].size, 2);
        assert_eq!(result.data.data_type(), NDDataType::Float64);
    }

    #[test]
    fn test_fft_1d_suppress_dc() {
        // Constant signal: DC component should be suppressed
        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..8 {
                v[i] = 1.0;
            }
        }

        let result = fft_1d_rows(&pool(), &arr, true).unwrap().unwrap();
        if let NDDataBuffer::F64(ref v) = result.data {
            // DC component should be zeroed out
            assert!((v[0]).abs() < 1e-15);
            // Other components should still be ~0 for constant signal
            assert!(v[1].abs() < 1e-10);
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_fft_2d_suppress_dc() {
        // 4x4 constant array, suppress_dc should zero out [0,0]
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::Float64,
        );
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for val in v.iter_mut() {
                *val = 3.0;
            }
        }

        let result = fft_2d(&pool(), &arr, true).unwrap().unwrap();
        if let NDDataBuffer::F64(ref v) = result.data {
            // DC at [0,0] should be zeroed
            assert!((v[0]).abs() < 1e-15);
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_fft_2d_known_dc() {
        // 4x4 constant=2.0 => DC = 4*4*2 = 32, normalized by 4*4 = 16 => 2.0
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::Float64,
        );
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for val in v.iter_mut() {
                *val = 2.0;
            }
        }

        let result = fft_2d(&pool(), &arr, false).unwrap().unwrap();
        // Half spectrum: 2x2
        assert_eq!(result.dims[0].size, 2);
        assert_eq!(result.dims[1].size, 2);
        if let NDDataBuffer::F64(ref v) = result.data {
            // DC normalized by N*M: 32 / 16 = 2.0
            assert!((v[0] - 2.0).abs() < 1e-10, "DC = {}, expected 2", v[0]);
            // All other bins should be ~0
            for i in 1..v.len() {
                assert!(v[i].abs() < 1e-10, "bin {} = {}, expected ~0", i, v[i]);
            }
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_fft_1d_known_cosine_peaks() {
        // Cosine at frequency 3 in N=16: peaks at k=3 and k=N-3=13
        let n = 16;
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..n {
                v[i] = (2.0 * std::f64::consts::PI * 3.0 * i as f64 / n as f64).cos();
            }
        }

        let result = fft_1d_rows(&pool(), &arr, false).unwrap().unwrap();
        // Half spectrum: 8 bins
        assert_eq!(result.dims[0].size, 8);
        if let NDDataBuffer::F64(ref v) = result.data {
            // DC should be ~0
            assert!(v[0].abs() < 1e-10);
            // k=3 should have magnitude N/2 / N = 8/16 = 0.5
            assert!(
                (v[3] - 0.5).abs() < 1e-10,
                "k=3 magnitude = {}, expected 0.5",
                v[3]
            );
            // Other bins in first half should be ~0
            for k in [1, 2, 4, 5, 6, 7] {
                assert!(
                    v[k].abs() < 1e-10,
                    "k={} magnitude = {}, expected ~0",
                    k,
                    v[k]
                );
            }
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_processor_with_config() {
        let config = FFTConfig {
            direction: FFTDirection::Forward,
            suppress_dc: true,
            num_average: 0,
        };
        let proc = FFTProcessor::with_config(config);
        let pool = NDArrayPool::new(0);

        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..8 {
                v[i] = 5.0;
            }
        }

        let result = proc.process_array(&Arc::new(arr), &pool);
        assert_eq!(result.output_arrays.len(), 1);
        if let NDDataBuffer::F64(ref v) = result.output_arrays[0].data {
            // suppress_dc: DC should be 0
            assert!(v[0].abs() < 1e-15);
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_processor_averaging() {
        let config = FFTConfig {
            direction: FFTDirection::Forward,
            suppress_dc: false,
            num_average: 2,
        };
        let proc = FFTProcessor::with_config(config);
        let pool = NDArrayPool::new(0);

        // Frame 1: constant = 2.0 => DC magnitude (normalized) = 2.0
        let mut arr1 = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr1.data {
            for i in 0..8 {
                v[i] = 2.0;
            }
        }

        // Frame 2: constant = 4.0 => DC magnitude (normalized) = 4.0
        let mut arr2 = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr2.data {
            for i in 0..8 {
                v[i] = 4.0;
            }
        }

        let r1 = proc.process_array(&Arc::new(arr1), &pool);
        assert_eq!(r1.output_arrays.len(), 1);
        // After 1 frame: exponential avg with N=1, so output = 2.0
        if let NDDataBuffer::F64(ref v) = r1.output_arrays[0].data {
            assert!((v[0] - 2.0).abs() < 1e-10, "partial avg DC = {}", v[0]);
        }

        let r2 = proc.process_array(&Arc::new(arr2), &pool);
        assert_eq!(r2.output_arrays.len(), 1);
        // After 2 frames: exp avg = 2.0*(1-1/2) + 4.0*(1/2) = 1.0 + 2.0 = 3.0
        if let NDDataBuffer::F64(ref v) = r2.output_arrays[0].data {
            assert!((v[0] - 3.0).abs() < 1e-10, "averaged DC = {}", v[0]);
        }
    }

    #[test]
    fn test_processor_averaging_dimension_change_resets() {
        let config = FFTConfig {
            direction: FFTDirection::Forward,
            suppress_dc: false,
            num_average: 3,
        };
        let proc = FFTProcessor::with_config(config);
        let pool = NDArrayPool::new(0);

        // Frame 1: width=8
        let mut arr1 = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr1.data {
            for i in 0..8 {
                v[i] = 1.0;
            }
        }
        let _ = proc.process_array(&Arc::new(arr1), &pool);
        assert_eq!(proc.state.lock().avg_count, 1);

        // Frame 2: width=4 — dimension change should reset
        let mut arr2 = NDArray::new(vec![NDDimension::new(4)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr2.data {
            for i in 0..4 {
                v[i] = 1.0;
            }
        }
        let _ = proc.process_array(&Arc::new(arr2), &pool);
        // After dimension change, avg_count should be 1 (reset + one new frame)
        assert_eq!(proc.state.lock().avg_count, 1);
    }

    #[test]
    fn test_fft_1d_multirow() {
        // 2 rows, each a different constant
        let w = 4;
        let h = 2;
        let mut arr = NDArray::new(
            vec![NDDimension::new(w), NDDimension::new(h)],
            NDDataType::Float64,
        );
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            // Row 0: all 1.0
            for i in 0..w {
                v[i] = 1.0;
            }
            // Row 1: all 3.0
            for i in w..2 * w {
                v[i] = 3.0;
            }
        }

        let result = fft_1d_rows(&pool(), &arr, false).unwrap().unwrap();
        let n_freq = w / 2; // half spectrum
        assert_eq!(result.dims[0].size, n_freq);
        if let NDDataBuffer::F64(ref v) = result.data {
            // Row 0 DC = 4*1/4 = 1.0 (normalized by N=4)
            assert!((v[0] - 1.0).abs() < 1e-10);
            // Row 1 DC = 4*3/4 = 3.0
            assert!((v[n_freq] - 3.0).abs() < 1e-10);
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_inverse_fft_1d() {
        // IFFT of a known forward FFT should give back the original magnitudes
        // For a real constant signal, forward FFT gives [N, 0, 0, ...0]
        // IFFT of [N, 0, ...0] (real input) should give constant = 1.0 for each sample
        let n = 8;
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v[0] = 8.0; // DC = N
            // rest are 0
        }

        let config = FFTConfig {
            direction: FFTDirection::Inverse,
            suppress_dc: false,
            num_average: 0,
        };
        let proc = FFTProcessor::with_config(config);
        let pool = NDArrayPool::new(0);

        let result = proc.process_array(&Arc::new(arr), &pool);
        assert_eq!(result.output_arrays.len(), 1);
        if let NDDataBuffer::F64(ref v) = result.output_arrays[0].data {
            // Each sample should be magnitude 1.0 (8/8 = 1.0 after normalization)
            for i in 0..n {
                assert!(
                    (v[i] - 1.0).abs() < 1e-10,
                    "sample {} = {}, expected 1.0",
                    i,
                    v[i]
                );
            }
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_fft_preserves_metadata() {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::Float64);
        arr.unique_id = 42;
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v[0] = 1.0;
        }

        let result = fft_1d_rows(&pool(), &arr, false).unwrap().unwrap();
        assert_eq!(result.unique_id, 42);
        assert_eq!(result.timestamp, arr.timestamp);
    }

    #[test]
    fn test_next_pow2() {
        assert_eq!(next_pow2(0), 1);
        assert_eq!(next_pow2(1), 1);
        assert_eq!(next_pow2(2), 2);
        assert_eq!(next_pow2(3), 4);
        assert_eq!(next_pow2(5), 8);
        assert_eq!(next_pow2(8), 8);
        assert_eq!(next_pow2(100), 128);
    }

    #[test]
    fn test_fft_1d_pads_to_power_of_two() {
        // Regression: a non-power-of-2 width is zero-padded to the next
        // power of two; nFreqX = paddedWidth / 2.
        let n = 5; // -> padded to 8 -> n_freq = 4
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..n {
                v[i] = 1.0;
            }
        }
        let result = fft_1d_rows(&pool(), &arr, false).unwrap().unwrap();
        assert_eq!(result.dims[0].size, 4); // 8 / 2, not 5 / 2 = 2
    }

    #[test]
    fn test_fft_2d_pads_to_power_of_two() {
        // 6x3 -> padded 8x4 -> n_freq 4x2.
        let arr = NDArray::new(
            vec![NDDimension::new(6), NDDimension::new(3)],
            NDDataType::Float64,
        );
        let result = fft_2d(&pool(), &arr, false).unwrap().unwrap();
        assert_eq!(result.dims[0].size, 4); // 8 / 2
        assert_eq!(result.dims[1].size, 2); // 4 / 2
    }

    #[test]
    fn test_adp9_processor_selects_2d_fft_from_input_rank() {
        // C dispatches on ndims (NDPluginFFT.cpp:298-315): a 2-D input drives a
        // full 2-D FFT, NOT per-row 1-D FFTs. The processor must produce 2-D
        // magnitude dims nFreqX x nFreqY ([2,2] for a 4x4 input), not [2,4].
        let proc = FFTProcessor::new();
        let pool = NDArrayPool::new(0);

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::Float64,
        );
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v.iter_mut().for_each(|x| *x = 2.0);
        }
        let result = proc.process_array(&Arc::new(arr), &pool);
        assert_eq!(result.output_arrays.len(), 1);
        let out = &result.output_arrays[0];
        assert_eq!(out.dims.len(), 2);
        assert_eq!(out.dims[0].size, 2); // nFreqX = 4/2
        assert_eq!(out.dims[1].size, 2); // nFreqY = 4/2 (per-row 1-D would be 4)
        if let NDDataBuffer::F64(ref v) = out.data {
            // 2-D DC: 32 / 16 = 2.0.
            assert!((v[0] - 2.0).abs() < 1e-10, "DC = {}", v[0]);
        }
    }

    #[test]
    fn test_adp9_processor_keeps_1d_input_1d() {
        // A 1-D input still drives a 1-D FFT (ndims==1): dims [nFreqX].
        let proc = FFTProcessor::new();
        let pool = NDArrayPool::new(0);
        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v.iter_mut().for_each(|x| *x = 1.0);
        }
        let result = proc.process_array(&Arc::new(arr), &pool);
        let out = &result.output_arrays[0];
        assert_eq!(out.dims.len(), 1);
        assert_eq!(out.dims[0].size, 4); // 8/2
    }

    #[test]
    fn test_adp9_processor_rejects_rank_above_2() {
        // ndims>2 is rejected with no NDArray and no waveforms (C error+return
        // before allocate/compute/callbacks).
        let proc = fft_proc_with_params(FFTConfig::default());
        let pool = NDArrayPool::new(0);
        let arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(4),
            ],
            NDDataType::Float64,
        );
        let result = proc.process_array(&Arc::new(arr), &pool);
        assert_eq!(result.output_arrays.len(), 0);
        assert!(
            result.param_updates.is_empty(),
            "rank>2 must emit no waveforms, got {} updates",
            result.param_updates.len()
        );
    }

    // ---- FFT waveform emission tests ----

    use ad_core_rs::plugin::runtime::ParamUpdate;

    /// Register the FFT params on a scratch port and return the processor.
    fn fft_proc_with_params(config: FFTConfig) -> FFTProcessor {
        let mut proc = FFTProcessor::with_config(config);
        let mut base =
            asyn_rs::port::PortDriverBase::new("FFT_TEST", 1, asyn_rs::port::PortFlags::default());
        proc.register_params(&mut base).unwrap();
        proc
    }

    /// Find a Float64Array update by param reason.
    fn find_array_update(updates: &[ParamUpdate], reason: usize) -> Option<&[f64]> {
        updates.iter().find_map(|u| match u {
            ParamUpdate::Float64Array {
                reason: r, value, ..
            } if *r == reason => Some(value.as_slice()),
            _ => None,
        })
    }

    #[test]
    fn test_fft_emits_all_waveforms() {
        // A forward FFT must emit FFTTimeSeries, FFTReal, FFTImaginary,
        // FFTAbsValue, FFTTimeAxis and FFTFreqAxis waveforms.
        let proc = fft_proc_with_params(FFTConfig::default());
        let pool = NDArrayPool::new(0);

        let n = 16;
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..n {
                v[i] = (2.0 * std::f64::consts::PI * 3.0 * i as f64 / n as f64).cos();
            }
        }
        let result = proc.process_array(&Arc::new(arr), &pool);
        let u = &result.param_updates;

        // All six FFT waveforms must be present, addressed by their param
        // reasons, and carry non-empty payloads.
        for reason in [
            proc.params.time_series.unwrap(),
            proc.params.real.unwrap(),
            proc.params.imaginary.unwrap(),
            proc.params.abs_value.unwrap(),
            proc.params.time_axis.unwrap(),
            proc.params.freq_axis.unwrap(),
        ] {
            let wf = find_array_update(u, reason)
                .unwrap_or_else(|| panic!("missing waveform for reason {reason}"));
            assert!(!wf.is_empty(), "waveform {reason} is empty");
        }
        let array_updates = u
            .iter()
            .filter(|x| matches!(x, ParamUpdate::Float64Array { .. }))
            .count();
        assert_eq!(
            array_updates, 6,
            "expected 6 waveform updates, got {array_updates}"
        );
    }

    #[test]
    fn test_fft_real_imaginary_match_spectrum() {
        // For a cosine at frequency 3 in N=16, the real part peaks at bin 3
        // (cosine -> real, even) and the imaginary part is ~0 everywhere.
        let proc = fft_proc_with_params(FFTConfig::default());
        let real_reason = proc.params.real.unwrap();
        let imag_reason = proc.params.imaginary.unwrap();
        let abs_reason = proc.params.abs_value.unwrap();
        let ts_reason = proc.params.time_series.unwrap();
        let pool = NDArrayPool::new(0);

        let n = 16;
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..n {
                v[i] = (2.0 * std::f64::consts::PI * 3.0 * i as f64 / n as f64).cos();
            }
        }
        let result = proc.process_array(&Arc::new(arr), &pool);
        let u = &result.param_updates;

        let real = find_array_update(u, real_reason).unwrap();
        let imag = find_array_update(u, imag_reason).unwrap();
        let abs = find_array_update(u, abs_reason).unwrap();
        let ts = find_array_update(u, ts_reason).unwrap();

        // n_freq = 16/2 = 8.
        assert_eq!(real.len(), 8);
        assert_eq!(imag.len(), 8);
        // Real part of a cosine: peak at bin 3 (= N/2 = 8), zero elsewhere.
        assert!((real[3] - 8.0).abs() < 1e-9, "real[3] = {}", real[3]);
        for k in [0usize, 1, 2, 4, 5, 6, 7] {
            assert!(real[k].abs() < 1e-9, "real[{k}] = {}", real[k]);
            assert!(imag[k].abs() < 1e-9, "imag[{k}] = {}", imag[k]);
        }
        // imag[3] is also ~0 for a pure cosine.
        assert!(imag[3].abs() < 1e-9, "imag[3] = {}", imag[3]);
        // FFTAbsValue at bin 3: magnitude 8 normalized by N=16 -> 0.5.
        assert!((abs[3] - 0.5).abs() < 1e-9, "abs[3] = {}", abs[3]);
        // Time series is the raw input row.
        assert_eq!(ts.len(), n);
        assert!((ts[0] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_fft_axes_scale_with_time_per_point() {
        // FFTTimeAxis = i*timePerPoint; FFTFreqAxis step = 0.5/tpp/(nFreq-1).
        let proc = fft_proc_with_params(FFTConfig::default());
        let time_axis_reason = proc.params.time_axis.unwrap();
        let freq_axis_reason = proc.params.freq_axis.unwrap();
        let tpp_reason = proc.params.time_per_point.unwrap();
        let pool = NDArrayPool::new(0);

        // Set timePerPoint = 0.5 s.
        use ad_core_rs::plugin::runtime::{ParamChangeValue, PluginParamSnapshot};
        proc.on_param_change(
            tpp_reason,
            &PluginParamSnapshot {
                enable_callbacks: true,
                reason: tpp_reason,
                addr: 0,
                value: ParamChangeValue::Float64(0.5),
            },
        );

        let n = 8;
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v[0] = 1.0;
        }
        let result = proc.process_array(&Arc::new(arr), &pool);
        let u = &result.param_updates;

        let time_axis = find_array_update(u, time_axis_reason).unwrap();
        let freq_axis = find_array_update(u, freq_axis_reason).unwrap();

        // Time axis: 8 points stepped by 0.5.
        assert_eq!(time_axis.len(), 8);
        assert!((time_axis[1] - 0.5).abs() < 1e-12);
        assert!((time_axis[7] - 3.5).abs() < 1e-12);
        // Freq axis: 4 bins, step = 0.5 / 0.5 / (4-1) = 1/3.
        assert_eq!(freq_axis.len(), 4);
        let step = 0.5 / 0.5 / 3.0;
        assert!((freq_axis[1] - step).abs() < 1e-12);
        assert!((freq_axis[3] - 3.0 * step).abs() < 1e-12);
    }

    #[test]
    fn test_adp25_timeseries_and_timeaxis_use_padded_length() {
        // C posts FFTTimeSeries and FFTTimeAxis at nTimeX = nextPow2(width),
        // zero-extending the series (NDPluginFFT.cpp allocateArrays +
        // doArrayCallbacks/createAxisArrays). width=5 -> padded 8.
        let proc = fft_proc_with_params(FFTConfig::default());
        let ts_reason = proc.params.time_series.unwrap();
        let time_axis_reason = proc.params.time_axis.unwrap();
        let real_reason = proc.params.real.unwrap();
        let freq_axis_reason = proc.params.freq_axis.unwrap();
        let pool = NDArrayPool::new(0);

        let n = 5;
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i + 1) as f64; // 1,2,3,4,5
            }
        }
        let result = proc.process_array(&Arc::new(arr), &pool);
        let u = &result.param_updates;

        let ts = find_array_update(u, ts_reason).unwrap();
        let time_axis = find_array_update(u, time_axis_reason).unwrap();
        let real = find_array_update(u, real_reason).unwrap();
        let freq_axis = find_array_update(u, freq_axis_reason).unwrap();

        // TimeSeries padded to nTimeX=8, zero-extended past the 5 inputs.
        assert_eq!(ts.len(), 8);
        assert_eq!(&ts[..5], &[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(&ts[5..], &[0.0, 0.0, 0.0]);
        // TimeAxis matches the padded length.
        assert_eq!(time_axis.len(), 8);
        // Real spectrum and FreqAxis stay at nFreqX = padded/2 = 4.
        assert_eq!(real.len(), 4);
        assert_eq!(freq_axis.len(), 4);
    }

    #[test]
    fn test_fft_inverse_emits_no_spectrum_waveforms() {
        // The inverse transform has no spectrum to publish.
        let config = FFTConfig {
            direction: FFTDirection::Inverse,
            suppress_dc: false,
            num_average: 0,
        };
        let proc = fft_proc_with_params(config);
        let pool = NDArrayPool::new(0);
        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v[0] = 8.0;
        }
        let result = proc.process_array(&Arc::new(arr), &pool);
        let array_updates = result
            .param_updates
            .iter()
            .filter(|x| matches!(x, ParamUpdate::Float64Array { .. }))
            .count();
        assert_eq!(
            array_updates, 0,
            "inverse FFT must not emit spectrum waveforms"
        );
    }

    #[test]
    fn test_inverse_fft_preserves_sign() {
        // Regression: the inverse transform must yield signed real samples.
        // Build a spectrum whose inverse is a signed cosine and verify the
        // output contains negative values (the old code took the modulus).
        let n = 8;
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            // Spectrum with a single non-DC bin: inverse is a real cosine
            // that swings negative.
            v[1] = 4.0;
            v[n - 1] = 4.0;
        }
        let config = FFTConfig {
            direction: FFTDirection::Inverse,
            suppress_dc: false,
            num_average: 0,
        };
        let proc = FFTProcessor::with_config(config);
        let pool = NDArrayPool::new(0);
        let result = proc.process_array(&Arc::new(arr), &pool);
        if let NDDataBuffer::F64(ref v) = result.output_arrays[0].data {
            let has_negative = v.iter().any(|&x| x < -1e-6);
            assert!(
                has_negative,
                "inverse FFT must keep negative samples: {v:?}"
            );
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn fft_output_comes_from_the_pool_and_is_reused() {
        let pool = pool();
        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v.fill(1.0);
        }

        let first = fft_1d_rows(&pool, &arr, false).unwrap().unwrap();
        assert_eq!(first.pool_id(), pool.id());
        let NDDataBuffer::F64(v) = &first.data else {
            panic!("expected F64 output");
        };
        assert!((v[0] - 1.0).abs() < 1e-10);
        let ptr = v.as_ptr();
        drop(first);

        let second = fft_1d_rows(&pool, &arr, true).unwrap().unwrap();
        let NDDataBuffer::F64(v) = &second.data else {
            panic!("expected F64 output");
        };
        assert_eq!(v[0], 0.0, "DC suppressed on the reused buffer");
        assert_eq!(v.as_ptr(), ptr, "the second frame reuses the freed buffer");
        assert_eq!(pool.num_alloc_buffers(), 1);
    }
}
