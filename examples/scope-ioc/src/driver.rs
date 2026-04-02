//! Digital oscilloscope simulator — port of EPICS testAsynPortDriver.
//!
//! Demonstrates the asynPortDriver pattern:
//!   1. Define parameters (scalars, enums, arrays)
//!   2. Override write_int32/write_float64 for control logic
//!   3. Run a background task that computes waveforms and pushes updates

use std::sync::Arc;

use asyn_rs::runtime::sync::Notify;
use parking_lot::Mutex;

use asyn_rs::error::AsynResult;
use asyn_rs::param::{EnumEntry, ParamType};
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::user::AsynUser;

// --- Constants (matching original testAsynPortDriver) ---

pub const FREQUENCY: f64 = 1000.0;
pub const AMPLITUDE: f64 = 1.0;
pub const NUM_DIVISIONS: f64 = 10.0;
pub const MIN_UPDATE_TIME: f64 = 0.02;
pub const DEFAULT_MAX_POINTS: i32 = 1000;
pub const DEFAULT_UPDATE_TIME: f64 = 0.5;

pub const VERT_GAIN_CHOICES: &[(&str, i32)] = &[("x1", 1), ("x2", 2), ("x5", 5), ("x10", 10)];
pub const TIME_PER_DIV_CHOICES: &[(&str, f64)] = &[
    ("0.001", 0.001),
    ("0.002", 0.002),
    ("0.005", 0.005),
    ("0.01", 0.01),
    ("0.02", 0.02),
    ("0.05", 0.05),
    ("0.1", 0.1),
    ("0.2", 0.2),
    ("0.5", 0.5),
    ("1.0", 1.0),
];

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
    pub fn next_centered(&mut self) -> f64 {
        self.next_f64() * 2.0 - 1.0
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
}

impl ScopeSimulator {
    pub fn new(port_name: &str, notify: Arc<Notify>) -> Self {
        let flags = PortFlags {
            can_block: true,
            ..PortFlags::default()
        };
        let mut base = PortDriverBase::new(port_name, 1, flags);

        let p_run = base.create_param("P_Run", ParamType::Int32).unwrap();
        let p_max_points = base.create_param("P_MaxPoints", ParamType::Int32).unwrap();
        let p_time_per_div = base
            .create_param("P_TimePerDiv", ParamType::Float64)
            .unwrap();
        let p_time_per_div_select = base
            .create_param("P_TimePerDivSelect", ParamType::Enum)
            .unwrap();
        let p_vert_gain = base.create_param("P_VertGain", ParamType::Float64).unwrap();
        let p_vert_gain_select = base
            .create_param("P_VertGainSelect", ParamType::Enum)
            .unwrap();
        let p_volts_per_div = base
            .create_param("P_VoltsPerDiv", ParamType::Float64)
            .unwrap();
        let p_volts_per_div_select = base
            .create_param("P_VoltsPerDivSelect", ParamType::Enum)
            .unwrap();
        let p_volt_offset = base
            .create_param("P_VoltOffset", ParamType::Float64)
            .unwrap();
        let p_trigger_delay = base
            .create_param("P_TriggerDelay", ParamType::Float64)
            .unwrap();
        let p_noise_amplitude = base
            .create_param("P_NoiseAmplitude", ParamType::Float64)
            .unwrap();
        let p_update_time = base
            .create_param("P_UpdateTime", ParamType::Float64)
            .unwrap();
        let p_waveform = base
            .create_param("P_Waveform", ParamType::Float64Array)
            .unwrap();
        let p_time_base = base
            .create_param("P_TimeBase", ParamType::Float64Array)
            .unwrap();
        let p_min_value = base.create_param("P_MinValue", ParamType::Float64).unwrap();
        let p_max_value = base.create_param("P_MaxValue", ParamType::Float64).unwrap();
        let p_mean_value = base
            .create_param("P_MeanValue", ParamType::Float64)
            .unwrap();

        base.set_int32_param(p_run, 0, 0).unwrap();
        base.set_int32_param(p_max_points, 0, DEFAULT_MAX_POINTS)
            .unwrap();
        base.set_float64_param(p_update_time, 0, DEFAULT_UPDATE_TIME)
            .unwrap();
        base.set_float64_param(p_volt_offset, 0, 0.0).unwrap();
        base.set_float64_param(p_trigger_delay, 0, 0.0).unwrap();
        base.set_float64_param(p_noise_amplitude, 0, 0.0).unwrap();
        base.set_float64_param(p_vert_gain, 0, 1.0).unwrap();
        base.set_float64_param(p_volts_per_div, 0, 1.0).unwrap();
        base.set_float64_param(p_time_per_div, 0, 0.001).unwrap();

        let vert_choices: Arc<[EnumEntry]> = Arc::from(
            VERT_GAIN_CHOICES
                .iter()
                .map(|(s, v)| EnumEntry {
                    string: s.to_string(),
                    value: *v,
                    severity: 0,
                })
                .collect::<Vec<_>>(),
        );
        base.set_enum_choices_param(p_vert_gain_select, 0, vert_choices)
            .unwrap();
        base.set_enum_index_param(p_vert_gain_select, 0, 0).unwrap();

        let time_choices: Arc<[EnumEntry]> = Arc::from(
            TIME_PER_DIV_CHOICES
                .iter()
                .enumerate()
                .map(|(i, (s, _))| EnumEntry {
                    string: s.to_string(),
                    value: i as i32,
                    severity: 0,
                })
                .collect::<Vec<_>>(),
        );
        base.set_enum_choices_param(p_time_per_div_select, 0, time_choices)
            .unwrap();
        base.set_enum_index_param(p_time_per_div_select, 0, 0)
            .unwrap();

        let vpd_choices = Self::make_volts_per_div_choices(1.0);
        base.set_enum_choices_param(p_volts_per_div_select, 0, vpd_choices)
            .unwrap();
        base.set_enum_index_param(p_volts_per_div_select, 0, 0)
            .unwrap();

        // Pre-compute initial waveform/time_base so PINI records get real data
        {
            let initial_config = SimConfig {
                run: false,
                max_points: DEFAULT_MAX_POINTS as usize,
                time_per_div: 0.001,
                volts_per_div: 1.0,
                volt_offset: 0.0,
                trigger_delay: 0.0,
                noise_amplitude: 0.0,
                update_time: DEFAULT_UPDATE_TIME,
            };
            let mut rng = Rng::new(0xDEAD_BEEF_CAFE_1234);
            let result = compute_waveform(&initial_config, &mut rng);
            let _ = base
                .params
                .set_float64_array(p_waveform, 0, result.waveform);
            let _ = base
                .params
                .set_float64_array(p_time_base, 0, result.time_base);
            let _ = base.set_float64_param(p_min_value, 0, result.min_val);
            let _ = base.set_float64_param(p_max_value, 0, result.max_val);
            let _ = base.set_float64_param(p_mean_value, 0, result.mean_val);
        }

        Self {
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
        }
    }

