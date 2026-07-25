use std::sync::Arc;
use std::time::Instant;

use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{PortRuntimeHandle, create_port_runtime, port_runtime_unavailable};
use asyn_rs::user::AsynUser;
// Same types as `epics_libcom_rs::runtime::task` — `epics-base-rs` re-exports
// the runtime layer, and this crate already depends on it unconditionally.
use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use parking_lot::Mutex;

// ===== Stats-specific channel definitions =====

/// Number of stats channels in the time series.
pub const NUM_STATS_TS_CHANNELS: usize = 23;

/// Channel names for the 23 NDStats time series channels.
pub const STATS_TS_CHANNEL_NAMES: [&str; NUM_STATS_TS_CHANNELS] = [
    "TSMinValue",
    "TSMinX",
    "TSMinY",
    "TSMaxValue",
    "TSMaxX",
    "TSMaxY",
    "TSMeanValue",
    "TSSigma",
    "TSTotal",
    "TSNet",
    "TSCentroidTotal",
    "TSCentroidX",
    "TSCentroidY",
    "TSSigmaX",
    "TSSigmaY",
    "TSSigmaXY",
    "TSSkewX",
    "TSSkewY",
    "TSKurtosisX",
    "TSKurtosisY",
    "TSEccentricity",
    "TSOrientation",
    "TSTimestamp",
];

// ===== Generic time series data =====

/// Shared data pushed from a plugin processor to a TS port driver.
/// `values` length must match the channel count configured on the driver.
pub struct TimeSeriesData {
    pub values: Vec<f64>,
}

/// Sender from plugin -> TS port.
pub type TimeSeriesSender = tokio::sync::mpsc::Sender<TimeSeriesData>;
/// Receiver in TS port background thread.
pub type TimeSeriesReceiver = tokio::sync::mpsc::Receiver<TimeSeriesData>;

/// Registry for pending TS receivers, keyed by upstream plugin port name.
/// NDStatsConfigure etc. store receivers here; NDTimeSeriesConfigure picks them up.
pub struct TsReceiverRegistry {
    inner: std::sync::Mutex<std::collections::HashMap<String, (TimeSeriesReceiver, Vec<String>)>>,
}

impl TsReceiverRegistry {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Store a receiver and its channel names for a given upstream port.
    pub fn store(
        &self,
        upstream_port: &str,
        receiver: TimeSeriesReceiver,
        channel_names: Vec<String>,
    ) {
        let mut map = self.inner.lock().unwrap();
        map.insert(upstream_port.to_string(), (receiver, channel_names));
    }

    /// Take a receiver for the given upstream port (returns None if not found or already taken).
    pub fn take(&self, upstream_port: &str) -> Option<(TimeSeriesReceiver, Vec<String>)> {
        let mut map = self.inner.lock().unwrap();
        map.remove(upstream_port)
    }
}

impl Default for TsReceiverRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Accumulation mode for time series.
///
/// Mirrors C++ `TSAcquireMode`: `OneShot` == `TSAcquireModeFixed` (acquisition
/// stops once `num_points` output points are collected); `RingBuffer` ==
/// `TSAcquireModeCircular` (the buffer wraps and acquisition continues).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSeriesMode {
    /// C++ `TSAcquireModeFixed`: stop at `num_points`.
    OneShot,
    /// C++ `TSAcquireModeCircular`: wrap and keep acquiring.
    RingBuffer,
}

/// Time-series accumulator: stores scalar/1D values from successive arrays.
pub struct TimeSeries {
    pub num_points: usize,
    pub mode: TimeSeriesMode,
    buffer: Vec<f64>,
    write_pos: usize,
    count: usize,
}

impl TimeSeries {
    pub fn new(num_points: usize, mode: TimeSeriesMode) -> Self {
        Self {
            num_points,
            mode,
            buffer: vec![0.0; num_points],
            write_pos: 0,
            count: 0,
        }
    }

    /// Add a value (e.g., mean of an array) to the time series.
    pub fn add_value(&mut self, value: f64) {
        match self.mode {
            TimeSeriesMode::OneShot => {
                if self.write_pos < self.num_points {
                    self.buffer[self.write_pos] = value;
                    self.write_pos += 1;
                    self.count = self.write_pos;
                }
            }
            TimeSeriesMode::RingBuffer => {
                self.buffer[self.write_pos % self.num_points] = value;
                self.write_pos += 1;
                self.count = self.count.max(self.write_pos.min(self.num_points));
            }
        }
    }

    /// Get the accumulated values in order.
    pub fn values(&self) -> Vec<f64> {
        match self.mode {
            TimeSeriesMode::OneShot => self.buffer[..self.count].to_vec(),
            TimeSeriesMode::RingBuffer => {
                if self.write_pos <= self.num_points {
                    self.buffer[..self.count].to_vec()
                } else {
                    let start = self.write_pos % self.num_points;
                    let mut result = Vec::with_capacity(self.num_points);
                    result.extend_from_slice(&self.buffer[start..]);
                    result.extend_from_slice(&self.buffer[..start]);
                    result
                }
            }
        }
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_pos = 0;
        self.count = 0;
    }

