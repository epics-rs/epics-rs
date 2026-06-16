//! Standalone `NDPluginTimeSeries` (port of ADCore `NDPluginTimeSeries.cpp`).
//!
//! Ingests raw 1-D or 2-D NDArrays shaped `[numSignalsIn, numTimes]` (signal is
//! the fastest-varying dimension, `dims[0]`; time is `dims[1]`) and produces, per
//! signal, a time-series waveform. Every `numAverage` input time points are
//! averaged into one output point and written into a per-signal circular buffer.
//!
//! This is distinct from the receiver-fed [`crate::time_series`] port driver: that
//! one accumulates already-derived per-frame scalars from Stats/ROIStat/Attribute
//! (C's *embedded* time series), whereas this plugin is the standalone areaDetector
//! `NDPluginTimeSeries` that consumes raw detector arrays.
//!
//! Output form matches C exactly: `TS_TIME_SERIES` is read at `addr == signal`
//! (NDTimeSeriesN.template binds `@asyn($(PORT),$(SIGNAL),...)TS_TIME_SERIES` with
//! `SCAN="I/O Intr"`), so the per-signal waveforms are delivered by the callback
//! push (C `doCallbacksFloat64Array`), not by a pull read.
//!
//! Integer-truncating averaging is the parity-critical detail: C computes the
//! averaged point as `(epicsType)averageStore_[signal] / numAveraged_`
//! (NDPluginTimeSeries.cpp:191) — the double *sum* is cast to the element type
//! first (wrapping/truncating), then integer-divided in the element type's
//! promoted domain. For e.g. `UInt8` inputs `200,200,200` with `numAverage == 3`
//! the sum is `600`, `(u8)600 == 88`, and `88 / 3 == 29` — not the `200` a plain
//! `f64` divide would give. [`averaged_value`] reproduces this deterministically.
//!
//! Residual (documented, not a parity gap for the waveform records): C also fires
//! `doCallbacksGenericPointer` downstream NDArrays — a 2-D array at `addr ==
//! numSignals_` and a 1-D array per signal — when `NDArrayCallbacks` is set. This
//! port does not yet emit those downstream NDArrays (the per-address NDArray
//! delivery is not expressible in the broadcast `ProcessResult` model); the
//! observable waveform/scalar records are fully implemented.

use std::time::Instant;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamChangeResult, ParamUpdate, PluginParamSnapshot, ProcessResult,
};
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;

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
    match data {
        NDDataBuffer::I8(v) => v[idx] as f64,
        NDDataBuffer::U8(v) => v[idx] as f64,
        NDDataBuffer::I16(v) => v[idx] as f64,
        NDDataBuffer::U16(v) => v[idx] as f64,
        NDDataBuffer::I32(v) => v[idx] as f64,
        NDDataBuffer::U32(v) => v[idx] as f64,
        NDDataBuffer::I64(v) => v[idx] as f64,
        NDDataBuffer::U64(v) => v[idx] as f64,
        NDDataBuffer::F32(v) => v[idx] as f64,
        NDDataBuffer::F64(v) => v[idx],
    }
}