    pub fn make_volts_per_div_choices(vert_gain: f64) -> Arc<[EnumEntry]> {
        let base_values = [0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0];
        Arc::from(
            base_values
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    let scaled = v / vert_gain;
                    EnumEntry {
                        string: format!("{scaled:.2}"),
                        value: i as i32,
                        severity: 0,
                    }
                })
                .collect::<Vec<_>>(),
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

        let is_enum = user.reason == self.p_vert_gain_select
            || user.reason == self.p_volts_per_div_select
            || user.reason == self.p_time_per_div_select;

        if is_enum {
            self.base
                .params
                .set_enum_index(user.reason, user.addr, value as usize)?;
        } else {
            self.base.params.set_int32(user.reason, user.addr, value)?;
        }

        if user.reason == self.p_run {
            if value != 0 {
                self.notify.notify_one();
            }
        } else if user.reason == self.p_vert_gain_select {
            let gain = VERT_GAIN_CHOICES
                .get(value as usize)
                .map(|(_, v)| *v as f64)
                .unwrap_or(1.0);
            self.base.set_float64_param(self.p_vert_gain, 0, gain)?;
            let choices = Self::make_volts_per_div_choices(gain);
            self.base
                .set_enum_choices_param(self.p_volts_per_div_select, 0, choices)?;
            let (idx, ch) = self.base.get_enum_param(self.p_volts_per_div_select, 0)?;
            if let Ok(v) = ch[idx].string.parse::<f64>() {
                self.base.set_float64_param(self.p_volts_per_div, 0, v)?;
            }
        } else if user.reason == self.p_volts_per_div_select {
            let (_, choices) = self.base.get_enum_param(self.p_volts_per_div_select, 0)?;
            if let Some(entry) = choices.get(value as usize) {
                if let Ok(v) = entry.string.parse::<f64>() {
                    self.base.set_float64_param(self.p_volts_per_div, 0, v)?;
                }
            }
        } else if user.reason == self.p_time_per_div_select {
            if let Some(&(_, tpd)) = TIME_PER_DIV_CHOICES.get(value as usize) {
                self.base.set_float64_param(self.p_time_per_div, 0, tpd)?;
            }
        }
        self.base.call_param_callbacks(user.addr)?;
        Ok(())
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        self.base.check_ready()?;
        let value = if user.reason == self.p_update_time {
            value.max(MIN_UPDATE_TIME)
        } else {
            value
        };
        self.base
            .params
            .set_float64(user.reason, user.addr, value)?;
        if user.reason == self.p_update_time {
            self.notify.notify_one();
        }
        self.base.call_param_callback(user.addr, user.reason)?;
        Ok(())
    }

    fn read_float64_array(&mut self, user: &AsynUser, buf: &mut [f64]) -> AsynResult<usize> {
        self.base.check_ready()?;
        let data = self.base.params.get_float64_array(user.reason, user.addr)?;
        let n = data.len().min(buf.len());
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
    pub max_points: usize,
    pub time_per_div: f64,
    pub volts_per_div: f64,
    pub volt_offset: f64,
    pub trigger_delay: f64,
    pub noise_amplitude: f64,
    pub update_time: f64,
}