    /// Resize the buffer. Resets all data.
    pub fn resize(&mut self, num_points: usize) {
        self.num_points = num_points;
        self.buffer = vec![0.0; num_points];
        self.write_pos = 0;
        self.count = 0;
    }

    /// Change the accumulation mode. Resets all data.
    pub fn set_mode(&mut self, mode: TimeSeriesMode) {
        self.mode = mode;
        self.reset();
    }
}

// ===== Time Series Port Driver =====

/// Param indices for the TS port.
pub struct TSParams {
    pub ts_acquire: usize,
    pub ts_read: usize,
    pub ts_num_points: usize,
    pub ts_current_point: usize,
    pub ts_time_per_point: usize,
    pub ts_averaging_time: usize,
    pub ts_num_average: usize,
    pub ts_elapsed_time: usize,
    pub ts_acquire_mode: usize,
    pub ts_time_axis: usize,
    /// Per-channel waveform param indices (length = num_channels).
    pub ts_channels: Vec<usize>,
    /// Channel names (kept for registry building).
    pub channel_names: Vec<String>,
    /// Generic time series waveform (for NDTimeSeries.template).
    pub ts_time_series: usize,
    /// Timestamp waveform (for NDTimeSeries.template).
    pub ts_timestamp: usize,
}

/// Shared state between the data ingestion thread and the TS port driver.
pub struct SharedTsState {
    pub buffers: Vec<TimeSeries>,
    pub acquiring: bool,
    pub start_time: Option<Instant>,
    pub num_points: usize,
    pub mode: TimeSeriesMode,
    /// Number of input samples averaged into one output time point
    /// (C++ `numAverage_`). `1` means each input sample is one output point.
    pub num_average: usize,
    /// Running per-channel sum of the input samples for the in-progress
    /// output point (C++ `averageStore_`).
    average_store: Vec<f64>,
    /// Number of input samples accumulated into `average_store` so far
    /// (C++ `numAveraged_`).
    num_averaged: usize,
}

impl SharedTsState {
    fn new(num_channels: usize, num_points: usize) -> Self {
        let buffers = (0..num_channels)
            .map(|_| TimeSeries::new(num_points, TimeSeriesMode::OneShot))
            .collect();
        Self {
            buffers,
            acquiring: false,
            start_time: None,
            num_points,
            mode: TimeSeriesMode::OneShot,
            num_average: 1,
            average_store: vec![0.0; num_channels],
            num_averaged: 0,
        }
    }

    /// Reset the running average accumulator (C++ resets `numAveraged_` and
    /// `averageStore_` on start/erase, resize, mode change).
    fn reset_average(&mut self) {
        for v in &mut self.average_store {
            *v = 0.0;
        }
        self.num_averaged = 0;
    }

    /// Accumulate one input sample vector and, once `num_average` samples
    /// have been collected, push the per-channel average into the buffers.
    ///
    /// Port of the inner loop of C++ `doAddToTimeSeriesT`: each call is one
    /// input time point; `num_average` of them produce one output point.
    /// Returns `true` when an output point was emitted this call.
    fn accumulate(&mut self, values: &[f64]) -> bool {
        let n = values.len().min(self.average_store.len());
        for i in 0..n {
            self.average_store[i] += values[i];
        }
        self.num_averaged += 1;
        if self.num_averaged < self.num_average.max(1) {
            return false;
        }
        let divisor = self.num_averaged as f64;
        let nb = n.min(self.buffers.len());
        for i in 0..nb {
            self.buffers[i].add_value(self.average_store[i] / divisor);
        }
        self.reset_average();
        true
    }
}

/// TS port driver: standalone asyn PortDriver for time series waveforms.
///
/// Generic over the number of channels — Stats uses 23, ROIStat uses
/// a different set, and NDTimeSeries standalone can use any count.
pub struct TimeSeriesPortDriver {
    base: PortDriverBase,
    params: TSParams,
    shared: Arc<Mutex<SharedTsState>>,
    num_channels: usize,
    time_per_point: f64,
}

impl TimeSeriesPortDriver {
    fn new(
        port_name: &str,
        channel_names: &[&str],
        num_points: usize,
        shared: Arc<Mutex<SharedTsState>>,
    ) -> Self {
        let num_channels = channel_names.len();
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags {
                multi_device: false,
                can_block: false,
                destructible: true,
            },
        );

        // NDPluginBase params (NDTimeSeries.template includes NDPluginBase.template)
        let _ = ad_core_rs::params::ndarray_driver::NDArrayDriverParams::create(&mut base);
        let _ = ad_core_rs::plugin::params::PluginBaseParams::create(&mut base);

