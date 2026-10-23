//! Standalone `NDPluginTimeSeries` (port of ADCore `NDPluginTimeSeries.cpp`).
//!
//! Ingests raw 1-D or 2-D NDArrays shaped `[numSignalsIn, numTimes]` (signal is
//! the fastest-varying dimension, `dims[0]`; time is `dims[1]`) and produces, per
//! signal, a time-series waveform. Every `numAverage` input time points are
//! averaged into one output point and written into a per-signal circular buffer.
//!
//! Stats and Attribute feed it as C does: each frame they post a 1-D Float64
//! array of their per-frame values at address 1 (`time_series_point`), and an
//! `NDTimeSeriesConfigure(…, "STATS1", 1, …)` port reads that address.
//!
//! Output form matches C exactly: `TS_TIME_SERIES` is read at `addr == signal`
//! (NDTimeSeriesN.template binds `@asyn($(PORT),$(SIGNAL),...)TS_TIME_SERIES` with
//! `SCAN="I/O Intr"`), so the per-signal waveforms are delivered by the callback
//! push (C `doCallbacksFloat64Array`), not by a pull read.
//!
//! Integer averaging divides *before* narrowing (CBUG-B25). Pre-#596 C computed
//! the averaged point as `(epicsType)averageStore_[signal] / numAveraged_`
//! (NDPluginTimeSeries.cpp:191), where C++ precedence casts the double *sum* to
//! the element type *first* (wrapping/truncating), then divides — so `UInt8`
//! inputs `200,200,200` with `numAverage == 3` gave `(u8)600 == 88`, `88 / 3 ==
//! 29` instead of `200`. That parenthesis bug was fixed upstream as ADCore #596
//! (merged 2026-07-16). `averaged_value` divides first and then narrows, which
//! this port has done since `d8f27b88` (2026-07-13) — it matches current
//! upstream C and yields the correct `200`.
//!
//! With `NDArrayCallbacks` set, each waveform callback also posts NDArrays: the
//! 2-D `[numTimePoints, numSignals]` array at `addr == numSignals_` and a 1-D
//! array per signal at `addr == signal` (NDPluginTimeSeries.cpp:341-385).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use ad_core_rs::ndarray::{
    NDArray, NDDataBuffer, NDDataType, NDDataView, NDDataViewMut, NDDimension,
};
use ad_core_rs::ndarray_pool::{DriverPool, NDArrayPool};
use ad_core_rs::plugin::runtime::{
    ArrayCallbacks, NDPluginProcess, ParamUpdate, PluginAttributes, PluginPool, ProcessResult,
    WriteOutcome, WritePort, WriteVerdict,
};
use ad_core_rs::timestamp::EpicsTimestamp;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::PortDriverBase;
use parking_lot::Mutex;

/// C `DEFAULT_NUM_TSPOINTS` (NDPluginTimeSeries.cpp:19).
const DEFAULT_NUM_TSPOINTS: usize = 2048;

/// C `TSAcquireMode` enum (NDPluginTimeSeries.cpp:21-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcquireMode {
    /// `TSAcquireModeFixed == 0`: stop once `numTimePoints` points are collected.
    Fixed,
    /// `TSAcquireModeCircular == 1`: wrap the buffer and keep acquiring.
    Circular,
}

/// Resolved parameter indices, filled by `register_params`.
#[derive(Clone)]
struct Params {
    ts_acquire: usize,
    ts_read: usize,
    ts_num_points: usize,
    ts_current_point: usize,
    ts_time_per_point: usize,
    ts_averaging_time: usize,
    ts_num_average: usize,
    ts_elapsed_time: usize,
    ts_acquire_mode: usize,
    ts_time_axis: usize,
    ts_timestamp: usize,
    ts_time_series: usize,
}

impl Params {
    /// Sentinel indices (`usize::MAX`) so an accidental `reason == 0` cannot match
    /// a real param before `register_params` runs.
    const fn sentinel() -> Self {
        Self {
            ts_acquire: usize::MAX,
            ts_read: usize::MAX,
            ts_num_points: usize::MAX,
            ts_current_point: usize::MAX,
            ts_time_per_point: usize::MAX,
            ts_averaging_time: usize::MAX,
            ts_num_average: usize::MAX,
            ts_elapsed_time: usize::MAX,
            ts_acquire_mode: usize::MAX,
            ts_time_axis: usize::MAX,
            ts_timestamp: usize::MAX,
            ts_time_series: usize::MAX,
        }
    }
}

/// Read one element of an NDArray data buffer as `f64` (C casts every sample to
/// `epicsFloat64` before accumulating: `averageStore_[signal] += (epicsFloat64)*pIn`).
#[inline]
fn sample_f64(data: &NDDataBuffer, idx: usize) -> f64 {
    match data.view() {
        NDDataView::I8(v) => v[idx] as f64,
        NDDataView::U8(v) => v[idx] as f64,
        NDDataView::I16(v) => v[idx] as f64,
        NDDataView::U16(v) => v[idx] as f64,
        NDDataView::I32(v) => v[idx] as f64,
        NDDataView::U32(v) => v[idx] as f64,
        NDDataView::I64(v) => v[idx] as f64,
        NDDataView::U64(v) => v[idx] as f64,
        NDDataView::F32(v) => v[idx] as f64,
        NDDataView::F64(v) => v[idx],
    }
}

/// Compute one averaged time-series point: **divide, then narrow**.
///
/// `averageStore_` is a `double` accumulator holding the SUM of `numAveraged_`
/// samples, and the averaged point is that sum divided by the count, narrowed to
/// the array's element type on its way into the `epicsType` circular buffer.
///
/// **CBUG-B25 — now matches upstream.** This divide-first form was a deliberate
/// deviation from pre-#596 C; ADCore #596 (merged 2026-07-16) applies the same
/// divide-then-narrow, so the port and current upstream C now agree. C before
/// #596 wrote (`NDPluginTimeSeries.cpp:191`):
///
/// ```c
/// pTimeCircular[signal*numTimePoints_ + currentTimePoint_] =
///     (epicsType)averageStore_[signal]/numAveraged_;
/// ```
///
/// and C++ binds the cast tighter than the divide, so it parses as
/// `((epicsType)averageStore_[signal]) / numAveraged_` — the SUM is truncated and
/// wrapped into the narrow element type *before* the division. The parentheses
/// are simply in the wrong place: three `UInt8` samples of 200 sum to 600, wrap
/// to 88, and divide to **29** instead of 200. Every averaged integer point whose
/// running sum exceeds the element range is wrong, and the sums routinely do.
/// Float signals were never affected (no narrowing to overflow).
///
/// Dividing first, the mean of in-range samples is in range, so the narrowing is
/// an ordinary truncation toward zero — the same one C's `(epicsType)` cast
/// performs, and the same value C's integer division produces whenever its wrap
/// did not fire.
fn averaged_value(sum: f64, num_averaged: usize, dt: NDDataType) -> f64 {
    let mean = sum / num_averaged.max(1) as f64;
    match dt {
        NDDataType::Float32 => mean as f32 as f64,
        NDDataType::Float64 => mean,
        // Every integer element type: C's `(epicsType)` cast of the mean.
        _ => mean.trunc(),
    }
}