pub struct SimResult {
    pub waveform: Vec<f64>,
    pub time_base: Vec<f64>,
    pub min_val: f64,
    pub max_val: f64,
    pub mean_val: f64,
}

pub fn read_config(base: &PortDriverBase, idx: &ParamIndices) -> SimConfig {
    SimConfig {
        run: base.params.get_int32(idx.p_run, 0).unwrap_or(0) != 0,
        max_points: base
            .params
            .get_int32(idx.p_max_points, 0)
            .unwrap_or(DEFAULT_MAX_POINTS) as usize,
        time_per_div: base
            .params
            .get_float64(idx.p_time_per_div, 0)
            .unwrap_or(0.001),
        volts_per_div: base
            .params
            .get_float64(idx.p_volts_per_div, 0)
            .unwrap_or(1.0),
        volt_offset: base.params.get_float64(idx.p_volt_offset, 0).unwrap_or(0.0),
        trigger_delay: base
            .params
            .get_float64(idx.p_trigger_delay, 0)
            .unwrap_or(0.0),
        noise_amplitude: base
            .params
            .get_float64(idx.p_noise_amplitude, 0)
            .unwrap_or(0.0),
        update_time: base
            .params
            .get_float64(idx.p_update_time, 0)
            .unwrap_or(DEFAULT_UPDATE_TIME),
    }
}

pub fn compute_waveform(config: &SimConfig, rng: &mut Rng) -> SimResult {
    let num_points = config.max_points;
    let time_range = NUM_DIVISIONS * config.time_per_div;
    let dt = time_range / num_points as f64;

    let mut waveform = Vec::with_capacity(num_points);
    let mut time_base = Vec::with_capacity(num_points);
    let mut min_val = f64::MAX;
    let mut max_val = f64::MIN;
    let mut sum = 0.0;

    for i in 0..num_points {
        let t = i as f64 * dt + config.trigger_delay;
        time_base.push(t);
        let y = AMPLITUDE * (2.0 * std::f64::consts::PI * FREQUENCY * t).sin()
            + config.noise_amplitude * rng.next_centered();
        let scaled = NUM_DIVISIONS / 2.0 + (config.volt_offset + y) / config.volts_per_div;
        if scaled < min_val {
            min_val = scaled;
        }
        if scaled > max_val {
            max_val = scaled;
        }
        sum += scaled;
        waveform.push(scaled);
    }

    let mean_val = if num_points > 0 {
        sum / num_points as f64
    } else {
        0.0
    };
    SimResult {
        waveform,
        time_base,
        min_val,
        max_val,
        mean_val,
    }
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

        {
            let mut guard = port.lock();
            let base = &mut guard.base;
            let _ = base
                .params
                .set_float64_array(idx.p_waveform, 0, result.waveform);
            let _ = base
                .params
                .set_float64_array(idx.p_time_base, 0, result.time_base);
            let _ = base.set_float64_param(idx.p_min_value, 0, result.min_val);
            let _ = base.set_float64_param(idx.p_max_value, 0, result.max_val);
            let _ = base.set_float64_param(idx.p_mean_value, 0, result.mean_val);
            let _ = base.call_param_callbacks(0);
        }

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

        {
            let mut guard = port.lock();
            let base = guard.base_mut();
            let _ = base
                .params
                .set_float64_array(idx.p_waveform, 0, result.waveform);
            let _ = base
                .params
                .set_float64_array(idx.p_time_base, 0, result.time_base);
            let _ = base.set_float64_param(idx.p_min_value, 0, result.min_val);
            let _ = base.set_float64_param(idx.p_max_value, 0, result.max_val);
            let _ = base.set_float64_param(idx.p_mean_value, 0, result.mean_val);
            let _ = base.call_param_callbacks(0);
        }

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

        let _ = handle
            .write_float64_array(idx.p_waveform, 0, result.waveform)
            .await;
        let _ = handle
            .write_float64_array(idx.p_time_base, 0, result.time_base)
            .await;
        let _ = handle
            .write_float64(idx.p_min_value, 0, result.min_val)
            .await;
        let _ = handle
            .write_float64(idx.p_max_value, 0, result.max_val)
            .await;
        let _ = handle
            .write_float64(idx.p_mean_value, 0, result.mean_val)
            .await;
        let _ = handle.call_param_callbacks(0).await;

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
            .unwrap_or(DEFAULT_MAX_POINTS) as usize,
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
