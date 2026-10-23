use asyn_rs::port::InterfaceMask;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use ad_core_rs::error::ADResult;
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::pixel_cast::PixelCast;
use ad_core_rs::plugin::runtime::{ArrayCallbacks, NDPluginProcess, ParamUpdate, ProcessResult};
use parking_lot::Mutex;
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::par_util;

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

/// The output array for `values`, as C's gated block (NDPluginFFT.cpp:212-218):
/// a pool `alloc`, the averaged values copied in, and `src`'s attributes. C
/// computes into its own buffers and allocates the output only inside
/// `if (arrayCallbacks)` (:209-212), and stamps it with `getTimeStamp(&epicsTS)`,
/// the port timestamp `beginProcessCallbacks` set from the input's epicsTS
/// (:216, :295; NDPluginDriver.cpp:232), and `timeStamp` with the wall clock
/// (:217-218), so the pair disagrees by design. `unique_id` stays at the pool
/// default: [`FFTProcessor`] owns that counter.
fn pool_output(
    pool: &NDArrayPool,
    src: &NDArray,
    dims: Vec<NDDimension>,
    values: &[f64],
) -> ADResult<NDArray> {
    let mut arr = pool.alloc(dims, NDDataType::Float64)?;
    let NDDataBuffer::F64(out) = &mut arr.data else {
        unreachable!("the output was allocated as Float64");
    };
    to_f64_into(out, values, jobs_for(values.len()));
    arr.timestamp = src.timestamp;
    arr.time_stamp = ad_core_rs::timestamp::EpicsTimestamp::now().as_f64();
    arr.attributes = src.attributes.clone();
    Ok(arr)
}

/// One frame's working buffers. [`FFTProcessor`] hands a frame the set an
/// earlier frame used, so a frame does not fault in fresh frame-sized pages.
#[derive(Default)]
struct Buffers {
    /// The transform's complex working array.
    data: Vec<Complex<f64>>,
    /// The gathered columns of [`fft_columns`].
    cols: Vec<Complex<f64>>,
    /// The spectrum, or the inverse transform's samples.
    out: Vec<f64>,
}

/// [`Buffers`] and the frame's samples as `f64`, apart so a transform reads
/// the samples while it writes the buffers.
#[derive(Default)]
struct Scratch {
    vals: Vec<f64>,
    bufs: Buffers,
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

    /// Transform every chunk of `fft.len()` elements of `buf` in place.
    fn run(&mut self, buf: &mut [Complex<f64>]) {
        self.fft.process_with_scratch(buf, &mut self.scratch);
    }
}

/// The pool jobs a transform over `elements` values may run as: 1, in order
/// on this thread, unless [`par_util::should_parallelize`] sends it to the
/// shared pool.
fn jobs_for(elements: usize) -> usize {
    if par_util::should_parallelize(elements) {
        #[cfg(feature = "parallel")]
        return par_util::thread_pool().current_num_threads();
    }
    1
}

/// `f(state, i, chunk)` for chunk `i` of the `len`-element chunks of `out`:
/// in order with one `state` when `jobs` is 1, else on the shared pool with
/// a `state` from `init` per pool job. A chunk's result is the same
/// whichever job computes it, so the output does not depend on `jobs`.
fn for_each_chunk<T: Send, S>(
    out: &mut [T],
    len: usize,
    jobs: usize,
    init: impl Fn() -> S + Sync + Send,
    f: impl Fn(&mut S, usize, &mut [T]) + Sync + Send,
) {
    if jobs > 1 {
        #[cfg(feature = "parallel")]
        return par_util::thread_pool().install(|| {
            use rayon::prelude::*;
            out.par_chunks_mut(len)
                .enumerate()
                .for_each_init(&init, |s, (i, c)| f(s, i, c));
        });
    }
    let mut s = init();
    for (i, c) in out.chunks_mut(len).enumerate() {
        f(&mut s, i, c);
    }
}

/// `buf` resized to `n` elements, new ones `fill`, written by the pool when
/// `jobs` is above 1. New pages fault in on first touch, which on one thread
/// costs more than the transforms the pool runs over them.
fn resize<T: Copy + Send + Sync>(buf: &mut Vec<T>, n: usize, fill: T, jobs: usize) {
    if jobs > 1 && n > buf.len() {
        #[cfg(feature = "parallel")]
        return par_util::thread_pool().install(|| {
            use rayon::prelude::*;
            let more = n - buf.len();
            buf.par_extend((0..more).into_par_iter().map(|_| fill));
        });
    }
    buf.resize(n, fill);
}

/// `v` as `f64` into `out`, each sample cast as `to_f64_vec` casts it.
fn to_f64_into<T: PixelCast + Sync>(out: &mut [f64], v: &[T], jobs: usize) {
    const CHUNK: usize = 1 << 16;
    for_each_chunk(
        out,
        CHUNK,
        jobs,
        || (),
        |(), i, out| {
            for (o, &x) in out.iter_mut().zip(&v[i * CHUNK..]) {
                *o = x.to_f64();
            }
        },
    );
}