        // Register control params
        let ts_acquire = base.create_param("TS_ACQUIRE", ParamType::Int32).unwrap();
        let _ = base.set_int32_param(ts_acquire, 0, 0);
        let ts_read = base.create_param("TS_READ", ParamType::Int32).unwrap();
        let ts_num_points = base
            .create_param("TS_NUM_POINTS", ParamType::Int32)
            .unwrap();
        let _ = base.set_int32_param(ts_num_points, 0, num_points as i32);
        let ts_current_point = base
            .create_param("TS_CURRENT_POINT", ParamType::Int32)
            .unwrap();
        let _ = base.set_int32_param(ts_current_point, 0, 0);
        let ts_time_per_point = base
            .create_param("TS_TIME_PER_POINT", ParamType::Float64)
            .unwrap();
        let ts_averaging_time = base
            .create_param("TS_AVERAGING_TIME", ParamType::Float64)
            .unwrap();
        let ts_num_average = base
            .create_param("TS_NUM_AVERAGE", ParamType::Int32)
            .unwrap();
        let _ = base.set_int32_param(ts_num_average, 0, 1);
        let ts_elapsed_time = base
            .create_param("TS_ELAPSED_TIME", ParamType::Float64)
            .unwrap();
        let ts_acquire_mode = base
            .create_param("TS_ACQUIRE_MODE", ParamType::Int32)
            .unwrap();
        let _ = base.set_int32_param(ts_acquire_mode, 0, 0);
        let ts_time_axis = base
            .create_param("TS_TIME_AXIS", ParamType::Float64Array)
            .unwrap();

        // Initialize time axis (scaled by time_per_point, default 1.0)
        let time_per_point = 1.0;
        let time_axis: Vec<f64> = (0..num_points).map(|i| i as f64 * time_per_point).collect();
        let _ = base.params.set_float64_array(ts_time_axis, 0, time_axis);

        // Channel waveform params — one Float64Array per channel
        let mut ts_channels = Vec::with_capacity(num_channels);
        for name in channel_names {
            let param_name = format!("TS_CHAN_{name}");
            let idx = base
                .create_param(&param_name, ParamType::Float64Array)
                .unwrap();
            let _ = base.params.set_float64_array(idx, 0, vec![0.0; num_points]);
            ts_channels.push(idx);
        }

        // Generic time series and timestamp waveform params
        let ts_time_series = base
            .create_param("TS_TIME_SERIES", ParamType::Float64Array)
            .unwrap();
        let ts_timestamp = base
            .create_param("TS_TIMESTAMP", ParamType::Float64Array)
            .unwrap();

        let params = TSParams {
            ts_acquire,
            ts_read,
            ts_num_points,
            ts_current_point,
            ts_time_per_point,
            ts_averaging_time,
            ts_num_average,
            ts_elapsed_time,
            ts_acquire_mode,
            ts_time_axis,
            ts_channels,
            channel_names: channel_names.iter().map(|s| s.to_string()).collect(),
            ts_time_series,
            ts_timestamp,
        };

        Self {
            base,
            params,
            shared,
            num_channels,
            time_per_point,
        }
    }

    /// Build the time axis for the current mode.
    ///
    /// Fixed (OneShot) mode uses an ascending axis `i * time_per_point`;
    /// Circular (RingBuffer) mode uses a signed axis ending at 0, so the
    /// most recent point is t=0 and older points are negative — C++
    /// `createAxisArray`: `timeAxis_[i] = -(numTimePoints-1-i)*timePerPoint`.
    fn build_time_axis(&self, num_points: usize, mode: TimeSeriesMode) -> Vec<f64> {
        (0..num_points)
            .map(|i| match mode {
                TimeSeriesMode::OneShot => i as f64 * self.time_per_point,
                TimeSeriesMode::RingBuffer => {
                    -((num_points.saturating_sub(1) - i) as f64) * self.time_per_point
                }
            })
            .collect()
    }

    /// Recompute and publish the time axis param for the current mode.
    fn refresh_time_axis(&mut self) {
        let (num_points, mode) = {
            let s = self.shared.lock();
            (s.num_points, s.mode)
        };
        let axis = self.build_time_axis(num_points, mode);
        let _ = self
            .base
            .params
            .set_float64_array(self.params.ts_time_axis, 0, axis);
    }

    /// Copy buffer data to Float64Array params and call callbacks.
    fn update_waveform_params(&mut self) {
        let state = self.shared.lock();
        let num_points = state.num_points;

        // Update per-channel waveform params
        for (i, buf) in state.buffers.iter().enumerate() {
            let mut values = buf.values();
            values.resize(num_points, 0.0);
            let _ = self
                .base
                .params
                .set_float64_array(self.params.ts_channels[i], 0, values);
        }

        // Update current point
        let current_point = state.buffers[0].count();
        let _ = self
            .base
            .set_int32_param(self.params.ts_current_point, 0, current_point as i32);

        // Update elapsed time
        if let Some(start) = state.start_time {
            let elapsed = start.elapsed().as_secs_f64();
            let _ = self
                .base
                .set_float64_param(self.params.ts_elapsed_time, 0, elapsed);
        }

        // Update acquire status (may have auto-stopped)
        let acquiring = state.acquiring;
        drop(state);

        let _ = self
            .base
            .set_int32_param(self.params.ts_acquire, 0, if acquiring { 1 } else { 0 });

        // Notify listeners
        let _ = self.base.call_param_callbacks(0);
    }
}