/// Compute one averaged time-series point exactly as C
/// `(epicsType)averageStore_[signal] / numAveraged_` (NDPluginTimeSeries.cpp:191).
///
/// The double `sum` is cast to the element type **first** — narrowing with C
/// wrapping semantics (gcc truncates the double toward zero into a 64-bit
/// register, then the assignment narrows to the element width) — and **then**
/// integer-divided in the element type's promoted domain. The result is the value
/// that lands in the `epicsType` circular buffer and is later widened back to
/// `double` for the waveform callback, so returning it as `f64` is exact.
///
/// For integer types narrower than 32 bits the wrap is reproduced via an `i64`
/// intermediate (exact for every reachable sum). For 32/64-bit types the cast is
/// taken directly; division follows C's promotion (signed for `int`-promoted
/// types, unsigned for `u32`/`u64`). Float types do a plain (non-truncating)
/// cast-then-divide.
fn averaged_value(sum: f64, num_averaged: usize, dt: NDDataType) -> f64 {
    let n = num_averaged.max(1);
    match dt {
        NDDataType::Int8 => {
            let c = sum.trunc() as i64 as i8;
            ((c as i32) / (n as i32)) as i8 as f64
        }
        NDDataType::UInt8 => {
            let c = sum.trunc() as i64 as u8;
            ((c as i32) / (n as i32)) as u8 as f64
        }
        NDDataType::Int16 => {
            let c = sum.trunc() as i64 as i16;
            ((c as i32) / (n as i32)) as i16 as f64
        }
        NDDataType::UInt16 => {
            let c = sum.trunc() as i64 as u16;
            ((c as i32) / (n as i32)) as u16 as f64
        }
        NDDataType::Int32 => {
            let c = sum.trunc() as i64 as i32;
            (c / (n as i32)) as f64
        }
        NDDataType::UInt32 => {
            let c = sum.trunc() as i64 as u32;
            (c / (n as u32)) as f64
        }
        NDDataType::Int64 => {
            let c = sum.trunc() as i64;
            (c / (n as i64)) as f64
        }
        NDDataType::UInt64 => {
            let c = sum.trunc() as u64;
            (c / (n as u64)) as f64
        }
        NDDataType::Float32 => ((sum as f32) / (n as f32)) as f64,
        NDDataType::Float64 => sum / (n as f64),
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
    fn key(u: &ParamUpdate) -> (usize, i32) {
        match u {
            ParamUpdate::Int32 { reason, addr, .. }
            | ParamUpdate::Float64 { reason, addr, .. }
            | ParamUpdate::Octet { reason, addr, .. }
            | ParamUpdate::Float64Array { reason, addr, .. } => (*reason, *addr),
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(updates.len());
    // Keep the last occurrence of each key while preserving relative order.
    for u in updates.into_iter().rev() {
        if seen.insert(key(&u)) {
            out.push(u);
        }
    }
    out.reverse();
    out
}

/// Standalone `NDPluginTimeSeries` processor.
pub struct TimeSeriesProcessor {
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
    /// C `averageStore_`: running per-signal sum (length `max_signals`).
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
    /// C `pTimeCircular_->pData`, stored as `f64` of the already-narrowed
    /// `epicsType` value: indexed `signal * num_time_points + t`.
    circular: Vec<f64>,
    /// C `timeStamp_`: per-output-point source timestamps (length `num_time_points`).
    time_stamp: Vec<f64>,
    /// C `startTime_`.
    start_time: Instant,
    p: Params,
}

impl TimeSeriesProcessor {
    /// Create a processor for `max_signals` signals (clamped to at least 1, per C
    /// `if (maxSignals < 1) maxSignals = 1`).
    pub fn new(max_signals: usize) -> Self {
        let max_signals = max_signals.max(1);
        let num_time_points = DEFAULT_NUM_TSPOINTS;
        Self {
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
            circular: vec![0.0; max_signals * num_time_points],
            time_stamp: vec![0.0; num_time_points],
            start_time: Instant::now(),
            p: Params::sentinel(),
        }
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
    /// and timestamps, reset `currentTimePoint_`, restart `startTime_`. C does
    /// **not** reset the averaging accumulator here.
    fn acquire_reset(&mut self) -> Vec<ParamUpdate> {
        self.circular.iter_mut().for_each(|v| *v = 0.0);
        self.time_stamp.iter_mut().for_each(|v| *v = 0.0);
        self.current_time_point = 0;
        self.start_time = Instant::now();
        vec![ParamUpdate::int32(self.p.ts_current_point, 0)]
    }

    /// C `allocateArrays` (NDPluginTimeSeries.cpp:115-135): (re)size the circular
    /// buffer and timestamps to `numSignals_ * numTimePoints_`, rebuild the axis,
    /// and reset acquisition.
    fn allocate_arrays(&mut self) -> Vec<ParamUpdate> {
        self.circular = vec![0.0; self.num_signals * self.num_time_points];
        self.time_stamp = vec![0.0; self.num_time_points];
        let mut updates = self.create_axis_array();
        updates.extend(self.acquire_reset());
        updates
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
        self.num_averaged = 0;
        let mut updates = vec![
            ParamUpdate::float64(self.p.ts_averaging_time, self.averaging_time_actual),
            ParamUpdate::int32(self.p.ts_num_average, self.num_average as i32),
        ];
        updates.extend(self.create_axis_array());
        updates
    }

    /// C `doTimeSeriesCallbacksT` (NDPluginTimeSeries.cpp:263-290): emit one
    /// `TS_TIME_SERIES` Float64Array per signal at `addr == signal`. Fixed mode
    /// emits the filled prefix (`currentTimePoint_` points); Circular mode emits
    /// the full ring rotated so the oldest point is first.
    fn do_time_series_callbacks(&self) -> Vec<ParamUpdate> {
        let ntp = self.num_time_points;
        let mut updates = Vec::with_capacity(self.num_signals);
        match self.acquire_mode {
            AcquireMode::Fixed => {
                for signal in 0..self.num_signals {
                    let start = signal * ntp;
                    let series = self.circular[start..start + self.current_time_point].to_vec();
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
                        series.push(self.circular[base + time_in]);
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
    fn add_to_time_series(&mut self, array: &NDArray) -> Vec<ParamUpdate> {
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
                self.circular[s * ntp + self.current_time_point] = avg;
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
                        updates.extend(self.do_time_series_callbacks());
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
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        // C processCallbacks: this plugin only handles 1-D or 2-D arrays.
        let ndims = array.dims.len();
        if !(1..=2).contains(&ndims) {
            return ProcessResult::empty();
        }

        let mut updates: Vec<ParamUpdate> = Vec::new();

        // C: reallocate when the data type or the input signal count changes.
        let num_signals_in = array.dims[0].size;
        let dtype = array.data.data_type();
        if dtype != self.data_type || (num_signals_in as i64) != self.num_signals_in {
            self.data_type = dtype;
            self.num_signals_in = num_signals_in as i64;
            self.num_signals = num_signals_in.min(self.max_signals);
            updates.extend(self.allocate_arrays());
        }

        // C: only accumulate while P_TSAcquire is set.
        if self.acquiring {
            updates.extend(self.add_to_time_series(array));
        }

        ProcessResult::sink(coalesce_updates(updates))
    }

    fn plugin_type(&self) -> &str {
        "NDPluginTimeSeries"
    }

    fn register_params(&mut self, base: &mut PortDriverBase) -> asyn_rs::error::AsynResult<()> {
        // Per-plugin parameters (NDPluginTimeSeries.cpp:69-79).
        self.p.ts_acquire = base.create_param("TS_ACQUIRE", ParamType::Int32)?;
        self.p.ts_read = base.create_param("TS_READ", ParamType::Int32)?;
        self.p.ts_num_points = base.create_param("TS_NUM_POINTS", ParamType::Int32)?;
        self.p.ts_current_point = base.create_param("TS_CURRENT_POINT", ParamType::Int32)?;
        self.p.ts_time_per_point = base.create_param("TS_TIME_PER_POINT", ParamType::Float64)?;
        self.p.ts_averaging_time = base.create_param("TS_AVERAGING_TIME", ParamType::Float64)?;
        self.p.ts_num_average = base.create_param("TS_NUM_AVERAGE", ParamType::Int32)?;
        self.p.ts_elapsed_time = base.create_param("TS_ELAPSED_TIME", ParamType::Float64)?;
        self.p.ts_acquire_mode = base.create_param("TS_ACQUIRE_MODE", ParamType::Int32)?;
        self.p.ts_time_axis = base.create_param("TS_TIME_AXIS", ParamType::Float64Array)?;
        self.p.ts_timestamp = base.create_param("TS_TIMESTAMP", ParamType::Float64Array)?;
        // Per-signal parameter (NDPluginTimeSeries.cpp:82); read at addr == signal.
        self.p.ts_time_series = base.create_param("TS_TIME_SERIES", ParamType::Float64Array)?;

        // Initial values (C constructor: setIntegerParam(P_TSNumPoints, 2048),
        // numAverage_ == 1, acquireMode_ == Fixed, etc.).
        base.set_int32_param(self.p.ts_num_points, 0, self.num_time_points as i32)?;
        base.set_int32_param(self.p.ts_num_average, 0, self.num_average as i32)?;
        base.set_int32_param(self.p.ts_acquire, 0, 0)?;
        base.set_int32_param(self.p.ts_acquire_mode, 0, 0)?;
        base.set_int32_param(self.p.ts_current_point, 0, 0)?;
        base.set_float64_param(self.p.ts_averaging_time, 0, self.averaging_time_actual)?;
        base.set_float64_param(self.p.ts_time_per_point, 0, self.time_per_point)?;

        // Initial Fixed-mode time axis (C constructor allocateArrays -> createAxisArray).
        let axis: Vec<f64> = (0..self.num_time_points)
            .map(|i| i as f64 * self.averaging_time_actual)
            .collect();
        base.params
            .set_float64_array(self.p.ts_time_axis, 0, axis)?;
        Ok(())
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        params: &PluginParamSnapshot,
    ) -> ParamChangeResult {
        let mut updates = Vec::new();
        if reason == self.p.ts_num_points {
            // C writeInt32 P_TSNumPoints -> allocateArrays().
            self.num_time_points = params.value.as_i32().max(1) as usize;
            updates.extend(self.allocate_arrays());
        } else if reason == self.p.ts_acquire_mode {
            // C writeInt32 P_TSAcquireMode -> acquireReset(); createAxisArray().
            self.acquire_mode = if params.value.as_i32() == 0 {
                AcquireMode::Fixed
            } else {
                AcquireMode::Circular
            };
            updates.extend(self.acquire_reset());
            updates.extend(self.create_axis_array());
        } else if reason == self.p.ts_acquire {
            // C writeInt32 P_TSAcquire -> if value acquireReset() else doTimeSeriesCallbacks().
            if params.value.as_i32() != 0 {
                self.acquiring = true;
                updates.extend(self.acquire_reset());
            } else {
                self.acquiring = false;
                updates.extend(self.do_time_series_callbacks());
            }
        } else if reason == self.p.ts_read {
            // C writeInt32 P_TSRead -> doTimeSeriesCallbacks().
            updates.extend(self.do_time_series_callbacks());
        } else if reason == self.p.ts_time_per_point {
            // C writeFloat64 P_TSTimePerPoint -> computeNumAverage().
            self.time_per_point = params.value.as_f64();
            updates.extend(self.compute_num_average());
        } else if reason == self.p.ts_averaging_time {
            // C writeFloat64 P_TSAveragingTime -> computeNumAverage().
            self.averaging_time_requested = params.value.as_f64();
            updates.extend(self.compute_num_average());
        }
        ParamChangeResult::updates(updates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::NDDimension;
    use asyn_rs::port::{PortDriverBase, PortFlags};

    // ---- averaged_value: the parity-critical integer truncation ----

    #[test]
    fn test_averaged_value_uint8_truncates_not_f64_divide() {
        // C NDPluginTimeSeries.cpp:191. 200*3 = 600; (u8)600 = 88; 88/3 = 29.
        assert_eq!(averaged_value(600.0, 3, NDDataType::UInt8), 29.0);
        // A plain f64 divide would yield 200 — prove we diverge from it.
        assert_ne!(averaged_value(600.0, 3, NDDataType::UInt8), 200.0);
    }

    #[test]
    fn test_averaged_value_int8_negative_truncates_toward_zero() {
        // -200*3 = -600; (i8)(-600) = -88; -88/3 = -29 (C integer division).
        assert_eq!(averaged_value(-600.0, 3, NDDataType::Int8), -29.0);
    }

    #[test]
    fn test_averaged_value_uint16_wraps() {
        // 35000*2 = 70000; (u16)70000 = 4464; 4464/2 = 2232.
        assert_eq!(averaged_value(70000.0, 2, NDDataType::UInt16), 2232.0);
    }

    #[test]
    fn test_averaged_value_int32_in_range_no_wrap() {
        assert_eq!(averaged_value(600.0, 3, NDDataType::Int32), 200.0);
    }

    #[test]
    fn test_averaged_value_float_types_exact() {
        assert_eq!(averaged_value(600.0, 3, NDDataType::Float64), 200.0);
        assert_eq!(averaged_value(600.0, 3, NDDataType::Float32), 200.0);
    }

    #[test]
    fn test_averaged_value_numaverage_one_is_passthrough() {
        assert_eq!(averaged_value(200.0, 1, NDDataType::UInt8), 200.0);
        assert_eq!(averaged_value(-50.0, 1, NDDataType::Int8), -50.0);
    }

    // ---- process_array end-to-end ----

    fn make_proc(max_signals: usize, port: &str) -> TimeSeriesProcessor {
        let mut proc = TimeSeriesProcessor::new(max_signals);
        let mut base = PortDriverBase::new(port, max_signals + 1, PortFlags::default());
        proc.register_params(&mut base).unwrap();
        proc
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

    #[test]
    fn test_process_array_uint8_truncating_average_per_signal() {
        let mut proc = make_proc(2, "TST_TS_U8");
        // numAverage = 3 via timePerPoint=1, averagingTime=3.
        proc.time_per_point = 1.0;
        proc.averaging_time_requested = 3.0;
        let _ = proc.compute_num_average();
        assert_eq!(proc.num_average, 3);
        proc.acquiring = true;

        let pool = NDArrayPool::new(1_000_000);
        // 2 signals, 3 time points, every sample 200. Layout signal-fastest.
        let arr = NDArray::with_data(
            vec![NDDimension::new(2), NDDimension::new(3)],
            NDDataBuffer::U8(vec![200; 6]),
        );
        let res = proc.process_array(&arr, &pool);

        // One averaged output point per signal: (u8)600 / 3 = 29.
        assert_eq!(proc.current_time_point, 1);
        assert_eq!(proc.circular[0 * proc.num_time_points], 29.0);
        assert_eq!(proc.circular[proc.num_time_points], 29.0); // signal 1, t0
        // Current point posted; Fixed mode not full yet, so no waveform callback.
        assert_eq!(find_int(&res, proc.p.ts_current_point), Some(1));
        assert!(find_array(&res, proc.p.ts_time_series, 0).is_none());
    }

    #[test]
    fn test_fixed_mode_fills_stops_and_emits_waveforms() {
        let mut proc = make_proc(1, "TST_TS_FIX");
        proc.num_time_points = 2; // small buffer; realloc on first frame
        proc.acquiring = true; // num_average stays 1

        let pool = NDArrayPool::new(1_000_000);
        // 1 signal, 3 time points: 10, 20, 30.
        let arr = NDArray::with_data(
            vec![NDDimension::new(1), NDDimension::new(3)],
            NDDataBuffer::F64(vec![10.0, 20.0, 30.0]),
        );
        let res = proc.process_array(&arr, &pool);

        // Buffer of 2 fills at the 2nd point: acquisition stops, 3rd point dropped.
        assert!(!proc.acquiring);
        assert_eq!(proc.current_time_point, 2);
        assert_eq!(find_int(&res, proc.p.ts_acquire), Some(0));
        let wf = find_array(&res, proc.p.ts_time_series, 0).expect("waveform emitted");
        assert_eq!(wf, vec![10.0, 20.0]);
    }

    #[test]
    fn test_circular_mode_wraps_and_rotates_oldest_first() {
        let mut proc = make_proc(1, "TST_TS_CIRC");
        proc.num_time_points = 3;
        proc.acquire_mode = AcquireMode::Circular;
        proc.acquiring = true;

        let pool = NDArrayPool::new(1_000_000);
        // 1 signal, 5 time points: 1..=5 into a 3-slot ring.
        let arr = NDArray::with_data(
            vec![NDDimension::new(1), NDDimension::new(5)],
            NDDataBuffer::F64(vec![1.0, 2.0, 3.0, 4.0, 5.0]),
        );
        proc.process_array(&arr, &pool);

        // Ring holds [4,5,3] with write position at 2; Circular never stops.
        assert!(proc.acquiring);
        assert_eq!(proc.current_time_point, 2);
        // Reading rotates so the oldest point is first: [3,4,5].
        let updates = proc.do_time_series_callbacks();
        let wf = updates
            .iter()
            .find_map(|u| match u {
                ParamUpdate::Float64Array {
                    reason,
                    addr,
                    value,
                } if *reason == proc.p.ts_time_series && *addr == 0 => Some(value.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(wf, vec![3.0, 4.0, 5.0]);
    }

    #[test]
    fn test_one_d_array_is_single_time_point_across_signals() {
        let mut proc = make_proc(3, "TST_TS_1D");
        proc.acquiring = true; // num_average == 1

        let pool = NDArrayPool::new(1_000_000);
        // 1-D array of 3 elements => 3 signals, 1 time point.
        let arr = NDArray::with_data(
            vec![NDDimension::new(3)],
            NDDataBuffer::F64(vec![11.0, 22.0, 33.0]),
        );
        proc.process_array(&arr, &pool);

        assert_eq!(proc.num_signals, 3);
        assert_eq!(proc.current_time_point, 1);
        let ntp = proc.num_time_points;
        assert_eq!(proc.circular[0], 11.0);
        assert_eq!(proc.circular[ntp], 22.0);
        assert_eq!(proc.circular[2 * ntp], 33.0);
    }

    #[test]
    fn test_num_signals_capped_at_max_signals() {
        let mut proc = make_proc(2, "TST_TS_CAP");
        proc.acquiring = true;

        let pool = NDArrayPool::new(1_000_000);
        // 4 input signals but max_signals == 2: only the first 2 are kept.
        let arr = NDArray::with_data(
            vec![NDDimension::new(4), NDDimension::new(1)],
            NDDataBuffer::F64(vec![1.0, 2.0, 3.0, 4.0]),
        );
        proc.process_array(&arr, &pool);

        assert_eq!(proc.num_signals, 2);
        assert_eq!(proc.circular[0], 1.0);
        assert_eq!(proc.circular[proc.num_time_points], 2.0);
    }

    #[test]
    fn test_invalid_ndims_is_ignored() {
        let mut proc = make_proc(1, "TST_TS_BAD");
        proc.acquiring = true;
        let pool = NDArrayPool::new(1_000_000);
        // 3-D array: C rejects (ndims must be 1 or 2).
        let arr = NDArray::with_data(
            vec![
                NDDimension::new(2),
                NDDimension::new(2),
                NDDimension::new(2),
            ],
            NDDataBuffer::F64(vec![0.0; 8]),
        );
        let res = proc.process_array(&arr, &pool);
        assert!(res.param_updates.is_empty());
        assert_eq!(proc.current_time_point, 0);
    }

    #[test]
    fn test_acquire_mode_flips_time_axis() {
        let mut proc = make_proc(1, "TST_TS_AXIS");
        proc.num_time_points = 4;
        // Realloc to size the axis (first frame would also do this).
        let _ = proc.allocate_arrays();

        // Fixed axis: 0, 1, 2, 3 (averagingTimeActual == 1).
        let fixed = proc.create_axis_array();
        let fixed_axis = match &fixed[0] {
            ParamUpdate::Float64Array { value, .. } => value.clone(),
            _ => panic!("expected axis"),
        };
        assert_eq!(fixed_axis, vec![0.0, 1.0, 2.0, 3.0]);

        // Circular axis ends at 0: -3, -2, -1, 0.
        proc.acquire_mode = AcquireMode::Circular;
        let circ = proc.create_axis_array();
        let circ_axis = match &circ[0] {
            ParamUpdate::Float64Array { value, .. } => value.clone(),
            _ => panic!("expected axis"),
        };
        assert_eq!(circ_axis, vec![-3.0, -2.0, -1.0, 0.0]);
    }

    #[test]
    fn test_compute_num_average_from_averaging_time() {
        let mut proc = make_proc(1, "TST_TS_NAVG");
        proc.time_per_point = 0.5;
        proc.averaging_time_requested = 2.0;
        proc.compute_num_average();
        // (int)(2.0/0.5 + 0.5) = (int)4.5 = 4; actual = 0.5 * 4 = 2.0.
        assert_eq!(proc.num_average, 4);
        assert_eq!(proc.averaging_time_actual, 2.0);

        // timePerPoint == 0 => numAverage 1, actual == requested.
        proc.time_per_point = 0.0;
        proc.averaging_time_requested = 7.0;
        proc.compute_num_average();
        assert_eq!(proc.num_average, 1);
        assert_eq!(proc.averaging_time_actual, 7.0);
    }
}