/// Collapse duplicate `(reason, addr)` param updates, keeping the last value.
///
/// C sets a parameter library entry with repeated `setIntegerParam`/
/// `setDoubleParam` calls and then fires `callParamCallbacks()` **once**, so each
/// changed parameter is posted a single time with its final value. A single
/// `process_array` here can touch the same param twice — e.g. a reallocating
/// frame resets `TS_CURRENT_POINT` to 0 (via `acquire_reset`) and the accumulation
/// then advances it — so coalescing reproduces C's one-callback-per-param
/// semantics instead of pushing the transient intermediate value.
fn coalesce_updates(updates: Vec<ParamUpdate>) -> Vec<ParamUpdate> {
    fn key(u: &ParamUpdate) -> Option<(usize, i32)> {
        match u {
            ParamUpdate::Int32 { reason, addr, .. }
            | ParamUpdate::Float64 { reason, addr, .. }
            | ParamUpdate::Octet { reason, addr, .. }
            | ParamUpdate::Float64Array { reason, addr, .. } => Some((*reason, *addr)),
            ParamUpdate::Callbacks { .. } => None,
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(updates.len());
    // Keep the last occurrence of each key while preserving relative order.
    for u in updates.into_iter().rev() {
        if key(&u).is_none_or(|k| seen.insert(k)) {
            out.push(u);
        }
    }
    out.reverse();
    out
}

/// Standalone `NDPluginTimeSeries` processor.
pub struct TimeSeriesProcessor {
    state: Mutex<TimeSeriesState>,
    /// The runtime's NDArrayCallbacks; the posts and the copy, allocation and
    /// uniqueId they take happen only while it is set (C :341-385).
    array_callbacks: ArrayCallbacks,
    attributes: Option<PluginAttributes>,
    /// Set by the first frame, before its axis post: TS_TIME_AXIS holds state
    /// from then on. C posts the axis next only on a size, type or setting
    /// change (:417-422), so a first frame that beat `interruptAccept` left
    /// every record without it; an axis from a write before any frame stays
    /// C's, which a closed gate drops for good.
    framed: Arc<AtomicBool>,
}

/// C `getAttributes(pArray->pAttributeList)` on every post
/// (NDPluginTimeSeries.cpp:364, :379).
fn with_attributes(
    posts: &mut [(usize, Arc<NDArray>)],
    attributes: &ad_core_rs::attributes::NDAttributeList,
) {
    for (_, post) in posts {
        if let Some(post) = Arc::get_mut(post) {
            post.attributes.copy_from(attributes);
        }
    }
}

/// Every field C's `NDPluginTimeSeries` mutates from a frame or a param write.
/// One frame reallocates, accumulates and posts in a single pass, so the whole
/// set moves under one lock.
struct TimeSeriesState {
    /// C `maxSignals_` (>= 1): the per-address fan-out and `averageStore_` size.
    max_signals: usize,
    /// C `numSignalsIn_`: `dims[0]` of the most recent input. `-1` until the first
    /// frame, so the first callback always triggers `allocate_arrays`.
    num_signals_in: i64,
    /// C `numSignals_ == min(numSignalsIn_, maxSignals_)`.
    num_signals: usize,
    /// C `dataType_` (NDFloat64 at construction).
    data_type: NDDataType,
    /// C `numTimePoints_`.
    num_time_points: usize,
    /// C `currentTimePoint_`.
    current_time_point: usize,
    /// C `numAverage_`: input time points averaged into one output point.
    num_average: usize,
    /// C `numAveraged_`: input points accumulated so far for the in-progress point.
    num_averaged: usize,
    /// C `averageStore_`: per-signal sum of the `num_averaged` samples
    /// (length `max_signals`); `restart_average` empties the two together.
    average_store: Vec<f64>,
    /// C `timePerPoint_`.
    time_per_point: f64,
    /// C `averagingTimeRequested_`.
    averaging_time_requested: f64,
    /// C `averagingTimeActual_` (also the time-axis scale).
    averaging_time_actual: f64,
    /// C `acquireMode_`.
    acquire_mode: AcquireMode,
    /// C `P_TSAcquire` value, mirrored so the data plane can gate on it.
    acquiring: bool,
    /// C `pNDArrayPool`, which `pTimeCircular_` is allocated from.
    pool: PluginPool,
    /// C `pTimeCircular_`: `[num_time_points, num_signals]` of `data_type`,
    /// indexed `signal * num_time_points + t`. `None` where C's is NULL, after
    /// the pool refused it.
    circular: Option<NDArray>,
    /// C `timeStamp_`: per-output-point source timestamps (length `num_time_points`).
    time_stamp: Vec<f64>,
    /// C `startTime_`.
    start_time: Instant,
    /// C `uniqueId_`, the uniqueId of the next posted NDArray.
    unique_id: i32,
    /// The port timestamp C stamps on its posts (`getTimeStamp`): the last
    /// input's epicsTS, set by `beginProcessCallbacks` (NDPluginDriver.cpp:232).
    epics_ts: EpicsTimestamp,
    p: Params,
}

impl TimeSeriesProcessor {
    /// Create a processor for `max_signals` signals (clamped to at least 1, per C
    /// `if (maxSignals < 1) maxSignals = 1`).
    pub fn new(max_signals: usize) -> Self {
        Self {
            state: Mutex::new(TimeSeriesState::new(max_signals)),
            array_callbacks: ArrayCallbacks::new(true),
            attributes: None,
            framed: Arc::default(),
        }
    }
}

impl TimeSeriesState {
    fn new(max_signals: usize) -> Self {
        let max_signals = max_signals.max(1);
        let num_time_points = DEFAULT_NUM_TSPOINTS;
        let mut state = Self {
            max_signals,
            num_signals_in: -1,
            num_signals: max_signals,
            data_type: NDDataType::Float64,
            num_time_points,
            current_time_point: 0,
            num_average: 1,
            num_averaged: 0,
            average_store: vec![0.0; max_signals],
            time_per_point: 0.0,
            averaging_time_requested: 1.0,
            // computeNumAverage with the default timePerPoint_==0 yields
            // averagingTimeActual_ == averagingTimeRequested_ == 1.
            averaging_time_actual: 1.0,
            acquire_mode: AcquireMode::Fixed,
            acquiring: false,
            pool: PluginPool::new(DriverPool::standalone(0)),
            circular: None,
            time_stamp: vec![0.0; num_time_points],
            start_time: Instant::now(),
            unique_id: 0,
            epics_ts: EpicsTimestamp::default(),
            p: Params::sentinel(),
        };
        // C's constructor allocateArrays (:88).
        state.allocate_arrays();
        state
    }

    /// C `createAxisArray` (NDPluginTimeSeries.cpp:147-161): Fixed mode ascends
    /// `i * averagingTimeActual_`; Circular mode ends at 0 with
    /// `-(numTimePoints_-1-i) * averagingTimeActual_`. Returns the `TS_TIME_AXIS`
    /// update (C `doCallbacksFloat64Array(timeAxis_, ..., P_TSTimeAxis, 0)`).
    fn create_axis_array(&self) -> Vec<ParamUpdate> {
        let axis: Vec<f64> = (0..self.num_time_points)
            .map(|i| match self.acquire_mode {
                AcquireMode::Fixed => i as f64 * self.averaging_time_actual,
                AcquireMode::Circular => {
                    -(((self.num_time_points - 1) - i) as f64) * self.averaging_time_actual
                }
            })
            .collect();
        vec![ParamUpdate::float64_array(self.p.ts_time_axis, axis)]
    }

    /// C `acquireReset` (NDPluginTimeSeries.cpp:137-145): zero the circular buffer
    /// and timestamps, reset `currentTimePoint_`, restart `startTime_`. C keeps
    /// the point in progress, so samples from before an Acquire, AcquireMode,
    /// NumPoints or input change land in point 0 of the new series, and a slot
    /// above a shrunk signal count keeps its sum until the count grows back;
    /// that C bug is not copied.
    fn acquire_reset(&mut self) -> Vec<ParamUpdate> {
        if let Some(circular) = &mut self.circular {
            circular.data.as_u8_slice_mut().fill(0);
        }
        self.restart_average();
        self.time_stamp.iter_mut().for_each(|v| *v = 0.0);
        self.current_time_point = 0;
        self.start_time = Instant::now();
        vec![ParamUpdate::int32(self.p.ts_current_point, 0)]
    }

    /// C `allocateArrays` (NDPluginTimeSeries.cpp:115-135): (re)size the circular
    /// buffer and timestamps to `numSignals_ * numTimePoints_`, rebuild the axis,
    /// and reset acquisition.
    fn allocate_arrays(&mut self) -> Vec<ParamUpdate> {
        // C releases the old buffer before it allocates the new one (:125).
        self.circular = None;
        let dims = vec![
            NDDimension::new(self.num_time_points),
            NDDimension::new(self.num_signals),
        ];
        // C dereferences a refused alloc (:141); the pool has printed its
        // line, and the plugin goes on with no buffer.
        self.circular = self.pool.current().pool().alloc(dims, self.data_type).ok();
        self.time_stamp = vec![0.0; self.num_time_points];
        let mut updates = self.create_axis_array();
        updates.extend(self.acquire_reset());
        updates
    }

    fn restart_average(&mut self) {
        self.num_averaged = 0;
        self.average_store.fill(0.0);
    }

    /// C `computeNumAverage` (NDPluginTimeSeries.cpp:97-113): derive `numAverage_`
    /// from the requested averaging time and the driver's time-per-point, then
    /// post the actual averaging time, the averaging count, and the rescaled axis.
    fn compute_num_average(&mut self) -> Vec<ParamUpdate> {
        if self.time_per_point == 0.0 {
            self.num_average = 1;
            self.averaging_time_actual = self.averaging_time_requested;
        } else {
            // C: (int)(averagingTimeRequested_/timePerPoint_ + 0.5), clamped >= 1.
            let n = (self.averaging_time_requested / self.time_per_point + 0.5) as i64;
            self.num_average = if n < 1 { 1 } else { n as usize };
            self.averaging_time_actual = self.time_per_point * self.num_average as f64;
        }
        // C zeroes only numAveraged_ (:108), so the next point divides the
        // old partial sum by the new count; that C bug is not copied.
        self.restart_average();
        let mut updates = vec![
            ParamUpdate::float64(self.p.ts_averaging_time, self.averaging_time_actual),
            ParamUpdate::int32(self.p.ts_num_average, self.num_average as i32),
        ];
        updates.extend(self.create_axis_array());
        updates
    }

    /// C `doTimeSeriesCallbacks` (NDPluginTimeSeries.cpp:296-385): the
    /// waveforms, then, with NDArrayCallbacks set (:341), the NDArray posts
    /// into `posts`.
    fn do_time_series_callbacks(
        &mut self,
        array_callbacks: &ArrayCallbacks,
        posts: &mut Vec<(usize, Arc<NDArray>)>,
    ) -> Vec<ParamUpdate> {
        // C reads a refused buffer through NULL (:269); with none there is
        // nothing to post.
        let Some(circular) = &self.circular else {
            return Vec::new();
        };
        let updates = self.time_series_waveforms(circular);
        if array_callbacks.get() {
            let values = circular.data.to_f64_vec();
            self.post_time_series_arrays(&values, posts);
        }
        updates
    }

    /// C :341-385: the whole series as a 2-D `[numTimePoints, numSignals]`
    /// array at `addr == numSignals_`, then each signal's row as a 1-D array at
    /// `addr == signal`. Fixed mode copies the buffer as is; Circular mode
    /// rotates it so the oldest point is first. Each post comes from C's
    /// `pNDArrayPool`, the last frame's pool (NDPluginDriver.cpp:419), as
    /// C's `copy` and `alloc` do (:347-351, :374), on the frame and write
    /// path alike; one the pool refuses is not posted.
    fn post_time_series_arrays(&mut self, values: &[f64], posts: &mut Vec<(usize, Arc<NDArray>)>) {
        let ntp = self.num_time_points;
        let ns = self.num_signals;
        let cur = self.current_time_point;
        let mut series = Vec::with_capacity(ns * ntp);
        for ring in values.chunks(ntp).take(ns) {
            match self.acquire_mode {
                AcquireMode::Fixed => series.extend_from_slice(ring),
                AcquireMode::Circular => {
                    series.extend_from_slice(&ring[cur..]);
                    series.extend_from_slice(&ring[..cur]);
                }
            }
        }
        let unique_id = self.unique_id;
        self.unique_id = self.unique_id.wrapping_add(1);
        let time_stamp = EpicsTimestamp::now().as_f64();
        let epics_ts = self.epics_ts;
        let data_type = self.data_type;
        let pool = self.pool.current();
        let make = |dims: Vec<NDDimension>, values: &[f64]| -> Option<Arc<NDArray>> {
            let mut arr = pool.pool().alloc(dims, data_type).ok()?;
            arr.data.fill_from_f64(values);
            arr.unique_id = unique_id;
            arr.time_stamp = time_stamp;
            arr.timestamp = epics_ts;
            Some(Arc::new(arr))
        };
        posts.extend(
            make(vec![NDDimension::new(ntp), NDDimension::new(ns)], &series).map(|a| (ns, a)),
        );
        for (signal, row) in series.chunks(ntp).enumerate() {
            posts.extend(make(vec![NDDimension::new(ntp)], row).map(|a| (signal, a)));
        }
    }

    /// C `doTimeSeriesCallbacksT` (NDPluginTimeSeries.cpp:263-290): emit one
    /// `TS_TIME_SERIES` Float64Array per signal at `addr == signal`. Fixed mode
    /// emits the filled prefix (`currentTimePoint_` points); Circular mode emits
    /// the full ring rotated so the oldest point is first.
    fn time_series_waveforms(&self, circular: &NDArray) -> Vec<ParamUpdate> {
        let at = |index| {
            circular
                .data
                .get_as_f64(index)
                .expect("index inside the circular buffer")
        };
        let ntp = self.num_time_points;
        let mut updates = Vec::with_capacity(self.num_signals);
        match self.acquire_mode {
            AcquireMode::Fixed => {
                for signal in 0..self.num_signals {
                    let start = signal * ntp;
                    let series = (start..start + self.current_time_point).map(at).collect();
                    updates.push(ParamUpdate::float64_array_addr(
                        self.p.ts_time_series,
                        signal as i32,
                        series,
                    ));
                }
            }
            AcquireMode::Circular => {
                for signal in 0..self.num_signals {
                    let base = signal * ntp;
                    let mut series = Vec::with_capacity(ntp);
                    let mut time_in = self.current_time_point;
                    for _ in 0..ntp {
                        series.push(at(base + time_in));
                        time_in += 1;
                        if time_in >= ntp {
                            time_in = 0;
                        }
                    }
                    updates.push(ParamUpdate::float64_array_addr(
                        self.p.ts_time_series,
                        signal as i32,
                        series,
                    ));
                }
            }
        }
        updates
    }

    /// C `doAddToTimeSeriesT` (NDPluginTimeSeries.cpp:168-213): accumulate each
    /// input time point into `averageStore_`; every `numAverage_` points, write
    /// the integer-truncated average into the circular buffer and advance. In
    /// Fixed mode a full buffer stops acquisition and fires the per-signal
    /// callbacks; in Circular mode the write position wraps.
    fn add_to_time_series(
        &mut self,
        array: &NDArray,
        array_callbacks: &ArrayCallbacks,
        posts: &mut Vec<(usize, Arc<NDArray>)>,
    ) -> Vec<ParamUpdate> {
        // C takes `pTimeCircular_->pData` first (:173); a refused buffer
        // takes no point.
        let Some(circular) = self.circular.as_mut() else {
            return Vec::new();
        };
        let mut updates = Vec::new();
        let num_signals_in = self.num_signals_in.max(0) as usize;
        if num_signals_in == 0 {
            return updates;
        }
        let data = &array.data;
        // C reads `numTimes` time points; 1-D arrays are a single time point.
        let mut num_times = if array.dims.len() == 2 {
            array.dims[1].size
        } else {
            1
        };
        // Defensive clamp against a malformed array whose buffer is shorter than
        // its declared dims (C trusts the dims and would read out of bounds).
        let max_times = data.len() / num_signals_in;
        if num_times > max_times {
            num_times = max_times;
        }

        let ntp = self.num_time_points;
        for i in 0..num_times {
            let base = i * num_signals_in;
            for s in 0..self.num_signals {
                self.average_store[s] += sample_f64(data, base + s);
            }
            self.num_averaged += 1;
            if self.num_averaged < self.num_average {
                continue;
            }
            // Collected the desired number of points to average.
            for s in 0..self.num_signals {
                let avg = averaged_value(self.average_store[s], self.num_averaged, self.data_type);
                circular
                    .data
                    .set_from_f64(s * ntp + self.current_time_point, avg);
                self.average_store[s] = 0.0;
            }
            self.num_averaged = 0;
            self.time_stamp[self.current_time_point] = array.time_stamp;
            self.current_time_point += 1;
            if self.current_time_point >= ntp {
                match self.acquire_mode {
                    AcquireMode::Fixed => {
                        // C setIntegerParam(P_TSAcquire, 0); doTimeSeriesCallbacks(); break.
                        self.acquiring = false;
                        updates.push(ParamUpdate::int32(self.p.ts_acquire, 0));
                        updates.extend(self.do_time_series_callbacks(array_callbacks, posts));
                        break;
                    }
                    AcquireMode::Circular => {
                        self.current_time_point = 0;
                    }
                }
            }
        }

        updates.push(ParamUpdate::int32(
            self.p.ts_current_point,
            self.current_time_point as i32,
        ));
        let elapsed = self.start_time.elapsed().as_secs_f64();
        updates.push(ParamUpdate::float64(self.p.ts_elapsed_time, elapsed));
        updates
    }
}

impl NDPluginProcess for TimeSeriesProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_MULTIDEVICE;
    // NDPluginTimeSeries.cpp:52-53, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::FLOAT64
        .or(InterfaceMask::FLOAT64_ARRAY)
        .or(InterfaceMask::GENERIC_POINTER);
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;
    // NDPluginTimeSeries.cpp:477-478, :520-521.
    const WRITE_INT32_EPILOGUE: ad_core_rs::plugin::runtime::WriteEpilogue =
        ad_core_rs::plugin::runtime::WriteEpilogue::AddrAndZero;
    const WRITE_FLOAT64_EPILOGUE: ad_core_rs::plugin::runtime::WriteEpilogue =
        ad_core_rs::plugin::runtime::WriteEpilogue::AddrAndZero;

    /// `maxSignals+1`, the 2-D series posting at address maxSignals
    /// (NDPluginTimeSeries.cpp:48-51). C adds the 1 before its
    /// `maxSignals < 1` clamp (:59-61), leaving maxSignals 0 no address for
    /// that post; that C bug is not copied.
    fn max_addr(&self) -> usize {
        self.state.lock().max_signals + 1
    }

    /// C `NDPluginTimeSeries::writeInt32` (NDPluginTimeSeries.cpp:441-490):
    /// its branch runs inside the write, before `callParamCallbacks`. It keeps
    /// the base class clamp but replaces its errorMessage with its own,
    /// carrying `getAddress`'s status (`"%s: status=%d, function=%d, value=%d"`,
    /// :481).
    fn validate_int32(
        &self,
        reason: usize,
        addr: i32,
        value: i32,
        verdict: WriteVerdict<i32>,
        params: &mut WritePort<'_>,
    ) -> WriteOutcome<i32> {
        let verdict = match verdict {
            WriteVerdict::Refuse { keep, .. } => WriteVerdict::Refuse {
                keep,
                message: format!(
                    "NDPluginTimeSeries::writeInt32: status=0, function={reason}, value={value}"
                ),
            },
            accepted => accepted,
        };
        let value = verdict.kept();
        let mut state = self.state.lock();
        let p = &state.p;
        let mut posts = Vec::new();
        let updates = if reason == p.ts_num_points {
            // C sizes the buffers from any value; 0 overruns them on the next
            // point and a negative one fails the calloc. That C bug is not
            // copied (the template's DRVL is 1).
            state.num_time_points = value.max(1) as usize;
            state.allocate_arrays()
        } else if reason == p.ts_acquire_mode {
            state.acquire_mode = if value == 0 {
                AcquireMode::Fixed
            } else {
                AcquireMode::Circular
            };
            let mut updates = state.acquire_reset();
            updates.extend(state.create_axis_array());
            updates
        } else if reason == p.ts_acquire {
            // processCallbacks reads the address-0 value (:425).
            if addr == 0 {
                state.acquiring = value != 0;
            }
            if value != 0 {
                state.acquire_reset()
            } else {
                state.do_time_series_callbacks(&self.array_callbacks, &mut posts)
            }
        } else if reason == p.ts_read {
            state.do_time_series_callbacks(&self.array_callbacks, &mut posts)
        } else {
            return verdict.into();
        };
        if let Some(fresh) = self
            .attributes
            .as_ref()
            .filter(|_| !posts.is_empty())
            .and_then(|a| a.evaluate_on(params))
        {
            with_attributes(&mut posts, &fresh);
        }
        WriteOutcome::from(verdict)
            .set_updates(updates)
            .post_addressed(posts)
            .settled()
    }

    /// C `NDPluginTimeSeries::writeFloat64` (NDPluginTimeSeries.cpp:492-530):
    /// `computeNumAverage` posts AveragingTime, NumAverage and the time axis
    /// inside the write (:509, :512).
    fn validate_float64(
        &self,
        reason: usize,
        _addr: i32,
        _value: f64,
        verdict: WriteVerdict<f64>,
        _params: &PortDriverBase,
    ) -> WriteOutcome<f64> {
        let value = verdict.kept();
        let mut state = self.state.lock();
        if reason == state.p.ts_time_per_point {
            state.time_per_point = value;
        } else if reason == state.p.ts_averaging_time {
            state.averaging_time_requested = value;
        } else {
            return verdict.into();
        }
        let updates = state.compute_num_average();
        WriteOutcome::from(verdict).set_updates(updates).settled()
    }

    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        let mut state = self.state.lock();
        state.epics_ts = array.timestamp;

        // C processCallbacks: this plugin only handles 1-D or 2-D arrays.
        let ndims = array.dims.len();
        if !(1..=2).contains(&ndims) {
            let mut result = ProcessResult::empty();
            result.trace.push((
                asyn_rs::trace::TraceMask::ERROR,
                "NDPluginTimeSeries::processCallbacks: error, number of array dimensions must be 1 or 2\n"
                    .into(),
            ));
            return result;
        }

        let mut updates: Vec<ParamUpdate> = Vec::new();
        let mut posts = Vec::new();

        // C: reallocate when the data type or the input signal count changes.
        let num_signals_in = array.dims[0].size;
        let dtype = array.data.data_type();
        if dtype != state.data_type || (num_signals_in as i64) != state.num_signals_in {
            state.data_type = dtype;
            state.num_signals_in = num_signals_in as i64;
            state.num_signals = num_signals_in.min(state.max_signals);
            self.framed.store(true, Ordering::Release);
            updates.extend(state.allocate_arrays());
        }

        // C: only accumulate while P_TSAcquire is set.
        if state.acquiring {
            updates.extend(state.add_to_time_series(array, &self.array_callbacks, &mut posts));
        }

        // Off the state lock: the evaluation waits on the actor, whose
        // writes take it.
        drop(state);
        if let Some(fresh) = self
            .attributes
            .as_ref()
            .filter(|_| !posts.is_empty())
            .and_then(|a| a.evaluate(&[]))
        {
            with_attributes(&mut posts, &fresh);
        }
        let mut result = ProcessResult::sink(coalesce_updates(updates));
        result.addressed = posts;
        result
    }

    fn bind_array_callbacks(&mut self, array_callbacks: ArrayCallbacks) {
        self.array_callbacks = array_callbacks;
    }

    fn bind_attributes(&mut self, attributes: PluginAttributes) {
        self.attributes = Some(attributes);
    }

    fn bind_pool(&mut self, pool: PluginPool) {
        // The constructor's allocateArrays ran on the pool C has at that
        // point, the plugin's own.
        let state = self.state.get_mut();
        state.pool = pool;
        state.allocate_arrays();
    }

    fn plugin_type(&self) -> &str {
        "NDPluginTimeSeries"
    }

    fn register_params(&mut self, base: &mut PortDriverBase) -> asyn_rs::error::AsynResult<()> {
        let state = self.state.get_mut();
        // Per-plugin parameters (NDPluginTimeSeries.cpp:69-79).
        state.p.ts_acquire = base.create_param("TS_ACQUIRE", ParamType::Int32)?;
        state.p.ts_read = base.create_param("TS_READ", ParamType::Int32)?;
        state.p.ts_num_points = base.create_param("TS_NUM_POINTS", ParamType::Int32)?;
        state.p.ts_current_point = base.create_param("TS_CURRENT_POINT", ParamType::Int32)?;
        state.p.ts_time_per_point = base.create_param("TS_TIME_PER_POINT", ParamType::Float64)?;
        state.p.ts_averaging_time = base.create_param("TS_AVERAGING_TIME", ParamType::Float64)?;
        state.p.ts_num_average = base.create_param("TS_NUM_AVERAGE", ParamType::Int32)?;
        state.p.ts_elapsed_time = base.create_param("TS_ELAPSED_TIME", ParamType::Float64)?;
        state.p.ts_acquire_mode = base.create_param("TS_ACQUIRE_MODE", ParamType::Int32)?;
        state.p.ts_time_axis = base.create_param("TS_TIME_AXIS", ParamType::Float64Array)?;
        let framed = self.framed.clone();
        base.set_state_array_rule(
            state.p.ts_time_axis,
            Arc::new(move || framed.load(Ordering::Acquire)),
        );
        state.p.ts_timestamp = base.create_param("TS_TIMESTAMP", ParamType::Float64Array)?;
        // Per-signal parameter (NDPluginTimeSeries.cpp:82); read at addr == signal.
        state.p.ts_time_series = base.create_param("TS_TIME_SERIES", ParamType::Float64Array)?;

        // NDPluginTimeSeries.cpp:87 and acquireReset (:143) set only these two.
        base.set_int32_param(state.p.ts_num_points, 0, state.num_time_points as i32)?;
        base.set_int32_param(state.p.ts_current_point, 0, 0)?;
        // The constructor's createAxisArray (:160) stores nothing: C keeps no
        // array params, and its callback fires before any record subscribes.
        Ok(())
    }
}