impl PortDriver for TimeSeriesPortDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> asyn_rs::error::AsynResult<()> {
        let reason = user.reason;

        if reason == self.params.ts_acquire {
            let mut state = self.shared.lock();
            if value != 0 {
                // Start acquiring
                if !state.acquiring {
                    // If buffers are empty, this is Erase/Start
                    if state.buffers[0].count() == 0 {
                        for buf in state.buffers.iter_mut() {
                            buf.reset();
                        }
                    }
                    // Start always resets the running average accumulator
                    // (C++ doTimeSeriesCallbacks / start path).
                    state.reset_average();
                    state.acquiring = true;
                    state.start_time = Some(Instant::now());
                }
            } else {
                // Stop
                state.acquiring = false;
            }
            drop(state);
            self.base.set_int32_param(reason, 0, value)?;
            self.base.call_param_callbacks(0)?;
        } else if reason == self.params.ts_read {
            // Trigger waveform update
            self.update_waveform_params();
        } else if reason == self.params.ts_num_points {
            let new_size = value.max(1) as usize;
            {
                let mut state = self.shared.lock();
                state.num_points = new_size;
                for buf in state.buffers.iter_mut() {
                    buf.resize(new_size);
                }
                state.reset_average();
                state.acquiring = false;
            }

            // Rebuild the time axis for the current mode.
            self.refresh_time_axis();

            // Re-initialize channel waveforms
            for i in 0..self.num_channels {
                let _ = self.base.params.set_float64_array(
                    self.params.ts_channels[i],
                    0,
                    vec![0.0; new_size],
                );
            }

            self.base.set_int32_param(reason, 0, value)?;
            self.base
                .set_int32_param(self.params.ts_current_point, 0, 0)?;
            self.base.set_int32_param(self.params.ts_acquire, 0, 0)?;
            self.base.call_param_callbacks(0)?;
        } else if reason == self.params.ts_num_average {
            // numAverage: input samples averaged per output time point
            // (C++ P_TSNumAverage). Resets the running accumulator.
            let n = value.max(1) as usize;
            {
                let mut state = self.shared.lock();
                state.num_average = n;
                state.reset_average();
            }
            self.base.set_int32_param(reason, 0, n as i32)?;
            self.base.call_param_callbacks(0)?;
        } else if reason == self.params.ts_acquire_mode {
            // 0 == TSAcquireModeFixed (OneShot), 1 == TSAcquireModeCircular.
            let mode = if value == 0 {
                TimeSeriesMode::OneShot
            } else {
                TimeSeriesMode::RingBuffer
            };
            {
                let mut state = self.shared.lock();
                state.mode = mode;
                for buf in state.buffers.iter_mut() {
                    buf.set_mode(mode);
                }
                state.reset_average();
                state.acquiring = false;
            }
            // Circular mode flips the time axis to a signed (ending-at-0) one.
            self.refresh_time_axis();

            self.base.set_int32_param(reason, 0, value)?;
            self.base.set_int32_param(self.params.ts_acquire, 0, 0)?;
            self.base.call_param_callbacks(0)?;
        } else {
            // Default: store in param cache
            self.base.set_int32_param(reason, user.addr, value)?;
            self.base.call_param_callbacks(user.addr)?;
        }

        Ok(())
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> asyn_rs::error::AsynResult<()> {
        let reason = user.reason;
        if reason == self.params.ts_time_per_point {
            self.time_per_point = value;
            self.base.set_float64_param(reason, user.addr, value)?;
            // Rebuild the time axis with the new scaling for the current mode.
            self.refresh_time_axis();
            self.base.call_param_callbacks(user.addr)?;
        } else {
            self.base.set_float64_param(reason, user.addr, value)?;
            self.base.call_param_callbacks(user.addr)?;
        }
        Ok(())
    }

