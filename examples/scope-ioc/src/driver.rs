//! Digital oscilloscope simulator — port of EPICS testAsynPortDriver.
//!
//! Demonstrates the asynPortDriver pattern:
//!   1. Define parameters (scalars, enums, arrays)
//!   2. Override write_int32/write_float64 for control logic
//!   3. Run a background task that computes waveforms and pushes updates

use std::sync::Arc;

use asyn_rs::runtime::sync::Notify;
use parking_lot::Mutex;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::param::{EnumEntry, ParamType, ParamValue};
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::request::ParamSetValue;
use asyn_rs::trace::TraceMask;
use asyn_rs::user::AsynUser;
use epics_libcom_rs::cfmt::CFloat;

// --- Constants (matching original testAsynPortDriver) ---

pub const FREQUENCY: f64 = 1000.0;
pub const AMPLITUDE: f64 = 1.0;
pub const NUM_DIVISIONS: f64 = 10.0;
pub const MIN_UPDATE_TIME: f64 = 0.02;
pub const DEFAULT_MAX_POINTS: i32 = 1000;
pub const DEFAULT_UPDATE_TIME: f64 = 0.5;

/// C `allVoltsPerDivSelections`, in volts at unit gain.
pub const ALL_VOLTS_PER_DIV_SELECTIONS: [i32; 4] = [1, 2, 5, 10];

// --- Simple xorshift64 RNG (no rand dependency) ---

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 as f64) / (u64::MAX as f64)
    }
}

// --- ScopeSimulator driver ---

pub struct ScopeSimulator {
    pub base: PortDriverBase,
    pub notify: Arc<Notify>,
    pub p_run: usize,
    pub p_max_points: usize,
    pub p_time_per_div: usize,
    pub p_time_per_div_select: usize,
    pub p_vert_gain: usize,
    pub p_vert_gain_select: usize,
    pub p_volts_per_div: usize,
    pub p_volts_per_div_select: usize,
    pub p_volt_offset: usize,
    pub p_trigger_delay: usize,
    pub p_noise_amplitude: usize,
    pub p_update_time: usize,
    pub p_waveform: usize,
    pub p_time_base: usize,
    pub p_min_value: usize,
    pub p_max_value: usize,
    pub p_mean_value: usize,
    /// Fixed at construction, as C `pTimeBase_`.
    time_base: Vec<f64>,
    /// C `voltsPerDivStrings_/Values_`: the table `readEnum` serves for
    /// `VoltsPerDivSelect`, values in mV.
    volts_per_div_choices: Arc<[EnumEntry]>,
}

impl ScopeSimulator {
    pub fn new(port_name: &str, notify: Arc<Notify>) -> Self {
        // testAsynPortDriver registers with ASYN_DESTRUCTIBLE alone: it never
        // blocks (testAsynPortDriver.cpp:54).
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags::default(),
            // testAsynPortDriver.cpp:52-53.
            InterfaceMask::INT32
                | InterfaceMask::FLOAT64
                | InterfaceMask::FLOAT64_ARRAY
                | InterfaceMask::ENUM
                | InterfaceMask::DRV_USER,
            InterfaceMask::INT32
                | InterfaceMask::FLOAT64
                | InterfaceMask::FLOAT64_ARRAY
                | InterfaceMask::ENUM,
        );