/// The samples of `data` as `f64` into `vals`, on the pool for a frame
/// [`jobs_for`] sends there.
fn load_f64(data: &NDDataBuffer, vals: &mut Vec<f64>) {
    let jobs = jobs_for(data.len());
    resize(vals, data.len(), 0.0, jobs);
    ad_core_rs::with_buffer!(data, |v| to_f64_into(vals, v, jobs))
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
///
/// A block's columns are strided through every row, so blocks cannot be
/// handed out as disjoint slices of `data`. Each wave of up to `jobs` blocks
/// is gathered and transformed into `cols`, one part per block, then every
/// row takes the wave's columns back: one job per block, then one job per
/// row.
fn fft_columns(
    fft: &Arc<dyn Fft<f64>>,
    data: &mut [Complex<f64>],
    w: usize,
    jobs: usize,
    cols: &mut Vec<Complex<f64>>,
) {
    const COLS: usize = 8;
    let h = fft.len();
    let blocks = w.div_ceil(COLS);
    let wave = jobs.min(blocks);
    // Column `j` of a wave, counted from its first, sits at `j * h`; the
    // gather writes every column the transforms then read.
    resize(cols, wave * COLS * h, Complex::new(0.0, 0.0), jobs);
    for b0 in (0..blocks).step_by(wave) {
        let c0 = b0 * COLS;
        let c1 = w.min(c0 + wave * COLS);
        let src = &*data;
        for_each_chunk(
            &mut cols[..(c1 - c0) * h],
            COLS * h,
            jobs,
            || Plan::new(fft.clone()),
            |plan, i, cols| {
                let c0 = c0 + i * COLS;
                let nc = cols.len() / h;
                for (row, r) in src.chunks_exact(w).enumerate() {
                    for (k, &v) in r[c0..c0 + nc].iter().enumerate() {
                        cols[k * h + row] = v;
                    }
                }
                plan.run(cols);
            },
        );
        let cols = &*cols;
        for_each_chunk(
            data,
            w,
            jobs,
            || (),
            |(), row, r| {
                for (j, v) in r[c0..c1].iter_mut().enumerate() {
                    *v = cols[j * h + row];
                }
            },
        );
    }
}

/// Compute 1D FFT magnitude for each row of a 2D array using rustfft.
/// Returns a Float64 array with half the *padded* width (positive frequencies
/// only). Like C++ NDPluginFFT, each row is zero-padded to the next power of
/// two before the transform, and `nFreqX = paddedWidth / 2`.
/// Magnitudes are normalized by the padded length.
pub fn fft_1d_rows(src: &NDArray, suppress_dc: bool) -> Option<NDArray> {
    let mut planner = FftPlanner::<f64>::new();
    let mut s = Scratch::default();
    load_f64(&src.data, &mut s.vals);
    let dims = fft_1d_rows_with(
        |n| planner.plan_fft_forward(n),
        jobs_for,
        src,
        &s.vals,
        suppress_dc,
        &mut s.bufs,
    )?;
    Some(NDArray::with_data(dims, NDDataBuffer::F64(s.bufs.out)))
}

/// [`fft_1d_rows`] on `vals`, the samples of `src` as `f64`, into
/// `bufs.out`, returning the spectrum's dims. The forward plan for the
/// padded width comes from `plan` and the pool jobs for a transform over a
/// given number of values from `jobs`.
fn fft_1d_rows_with(
    plan: impl FnOnce(usize) -> Arc<dyn Fft<f64>>,
    jobs: fn(usize) -> usize,
    src: &NDArray,
    vals: &[f64],
    suppress_dc: bool,
    bufs: &mut Buffers,
) -> Option<Vec<NDDimension>> {
    if src.dims.is_empty() {
        return None;
    }

    let width = src.dims[0].size;
    let height = if src.dims.len() >= 2 {
        src.dims[1].size
    } else {
        1
    };

    if width == 0 {
        return None;
    }

    // C++ rounds the time dimension up to the next power of two and zero-pads.
    let padded = next_pow2(width);

    // C++: nFreqX = paddedWidth / 2 (only positive frequencies)
    let n_freq = padded / 2;
    if n_freq == 0 {
        return None;
    }
    let fft = plan(padded);

    let dims = if height > 1 {
        vec![NDDimension::new(n_freq), NDDimension::new(height)]
    } else {
        vec![NDDimension::new(n_freq)]
    };
    let row_jobs = jobs(height * padded);
    let magnitudes = &mut bufs.out;
    resize(magnitudes, n_freq * height, 0.0, row_jobs);

    // The rows are real, so each transform carries two of them; an odd
    // height leaves the last chunk one row.
    let zero = Complex::new(0.0, 0.0);
    for_each_chunk(
        magnitudes,
        2 * n_freq,
        row_jobs,
        || {
            let plan = Plan::new(fft.clone());
            (
                plan,
                vec![zero; padded],
                vec![zero; n_freq],
                vec![zero; n_freq],
            )
        },
        |(plan, row_buf, a_bins, b_bins), pair, mags| {
            if mags.len() == n_freq {
                load_row(row_buf, vals, height - 1, width);
                plan.run(row_buf);
                magnitudes_into(mags, &row_buf[..n_freq], padded as f64);
                if suppress_dc {
                    mags[0] = 0.0;
                }
                return;
            }
            load_row_pair(row_buf, vals, 2 * pair, width);
            plan.run(row_buf);
            unpack_pair(row_buf, a_bins, b_bins);
            let (ma, mb) = mags.split_at_mut(n_freq);
            magnitudes_into(ma, a_bins, padded as f64);
            magnitudes_into(mb, b_bins, padded as f64);
            if suppress_dc {
                ma[0] = 0.0;
                mb[0] = 0.0;
            }
        },
    );

    Some(dims)
}

/// Compute 2D FFT magnitude using separable row-then-column FFT via rustfft.
pub fn fft_2d(src: &NDArray, suppress_dc: bool) -> Option<NDArray> {
    let mut planner = FftPlanner::<f64>::new();
    let mut s = Scratch::default();
    load_f64(&src.data, &mut s.vals);
    let dims = fft_2d_with(
        |n| planner.plan_fft_forward(n),
        jobs_for,
        src,
        &s.vals,
        suppress_dc,
        &mut s.bufs,
    )?;
    Some(NDArray::with_data(dims, NDDataBuffer::F64(s.bufs.out)))
}

/// [`fft_2d`] as [`fft_1d_rows_with`] runs [`fft_1d_rows`], with the
/// forward plans for the padded width and height from `plan`.
fn fft_2d_with(
    mut plan: impl FnMut(usize) -> Arc<dyn Fft<f64>>,
    jobs: fn(usize) -> usize,
    src: &NDArray,
    vals: &[f64],
    suppress_dc: bool,
    bufs: &mut Buffers,
) -> Option<Vec<NDDimension>> {
    if src.dims.len() < 2 {
        return None;
    }

    let src_w = src.dims[0].size;
    let src_h = src.dims[1].size;

    if src_w == 0 || src_h == 0 {
        return None;
    }

    // C++ zero-pads each dimension to the next power of two.
    let w = next_pow2(src_w);
    let h = next_pow2(src_h);

    // C++: nFreqX = paddedX/2, nFreqY = paddedY/2; normalize by padded N*M
    let n_freq_x = w / 2;
    let n_freq_y = h / 2;
    if n_freq_x == 0 || n_freq_y == 0 {
        return None;
    }

    let rows = plan(w);
    let cols = plan(h);

    // The rows are real, so each row transform carries two of them, and
    // only bins below nFreqX reach the output, so the column transforms run
    // over those bins alone: `data` is nFreqX wide and h tall, its padding
    // rows zero as the transform of zeros. A reused `data` holds an earlier
    // frame there.
    let zero = Complex::new(0.0, 0.0);
    let col_jobs = jobs(n_freq_x * h);
    let data = &mut bufs.data;
    resize(data, n_freq_x * h, zero, col_jobs);
    let (rows_in, padding) = data.split_at_mut(src_h * n_freq_x);
    for_each_chunk(padding, n_freq_x, col_jobs, || (), |(), _, r| r.fill(zero));
    for_each_chunk(
        rows_in,
        2 * n_freq_x,
        jobs(src_h * w),
        || (Plan::new(rows.clone()), vec![zero; w]),
        |(plan, row_buf), pair, bins| {
            if bins.len() == n_freq_x {
                load_row(row_buf, vals, src_h - 1, src_w);
                plan.run(row_buf);
                bins.copy_from_slice(&row_buf[..n_freq_x]);
                return;
            }
            load_row_pair(row_buf, vals, 2 * pair, src_w);
            plan.run(row_buf);
            let (a, b) = bins.split_at_mut(n_freq_x);
            unpack_pair(row_buf, a, b);
        },
    );
    fft_columns(&cols, data, n_freq_x, col_jobs, &mut bufs.cols);

    let data = &*data;
    let magnitudes = &mut bufs.out;
    resize(magnitudes, n_freq_x * n_freq_y, 0.0, col_jobs);
    for_each_chunk(
        magnitudes,
        n_freq_x,
        col_jobs,
        || (),
        |(), fy, mags| {
            magnitudes_into(
                mags,
                &data[fy * n_freq_x..(fy + 1) * n_freq_x],
                (w * h) as f64,
            );
        },
    );

    if suppress_dc {
        magnitudes[0] = 0.0;
    }

    Some(vec![NDDimension::new(n_freq_x), NDDimension::new(n_freq_y)])
}

/// The inverse transform of each `width`-wide row of `vals`, scaled by
/// `1/width`, into `bufs.out`, with the inverse plan from `plan` and the
/// pool jobs as [`fft_1d_rows_with`] takes them.
fn fft_1d_rows_inverse_with(
    plan: impl FnOnce(usize) -> Arc<dyn Fft<f64>>,
    jobs: fn(usize) -> usize,
    src: &NDArray,
    vals: &[f64],
    suppress_dc: bool,
    bufs: &mut Buffers,
) -> Option<Vec<NDDimension>> {
    if src.dims.is_empty() {
        return None;
    }

    let width = src.dims[0].size;
    let height = if src.dims.len() >= 2 {
        src.dims[1].size
    } else {
        1
    };

    if width == 0 {
        return None;
    }

    let fft = plan(width);
    let scale = 1.0 / width as f64;

    // An inverse transform of a real-valued spectrum yields signed real
    // samples: take the real part, not the modulus, so negative samples
    // survive a forward->inverse round trip.
    let row_jobs = jobs(width * height);
    let samples = &mut bufs.out;
    resize(samples, width * height, 0.0, row_jobs);

    let zero = Complex::new(0.0, 0.0);
    for_each_chunk(
        samples,
        width,
        row_jobs,
        || (Plan::new(fft.clone()), vec![zero; width]),
        |(plan, row_buf), row, out| {
            load_row(row_buf, vals, row, width);
            if suppress_dc {
                row_buf[0] = zero;
            }
            plan.run(row_buf);
            for (s, c) in out.iter_mut().zip(&*row_buf) {
                *s = c.re * scale;
            }
        },
    );

    Some(src.dims.clone())
}

/// The 2-D inverse transform of `vals`, columns then rows, scaled by
/// `1/(w*h)`, into `bufs.out`, as [`fft_1d_rows_inverse_with`] runs.
fn fft_2d_inverse_with(
    mut plan: impl FnMut(usize) -> Arc<dyn Fft<f64>>,
    jobs: fn(usize) -> usize,
    src: &NDArray,
    vals: &[f64],
    suppress_dc: bool,
    bufs: &mut Buffers,
) -> Option<Vec<NDDimension>> {
    if src.dims.len() < 2 {
        return None;
    }

    let w = src.dims[0].size;
    let h = src.dims[1].size;

    if w == 0 || h == 0 {
        return None;
    }

    let rows = plan(w);
    let cols = plan(h);
    let scale = 1.0 / (w * h) as f64;

    // Every row loads in full, so a reused `data` keeps nothing of an
    // earlier frame.
    let zero = Complex::new(0.0, 0.0);
    let jobs = jobs(w * h);
    let data = &mut bufs.data;
    resize(data, w * h, zero, jobs);
    for_each_chunk(data, w, jobs, || (), |(), row, r| load_row(r, vals, row, w));

    if suppress_dc {
        data[0] = zero;
    }

    fft_columns(&cols, data, w, jobs, &mut bufs.cols);
    for_each_chunk(
        data,
        w,
        jobs,
        || Plan::new(rows.clone()),
        |plan, _, r| plan.run(r),
    );

    // Inverse transform yields signed real samples: keep the real part.
    let data = &*data;
    let samples = &mut bufs.out;
    resize(samples, w * h, 0.0, jobs);
    for_each_chunk(
        samples,
        w,
        jobs,
        || (),
        |(), row, out| {
            for (s, c) in out.iter_mut().zip(&data[row * w..]) {
                *s = c.re * scale;
            }
        },
    );
    Some(vec![NDDimension::new(w), NDDimension::new(h)])
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
    /// C `FFTAbsValue_`: the running average of the output.
    average: Vec<f64>,
    /// C `numAveraged`: the weight, `1/num_averaged`, the next frame enters
    /// the average at.
    num_averaged: usize,
    /// C `numAverage_`: the NumAverage the average last restarted for.
    averaged_for: usize,
    /// The ResetAverage param, which C reads and clears at the next frame.
    reset_average: bool,
    /// C `nTimeXIn_` and `nTimeYIn_`, with the direction, that the average
    /// holds frames of.
    averaged_shape: Option<(usize, usize, FFTDirection)>,
    /// Seconds per input time point (C++ `timePerPoint_`); scales the time
    /// and frequency axis waveforms.
    time_per_point: f64,
    /// C `nTimeXIn_`, `nTimeYIn_` and `timePerPoint_` as the axes were last
    /// posted for.
    axes_for: Option<(usize, usize, f64)>,
}

pub struct FFTProcessor {
    state: Mutex<FFTState>,
    /// C `uniqueId_` (NDPluginFFT.h:83, zeroed at NDPluginFFT.cpp:54): the
    /// plugin's own output numbering, post-incremented once per emitted array
    /// (`:219`), so the first output is 0 regardless of the input's id.
    unique_id: AtomicI32,
    /// The runtime's NDArrayCallbacks, which gates the output array.
    array_callbacks: ArrayCallbacks,
    /// Plan cache, shared across frames and across pool workers. Locked only
    /// for the plan lookup itself (one or two per frame), never across a
    /// transform.
    planner: Mutex<FftPlanner<f64>>,
    /// Free frame buffers. A frame takes a set for itself and gives it back:
    /// one shared set would serialise the NumThreads workers, and fresh
    /// buffers per frame, as C callocs them (NDPluginFFT.cpp:106-110), spend
    /// more on faulting their pages in than on the transforms.
    scratch: Mutex<Vec<Scratch>>,
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
                average: Vec::new(),
                num_averaged: 1,
                averaged_for: 0,
                reset_average: false,
                averaged_shape: None,
                time_per_point: 1.0,
                axes_for: None,
            }),
            unique_id: AtomicI32::new(0),
            array_callbacks: ArrayCallbacks::new(true),
            planner: Mutex::new(FftPlanner::new()),
            scratch: Mutex::new(Vec::new()),
            params: FFTParamIndices::default(),
        }
    }

    /// C `createAxisArrays`' two posts (NDPluginFFT.cpp:253-254) for an
    /// input `n_x` wide, padded as the transform pads it.
    fn axis_posts(&self, frame: &FFTFrame<'_>, n_x: usize) -> Vec<ParamUpdate> {
        let n_time = next_pow2(n_x);
        let time = (self.params.time_axis)
            .map(|idx| ParamUpdate::float64_array(idx, frame.time_axis(n_time)));
        let freq = (self.params.freq_axis)
            .map(|idx| ParamUpdate::float64_array(idx, frame.freq_axis(n_time / 2)));
        time.into_iter().chain(freq).collect()
    }
}