    fn read_float64_array(
        &mut self,
        user: &AsynUser,
        buf: &mut [f64],
    ) -> asyn_rs::error::AsynResult<usize> {
        let data = self.base.params.get_float64_array(user.reason, user.addr)?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
}

/// Background thread that receives data from a plugin and accumulates into
/// shared buffers.
///
/// Each received `TimeSeriesData` is one input time point. `num_average`
/// consecutive input points are averaged into one output time point
/// (C++ `doAddToTimeSeriesT`). In `OneShot`/Fixed mode acquisition auto-stops
/// once `num_points` output points exist; in `RingBuffer`/Circular mode the
/// `TimeSeries` ring buffer wraps and acquisition continues.
fn ts_data_thread(shared: Arc<Mutex<SharedTsState>>, mut data_rx: TimeSeriesReceiver) {
    while let Some(data) = data_rx.blocking_recv() {
        let mut state = shared.lock();
        if !state.acquiring {
            continue;
        }
        let emitted = state.accumulate(&data.values);
        // Auto-stop for Fixed (OneShot) mode once num_points output points
        // have been collected. Only check when an output point was emitted.
        if emitted
            && state.mode == TimeSeriesMode::OneShot
            && state.buffers[0].count() >= state.num_points
        {
            state.acquiring = false;
        }
    }
}

/// Create a TS port runtime.
///
/// `channel_names` defines the number and names of time series channels.
/// Returns the port runtime handle, the TS params (for building a registry),
/// and thread join handles for the actor and data ingestion threads.
pub fn create_ts_port_runtime(
    port_name: &str,
    channel_names: &[&str],
    num_points: usize,
    data_rx: TimeSeriesReceiver,
) -> (
    PortRuntimeHandle,
    TSParams,
    std::thread::JoinHandle<()>,
    std::thread::JoinHandle<()>,
) {
    let num_channels = channel_names.len();
    let shared = Arc::new(Mutex::new(SharedTsState::new(num_channels, num_points)));

    let driver = TimeSeriesPortDriver::new(port_name, channel_names, num_points, shared.clone());

    // Capture params before the driver is moved into the actor
    let ts_params = TSParams {
        ts_acquire: driver.params.ts_acquire,
        ts_read: driver.params.ts_read,
        ts_num_points: driver.params.ts_num_points,
        ts_current_point: driver.params.ts_current_point,
        ts_time_per_point: driver.params.ts_time_per_point,
        ts_averaging_time: driver.params.ts_averaging_time,
        ts_num_average: driver.params.ts_num_average,
        ts_elapsed_time: driver.params.ts_elapsed_time,
        ts_acquire_mode: driver.params.ts_acquire_mode,
        ts_time_axis: driver.params.ts_time_axis,
        ts_channels: driver.params.ts_channels.clone(),
        channel_names: driver.params.channel_names.clone(),
        ts_time_series: driver.params.ts_time_series,
        ts_timestamp: driver.params.ts_timestamp,
    };

    // Constructor-shaped like the plugin runtimes in `ad-core-rs`: this
    // function returns the built port and has no error channel to its
    // `*Configure` caller, so the only alternative is a handle to a port that
    // does not exist. C prints and throws here (asynPortDriver.cpp:4036-4040)
    // and iocsh catches it (iocsh.cpp:1274-1284), leaving the C IOC serving
    // without the port; we deviate on purpose — see `port_runtime_unavailable`.
    let (runtime_handle, actor_jh) = create_port_runtime(driver, RuntimeConfig::default())
        .unwrap_or_else(|e| port_runtime_unavailable(port_name, &e));

    // Spawn data ingestion thread. `NDPluginTimeSeries` derives from
    // `NDPluginDriver`, so in C this loop runs on the plugin callback threads
    // built at `NDPluginDriver.cpp:1016` — band and stack from
    // `asynNDArrayDriver.cpp:876-879`, and a creation failure throws
    // `epicsThread::unableToCreateThread` (`epicsThread.cpp:214-220`) rather
    // than returning a status the plugin carries on from.
    let data_jh = MandatoryThread::new(
        format!("ts-data-{port_name}"),
        ThreadPriority::Medium,
        StackSizeClass::Medium,
    )
    .spawn(move || {
        ts_data_thread(shared, data_rx);
    });

    (runtime_handle, ts_params, actor_jh, data_jh)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// # Invariant
    ///
    /// MUST: the `ts-data-*` thread be created through [`MandatoryThread`].
    ///
    /// `NDPluginTimeSeries` derives from `NDPluginDriver`, so in C this loop
    /// runs on the callback threads built at `NDPluginDriver.cpp:1016`, and a
    /// creation failure there throws `epicsThread::unableToCreateThread`
    /// (`epicsThread.cpp:214-220`) rather than returning a status. Where C ends
    /// up is not where we do, deliberately: iocsh catches what a command throws
    /// (`iocsh.cpp:1274-1284`) and st.cmd continues by default
    /// (`iocsh.cpp:1001`, `:1129`), so the C IOC keeps the plugin's port
    /// registered with nothing ingesting behind it. The `.expect` this replaced
    /// produced that same zombie by unwinding one thread on a
    /// `panic = "unwind"` target.
    #[test]
    fn the_ts_data_thread_is_mandatory() {
        let src = include_str!("time_series.rs");
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        assert_eq!(prod.matches("MandatoryThread::new(").count(), 1);
        let strays: Vec<&str> = prod
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .filter(|l| {
                l.contains(concat!("thread", "::Builder::new()"))
                    || l.contains(concat!("thread", "::spawn("))
            })
            .collect();
        assert!(
            strays.is_empty(),
            "a data thread created outside `MandatoryThread` resolves its own \
             spawn failure locally: {strays:?}"
        );
    }

    #[test]
    fn test_one_shot() {
        let mut ts = TimeSeries::new(5, TimeSeriesMode::OneShot);
        for i in 0..5 {
            ts.add_value(i as f64);
        }
        assert_eq!(ts.count(), 5);
        assert_eq!(ts.values(), vec![0.0, 1.0, 2.0, 3.0, 4.0]);

        // Adding beyond capacity is a no-op
        ts.add_value(99.0);
        assert_eq!(ts.count(), 5);
    }

    #[test]
    fn test_ring_buffer() {
        let mut ts = TimeSeries::new(4, TimeSeriesMode::RingBuffer);
        for i in 0..6 {
            ts.add_value(i as f64);
        }
        assert_eq!(ts.count(), 4);
        // Should contain [2, 3, 4, 5] in order
        assert_eq!(ts.values(), vec![2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn test_ring_buffer_partial() {
        let mut ts = TimeSeries::new(4, TimeSeriesMode::RingBuffer);
        ts.add_value(10.0);
        ts.add_value(20.0);
        assert_eq!(ts.count(), 2);
        assert_eq!(ts.values(), vec![10.0, 20.0]);
    }

    #[test]
    fn test_reset() {
        let mut ts = TimeSeries::new(3, TimeSeriesMode::OneShot);
        ts.add_value(1.0);
        ts.add_value(2.0);
        ts.reset();
        assert_eq!(ts.count(), 0);
        assert!(ts.values().is_empty());
    }

    #[test]
    fn test_resize() {
        let mut ts = TimeSeries::new(5, TimeSeriesMode::OneShot);
        ts.add_value(1.0);
        ts.add_value(2.0);
        ts.resize(3);
        assert_eq!(ts.num_points, 3);
        assert_eq!(ts.count(), 0);
        assert!(ts.values().is_empty());
    }

    #[test]
    fn test_set_mode() {
        let mut ts = TimeSeries::new(5, TimeSeriesMode::OneShot);
        ts.add_value(1.0);
        ts.set_mode(TimeSeriesMode::RingBuffer);
        assert_eq!(ts.mode, TimeSeriesMode::RingBuffer);
        assert_eq!(ts.count(), 0);
    }

    // --- TS port driver tests (using a small channel set for simplicity) ---

    const TEST_CHANNELS: [&str; 3] = ["ChA", "ChB", "ChC"];

    #[test]
    fn test_shared_ts_state_init() {
        let state = SharedTsState::new(3, 100);
        assert_eq!(state.buffers.len(), 3);
        assert_eq!(state.num_points, 100);
        assert!(!state.acquiring);
        assert_eq!(state.mode, TimeSeriesMode::OneShot);
    }

    #[test]
    fn test_ts_port_driver_create() {
        let shared = Arc::new(Mutex::new(SharedTsState::new(3, 100)));
        let driver = TimeSeriesPortDriver::new("TEST_TS", &TEST_CHANNELS, 100, shared);
        assert_eq!(driver.base().port_name, "TEST_TS");
        assert_eq!(driver.num_channels, 3);
        assert!(!driver.base().flags.multi_device);
    }

    #[test]
    fn test_ts_port_driver_write_acquire() {
        let shared = Arc::new(Mutex::new(SharedTsState::new(3, 100)));
        let mut driver = TimeSeriesPortDriver::new("TEST_TS", &TEST_CHANNELS, 100, shared.clone());

        // Start acquiring
        let mut user = AsynUser::new(driver.params.ts_acquire);
        driver.write_int32(&mut user, 1).unwrap();
        assert!(shared.lock().acquiring);

        // Stop acquiring
        driver.write_int32(&mut user, 0).unwrap();
        assert!(!shared.lock().acquiring);
    }

    #[test]
    fn test_ts_port_driver_write_num_points() {
        let shared = Arc::new(Mutex::new(SharedTsState::new(3, 100)));
        let mut driver = TimeSeriesPortDriver::new("TEST_TS", &TEST_CHANNELS, 100, shared.clone());

        let mut user = AsynUser::new(driver.params.ts_num_points);
        driver.write_int32(&mut user, 50).unwrap();

        let state = shared.lock();
        assert_eq!(state.num_points, 50);
        for buf in &state.buffers {
            assert_eq!(buf.num_points, 50);
        }
    }

    #[test]
    fn test_ts_port_driver_write_mode() {
        let shared = Arc::new(Mutex::new(SharedTsState::new(3, 100)));
        let mut driver = TimeSeriesPortDriver::new("TEST_TS", &TEST_CHANNELS, 100, shared.clone());

        let mut user = AsynUser::new(driver.params.ts_acquire_mode);
        driver.write_int32(&mut user, 1).unwrap();

        let state = shared.lock();
        assert_eq!(state.mode, TimeSeriesMode::RingBuffer);
        for buf in &state.buffers {
            assert_eq!(buf.mode, TimeSeriesMode::RingBuffer);
        }
    }

    #[test]
    fn test_ts_port_driver_update_waveforms() {
        let shared = Arc::new(Mutex::new(SharedTsState::new(3, 10)));
        let mut driver = TimeSeriesPortDriver::new("TEST_TS", &TEST_CHANNELS, 10, shared.clone());

        // Add some data
        {
            let mut state = shared.lock();
            state.acquiring = true;
            state.start_time = Some(Instant::now());
            for buf in state.buffers.iter_mut() {
                buf.add_value(42.0);
                buf.add_value(43.0);
            }
        }

        // Trigger update
        driver.update_waveform_params();

        // Check current point was updated
        let cp = driver
            .base
            .get_int32_param(driver.params.ts_current_point, 0)
            .unwrap();
        assert_eq!(cp, 2);

        // Check waveform data was written
        let data = driver
            .base
            .params
            .get_float64_array(driver.params.ts_channels[0], 0)
            .unwrap();
        assert_eq!(data[0], 42.0);
        assert_eq!(data[1], 43.0);
    }

    #[test]
    fn test_ts_port_driver_read_array() {
        let shared = Arc::new(Mutex::new(SharedTsState::new(3, 5)));
        let mut driver = TimeSeriesPortDriver::new("TEST_TS", &TEST_CHANNELS, 5, shared);

        let user = AsynUser::new(driver.params.ts_time_axis);
        let mut buf = vec![0.0; 5];
        let n = driver.read_float64_array(&user, &mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(buf, vec![0.0, 1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn test_ts_data_ingestion_oneshot() {
        let shared = Arc::new(Mutex::new(SharedTsState::new(3, 3)));
        let (tx, rx) = tokio::sync::mpsc::channel(16);

        // Start acquiring
        shared.lock().acquiring = true;

        let shared_clone = shared.clone();
        let jh = std::thread::spawn(move || ts_data_thread(shared_clone, rx));

        // Send data
        tx.blocking_send(TimeSeriesData {
            values: vec![1.0, 10.0, 100.0],
        })
        .unwrap();
        tx.blocking_send(TimeSeriesData {
            values: vec![2.0, 20.0, 200.0],
        })
        .unwrap();
        tx.blocking_send(TimeSeriesData {
            values: vec![3.0, 30.0, 300.0],
        })
        .unwrap();
        tx.blocking_send(TimeSeriesData {
            values: vec![4.0, 40.0, 400.0],
        })
        .unwrap(); // beyond capacity

        // Close channel and wait for thread
        drop(tx);
        jh.join().unwrap();

        let state = shared.lock();
        assert_eq!(state.buffers[0].count(), 3);
        assert_eq!(state.buffers[0].values(), vec![1.0, 2.0, 3.0]);
        assert_eq!(state.buffers[1].values(), vec![10.0, 20.0, 30.0]);
        assert_eq!(state.buffers[2].values(), vec![100.0, 200.0, 300.0]);
        assert!(!state.acquiring); // auto-stopped
    }

    #[test]
    fn test_ts_data_ingestion_not_acquiring() {
        let shared = Arc::new(Mutex::new(SharedTsState::new(3, 10)));
        let (tx, rx) = tokio::sync::mpsc::channel(16);

        // Not acquiring (default)
        let shared_clone = shared.clone();
        let jh = std::thread::spawn(move || ts_data_thread(shared_clone, rx));

        tx.blocking_send(TimeSeriesData {
            values: vec![1.0, 2.0, 3.0],
        })
        .unwrap();

        drop(tx);
        jh.join().unwrap();

        let state = shared.lock();
        assert_eq!(state.buffers[0].count(), 0);
    }

    #[test]
    fn test_num_average_averages_input_samples() {
        // numAverage = 3: every 3 input samples produce one averaged output
        // point. Channel A inputs 0,1,2 -> mean 1; 3,4,5 -> mean 4.
        let mut state = SharedTsState::new(1, 10);
        state.num_average = 3;
        assert!(!state.accumulate(&[0.0]));
        assert!(!state.accumulate(&[1.0]));
        assert!(state.accumulate(&[2.0])); // emits mean of 0,1,2 = 1
        assert!(!state.accumulate(&[3.0]));
        assert!(!state.accumulate(&[4.0]));
        assert!(state.accumulate(&[5.0])); // emits mean of 3,4,5 = 4
        let vals = state.buffers[0].values();
        assert_eq!(vals.len(), 2);
        assert!((vals[0] - 1.0).abs() < 1e-10);
        assert!((vals[1] - 4.0).abs() < 1e-10);
    }

    #[test]
    fn test_num_average_one_is_passthrough() {
        // numAverage = 1: each input sample is one output point unchanged.
        let mut state = SharedTsState::new(2, 10);
        state.num_average = 1;
        assert!(state.accumulate(&[5.0, 50.0]));
        assert!(state.accumulate(&[6.0, 60.0]));
        assert_eq!(state.buffers[0].values(), vec![5.0, 6.0]);
        assert_eq!(state.buffers[1].values(), vec![50.0, 60.0]);
    }

    #[test]
    fn test_num_average_drives_ingestion_thread() {
        // The data thread must average numAverage=2 samples per output point.
        let shared = Arc::new(Mutex::new(SharedTsState::new(1, 5)));
        {
            let mut s = shared.lock();
            s.num_average = 2;
            s.acquiring = true;
        }
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let shared_clone = shared.clone();
        let jh = std::thread::spawn(move || ts_data_thread(shared_clone, rx));

        for v in [10.0, 20.0, 30.0, 40.0] {
            tx.blocking_send(TimeSeriesData { values: vec![v] })
                .unwrap();
        }
        drop(tx);
        jh.join().unwrap();

        // 4 input samples / numAverage 2 -> 2 output points: 15, 35.
        let state = shared.lock();
        let vals = state.buffers[0].values();
        assert_eq!(vals.len(), 2);
        assert!((vals[0] - 15.0).abs() < 1e-10);
        assert!((vals[1] - 35.0).abs() < 1e-10);
    }

    #[test]
    fn test_fixed_mode_stops_at_num_points() {
        // Fixed (OneShot) mode: acquisition auto-stops once num_points output
        // points are collected, even with more input pending.
        let shared = Arc::new(Mutex::new(SharedTsState::new(1, 3)));
        {
            let mut s = shared.lock();
            s.num_average = 1;
            s.mode = TimeSeriesMode::OneShot;
            s.acquiring = true;
        }
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let shared_clone = shared.clone();
        let jh = std::thread::spawn(move || ts_data_thread(shared_clone, rx));
        for v in [1.0, 2.0, 3.0, 4.0, 5.0] {
            tx.blocking_send(TimeSeriesData { values: vec![v] })
                .unwrap();
        }
        drop(tx);
        jh.join().unwrap();

        let state = shared.lock();
        assert!(!state.acquiring, "Fixed mode must auto-stop");
        assert_eq!(state.buffers[0].count(), 3);
        assert_eq!(state.buffers[0].values(), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_circular_mode_wraps_and_keeps_acquiring() {
        // Circular (RingBuffer) mode: the buffer wraps; acquisition does not
        // auto-stop.
        let shared = Arc::new(Mutex::new(SharedTsState::new(1, 3)));
        {
            let mut s = shared.lock();
            s.num_average = 1;
            s.mode = TimeSeriesMode::RingBuffer;
            for buf in s.buffers.iter_mut() {
                buf.set_mode(TimeSeriesMode::RingBuffer);
            }
            s.acquiring = true;
        }
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let shared_clone = shared.clone();
        let jh = std::thread::spawn(move || ts_data_thread(shared_clone, rx));
        for v in [1.0, 2.0, 3.0, 4.0, 5.0] {
            tx.blocking_send(TimeSeriesData { values: vec![v] })
                .unwrap();
        }
        drop(tx);
        jh.join().unwrap();

        let state = shared.lock();
        assert!(state.acquiring, "Circular mode must keep acquiring");
        // Ring buffer of 3 keeps the last 3 points: 3,4,5.
        assert_eq!(state.buffers[0].values(), vec![3.0, 4.0, 5.0]);
    }

    #[test]
    fn test_acquire_mode_param_drives_behavior_and_axis() {
        // Writing TS_ACQUIRE_MODE must switch the buffer mode AND flip the
        // time axis from ascending (Fixed) to signed-ending-at-0 (Circular).
        let shared = Arc::new(Mutex::new(SharedTsState::new(1, 4)));
        let mut driver = TimeSeriesPortDriver::new("TEST_TS_MODE", &["Ch0"], 4, shared.clone());

        // Fixed mode axis: 0, 1, 2, 3.
        let axis = driver
            .base
            .params
            .get_float64_array(driver.params.ts_time_axis, 0)
            .unwrap();
        assert_eq!(&*axis, &[0.0, 1.0, 2.0, 3.0]);

        // Switch to Circular mode.
        let mut user = AsynUser::new(driver.params.ts_acquire_mode);
        driver.write_int32(&mut user, 1).unwrap();
        assert_eq!(shared.lock().mode, TimeSeriesMode::RingBuffer);

        // Circular axis: -3, -2, -1, 0 (most recent point is t=0).
        let axis = driver
            .base
            .params
            .get_float64_array(driver.params.ts_time_axis, 0)
            .unwrap();
        assert_eq!(&*axis, &[-3.0, -2.0, -1.0, 0.0]);
    }

    #[test]
    fn test_num_average_param_drives_state() {
        // Writing TS_NUM_AVERAGE must update SharedTsState::num_average.
        let shared = Arc::new(Mutex::new(SharedTsState::new(1, 10)));
        let mut driver = TimeSeriesPortDriver::new("TEST_TS_NAVG", &["Ch0"], 10, shared.clone());
        let mut user = AsynUser::new(driver.params.ts_num_average);
        driver.write_int32(&mut user, 5).unwrap();
        assert_eq!(shared.lock().num_average, 5);
        // A value of 0 is clamped to 1.
        driver.write_int32(&mut user, 0).unwrap();
        assert_eq!(shared.lock().num_average, 1);
    }

    #[test]
    fn test_create_ts_port_runtime() {
        let (_tx, rx) = tokio::sync::mpsc::channel(16);
        let (handle, params, _actor_jh, _data_jh) =
            create_ts_port_runtime("TEST_TS_RT", &TEST_CHANNELS, 100, rx);
        assert_eq!(handle.port_name(), "TEST_TS_RT");
        assert_eq!(params.ts_channels.len(), 3);
        handle.shutdown();
    }
}