        let p_run = base.create_param("SCOPE_RUN", ParamType::Int32).unwrap();
        let p_max_points = base
            .create_param("SCOPE_MAX_POINTS", ParamType::Int32)
            .unwrap();
        let p_time_per_div = base
            .create_param("SCOPE_TIME_PER_DIV", ParamType::Float64)
            .unwrap();
        let p_time_per_div_select = base
            .create_param("SCOPE_TIME_PER_DIV_SELECT", ParamType::Int32)
            .unwrap();
        let p_vert_gain = base
            .create_param("SCOPE_VERT_GAIN", ParamType::Float64)
            .unwrap();
        let p_vert_gain_select = base
            .create_param("SCOPE_VERT_GAIN_SELECT", ParamType::Int32)
            .unwrap();
        let p_volts_per_div = base
            .create_param("SCOPE_VOLTS_PER_DIV", ParamType::Float64)
            .unwrap();
        let p_volts_per_div_select = base
            .create_param("SCOPE_VOLTS_PER_DIV_SELECT", ParamType::Int32)
            .unwrap();
        let p_volt_offset = base
            .create_param("SCOPE_VOLT_OFFSET", ParamType::Float64)
            .unwrap();
        let p_trigger_delay = base
            .create_param("SCOPE_TRIGGER_DELAY", ParamType::Float64)
            .unwrap();
        let p_noise_amplitude = base
            .create_param("SCOPE_NOISE_AMPLITUDE", ParamType::Float64)
            .unwrap();
        let p_update_time = base
            .create_param("SCOPE_UPDATE_TIME", ParamType::Float64)
            .unwrap();
        let p_waveform = base
            .create_param("SCOPE_WAVEFORM", ParamType::Float64Array)
            .unwrap();
        let p_time_base = base
            .create_param("SCOPE_TIME_BASE", ParamType::Float64Array)
            .unwrap();
        let p_min_value = base
            .create_param("SCOPE_MIN_VALUE", ParamType::Float64)
            .unwrap();
        let p_max_value = base
            .create_param("SCOPE_MAX_VALUE", ParamType::Float64)
            .unwrap();
        let p_mean_value = base
            .create_param("SCOPE_MEAN_VALUE", ParamType::Float64)
            .unwrap();

        base.set_int32_param(p_run, 0, 0).unwrap();
        base.set_int32_param(p_max_points, 0, DEFAULT_MAX_POINTS)
            .unwrap();
        base.set_float64_param(p_update_time, 0, DEFAULT_UPDATE_TIME)
            .unwrap();
        base.set_float64_param(p_volt_offset, 0, 0.0).unwrap();
        base.set_float64_param(p_trigger_delay, 0, 0.0).unwrap();
        base.set_float64_param(p_noise_amplitude, 0, 0.1).unwrap();
        base.set_float64_param(p_volts_per_div, 0, 1.0).unwrap();
        base.set_float64_param(p_time_per_div, 0, 0.001).unwrap();

        // A driver array, not a param, so no callback ever posts it
        // (testAsynPortDriver.cpp:71, served by readFloat64Array :360).
        let time_base = (0..DEFAULT_MAX_POINTS)
            .map(|i| i as f64 / (DEFAULT_MAX_POINTS - 1) as f64 * NUM_DIVISIONS)
            .collect();

        base.set_float64_param(p_min_value, 0, 0.0).unwrap();
        base.set_float64_param(p_max_value, 0, 0.0).unwrap();
        base.set_float64_param(p_mean_value, 0, 0.0).unwrap();
        // C's calloc'd pData_, served before the first run (:63, :356).
        base.params
            .set_float64_array(p_waveform, 0, vec![0.0; DEFAULT_MAX_POINTS as usize])
            .unwrap();

        let mut driver = Self {
            base,
            notify,
            p_run,
            p_max_points,
            p_time_per_div,
            p_time_per_div_select,
            p_vert_gain,
            p_vert_gain_select,
            p_volts_per_div,
            p_volts_per_div_select,
            p_volt_offset,
            p_trigger_delay,
            p_noise_amplitude,
            p_update_time,
            p_waveform,
            p_time_base,
            p_min_value,
            p_max_value,
            p_mean_value,
            time_base,
            volts_per_div_choices: Arc::from([]),
        };
        driver
            .base
            .set_int32_param(driver.p_vert_gain_select, 0, 10)
            .unwrap();
        driver.set_vert_gain().unwrap();
        driver
    }

    /// C `setVertGain`: the gain is the select's raw value, and the
    /// volts-per-division table is rebuilt for it.
    fn set_vert_gain(&mut self) -> AsynResult<()> {
        let gain = self.base.get_int32_param(self.p_vert_gain_select, 0)? as f64;
        self.base.set_float64_param(self.p_vert_gain, 0, gain)?;
        self.volts_per_div_choices = ALL_VOLTS_PER_DIV_SELECTIONS
            .iter()
            .map(|&v| EnumEntry {
                string: format!("{:.2}", CFloat(v as f64 / gain)),
                value: (v as f64 / gain * 1000.0 + 0.5) as i32,
                severity: 0,
            })
            .collect();
        self.base.do_callbacks_enum(
            self.p_volts_per_div_select,
            0,
            self.volts_per_div_choices.clone(),
        )
    }

    pub fn param_indices(&self) -> ParamIndices {
        ParamIndices {
            p_run: self.p_run,
            p_max_points: self.p_max_points,
            p_time_per_div: self.p_time_per_div,
            p_volts_per_div: self.p_volts_per_div,
            p_volt_offset: self.p_volt_offset,
            p_trigger_delay: self.p_trigger_delay,
            p_noise_amplitude: self.p_noise_amplitude,
            p_update_time: self.p_update_time,
            p_waveform: self.p_waveform,
            p_time_base: self.p_time_base,
            p_min_value: self.p_min_value,
            p_max_value: self.p_max_value,
            p_mean_value: self.p_mean_value,
        }
    }
}