/// The 1-D Float64 array Stats and Attribute post at address 1 for an
/// NDPluginTimeSeries port (NDPluginStats.cpp:555-586,
/// NDPluginAttribute.cpp:98-108): one element per value, carrying the
/// source frame's uniqueId, timeStamp and epicsTS.
pub(crate) fn time_series_point(
    pool: &NDArrayPool,
    src: &NDArray,
    values: &[f64],
) -> Option<Arc<NDArray>> {
    let mut arr = pool
        .alloc(vec![NDDimension::new(values.len())], NDDataType::Float64)
        .ok()?;
    if let NDDataViewMut::F64(v) = arr.data.view_mut() {
        v.copy_from_slice(values);
    }
    arr.unique_id = src.unique_id;
    arr.time_stamp = src.time_stamp;
    arr.timestamp = src.timestamp;
    Some(Arc::new(arr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::plugin::runtime::SideParam;

    /// NDPluginTimeSeries.cpp:51 passes maxSignals+1 whoever builds the
    /// plugin.
    #[test]
    fn a_time_series_port_has_c_s_addresses() {
        let (handle, _jh) = ad_core_rs::plugin::runtime::create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("TS_ADDR").unwrap(),
            TimeSeriesProcessor::new(3),
            1_000_000,
            1,
            0,
            "",
            Arc::new(ad_core_rs::plugin::wiring::WiringRegistry::new()),
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );
        assert_eq!(handle.port_runtime().port_handle().max_addr(), 4);
    }

    /// C `getAttributes` on each post (NDPluginTimeSeries.cpp:364, :379),
    /// whether a TSRead write or a filled series made it.
    #[test]
    fn every_post_carries_the_plugin_attributes() {
        use crate::test_util::{publish, with_attributes_file};
        use ad_core_rs::attributes::NDAttrValue;
        let xml = r#"<Attributes>
            <Attribute name="Points" type="PARAM" source="TS_NUM_POINTS" datatype="INT"/>
        </Attributes>"#;
        let (handle, mut rx, _jh) =
            with_attributes_file("TS_ATTRS", TimeSeriesProcessor::new(1), xml);
        let port = handle.port_runtime().port_handle();
        let reason = |name: &str| {
            port.drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new(name, 0))
                .unwrap()
                .reason
        };
        let write = |reason: usize, v: i32| {
            port.write_int32_blocking(reason, 0, v).unwrap();
            assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        };
        write(reason("TS_NUM_POINTS"), 2);
        write(reason("TS_ACQUIRE"), 1);
        let frame = NDArray::with_data(
            vec![NDDimension::new(1)],
            NDDataBuffer::from_vec::<f64>(vec![1.0]),
        );
        publish(&handle, &frame);
        write(reason("TS_READ"), 1);
        publish(&handle, &frame);
        for _ in 0..2 {
            let post = rx.blocking_recv().unwrap();
            assert_eq!(
                post.attributes.get("Points").map(|a| a.value.clone()),
                Some(NDAttrValue::Int32(2))
            );
        }
    }

    /// `"%s: status=%d, function=%d, value=%d"` (NDPluginTimeSeries.cpp:481).
    #[test]
    fn a_refused_base_write_carries_c_error_message() {
        let proc = TimeSeriesProcessor::new(1);
        let mut base = asyn_rs::port::PortDriverBase::new(
            "T",
            1,
            Default::default(),
            asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
            asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let clamp = WriteVerdict::Refuse {
            keep: 1,
            message: String::new(),
        };
        assert_eq!(
            proc.validate_int32(
                71,
                0,
                99,
                clamp,
                &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base)
            )
            .verdict,
            WriteVerdict::Refuse {
                keep: 1,
                message: "NDPluginTimeSeries::writeInt32: status=0, function=71, value=99".into(),
            }
        );
        assert_eq!(
            proc.validate_int32(
                71,
                0,
                1,
                WriteVerdict::Accept(1),
                &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base)
            )
            .verdict,
            WriteVerdict::Accept(1)
        );
    }
    use asyn_rs::port::InterfaceMask;
    use asyn_rs::port::{PortDriverBase, PortFlags};

    // ---- averaged_value: divide first, then narrow (CBUG-B25) ----

    /// CBUG-B25 — the average of three UInt8 samples of 200 is **200**.
    ///
    /// This test was `test_averaged_value_uint8_truncates_not_f64_divide` and
    /// pinned C's answer, 29: C narrows the running sum to the element type before
    /// dividing (`(epicsType)averageStore_[signal]/numAveraged_`,
    /// NDPluginTimeSeries.cpp:191), so 600 wraps to (u8)88 and 88/3 = 29. It even
    /// asserted `!= 200.0` — the correct value — to prove the divergence.
    #[test]
    fn test_averaged_value_uint8_divides_before_narrowing() {
        assert_eq!(averaged_value(600.0, 3, NDDataType::UInt8), 200.0); // C: 29
    }

    /// CBUG-B25. Was `test_averaged_value_int8_negative_truncates_toward_zero`,
    /// pinning C's -29: (i8)(-600) = -88, -88/3 = -29.
    #[test]
    fn test_averaged_value_int8_negative_divides_before_narrowing() {
        assert_eq!(averaged_value(-600.0, 3, NDDataType::Int8), -200.0); // C: -29
    }

    /// CBUG-B25. Was `test_averaged_value_uint16_wraps`, pinning C's 2232:
    /// (u16)70000 = 4464, 4464/2 = 2232.
    #[test]
    fn test_averaged_value_uint16_does_not_wrap() {
        assert_eq!(averaged_value(70000.0, 2, NDDataType::UInt16), 35000.0); // C: 2232
    }

    /// The narrowing itself is still C's `(epicsType)` cast — truncation toward
    /// zero — applied to the MEAN, which for in-range samples is in range. This
    /// is also the value C produced whenever its wrap did not fire.
    #[test]
    fn test_averaged_value_narrows_the_mean_toward_zero() {
        // 7/2 = 3.5 -> 3;  -7/2 = -3.5 -> -3 (C integer division truncates too).
        assert_eq!(averaged_value(7.0, 2, NDDataType::Int8), 3.0);
        assert_eq!(averaged_value(-7.0, 2, NDDataType::Int8), -3.0);
        assert_eq!(averaged_value(7.0, 2, NDDataType::UInt16), 3.0);
    }

    #[test]
    fn test_averaged_value_int32_in_range_no_wrap() {
        assert_eq!(averaged_value(600.0, 3, NDDataType::Int32), 200.0);
    }

    /// Float signals were never affected by CBUG-B25 — no narrowing to overflow.
    #[test]
    fn test_averaged_value_float_types_exact() {
        assert_eq!(averaged_value(600.0, 3, NDDataType::Float64), 200.0);
        assert_eq!(averaged_value(600.0, 3, NDDataType::Float32), 200.0);
        // ... and they do NOT truncate.
        assert_eq!(averaged_value(7.0, 2, NDDataType::Float64), 3.5);
    }

    #[test]
    fn test_averaged_value_numaverage_one_is_passthrough() {
        assert_eq!(averaged_value(200.0, 1, NDDataType::UInt8), 200.0);
        assert_eq!(averaged_value(-50.0, 1, NDDataType::Int8), -50.0);
    }

    #[test]
    fn test_register_params_seeds_only_what_the_c_constructor_sets() {
        let mut proc = TimeSeriesProcessor::new(2);
        let mut base = PortDriverBase::new(
            "TS_INIT",
            3,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        proc.register_params(&mut base).unwrap();
        let int = |name: &str| base.get_int32_param(base.find_param(name).unwrap(), 0);
        let dbl = |name: &str| base.get_float64_param(base.find_param(name).unwrap(), 0);
        assert_eq!(int("TS_NUM_POINTS").unwrap(), 2048);
        assert_eq!(int("TS_CURRENT_POINT").unwrap(), 0);
        for name in ["TS_ACQUIRE", "TS_ACQUIRE_MODE", "TS_NUM_AVERAGE"] {
            assert!(int(name).is_err(), "{name}");
        }
        for name in ["TS_AVERAGING_TIME", "TS_TIME_PER_POINT"] {
            assert!(dbl(name).is_err(), "{name}");
        }
        let axis = base.find_param("TS_TIME_AXIS").unwrap();
        assert!(base.params.get_float64_array(axis, 0).unwrap().is_empty());
    }

    // ---- process_array end-to-end ----

    fn make_proc(max_signals: usize, port: &str) -> TimeSeriesProcessor {
        let mut proc = TimeSeriesProcessor::new(max_signals);
        let mut base = PortDriverBase::new(
            port,
            max_signals + 1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        proc.register_params(&mut base).unwrap();
        proc
    }

    /// A 2-signal UInt16 port with a 3-point Fixed buffer, filled by one
    /// 2-D frame `[[1,10],[2,20],[3,30]]`.
    fn filled_fixed_u16(port: &str) -> (TimeSeriesProcessor, ProcessResult) {
        let proc = make_proc(2, port);
        {
            let mut st = proc.state.lock();
            st.num_time_points = 3;
            let _ = st.allocate_arrays();
            st.acquiring = true;
        }
        let arr = NDArray::with_data(
            vec![NDDimension::new(2), NDDimension::new(3)],
            NDDataBuffer::from_vec::<u16>(vec![1, 10, 2, 20, 3, 30]),
        );
        let res = proc.process_array(&Arc::new(arr), &NDArrayPool::new(0));
        (proc, res)
    }

    fn u16s(arr: &NDArray) -> Vec<u16> {
        match arr.data.view() {
            NDDataView::U16(v) => v.to_vec(),
            _ => panic!("not UInt16"),
        }
    }

    /// C NDPluginTimeSeries.cpp:368 and :382: a full Fixed buffer posts the
    /// `[numTimePoints, numSignals]` array at `addr == numSignals_` and each
    /// signal's row at `addr == signal`, in the input's data type.
    #[test]
    fn test_full_fixed_buffer_posts_the_series_and_each_signal() {
        let (_proc, res) = filled_fixed_u16("TST_TS_POST");
        let at = |addr: usize| {
            res.addressed
                .iter()
                .find(|(a, _)| *a == addr)
                .map(|(_, arr)| arr.clone())
                .expect("posted")
        };
        assert_eq!(res.addressed.len(), 3);
        let all = at(2);
        assert_eq!(
            all.dims.iter().map(|d| d.size).collect::<Vec<_>>(),
            vec![3, 2]
        );
        assert_eq!(u16s(&all), vec![1, 2, 3, 10, 20, 30]);
        assert_eq!(u16s(&at(0)), vec![1, 2, 3]);
        assert_eq!(u16s(&at(1)), vec![10, 20, 30]);
        assert!(res.addressed.iter().all(|(_, a)| a.unique_id == 0));
    }

    /// A 2-signal port with a 3-point Fixed buffer on `pool`, acquiring.
    fn on_pool(port: &str, pool: &DriverPool) -> TimeSeriesProcessor {
        let mut proc = make_proc(2, port);
        proc.state.get_mut().num_time_points = 3;
        proc.bind_pool(PluginPool::new(pool.clone()));
        proc.state.get_mut().acquiring = true;
        proc
    }

    /// The UInt16 frame `[[1,10],[2,20],[3,30]]`, which fills a 3-point buffer.
    fn three_points() -> Arc<NDArray> {
        Arc::new(NDArray::with_data(
            vec![NDDimension::new(2), NDDimension::new(3)],
            NDDataBuffer::from_vec::<u16>(vec![1, 10, 2, 20, 3, 30]),
        ))
    }

    /// C takes every post from `pNDArrayPool`, the series by `copy`
    /// (:347-351) and each signal by `alloc` (:374), so each is charged there.
    #[test]
    fn every_post_is_allocated_from_the_plugin_pool() {
        let pool = DriverPool::standalone(0);
        let proc = on_pool("TST_TS_POOLED", &pool);

        let res = proc.process_array(&three_points(), pool.pool());

        assert_eq!(res.addressed.len(), 3);
        assert!(res.addressed.iter().all(|(_, a)| a.pool_id() == pool.id()));
        // The 12-byte buffer, the 12-byte series and two 6-byte signals.
        assert_eq!(pool.allocated_bytes(), 12 + 12 + 6 + 6);
    }

    /// C allocates `pTimeCircular_` from `pNDArrayPool` (:132) and
    /// dereferences a refused one (:141). Here the plugin keeps no buffer:
    /// a frame takes no point and a TSRead posts nothing.
    #[test]
    fn a_buffer_the_pool_refuses_takes_no_points() {
        // Under the 12 bytes of a [3, 2] UInt16 buffer.
        let pool = DriverPool::standalone(11);
        let proc = on_pool("TST_TS_REFUSED", &pool);

        let res = proc.process_array(&three_points(), pool.pool());

        assert!(res.addressed.is_empty());
        let series = proc.state.lock().p.ts_time_series;
        assert!(
            !res.param_updates.iter().any(
                |u| matches!(u, ParamUpdate::Float64Array { reason, .. } if *reason == series)
            )
        );
        assert_eq!(proc.state.lock().current_time_point, 0);
        let mut base = PortDriverBase::new(
            "TST_TS_REFUSED_B",
            3,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let read = proc.state.lock().p.ts_read;
        let out = proc.validate_int32(
            read,
            0,
            1,
            WriteVerdict::Accept(1),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert_eq!(side_array(&out, series, 0), None);
        assert!(out.addressed.0.is_empty());
        assert_eq!(pool.allocated_bytes(), 0);
    }

    fn side_array(out: &WriteOutcome<i32>, reason: usize, addr: i32) -> Option<Vec<f64>> {
        out.side.iter().find_map(|p| match p {
            SideParam::Float64Array {
                reason: r,
                addr: a,
                value,
            } if *r == reason && *a == addr => Some(value.clone()),
            _ => None,
        })
    }

    /// A TSAcquire 0 write posts its waveforms and arrays inside the write
    /// (C writeInt32 :468), and each post takes the next uniqueId (:367).
    #[test]
    fn test_stop_write_posts_with_the_next_unique_id() {
        let (proc, _) = filled_fixed_u16("TST_TS_STOP");
        let mut base = PortDriverBase::new(
            "TST_TS_STOP_B",
            3,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let (acquire, series) = {
            let st = proc.state.lock();
            (st.p.ts_acquire, st.p.ts_time_series)
        };
        let out = proc.validate_int32(
            acquire,
            0,
            0,
            WriteVerdict::Accept(0),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert!(out.settled);
        assert!(!proc.state.lock().acquiring);
        assert_eq!(side_array(&out, series, 0), Some(vec![1.0, 2.0, 3.0]));
        assert_eq!(side_array(&out, series, 1), Some(vec![10.0, 20.0, 30.0]));
        assert_eq!(out.addressed.0.len(), 3);
        assert!(out.addressed.0.iter().all(|(_, a)| a.unique_id == 1));
    }

    /// processCallbacks gates on TSAcquire at address 0 (:425); a write at
    /// another address stores there and resets, but does not start a run.
    #[test]
    fn test_acquire_write_off_address_zero_does_not_gate_frames() {
        let proc = make_proc(2, "TST_TS_ADDR");
        let mut base = PortDriverBase::new(
            "TST_TS_ADDR_B",
            3,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_ALL,
        );
        let acquire = proc.state.lock().p.ts_acquire;
        let out = proc.validate_int32(
            acquire,
            1,
            1,
            WriteVerdict::Accept(1),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert!(out.settled);
        assert!(!proc.state.lock().acquiring);
        proc.validate_int32(
            acquire,
            0,
            1,
            WriteVerdict::Accept(1),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert!(proc.state.lock().acquiring);
    }

    /// C writeInt32 :457 and :459: NumPoints reallocates and AcquireMode
    /// flips the axis, both inside the write.
    #[test]
    fn test_num_points_and_acquire_mode_writes_carry_the_axis() {
        let proc = make_proc(1, "TST_TS_NPTS");
        let mut base = PortDriverBase::new(
            "TST_TS_NPTS_B",
            2,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let p = proc.state.lock().p.clone();
        proc.state.lock().averaging_time_actual = 1.0;
        let out = proc.validate_int32(
            p.ts_num_points,
            0,
            3,
            WriteVerdict::Accept(3),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert!(out.settled);
        assert_eq!(
            side_array(&out, p.ts_time_axis, 0),
            Some(vec![0.0, 1.0, 2.0])
        );
        assert!(out.side.contains(&SideParam::Int32 {
            reason: p.ts_current_point,
            addr: 0,
            value: 0,
        }));
        let out = proc.validate_int32(
            p.ts_acquire_mode,
            0,
            1,
            WriteVerdict::Accept(1),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert!(out.settled);
        assert_eq!(
            side_array(&out, p.ts_time_axis, 0),
            Some(vec![-2.0, -1.0, 0.0])
        );
        let out = proc.validate_int32(
            p.ts_acquire,
            0,
            1,
            WriteVerdict::Accept(1),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert!(out.settled && proc.state.lock().acquiring);
        assert!(out.addressed.0.is_empty());
    }

    /// C writeFloat64 :509 and :512: computeNumAverage posts AveragingTime,
    /// NumAverage and the axis inside the write.
    #[test]
    fn test_averaging_time_write_carries_num_average() {
        let proc = make_proc(1, "TST_TS_AVG");
        let base = PortDriverBase::new(
            "TST_TS_AVG_B",
            2,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let p = proc.state.lock().p.clone();
        let out = proc.validate_float64(
            p.ts_time_per_point,
            0,
            0.5,
            WriteVerdict::Accept(0.5),
            &base,
        );
        assert!(out.settled);
        let out = proc.validate_float64(
            p.ts_averaging_time,
            0,
            2.0,
            WriteVerdict::Accept(2.0),
            &base,
        );
        assert!(out.settled);
        assert!(out.side.contains(&SideParam::Float64 {
            reason: p.ts_averaging_time,
            addr: 0,
            value: 2.0,
        }));
        assert!(out.side.contains(&SideParam::Int32 {
            reason: p.ts_num_average,
            addr: 0,
            value: 4,
        }));
        assert!(out.side.iter().any(|s| matches!(
            s,
            SideParam::Float64Array { reason, .. } if *reason == p.ts_time_axis
        )));
    }

    /// A base-class param is left to the port: no side params, not settled.
    #[test]
    fn test_a_base_param_write_is_not_settled() {
        let proc = make_proc(1, "TST_TS_BASE");
        let mut base = PortDriverBase::new(
            "TST_TS_BASE_B",
            2,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        // make_proc numbers the TS params from 0; this is none of them.
        const BASE: usize = 1000;
        let out = proc.validate_int32(
            BASE,
            0,
            1,
            WriteVerdict::Accept(1),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );
        assert!(!out.settled && out.side.is_empty() && out.addressed.0.is_empty());
        let out = proc.validate_float64(BASE, 0, 1.0, WriteVerdict::Accept(1.0), &base);
        assert!(!out.settled && out.side.is_empty());
    }

    fn find_array(res: &ProcessResult, reason: usize, addr: i32) -> Option<Vec<f64>> {
        res.param_updates.iter().find_map(|u| match u {
            ParamUpdate::Float64Array {
                reason: r,
                addr: a,
                value,
            } if *r == reason && *a == addr => Some(value.clone()),
            _ => None,
        })
    }

    fn find_int(res: &ProcessResult, reason: usize) -> Option<i32> {
        res.param_updates.iter().find_map(|u| match u {
            ParamUpdate::Int32 {
                reason: r, value, ..
            } if *r == reason => Some(*value),
            _ => None,
        })
    }

    /// CBUG-B25, end to end. Was
    /// `test_process_array_uint8_truncating_average_per_signal`, which asserted
    /// 29.0 — C's answer, because it narrowed the sum (600) to u8 (88) before
    /// dividing by 3. The average of three 200s is 200.
    #[test]
    fn test_process_array_uint8_average_per_signal() {
        let proc = make_proc(2, "TST_TS_U8");
        // numAverage = 3 via timePerPoint=1, averagingTime=3.
        proc.state.lock().time_per_point = 1.0;
        proc.state.lock().averaging_time_requested = 3.0;
        let _ = proc.state.lock().compute_num_average();
        assert_eq!(proc.state.lock().num_average, 3);
        proc.state.lock().acquiring = true;

        let pool = NDArrayPool::new(1_000_000);
        // 2 signals, 3 time points, every sample 200. Layout signal-fastest.
        let arr = NDArray::with_data(
            vec![NDDimension::new(2), NDDimension::new(3)],
            NDDataBuffer::from_vec::<u8>(vec![200; 6]),
        );
        let res = proc.process_array(&Arc::new(arr), &pool);

        // One averaged output point per signal: 600 / 3 = 200 (C: 29).
        assert_eq!(proc.state.lock().current_time_point, 1);
        let ntp = proc.state.lock().num_time_points;
        assert_eq!(
            proc.state
                .lock()
                .circular
                .as_ref()
                .unwrap()
                .data
                .get_as_f64(0)
                .unwrap(),
            200.0
        );
        assert_eq!(
            proc.state
                .lock()
                .circular
                .as_ref()
                .unwrap()
                .data
                .get_as_f64(ntp)
                .unwrap(),
            200.0
        ); // signal 1, t0
        // Current point posted; Fixed mode not full yet, so no waveform callback.
        assert_eq!(
            find_int(&res, proc.state.lock().p.ts_current_point),
            Some(1)
        );
        assert!(find_array(&res, proc.state.lock().p.ts_time_series, 0).is_none());
    }

    #[test]
    fn test_fixed_mode_fills_stops_and_emits_waveforms() {
        let proc = make_proc(1, "TST_TS_FIX");
        proc.state.lock().num_time_points = 2; // small buffer; realloc on first frame
        proc.state.lock().acquiring = true; // num_average stays 1

        let pool = NDArrayPool::new(1_000_000);
        // 1 signal, 3 time points: 10, 20, 30.
        let arr = NDArray::with_data(
            vec![NDDimension::new(1), NDDimension::new(3)],
            NDDataBuffer::from_vec::<f64>(vec![10.0, 20.0, 30.0]),
        );
        let res = proc.process_array(&Arc::new(arr), &pool);

        // Buffer of 2 fills at the 2nd point: acquisition stops, 3rd point dropped.
        assert!(!proc.state.lock().acquiring);
        assert_eq!(proc.state.lock().current_time_point, 2);
        assert_eq!(find_int(&res, proc.state.lock().p.ts_acquire), Some(0));
        let wf = find_array(&res, proc.state.lock().p.ts_time_series, 0).expect("waveform emitted");
        assert_eq!(wf, vec![10.0, 20.0]);
    }

    #[test]
    fn test_circular_mode_wraps_and_rotates_oldest_first() {
        let proc = make_proc(1, "TST_TS_CIRC");
        proc.state.lock().num_time_points = 3;
        proc.state.lock().acquire_mode = AcquireMode::Circular;
        proc.state.lock().acquiring = true;

        let pool = NDArrayPool::new(1_000_000);
        // 1 signal, 5 time points: 1..=5 into a 3-slot ring.
        let arr = NDArray::with_data(
            vec![NDDimension::new(1), NDDimension::new(5)],
            NDDataBuffer::from_vec::<f64>(vec![1.0, 2.0, 3.0, 4.0, 5.0]),
        );
        proc.process_array(&Arc::new(arr), &pool);

        // Ring holds [4,5,3] with write position at 2; Circular never stops.
        assert!(proc.state.lock().acquiring);
        assert_eq!(proc.state.lock().current_time_point, 2);
        // Reading rotates so the oldest point is first: [3,4,5].
        let updates = {
            let state = proc.state.lock();
            state.time_series_waveforms(state.circular.as_ref().unwrap())
        };
        let wf = updates
            .iter()
            .find_map(|u| match u {
                ParamUpdate::Float64Array {
                    reason,
                    addr,
                    value,
                } if *reason == proc.state.lock().p.ts_time_series && *addr == 0 => {
                    Some(value.clone())
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(wf, vec![3.0, 4.0, 5.0]);
    }

    #[test]
    fn test_one_d_array_is_single_time_point_across_signals() {
        let proc = make_proc(3, "TST_TS_1D");
        proc.state.lock().acquiring = true; // num_average == 1

        let pool = NDArrayPool::new(1_000_000);
        // 1-D array of 3 elements => 3 signals, 1 time point.
        let arr = NDArray::with_data(
            vec![NDDimension::new(3)],
            NDDataBuffer::from_vec::<f64>(vec![11.0, 22.0, 33.0]),
        );
        proc.process_array(&Arc::new(arr), &pool);

        assert_eq!(proc.state.lock().num_signals, 3);
        assert_eq!(proc.state.lock().current_time_point, 1);
        let ntp = proc.state.lock().num_time_points;
        assert_eq!(
            proc.state
                .lock()
                .circular
                .as_ref()
                .unwrap()
                .data
                .get_as_f64(0)
                .unwrap(),
            11.0
        );
        assert_eq!(
            proc.state
                .lock()
                .circular
                .as_ref()
                .unwrap()
                .data
                .get_as_f64(ntp)
                .unwrap(),
            22.0
        );
        assert_eq!(
            proc.state
                .lock()
                .circular
                .as_ref()
                .unwrap()
                .data
                .get_as_f64(2 * ntp)
                .unwrap(),
            33.0
        );
    }

    #[test]
    fn test_num_signals_capped_at_max_signals() {
        let proc = make_proc(2, "TST_TS_CAP");
        proc.state.lock().acquiring = true;

        let pool = NDArrayPool::new(1_000_000);
        // 4 input signals but max_signals == 2: only the first 2 are kept.
        let arr = NDArray::with_data(
            vec![NDDimension::new(4), NDDimension::new(1)],
            NDDataBuffer::from_vec::<f64>(vec![1.0, 2.0, 3.0, 4.0]),
        );
        proc.process_array(&Arc::new(arr), &pool);

        assert_eq!(proc.state.lock().num_signals, 2);
        assert_eq!(
            proc.state
                .lock()
                .circular
                .as_ref()
                .unwrap()
                .data
                .get_as_f64(0)
                .unwrap(),
            1.0
        );
        let ntp = proc.state.lock().num_time_points;
        assert_eq!(
            proc.state
                .lock()
                .circular
                .as_ref()
                .unwrap()
                .data
                .get_as_f64(ntp)
                .unwrap(),
            2.0
        );
    }

    #[test]
    fn test_invalid_ndims_is_ignored() {
        let proc = make_proc(1, "TST_TS_BAD");
        proc.state.lock().acquiring = true;
        let pool = NDArrayPool::new(1_000_000);
        // 3-D array: C rejects (ndims must be 1 or 2).
        let mut arr = NDArray::with_data(
            vec![
                NDDimension::new(2),
                NDDimension::new(2),
                NDDimension::new(2),
            ],
            NDDataBuffer::from_vec::<f64>(vec![0.0; 8]),
        );
        let stamp = EpicsTimestamp { sec: 7, nsec: 9 };
        arr.timestamp = stamp;
        let res = proc.process_array(&Arc::new(arr), &pool);
        // beginProcessCallbacks stamped the port before the refusal.
        assert_eq!(proc.state.lock().epics_ts, stamp);
        assert!(res.param_updates.is_empty());
        assert_eq!(
            res.trace,
            vec![(
                asyn_rs::trace::TraceMask::ERROR,
                "NDPluginTimeSeries::processCallbacks: error, number of array dimensions must be 1 or 2\n"
                    .to_string()
            )]
        );
        assert_eq!(proc.state.lock().current_time_point, 0);
    }

    #[test]
    fn test_acquire_mode_flips_time_axis() {
        let proc = make_proc(1, "TST_TS_AXIS");
        proc.state.lock().num_time_points = 4;
        // Realloc to size the axis (first frame would also do this).
        let _ = proc.state.lock().allocate_arrays();

        // Fixed axis: 0, 1, 2, 3 (averagingTimeActual == 1).
        let fixed = proc.state.lock().create_axis_array();
        let fixed_axis = match &fixed[0] {
            ParamUpdate::Float64Array { value, .. } => value.clone(),
            _ => panic!("expected axis"),
        };
        assert_eq!(fixed_axis, vec![0.0, 1.0, 2.0, 3.0]);

        // Circular axis ends at 0: -3, -2, -1, 0.
        proc.state.lock().acquire_mode = AcquireMode::Circular;
        let circ = proc.state.lock().create_axis_array();
        let circ_axis = match &circ[0] {
            ParamUpdate::Float64Array { value, .. } => value.clone(),
            _ => panic!("expected axis"),
        };
        assert_eq!(circ_axis, vec![-3.0, -2.0, -1.0, 0.0]);
    }

    /// A Fixed 8-point port on 8 signals at TimePerPoint 1, driven through
    /// its writes and frames as the cbAD oracle drove C.
    struct AverageRig {
        proc: TimeSeriesProcessor,
        base: PortDriverBase,
        p: Params,
        int32: bool,
    }

    impl AverageRig {
        fn new(port: &str) -> Self {
            let proc = make_proc(8, port);
            let base = PortDriverBase::new(
                &format!("{port}_B"),
                9,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            let p = proc.state.lock().p.clone();
            let mut rig = Self {
                proc,
                base,
                p,
                int32: false,
            };
            rig.int(rig.p.ts_num_points, 8);
            rig.dbl(rig.p.ts_time_per_point, 1.0);
            rig
        }

        fn int(&mut self, reason: usize, value: i32) -> WriteOutcome<i32> {
            self.proc.validate_int32(
                reason,
                0,
                value,
                WriteVerdict::Accept(value),
                &mut ad_core_rs::plugin::runtime::WritePort::new(&mut self.base),
            )
        }

        fn dbl(&mut self, reason: usize, value: f64) {
            self.proc
                .validate_float64(reason, 0, value, WriteVerdict::Accept(value), &self.base);
        }

        /// One time point per frame, `v` on each of `signals`.
        fn frames(&self, signals: usize, values: &[f64]) {
            for &v in values {
                let data = if self.int32 {
                    NDDataBuffer::from_vec::<i32>(vec![v as i32; signals])
                } else {
                    NDDataBuffer::from_vec::<f64>(vec![v; signals])
                };
                let arr =
                    NDArray::with_data(vec![NDDimension::new(signals), NDDimension::new(1)], data);
                self.proc
                    .process_array(&Arc::new(arr), &NDArrayPool::new(0));
            }
        }

        /// TSRead's series for `addr`; empty when nothing posts there.
        fn read(&mut self, addr: i32) -> Vec<f64> {
            let out = self.int(self.p.ts_read, 1);
            side_array(&out, self.p.ts_time_series, addr).unwrap_or_default()
        }

        /// Two of four samples of 10 in progress, then `reset`.
        fn partial_then(&mut self, reset: impl FnOnce(&mut Self)) -> [Vec<f64>; 2] {
            self.dbl(self.p.ts_averaging_time, 4.0);
            self.int(self.p.ts_acquire, 1);
            self.frames(8, &[10.0, 10.0]);
            reset(self);
            self.frames(8, &[50.0, 50.0]);
            let first = self.read(0);
            self.frames(8, &[50.0, 50.0]);
            [first, self.read(0)]
        }
    }

    /// The window `averageStore_`/`numAveraged_` restarts as one, whenever the
    /// series or NumAverage does. C's values for each read, from the cbAD
    /// oracle, are in the comments.
    #[test]
    fn the_average_restarts_with_its_window() {
        // NumAverage 4 -> 2 after three samples of 10. C: [35], [35, 20].
        let mut rig = AverageRig::new("TST_TS_W_NAVG");
        rig.dbl(rig.p.ts_averaging_time, 4.0);
        rig.int(rig.p.ts_acquire, 1);
        rig.frames(8, &[10.0, 10.0, 10.0]);
        rig.dbl(rig.p.ts_averaging_time, 2.0);
        rig.frames(8, &[20.0, 20.0]);
        assert_eq!(rig.read(0), [20.0]);
        rig.frames(8, &[20.0, 20.0]);
        assert_eq!(rig.read(0), [20.0, 20.0]);

        // Acquire stop and start. C: [30], [30].
        let mut rig = AverageRig::new("TST_TS_W_ACQ");
        let (acquire, num_points, mode) =
            (rig.p.ts_acquire, rig.p.ts_num_points, rig.p.ts_acquire_mode);
        let reads = rig.partial_then(|r| {
            r.int(acquire, 0);
            r.int(acquire, 1);
        });
        assert_eq!(reads, [vec![], vec![50.0]]);

        // NumPoints 8 -> 6. C: [30], [30].
        let mut rig = AverageRig::new("TST_TS_W_NPTS");
        let reads = rig.partial_then(|r| {
            r.int(num_points, 6);
        });
        assert_eq!(reads, [vec![], vec![50.0]]);

        // Circular mode, rotated oldest first. C: [0 x7, 30] both times.
        let mut rig = AverageRig::new("TST_TS_W_MODE");
        let reads = rig.partial_then(|r| {
            r.int(mode, 1);
        });
        let mut last = vec![0.0; 8];
        last[7] = 50.0;
        assert_eq!(reads, [vec![0.0; 8], last]);

        // Float64 -> Int32 frames. C: [30], [30].
        let mut rig = AverageRig::new("TST_TS_W_TYPE");
        let reads = rig.partial_then(|r| r.int32 = true);
        assert_eq!(reads, [vec![], vec![50.0]]);

        // 8 -> 4 -> 8 signals at NumAverage 2. C: [15]; [30] and [35];
        // [30, 30] and [35, 30].
        let mut rig = AverageRig::new("TST_TS_W_SIGS");
        rig.dbl(rig.p.ts_averaging_time, 2.0);
        rig.int(rig.p.ts_acquire, 1);
        rig.frames(8, &[10.0]);
        rig.frames(4, &[20.0]);
        assert_eq!(rig.read(0), Vec::<f64>::new());
        rig.frames(8, &[30.0, 30.0]);
        assert_eq!((rig.read(0), rig.read(4)), (vec![30.0], vec![30.0]));
        rig.frames(8, &[30.0, 30.0]);
        assert_eq!(
            (rig.read(0), rig.read(4)),
            (vec![30.0, 30.0], vec![30.0, 30.0])
        );
    }

    #[test]
    fn test_compute_num_average_from_averaging_time() {
        let proc = make_proc(1, "TST_TS_NAVG");
        proc.state.lock().time_per_point = 0.5;
        proc.state.lock().averaging_time_requested = 2.0;
        proc.state.lock().compute_num_average();
        // (int)(2.0/0.5 + 0.5) = (int)4.5 = 4; actual = 0.5 * 4 = 2.0.
        assert_eq!(proc.state.lock().num_average, 4);
        assert_eq!(proc.state.lock().averaging_time_actual, 2.0);

        // timePerPoint == 0 => numAverage 1, actual == requested.
        proc.state.lock().time_per_point = 0.0;
        proc.state.lock().averaging_time_requested = 7.0;
        proc.state.lock().compute_num_average();
        assert_eq!(proc.state.lock().num_average, 1);
        assert_eq!(proc.state.lock().averaging_time_actual, 7.0);
    }
}