impl FFTState {
    /// Whether this frame posts the axes: C `createAxisArrays` runs when the
    /// input size (NDPluginFFT.cpp:317-322, :118) or TimePerPoint (:328-331)
    /// changed. A frame changing both posts twice in C, the first time with
    /// the old timePerPoint_ (0 on the first frame, a NaN frequency axis);
    /// rs posts once, with the current one.
    fn axes_changed(&mut self, dims: &[NDDimension]) -> bool {
        let key = (
            dims[0].size,
            dims.get(1).map_or(1, |d| d.size),
            self.time_per_point,
        );
        let changed = self.axes_for != Some(key);
        self.axes_for = Some(key);
        changed
    }

    /// Restart the average on a new input size, C's `sizeChanged`
    /// (NDPluginFFT.cpp:317-322), or a new direction. C zeroes
    /// `FFTAbsValue_` there (:111-115) but keeps `numAveraged`, so the next
    /// frames average against zeros; that C bug is not copied.
    fn check_dims_changed(&mut self, dims: &[NDDimension]) {
        let shape = (
            dims[0].size,
            dims.get(1).map_or(1, |d| d.size),
            self.config.direction,
        );
        if self.averaged_shape != Some(shape) {
            self.averaged_shape = Some(shape);
            self.num_averaged = 1;
        }
    }

