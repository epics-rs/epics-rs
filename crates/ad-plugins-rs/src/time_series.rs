use std::sync::Arc;
use std::time::Instant;

use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{PortRuntimeHandle, create_port_runtime};
use asyn_rs::user::AsynUser;
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

/// Accumulation mode for time series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSeriesMode {
    OneShot,
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
}

/// Shared state between the data ingestion thread and the TS port driver.
pub struct SharedTsState {
    pub buffers: Vec<TimeSeries>,
    pub acquiring: bool,
    pub start_time: Option<Instant>,
    pub num_points: usize,
    pub mode: TimeSeriesMode,
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
        }
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

        // Initialize time axis
        let time_axis: Vec<f64> = (0..num_points).map(|i| i as f64).collect();
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
        };

        Self {
            base,
            params,
            shared,
            num_channels,
        }
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
            let mut state = self.shared.lock();
            state.num_points = new_size;
            for buf in state.buffers.iter_mut() {
                buf.resize(new_size);
            }
            state.acquiring = false;
            drop(state);

            // Update time axis
            let time_axis: Vec<f64> = (0..new_size).map(|i| i as f64).collect();
            let _ = self
                .base
                .params
                .set_float64_array(self.params.ts_time_axis, 0, time_axis);

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
        } else if reason == self.params.ts_acquire_mode {
            let mode = if value == 0 {
                TimeSeriesMode::OneShot
            } else {
                TimeSeriesMode::RingBuffer
            };
            let mut state = self.shared.lock();
            state.mode = mode;
            for buf in state.buffers.iter_mut() {
                buf.set_mode(mode);
            }
            state.acquiring = false;
            drop(state);

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

/// Background thread that receives data from a plugin and accumulates into shared buffers.
fn ts_data_thread(shared: Arc<Mutex<SharedTsState>>, mut data_rx: TimeSeriesReceiver) {
    while let Some(data) = data_rx.blocking_recv() {
        let mut state = shared.lock();
        if !state.acquiring {
            continue;
        }
        let n = data.values.len().min(state.buffers.len());
        for i in 0..n {
            state.buffers[i].add_value(data.values[i]);
        }
        // Auto-stop for OneShot mode
        if state.mode == TimeSeriesMode::OneShot && state.buffers[0].count() >= state.num_points {
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
    };

    let (runtime_handle, actor_jh) = create_port_runtime(driver, RuntimeConfig::default());

    // Spawn data ingestion thread
    let data_jh = std::thread::Builder::new()
        .name(format!("ts-data-{port_name}"))
        .spawn(move || {
            ts_data_thread(shared, data_rx);
        })
        .expect("failed to spawn TS data thread");

    (runtime_handle, ts_params, actor_jh, data_jh)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_create_ts_port_runtime() {
        let (_tx, rx) = tokio::sync::mpsc::channel(16);
        let (handle, params, _actor_jh, _data_jh) =
            create_ts_port_runtime("TEST_TS_RT", &TEST_CHANNELS, 100, rx);
        assert_eq!(handle.port_name(), "TEST_TS_RT");
        assert_eq!(params.ts_channels.len(), 3);
        handle.shutdown();
    }
}