impl PortDriver for ScopeSimulator {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        self.base.check_ready()?;

        self.base.set_int32_param(user.reason, user.addr, value)?;

        if user.reason == self.p_run {
            if value != 0 {
                self.notify.notify_one();
            }
        } else if user.reason == self.p_vert_gain_select {
            self.set_vert_gain()?;
        } else if user.reason == self.p_volts_per_div_select {
            // C setVoltsPerDiv: the select's value is mV.
            let mv = self.base.get_int32_param(self.p_volts_per_div_select, 0)?;
            self.base
                .set_float64_param(self.p_volts_per_div, 0, mv as f64 / 1000.0)?;
        } else if user.reason == self.p_time_per_div_select {
            // C setTimePerDiv: the select's value is microseconds.
            let us = self.base.get_int32_param(self.p_time_per_div_select, 0)?;
            self.base
                .set_float64_param(self.p_time_per_div, 0, us as f64 / 1e6)?;
        }
        self.base.call_param_callbacks(user.addr)?;
        Ok(())
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        self.base.check_ready()?;
        self.base.set_float64_param(user.reason, user.addr, value)?;
        if user.reason == self.p_update_time {
            if value < MIN_UPDATE_TIME {
                self.base.trace_print_at(
                    user.addr,
                    user.reason as i32,
                    TraceMask::WARNING,
                    file!(),
                    line!(),
                    format_args!(
                        "testAsynPortDriver:writeFloat64: warning, update time too small, \
                         changed from {:.6} to {:.6}\n",
                        CFloat(value),
                        CFloat(MIN_UPDATE_TIME)
                    ),
                );
                self.base
                    .set_float64_param(self.p_update_time, 0, MIN_UPDATE_TIME)?;
            }
            if self.base.get_int32_param(self.p_run, 0)? != 0 {
                self.notify.notify_one();
            }
        }
        self.base.call_param_callbacks(user.addr)?;
        Ok(())
    }

    /// C `readEnum`: only `VoltsPerDivSelect` has a driver table; the other
    /// selects keep their db strings.
    fn read_enum(&mut self, user: &AsynUser) -> AsynResult<(usize, Arc<[EnumEntry]>)> {
        if user.reason == self.p_volts_per_div_select {
            return Ok((0, self.volts_per_div_choices.clone()));
        }
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "no enum table".into(),
        })
    }

    /// C `readFloat64Array` (:338-357): either array, cut to MaxPoints.
    fn read_float64_array(&mut self, user: &AsynUser, buf: &mut [f64]) -> AsynResult<usize> {
        self.base.check_ready()?;
        let max_points = self.base.get_int32_param(self.p_max_points, 0)?;
        let waveform;
        let data: &[f64] = if user.reason == self.p_time_base {
            &self.time_base
        } else {
            waveform = self.base.params.get_float64_array(user.reason, user.addr)?;
            &waveform
        };
        let n = (max_points.max(0) as usize).min(buf.len()).min(data.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
}

// --- Simulation config and background task ---

#[derive(Clone, Copy)]
pub struct ParamIndices {
    pub p_run: usize,
    pub p_max_points: usize,
    pub p_time_per_div: usize,
    pub p_volts_per_div: usize,
    pub p_volt_offset: usize,
    pub p_trigger_delay: usize,
    pub p_noise_amplitude: usize,
    pub p_update_time: usize,
    pub p_waveform: usize,
    pub p_time_base: usize,
    pub p_min_value: usize,
    pub p_max_value: usize,
    pub p_mean_value: usize,
}

pub struct SimConfig {
    pub run: bool,
    /// C `epicsInt32 maxPoints`: a negative one is an empty trace.
    pub max_points: i32,
    pub time_per_div: f64,
    pub volts_per_div: f64,
    pub volt_offset: f64,
    pub trigger_delay: f64,
    pub noise_amplitude: f64,
    pub update_time: f64,
}

pub struct SimResult {
    pub waveform: Vec<f64>,
    pub min_val: f64,
    pub max_val: f64,
    pub mean_val: f64,
}

pub fn read_config(base: &PortDriverBase, idx: &ParamIndices) -> SimConfig {
    SimConfig {
        run: base.get_int32_param(idx.p_run, 0).unwrap_or(0) != 0,
        max_points: base
            .get_int32_param(idx.p_max_points, 0)
            .unwrap_or(DEFAULT_MAX_POINTS),
        time_per_div: base
            .get_float64_param(idx.p_time_per_div, 0)
            .unwrap_or(0.001),
        volts_per_div: base
            .get_float64_param(idx.p_volts_per_div, 0)
            .unwrap_or(1.0),
        volt_offset: base.get_float64_param(idx.p_volt_offset, 0).unwrap_or(0.0),
        trigger_delay: base
            .get_float64_param(idx.p_trigger_delay, 0)
            .unwrap_or(0.0),
        noise_amplitude: base
            .get_float64_param(idx.p_noise_amplitude, 0)
            .unwrap_or(0.0),
        update_time: base
            .get_float64_param(idx.p_update_time, 0)
            .unwrap_or(DEFAULT_UPDATE_TIME),
    }
}

pub fn compute_waveform(config: &SimConfig, rng: &mut Rng) -> SimResult {
    let num_points = config.max_points;
    let time_range = NUM_DIVISIONS * config.time_per_div;
    let dt = time_range / num_points as f64;

    let mut waveform = Vec::with_capacity(num_points.max(0) as usize);
    let mut min_val = 1e6;
    let mut max_val = -1e6;
    let mut sum = 0.0;

    for i in 0..num_points {
        let t = i as f64 * dt + config.trigger_delay;
        // C simTask (testAsynPortDriver.cpp:204-212): noise spans half the
        // amplitude either way, and the statistics are of the unscaled volts.
        let y = AMPLITUDE * (2.0 * std::f64::consts::PI * FREQUENCY * t).sin()
            + config.noise_amplitude * (rng.next_f64() - 0.5);
        if y < min_val {
            min_val = y;
        }
        if y > max_val {
            max_val = y;
        }
        sum += y;
        waveform.push(NUM_DIVISIONS / 2.0 + (config.volt_offset + y) / config.volts_per_div);
    }

    let mean_val = sum / num_points as f64;
    SimResult {
        waveform,
        min_val,
        max_val,
        mean_val,
    }
}

/// The end of a C simTask cycle (testAsynPortDriver.cpp:220-230): the
/// statistics go out with callParamCallbacks before the trace's
/// doCallbacksFloat64Array.
fn post_cycle(base: &mut PortDriverBase, idx: &ParamIndices, result: SimResult) {
    base.update_time_stamp();
    let _ = base.set_float64_param(idx.p_min_value, 0, result.min_val);
    let _ = base.set_float64_param(idx.p_max_value, 0, result.max_val);
    let _ = base.set_float64_param(idx.p_mean_value, 0, result.mean_val);
    let _ = base.call_param_callbacks(0);
    let _ = base
        .params
        .set_float64_array(idx.p_waveform, 0, result.waveform);
    let _ = base.call_param_callback(0, idx.p_waveform);
}

/// Background simulation task using the concrete ScopeSimulator type.
pub async fn sim_task(port: Arc<Mutex<ScopeSimulator>>, notify: Arc<Notify>, idx: ParamIndices) {
    let mut rng = Rng::new(0xDEAD_BEEF_CAFE_1234);

    loop {
        let config = {
            let guard = port.lock();
            read_config(&guard.base, &idx)
        };

        if !config.run {
            notify.notified().await;
            continue;
        }

        let result = compute_waveform(&config, &mut rng);

        post_cycle(&mut port.lock().base, &idx, result);

        let sleep_dur = std::time::Duration::from_secs_f64(config.update_time);
        asyn_rs::runtime::select! {
            _ = asyn_rs::runtime::task::sleep(sleep_dur) => {}
            _ = notify.notified() => {}
        }
    }
}

/// Background simulation task using the trait-object `dyn PortDriver`.
/// Used by the standalone example which registers with PortManager.
#[deprecated(note = "use sim_task_handle() with PortHandle instead")]
pub async fn sim_task_dyn(
    port: Arc<Mutex<dyn PortDriver>>,
    notify: Arc<Notify>,
    idx: ParamIndices,
) {
    let mut rng = Rng::new(0xDEAD_BEEF_CAFE_1234);

    loop {
        let config = {
            let guard = port.lock();
            read_config(guard.base(), &idx)
        };

        if !config.run {
            notify.notified().await;
            continue;
        }

        let result = compute_waveform(&config, &mut rng);

        post_cycle(port.lock().base_mut(), &idx, result);

        let sleep_dur = std::time::Duration::from_secs_f64(config.update_time);
        asyn_rs::runtime::select! {
            _ = asyn_rs::runtime::task::sleep(sleep_dur) => {}
            _ = notify.notified() => {}
        }
    }
}

/// Background simulation task using [`asyn_rs::port_handle::PortHandle`].
///
/// Reads config and writes results via the actor's async channel API.
pub async fn sim_task_handle(
    handle: asyn_rs::port_handle::PortHandle,
    notify: Arc<Notify>,
    idx: ParamIndices,
) {
    let mut rng = Rng::new(0xDEAD_BEEF_CAFE_1234);

    loop {
        let config = read_config_handle(&handle, &idx).await;

        if !config.run {
            notify.notified().await;
            continue;
        }

        let result = compute_waveform(&config, &mut rng);

        // C simTask stamps the port (updateTimeStamp, testAsynPortDriver.cpp
        // :220), sets the statistics, calls callParamCallbacks and posts the
        // waveform with doCallbacksFloat64Array (:224-230); it never goes
        // through writeFloat64Array, which the asynPortDriver default refuses.
        let _ = handle.update_time_stamp().await;
        let _ = handle
            .set_params_and_notify(
                0,
                vec![
                    ParamSetValue::new(idx.p_min_value, 0, ParamValue::Float64(result.min_val)),
                    ParamSetValue::new(idx.p_max_value, 0, ParamValue::Float64(result.max_val)),
                    ParamSetValue::new(idx.p_mean_value, 0, ParamValue::Float64(result.mean_val)),
                    ParamSetValue::new(
                        idx.p_waveform,
                        0,
                        ParamValue::Float64Array(result.waveform.into()),
                    ),
                ],
            )
            .await;

        let sleep_dur = std::time::Duration::from_secs_f64(config.update_time);
        asyn_rs::runtime::select! {
            _ = asyn_rs::runtime::task::sleep(sleep_dur) => {}
            _ = notify.notified() => {}
        }
    }
}

async fn read_config_handle(
    handle: &asyn_rs::port_handle::PortHandle,
    idx: &ParamIndices,
) -> SimConfig {
    SimConfig {
        run: handle.read_int32(idx.p_run, 0).await.unwrap_or(0) != 0,
        max_points: handle
            .read_int32(idx.p_max_points, 0)
            .await
            .unwrap_or(DEFAULT_MAX_POINTS),
        time_per_div: handle
            .read_float64(idx.p_time_per_div, 0)
            .await
            .unwrap_or(0.001),
        volts_per_div: handle
            .read_float64(idx.p_volts_per_div, 0)
            .await
            .unwrap_or(1.0),
        volt_offset: handle
            .read_float64(idx.p_volt_offset, 0)
            .await
            .unwrap_or(0.0),
        trigger_delay: handle
            .read_float64(idx.p_trigger_delay, 0)
            .await
            .unwrap_or(0.0),
        noise_amplitude: handle
            .read_float64(idx.p_noise_amplitude, 0)
            .await
            .unwrap_or(0.0),
        update_time: handle
            .read_float64(idx.p_update_time, 0)
            .await
            .unwrap_or(DEFAULT_UPDATE_TIME),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// testAsynPortDriver.cpp:52-53; asynPortDriver adds Common itself.
    #[test]
    fn port_carries_the_c_interface_and_interrupt_masks() {
        let driver = ScopeSimulator::new("scopeMasks", Arc::new(Notify::new()));
        let interrupts = InterfaceMask::INT32
            | InterfaceMask::FLOAT64
            | InterfaceMask::FLOAT64_ARRAY
            | InterfaceMask::ENUM;
        assert_eq!(driver.base.interrupt_mask(), interrupts);
        assert_eq!(
            driver.base.interface_mask(),
            interrupts | InterfaceMask::DRV_USER | InterfaceMask::COMMON
        );
    }
}