    /// C `doArrayCallbacks`' average (NDPluginFFT.cpp:188-208): a reset or a
    /// new NumAverage restarts it, `values` enter at weight `1/numAveraged`
    /// and become the average, and the count C then posts as NumAveraged
    /// returns with whether a reset was consumed.
    fn average(&mut self, values: &mut [f64]) -> (usize, bool) {
        let reset = std::mem::take(&mut self.reset_average);
        if reset || self.config.num_average != self.averaged_for {
            self.averaged_for = self.config.num_average;
            self.num_averaged = 1;
        }
        let n = self.num_averaged;
        if n < self.config.num_average {
            self.num_averaged += 1;
        }
        if n > 1 {
            let new_fraction = 1.0 / n as f64;
            let old_fraction = 1.0 - new_fraction;
            for (a, v) in self.average.iter_mut().zip(values) {
                *a = *a * old_fraction + *v * new_fraction;
                *v = *a;
            }
        } else if self.num_averaged > 1 {
            // Only a frame weighted below 1 reads the average.
            self.average.clear();
            self.average.extend_from_slice(values);
        }
        (self.num_averaged, reset)
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

    /// Compute FFT using cached planner for plan reuse across frames, into
    /// `bufs.out`, returning the output's dims.
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
        src: &NDArray,
        vals: &[f64],
        bufs: &mut Buffers,
    ) -> Option<Vec<NDDimension>> {
        let suppress_dc = self.config.suppress_dc;

        match (src.dims.len(), self.config.direction) {
            (1, FFTDirection::Forward) => fft_1d_rows_with(
                |n| self.plan_forward(n),
                jobs_for,
                src,
                vals,
                suppress_dc,
                bufs,
            ),
            (1, FFTDirection::Inverse) => fft_1d_rows_inverse_with(
                |n| self.plan_inverse(n),
                jobs_for,
                src,
                vals,
                suppress_dc,
                bufs,
            ),
            (2, FFTDirection::Forward) => fft_2d_with(
                |n| self.plan_forward(n),
                jobs_for,
                src,
                vals,
                suppress_dc,
                bufs,
            ),
            (2, FFTDirection::Inverse) => fft_2d_inverse_with(
                |n| self.plan_inverse(n),
                jobs_for,
                src,
                vals,
                suppress_dc,
                bufs,
            ),
            _ => None,
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
}

impl Default for FFTProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for FFTProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_NONE;
    // NDPluginFFT.cpp:51-52, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::FLOAT64
        .or(InterfaceMask::FLOAT64_ARRAY)
        .or(InterfaceMask::GENERIC_POINTER);
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;
    fn process_array(&self, array: &Arc<NDArray>, pool: &NDArrayPool) -> ProcessResult {
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
        let (frame, post_axes) = {
            let mut state = self.state.lock();
            state.check_dims_changed(&array.dims);
            let post_axes = state.axes_changed(&array.dims);
            (
                FFTFrame {
                    config: state.config,
                    time_per_point: state.time_per_point,
                    planner: &self.planner,
                },
                post_axes,
            )
        };

        let mut scratch = self.scratch.lock().pop().unwrap_or_default();
        load_f64(&array.data, &mut scratch.vals);
        let dims = frame.compute_fft(array, &scratch.vals, &mut scratch.bufs);
        let mut updates = Vec::new();
        // C `createAxisArrays` posts with the change itself, whatever the frame
        // then computes (NDPluginFFT.cpp:118, :328-331).
        if post_axes {
            updates.extend(self.axis_posts(&frame, array.dims[0].size));
        }

        // Emit the C++ NDPluginFFT waveform records. On a forward transform
        // these are the time series and the real/imaginary/abs spectrum
        // (C++ doFFTCallbacks).
        // The inverse transform has no spectrum to publish.
        if frame.config.direction == FFTDirection::Forward {
            let suppress_dc = frame.config.suppress_dc;
            if let Some((time_series, real, imag)) =
                frame.compute_row_spectrum(array, &scratch.vals, suppress_dc)
            {
                if let Some(idx) = self.params.time_series {
                    updates.push(ParamUpdate::float64_array(idx, time_series));
                }
                if let Some(idx) = self.params.real {
                    updates.push(ParamUpdate::float64_array(idx, real));
                }
                if let Some(idx) = self.params.imaginary {
                    updates.push(ParamUpdate::float64_array(idx, imag));
                }
            }
        }

        // The average is the one part of the frame that cannot run released:
        // C re-takes the lock for it (NDPluginFFT.cpp:373) and reads
        // NumAverage and ResetAverage there (:188-190), not before the
        // transform. It advances once per frame, so the FFTAbsValue waveform
        // and the output array below share the one pass.
        let (num_averaged, reset) = {
            let values = match dims {
                Some(_) => scratch.bufs.out.as_mut_slice(),
                None => &mut [],
            };
            self.state.lock().average(values)
        };
        if let Some(idx) = self.params.num_averaged {
            updates.push(ParamUpdate::int32(idx, num_averaged as i32));
        }
        if let Some(idx) = self.params.reset_average
            && reset
        {
            updates.push(ParamUpdate::int32(idx, 0));
        }

        let output = dims.and_then(|dims| {
            let mags = &scratch.bufs.out;
            // FFTAbsValue is the first row of the (possibly averaged)
            // spectrum: C posts `MIN(nFreqX, nFreqX_)` of it
            // (NDPluginFFT.cpp:227), and the averaging restarts on a size
            // change, so the two widths agree.
            if frame.config.direction == FFTDirection::Forward
                && let Some(idx) = self.params.abs_value
                && !mags.is_empty()
            {
                let first_row = mags[..dims[0].size].to_vec();
                updates.push(ParamUpdate::float64_array(idx, first_row));
            }
            // C allocates, copies and numbers the output only
            // `if (arrayCallbacks)` (NDPluginFFT.cpp:209-220).
            self.array_callbacks
                .get()
                .then(|| pool_output(pool, array, dims, mags))
        });
        self.scratch.lock().push(scratch);
        match output {
            Some(Ok(mut out)) => {
                out.unique_id = self.unique_id.fetch_add(1, Ordering::Relaxed);
                let mut r = ProcessResult::arrays(vec![Arc::new(out)]);
                r.param_updates = updates;
                r
            }
            Some(Err(e)) => {
                tracing::warn!(error = %e, "FFT output allocation failed; dropping frame");
                ProcessResult::sink(updates)
            }
            None => ProcessResult::sink(updates),
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginFFT"
    }

    fn bind_array_callbacks(&mut self, array_callbacks: ArrayCallbacks) {
        self.array_callbacks = array_callbacks;
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
        // C posts the axes next only on another size or TimePerPoint change
        // (:118, :330), so a first frame that beat `interruptAccept` left every
        // record without them.
        for axis in [self.params.time_axis, self.params.freq_axis]
            .into_iter()
            .flatten()
        {
            base.set_state_array_rule(axis, Arc::new(|| true));
        }
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
            state.reset_average = params.value.as_i32() != 0;
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

    /// C NDPluginFFT.cpp:216-218: the spectrum's epicsTS is the input's, by
    /// way of the port timestamp; its `timeStamp` is the wall clock, not the
    /// input's double.
    #[test]
    fn the_pool_output_takes_the_input_epics_ts_and_a_fresh_time_stamp() {
        let mut src = NDArray::with_data(
            vec![NDDimension::new(4), NDDimension::new(2)],
            NDDataBuffer::F64(vec![0.0; 8]),
        );
        src.timestamp = ad_core_rs::timestamp::EpicsTimestamp { sec: 7, nsec: 11 };
        src.time_stamp = 123.5;

        let pool = NDArrayPool::new(0);
        let before = ad_core_rs::timestamp::EpicsTimestamp::now().as_f64();
        let out = pool_output(&pool, &src, src.dims.clone(), &[0.0; 8]).unwrap();

        assert_eq!(out.timestamp, src.timestamp);
        assert!(out.time_stamp >= before, "timeStamp is the wall clock");
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
        let got = fft_1d_rows(&arr, false).unwrap();
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
        let got = fft_2d(&arr, false).unwrap();
        let got_2d = match &got.data {
            NDDataBuffer::F64(v) => v.clone(),
            _ => unreachable!(),
        };
        assert_eq!(got_2d.len(), want_2d.len());
        for (g, e) in got_2d.iter().zip(&want_2d) {
            assert!((g - e).abs() <= 1e-12 * e.abs().max(1.0), "2d {g} vs {e}");
        }
    }

    fn serial(_: usize) -> usize {
        1
    }

    fn three(_: usize) -> usize {
        3
    }

    fn bits(v: &[f64]) -> Vec<u64> {
        v.iter().map(|x| x.to_bits()).collect()
    }

    /// The dims a transform returned and the bits it wrote to `bufs.out`.
    fn out_bits(dims: Option<Vec<NDDimension>>, bufs: Buffers) -> (Vec<usize>, Vec<u64>) {
        let dims = dims.unwrap().iter().map(|d| d.size).collect();
        (dims, bits(&bufs.out))
    }

    /// The 2-D inverse as one serial pass: every row loaded, the columns,
    /// then one `process` over the whole frame.
    fn whole_frame_inverse(
        planner: &mut FftPlanner<f64>,
        vals: &[f64],
        (w, h): (usize, usize),
        suppress_dc: bool,
    ) -> Vec<u64> {
        let mut data = vec![Complex::new(0.0, 0.0); w * h];
        for (row, r) in data.chunks_exact_mut(w).enumerate() {
            load_row(r, vals, row, w);
        }
        if suppress_dc {
            data[0] = Complex::new(0.0, 0.0);
        }
        fft_columns(
            &planner.plan_fft_inverse(h),
            &mut data,
            w,
            1,
            &mut Vec::new(),
        );
        Plan::new(planner.plan_fft_inverse(w)).run(&mut data);
        let scale = 1.0 / (w * h) as f64;
        data.iter().map(|c| (c.re * scale).to_bits()).collect()
    }

    /// The pool cuts the rows into pair chunks or single rows, the columns
    /// into waves of blocks and the magnitudes into rows; each runs the
    /// serial arithmetic, so the output is the serial one bit for bit, and
    /// the 2-D inverse is the whole-frame pass it replaced. Odd and even
    /// heights, non-power-of-two widths, and `jobs_for`, which takes the
    /// 100-wide frames to the whole pool.
    #[test]
    fn pooled_transforms_are_bit_identical_to_serial() {
        let mut planner = FftPlanner::<f64>::new();
        for (w, h) in [(37, 1), (5, 3), (13, 7), (200, 2), (100, 64), (100, 65)] {
            let vals: Vec<f64> = (0..w * h)
                .map(|i| ((i * 7919) % 251) as f64 - 120.0)
                .collect();
            let dims = if h > 1 { vec![w, h] } else { vec![w] };
            let arr = NDArray::with_data(
                dims.into_iter().map(NDDimension::new).collect(),
                NDDataBuffer::F64(vals.clone()),
            );
            for suppress_dc in [false, true] {
                let mut rows_1d = |jobs: fn(usize) -> usize| {
                    let mut bufs = Buffers::default();
                    let dims = fft_1d_rows_with(
                        |n| planner.plan_fft_forward(n),
                        jobs,
                        &arr,
                        &vals,
                        suppress_dc,
                        &mut bufs,
                    );
                    out_bits(dims, bufs)
                };
                let want = rows_1d(serial);
                assert_eq!(rows_1d(three), want, "1d {w}x{h}");
                assert_eq!(rows_1d(jobs_for), want, "1d {w}x{h}");
                let mut inverse_1d = |jobs: fn(usize) -> usize| {
                    let mut bufs = Buffers::default();
                    let dims = fft_1d_rows_inverse_with(
                        |n| planner.plan_fft_inverse(n),
                        jobs,
                        &arr,
                        &vals,
                        suppress_dc,
                        &mut bufs,
                    );
                    out_bits(dims, bufs)
                };
                let want = inverse_1d(serial);
                assert_eq!(inverse_1d(three), want, "1d inverse {w}x{h}");
                assert_eq!(inverse_1d(jobs_for), want, "1d inverse {w}x{h}");
                if h == 1 {
                    continue;
                }
                let mut fft_2d = |jobs: fn(usize) -> usize| {
                    let mut bufs = Buffers::default();
                    let dims = fft_2d_with(
                        |n| planner.plan_fft_forward(n),
                        jobs,
                        &arr,
                        &vals,
                        suppress_dc,
                        &mut bufs,
                    );
                    out_bits(dims, bufs)
                };
                let want = fft_2d(serial);
                assert_eq!(fft_2d(three), want, "2d {w}x{h}");
                assert_eq!(fft_2d(jobs_for), want, "2d {w}x{h}");
                let mut inverse_2d = |jobs: fn(usize) -> usize| {
                    let mut bufs = Buffers::default();
                    let dims = fft_2d_inverse_with(
                        |n| planner.plan_fft_inverse(n),
                        jobs,
                        &arr,
                        &vals,
                        suppress_dc,
                        &mut bufs,
                    );
                    out_bits(dims, bufs)
                };
                let want = inverse_2d(serial);
                assert_eq!(inverse_2d(three), want, "2d inverse {w}x{h}");
                assert_eq!(inverse_2d(jobs_for), want, "2d inverse {w}x{h}");
                assert_eq!(
                    want.1,
                    whole_frame_inverse(&mut planner, &vals, (w, h), suppress_dc),
                    "2d inverse {w}x{h} against one pass"
                );
            }
        }
    }

    /// The inverse runs `fft_columns` over the full, unpadded width: a last
    /// block of 5 columns, alone in a wave of 2 and in a wave of 3 blocks.
    #[test]
    fn pooled_columns_are_bit_identical_to_serial() {
        let (w, h) = (21, 6);
        let fft = FftPlanner::<f64>::new().plan_fft_inverse(h);
        let data: Vec<Complex<f64>> = (0..w * h)
            .map(|i| Complex::new(((i * 7919) % 251) as f64, ((i * 104729) % 97) as f64))
            .collect();
        let columns = |jobs| {
            let mut d = data.clone();
            fft_columns(&fft, &mut d, w, jobs, &mut Vec::new());
            d.iter()
                .map(|c| (c.re.to_bits(), c.im.to_bits()))
                .collect::<Vec<_>>()
        };
        let want = columns(1);
        assert_eq!(columns(2), want);
        assert_eq!(columns(3), want);
    }

    /// The pooled conversion casts each sample as `to_f64_vec` does, over
    /// several chunks and a short last one, into a reused buffer shorter and
    /// longer than the frame.
    #[test]
    fn pooled_conversion_matches_to_f64_vec() {
        let n = 3 * (1 << 16) + 5;
        let ints = (0..n as i64).map(|i| (i * 7919) % 70001 - 35000);
        for data in [
            NDDataBuffer::U16(ints.clone().map(|i| i as u16).collect()),
            NDDataBuffer::I32(ints.clone().map(|i| i as i32 * 977).collect()),
            // Past 2^53, so the cast rounds.
            NDDataBuffer::I64(ints.clone().map(|i| (i << 40) | 1).collect()),
            NDDataBuffer::F32(ints.clone().map(|i| i as f32 / 3.0).collect()),
            NDDataBuffer::F64(ints.clone().map(|i| i as f64 / 7.0).collect()),
        ] {
            let want = bits(&data.to_f64_vec());
            for start in [9, n + 9] {
                let mut vals = vec![1.0; start];
                load_f64(&data, &mut vals);
                assert_eq!(bits(&vals), want, "{:?} from {start}", data.data_type());
            }
            let mut out = vec![0.0; n];
            ad_core_rs::with_buffer!(&data, |v| to_f64_into(&mut out, v, 3));
            assert_eq!(bits(&out), want, "{:?} in 3 jobs", data.data_type());
        }
    }

    /// A processor's second frame, on the buffers its first frame filled,
    /// is a fresh processor's bit for bit. The 2-D forward pair pads to the
    /// same 128x64, so the second frame's padding rows must read zero, not
    /// the first frame's rows.
    #[test]
    fn reused_frame_buffers_keep_nothing_of_the_last_frame() {
        let frame = |w: usize, h: usize, seed: usize| {
            let dims = if h > 1 { vec![w, h] } else { vec![w] };
            Arc::new(NDArray::with_data(
                dims.into_iter().map(NDDimension::new).collect(),
                NDDataBuffer::U16(
                    (0..w * h)
                        .map(|i| ((i * 7919 + seed) % 4093) as u16 + 1)
                        .collect(),
                ),
            ))
        };
        let pool = NDArrayPool::new(0);
        let out = |proc: &FFTProcessor, arr: &Arc<NDArray>| {
            let r = proc.process_array(arr, &pool);
            match &r.output_arrays[0].data {
                NDDataBuffer::F64(v) => bits(v),
                _ => unreachable!(),
            }
        };
        for direction in [FFTDirection::Forward, FFTDirection::Inverse] {
            let config = FFTConfig {
                direction,
                ..FFTConfig::default()
            };
            for (first, second) in [
                (frame(100, 64, 1), frame(97, 33, 2)),
                (frame(250, 1, 1), frame(129, 1, 2)),
            ] {
                let reused = FFTProcessor::with_config(config);
                out(&reused, &first);
                let fresh = FFTProcessor::with_config(config);
                assert_eq!(
                    out(&reused, &second),
                    out(&fresh, &second),
                    "{direction:?} {:?}",
                    second.dims
                );
            }
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

        let result = fft_1d_rows(&arr, false).unwrap();
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

        let result = fft_1d_rows(&arr, false).unwrap();
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
        let result = fft_2d(&arr, false).unwrap();
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

        let result = fft_1d_rows(&arr, true).unwrap();
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

        let result = fft_2d(&arr, true).unwrap();
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

        let result = fft_2d(&arr, false).unwrap();
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

        let result = fft_1d_rows(&arr, false).unwrap();
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
        let arr1 = Arc::new(arr1);
        let _ = proc.process_array(&arr1, &pool);
        let _ = proc.process_array(&arr1, &pool);
        assert_eq!(proc.state.lock().num_averaged, 3);

        // Frame 3: width=4 — dimension change should reset
        let mut arr2 = NDArray::new(vec![NDDimension::new(4)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr2.data {
            for i in 0..4 {
                v[i] = 1.0;
            }
        }
        let _ = proc.process_array(&Arc::new(arr2), &pool);
        // Restarted at weight 1, so the next frame enters at 1/2.
        assert_eq!(proc.state.lock().num_averaged, 2);
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

        let result = fft_1d_rows(&arr, false).unwrap();
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

    /// C `NDPluginFFT::doArrayCallbacks` (NDPluginFFT.cpp:212-220) allocates a
    /// bare array, copies the attribute list, takes epicsTS from the port (the
    /// input's, :295) and timeStamp from the wall clock, and numbers it
    /// `uniqueId_++` — so a spectrum carries the input's attributes and
    /// epicsTS but neither its id nor its timeStamp.
    #[test]
    fn test_fft_output_identity_is_the_plugins_not_the_inputs() {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::Float64);
        arr.unique_id = 42;
        arr.timestamp = ad_core_rs::timestamp::EpicsTimestamp { sec: 7, nsec: 11 };
        arr.time_stamp = 123.5;
        arr.attributes
            .add(ad_core_rs::attributes::NDAttribute::new_static(
                "ColorMode",
                "Color mode",
                ad_core_rs::attributes::NDAttrSource::Driver,
                ad_core_rs::attributes::NDAttrValue::Int32(0),
            ));
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v[0] = 1.0;
        }

        let arr = Arc::new(arr);
        let output = FFTProcessor::new()
            .process_array(&arr, &pool())
            .output_arrays;
        let result = &output[0];
        assert_ne!(result.unique_id, 42, "the input's id was inherited");
        assert_eq!(result.timestamp, arr.timestamp);
        assert_ne!(result.time_stamp, arr.time_stamp);
        assert!(result.attributes.get("ColorMode").is_some());
    }

    /// C `getAttributes(pArrayOut->pAttributeList)` (NDPluginFFT.cpp:215)
    /// runs after `setIntegerParam(P_FFTNumAveraged, ...)` (:202): the
    /// spectrum carries the plugin's set with this frame's NumAveraged. The
    /// runtime's `endProcessCallbacks` evaluation is the same call here.
    #[test]
    fn the_spectrum_carries_the_plugin_attributes() {
        use crate::test_util::{publish, with_attributes_file};
        use ad_core_rs::attributes::NDAttrValue;
        let xml = r#"<Attributes>
            <Attribute name="Averaged" type="PARAM" source="FFT_NUM_AVERAGED" datatype="INT"/>
        </Attributes>"#;
        let (handle, mut rx, _jh) = with_attributes_file("FFT_ATTRS", FFTProcessor::new(), xml);
        let port = handle.port_runtime().port_handle();
        let num_average = port
            .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("FFT_NUM_AVERAGE", 0))
            .unwrap()
            .reason;
        port.write_int32_blocking(num_average, 0, 4).unwrap();
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v[0] = 1.0;
        }
        let mut averaged = Vec::new();
        for _ in 0..2 {
            publish(&handle, &arr);
            let out = rx.blocking_recv().unwrap();
            averaged.push(out.attributes.get("Averaged").map(|a| a.value.clone()));
        }
        let read = port
            .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("FFT_NUM_AVERAGED", 0))
            .unwrap()
            .reason;
        let last = port.read_int32_blocking(read, 0).unwrap();
        assert_eq!(averaged[1], Some(NDAttrValue::Int32(last)));
        assert_ne!(averaged[0], averaged[1]);
    }

    /// NumAveraged per frame as C's cbAD oracle posts it (NDPluginFFT.cpp:
    /// 188-203), each with whether the frame came out alone (weight 1): a
    /// reset or a new NumAverage restarts at weight 1 and posts 2, the count
    /// climbs to NumAverage, and NumAverage <= 1 posts 1. Both are read at
    /// the frame, so a reset cancelled or a NumAverage restored before it
    /// restarts nothing.
    #[test]
    fn num_averaged_follows_c() {
        use crate::test_util::{publish, with_attributes_file};
        use ad_core_rs::attributes::NDAttrValue;
        let xml = r#"<Attributes>
            <Attribute name="Averaged" type="PARAM" source="FFT_NUM_AVERAGED" datatype="INT"/>
        </Attributes>"#;
        let (handle, mut rx, _jh) = with_attributes_file("FFT_NAVG", FFTProcessor::new(), xml);
        let port = handle.port_runtime().port_handle();
        let reason = |name| {
            port.drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new(name, 0))
                .unwrap()
                .reason
        };
        let (avg, reset) = (reason("FFT_NUM_AVERAGE"), reason("FFT_RESET_AVERAGE"));
        let mut level = 0.0;
        let mut run = |writes: &[(usize, i32)], width: usize, frames: usize| {
            for &(r, v) in writes {
                port.write_int32_blocking(r, 0, v).unwrap();
            }
            assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
            (0..frames)
                .map(|_| {
                    // A constant frame's DC magnitude is its level.
                    level += 1.0;
                    let mut arr = NDArray::new(vec![NDDimension::new(width)], NDDataType::Float64);
                    if let NDDataBuffer::F64(ref mut v) = arr.data {
                        v.fill(level);
                    }
                    publish(&handle, &arr);
                    let out = rx.blocking_recv().unwrap();
                    let n = match out.attributes.get("Averaged").map(|a| &a.value) {
                        Some(NDAttrValue::Int32(n)) => *n,
                        other => panic!("Averaged = {other:?}"),
                    };
                    (n, out.data.get_as_f64(0) == Some(level))
                })
                .collect::<Vec<_>>()
        };
        let (t, f) = (true, false);
        // The template's PINI writes.
        assert_eq!(run(&[(avg, 1), (reset, 1)], 8, 3), [(1, t); 3]);
        assert_eq!(
            run(&[(avg, 4)], 8, 6),
            [(2, t), (3, f), (4, f), (4, f), (4, f), (4, f)]
        );
        assert_eq!(run(&[(reset, 1)], 8, 3), [(2, t), (3, f), (4, f)]);
        assert_eq!(
            port.read_int32_blocking(reset, 0).unwrap(),
            0,
            "C clears it (:192)"
        );
        assert_eq!(run(&[(avg, 2)], 8, 3), [(2, t), (2, f), (2, f)]);
        assert_eq!(run(&[(avg, 4)], 8, 3), [(2, t), (3, f), (4, f)]);
        assert_eq!(run(&[(reset, 1), (reset, 0)], 8, 2), [(4, f); 2]);
        assert_eq!(run(&[(avg, 3), (avg, 4)], 8, 2), [(4, f); 2]);
        assert_eq!(run(&[(avg, 0)], 8, 3), [(1, t); 3]);
        assert_eq!(run(&[(avg, 4)], 8, 3), [(2, t), (3, f), (4, f)]);
        // C posts 4, 4 here: it zeroes FFTAbsValue_ on the new size (:111-115)
        // but keeps numAveraged, so the frames average against zeros.
        assert_eq!(run(&[], 4, 2), [(2, t), (3, f)]);
    }

    /// The id comes off the plugin's own counter and runs 0, 1, 2 — C's
    /// `uniqueId_` is zeroed in the constructor (NDPluginFFT.cpp:54) and
    /// post-incremented per emitted array (`:219`), so it is independent of
    /// whatever the inputs were numbered.
    #[test]
    fn the_processor_numbers_its_outputs_from_its_own_counter() {
        let proc = FFTProcessor::new();
        let pool = NDArrayPool::new(0);

        let ids: Vec<i32> = [100, 7, 3]
            .into_iter()
            .map(|input_id| {
                let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
                arr.unique_id = input_id;
                if let NDDataBuffer::F64(ref mut v) = arr.data {
                    v.iter_mut().for_each(|s| *s = 2.0);
                }
                let r = proc.process_array(&Arc::new(arr), &pool);
                assert_eq!(r.output_arrays.len(), 1);
                r.output_arrays[0].unique_id
            })
            .collect();

        assert_eq!(ids, vec![0, 1, 2]);
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
        let result = fft_1d_rows(&arr, false).unwrap();
        assert_eq!(result.dims[0].size, 4); // 8 / 2, not 5 / 2 = 2
    }

    #[test]
    fn test_fft_2d_pads_to_power_of_two() {
        // 6x3 -> padded 8x4 -> n_freq 4x2.
        let arr = NDArray::new(
            vec![NDDimension::new(6), NDDimension::new(3)],
            NDDataType::Float64,
        );
        let result = fft_2d(&arr, false).unwrap();
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
        let mut base = asyn_rs::port::PortDriverBase::new(
            "FFT_TEST",
            1,
            asyn_rs::port::PortFlags::default(),
            asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
            asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
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

    /// C posts FFTAbsValue as the first row of the spectrum,
    /// `MIN(nFreqX, nFreqX_)` elements (NDPluginFFT.cpp:227), not the whole
    /// 2-D array the output carries.
    #[test]
    fn abs_value_is_the_first_row_of_a_2d_spectrum() {
        let proc = fft_proc_with_params(FFTConfig::default());
        let pool = NDArrayPool::new(0);
        let (w, h) = (16, 8);
        let arr = NDArray::with_data(
            vec![NDDimension::new(w), NDDimension::new(h)],
            NDDataBuffer::F64((0..w * h).map(|i| ((i * 7919) % 251) as f64).collect()),
        );
        let r = proc.process_array(&Arc::new(arr), &pool);
        let abs = find_array_update(&r.param_updates, proc.params.abs_value.unwrap()).unwrap();
        let out = &r.output_arrays[0];
        let NDDataBuffer::F64(spectrum) = &out.data else {
            panic!("expected F64 output");
        };
        let dims: Vec<usize> = out.dims.iter().map(|d| d.size).collect();
        assert_eq!(dims, [8, 4]);
        assert_eq!(abs, &spectrum[..8]);
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

    /// C posts the axes from `createAxisArrays` ahead of the spectrum
    /// (NDPluginFFT.cpp:324-331 before :224-227), and only on a frame whose
    /// input size or TimePerPoint changed.
    #[test]
    fn the_axes_post_first_and_only_when_the_size_or_time_per_point_changes() {
        use ad_core_rs::plugin::runtime::{ParamChangeValue, ParamUpdate, PluginParamSnapshot};
        let proc = fft_proc_with_params(FFTConfig::default());
        let p = &proc.params;
        let (time_axis, freq_axis) = (p.time_axis.unwrap(), p.freq_axis.unwrap());
        let spectrum = [
            p.time_series.unwrap(),
            p.real.unwrap(),
            p.imaginary.unwrap(),
            p.abs_value.unwrap(),
        ];
        let pool = NDArrayPool::new(0);
        let posted = |n: usize| -> Vec<usize> {
            let arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
            proc.process_array(&Arc::new(arr), &pool)
                .param_updates
                .iter()
                .filter_map(|u| match u {
                    ParamUpdate::Float64Array { reason, .. } => Some(*reason),
                    _ => None,
                })
                .collect()
        };
        let with_axes: Vec<usize> = [time_axis, freq_axis].into_iter().chain(spectrum).collect();
        assert_eq!(posted(8), with_axes, "first frame");
        assert_eq!(posted(8), spectrum, "nothing changed");
        let tpp = p.time_per_point.unwrap();
        proc.on_param_change(
            tpp,
            &PluginParamSnapshot {
                enable_callbacks: true,
                reason: tpp,
                addr: 0,
                value: ParamChangeValue::Float64(0.5),
            },
        );
        assert_eq!(posted(8), with_axes, "TimePerPoint changed");
        assert_eq!(posted(16), with_axes, "size changed");
        assert_eq!(posted(16), spectrum, "nothing changed again");
    }

    /// C runs `createAxisArrays` on every size or TimePerPoint change,
    /// whatever the frame then computes (NDPluginFFT.cpp:118, :328-331): it
    /// reads no P_FFTDirection, and a width-1 input posts a one-point time
    /// axis and an empty frequency axis.
    #[test]
    fn an_inverse_or_width_1_frame_posts_the_axes_it_changes() {
        use ad_core_rs::plugin::runtime::{ParamChangeValue, PluginParamSnapshot};
        let proc = fft_proc_with_params(FFTConfig::default());
        let p = &proc.params;
        let (time_axis, freq_axis) = (p.time_axis.unwrap(), p.freq_axis.unwrap());
        let pool = NDArrayPool::new(0);
        let axes = |n: usize| {
            let arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
            let u = proc.process_array(&Arc::new(arr), &pool).param_updates;
            (
                find_array_update(&u, time_axis).map(<[f64]>::to_vec),
                find_array_update(&u, freq_axis).map(<[f64]>::to_vec),
            )
        };
        let direction = p.direction.unwrap();
        let set_direction = |v| {
            proc.on_param_change(
                direction,
                &PluginParamSnapshot {
                    enable_callbacks: true,
                    reason: direction,
                    addr: 0,
                    value: ParamChangeValue::Int32(v),
                },
            )
        };
        set_direction(1);
        let (t, f) = axes(8);
        assert_eq!((t.map(|t| t.len()), f.map(|f| f.len())), (Some(8), Some(4)));
        set_direction(0);
        assert_eq!(axes(8), (None, None), "a forward frame of the same size");
        assert_eq!(axes(1), (Some(vec![0.0]), Some(vec![])), "a width-1 frame");
        assert_eq!(axes(1), (None, None), "the same width again");
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
        let p = &proc.params;
        let spectrum = [p.time_series, p.real, p.imaginary, p.abs_value].map(Option::unwrap);
        let array_updates = result
            .param_updates
            .iter()
            .filter(|x| matches!(x, ParamUpdate::Float64Array { reason, .. } if spectrum.contains(reason)))
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

    fn ones(n: usize) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v.fill(1.0);
        }
        Arc::new(arr)
    }

    #[test]
    fn fft_output_comes_from_the_pool_and_is_reused() {
        let pool = pool();
        let proc = FFTProcessor::new();

        let first = proc.process_array(&ones(8), &pool).output_arrays;
        assert_eq!(first[0].pool_id(), pool.id());
        let NDDataBuffer::F64(v) = &first[0].data else {
            panic!("expected F64 output");
        };
        assert!((v[0] - 1.0).abs() < 1e-10);
        let ptr = v.as_ptr();
        drop(first);

        let second = proc.process_array(&ones(8), &pool).output_arrays;
        let NDDataBuffer::F64(v) = &second[0].data else {
            panic!("expected F64 output");
        };
        assert_eq!(v.as_ptr(), ptr, "the second frame reuses the freed buffer");
        assert_eq!(pool.num_alloc_buffers(), 1);
    }

    /// C allocates, copies and numbers the output only `if (arrayCallbacks)`
    /// (NDPluginFFT.cpp:209-220): frames at ArrayCallbacks=0 take no pool
    /// buffer and no uniqueId, and still post FFTAbsValue.
    #[test]
    fn array_callbacks_0_takes_no_buffer_and_no_unique_id() {
        let pool = pool();
        let mut proc = FFTProcessor::new();
        proc.params.abs_value = Some(7);
        proc.bind_array_callbacks(ArrayCallbacks::new(false));

        for _ in 0..3 {
            let r = proc.process_array(&ones(8), &pool);
            assert!(r.output_arrays.is_empty());
            let abs_posted = r.param_updates.iter().any(|u| {
                matches!(
                    u,
                    ad_core_rs::plugin::runtime::ParamUpdate::Float64Array { reason: 7, .. }
                )
            });
            assert!(abs_posted, "FFTAbsValue posted");
        }
        assert_eq!(pool.num_alloc_buffers(), 0);

        proc.bind_array_callbacks(ArrayCallbacks::new(true));
        let r = proc.process_array(&ones(8), &pool);
        assert_eq!(r.output_arrays[0].unique_id, 0);
        assert_eq!(pool.num_alloc_buffers(), 1);
    }
}
