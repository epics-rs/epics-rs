//! Asyn port driver for the Oxford Quad X-ray Beam Position Monitor.
//!
//! Provides `QxbpmDriver` (the port driver with parameter cache and poll loop),
//! `SimQxbpm` (in-memory simulation with configurable beam position and noise),
//! and `QxbpmHolder` (IOC startup command registration).
//!
//! The diode-current and position math is `sncqxbpm.st`'s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriverBase, PortFlags};
use asyn_rs::user::AsynUser;
use epics_libcom_rs::cfmt::CFloat;

// ---------------------------------------------------------------------------
// Diode currents and beam position
// ---------------------------------------------------------------------------

/// Number of gain ranges.
pub const NUM_GAINS: usize = 6;

/// Number of diode channels.
pub const NUM_CHANNELS: usize = 4;

/// Default geometric scaling factor for X position.
pub const DEFAULT_GX: f64 = 4.5;

/// Default geometric scaling factor for Y position.
pub const DEFAULT_GY: f64 = 4.5;

/// Default low-current raw threshold.
pub const DEFAULT_LOW_CURRENT_RAW: u32 = 1000;

/// Raw unsigned counts from the four diode channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawDiodeData {
    pub a: u32,
    pub b: u32,
    pub c: u32,
    pub d: u32,
}

/// Per-gain, per-channel calibration data.
///
/// Indexed as `[gain_index * NUM_CHANNELS + channel_index]` where channel
/// index is 0=A, 1=B, 2=C, 3=D.
#[derive(Debug, Clone)]
pub struct CalibrationData {
    /// Gain trim factors: `current = gain_trim * (raw - offset)`.
    pub gain_trim: Vec<f64>,
    /// Dark current offsets (raw counts).
    pub offset: Vec<i32>,
}

impl CalibrationData {
    /// Index into the flat arrays.
    fn idx(gain: usize, channel: usize) -> usize {
        gain * NUM_CHANNELS + channel
    }

    /// Get the gain trim for a specific gain and channel.
    pub fn get_trim(&self, gain: usize, channel: usize) -> f64 {
        let i = Self::idx(gain, channel);
        if i < self.gain_trim.len() {
            self.gain_trim[i]
        } else {
            1.0
        }
    }

    /// Get the offset for a specific gain and channel.
    pub fn get_offset(&self, gain: usize, channel: usize) -> i32 {
        let i = Self::idx(gain, channel);
        if i < self.offset.len() {
            self.offset[i]
        } else {
            0
        }
    }

    /// Set the offset for a specific gain and channel.
    pub fn set_offset(&mut self, gain: usize, channel: usize, value: i32) {
        let i = Self::idx(gain, channel);
        if i < self.offset.len() {
            self.offset[i] = value;
        }
    }

    /// Set the gain trim for a specific gain and channel.
    pub fn set_trim(&mut self, gain: usize, channel: usize, value: f64) {
        let i = Self::idx(gain, channel);
        if i < self.gain_trim.len() {
            self.gain_trim[i] = value;
        }
    }
}

impl Default for CalibrationData {
    fn default() -> Self {
        Self {
            gain_trim: vec![1.0; NUM_GAINS * NUM_CHANNELS],
            offset: vec![0; NUM_GAINS * NUM_CHANNELS],
        }
    }
}

/// Full-scale current in amps of each gain range (`sncqxbpm.st:568-573`).
const FULL_SCALE_AMPS: [f64; NUM_GAINS] = [350e-9, 700e-9, 1400e-9, 7e-6, 70e-6, 700e-6];

/// Compute the default gain trim factors (from the original set_defaults).
///
/// These are based on the full-scale current for each range divided by
/// 10 VDC full-scale and 100 kHz/VDC.
pub fn default_gain_trims() -> Vec<f64> {
    let mut trims = vec![0.0; NUM_GAINS * NUM_CHANNELS];
    for gain in 0..NUM_GAINS {
        let factor = FULL_SCALE_AMPS[gain] / 10.0 / 1e5;
        for ch in 0..NUM_CHANNELS {
            trims[gain * NUM_CHANNELS + ch] = factor;
        }
    }
    trims
}

/// Create a CalibrationData with default trims and zero offsets.
pub fn default_calibration() -> CalibrationData {
    CalibrationData {
        gain_trim: default_gain_trims(),
        offset: vec![0; NUM_GAINS * NUM_CHANNELS],
    }
}

/// Compute the calibrated current for one channel.
pub fn diode_current(raw: u32, gain: usize, channel: usize, cal: &CalibrationData) -> f64 {
    let trim = cal.get_trim(gain, channel);
    let offset = cal.get_offset(gain, channel);
    trim * (raw as f64 - offset as f64)
}

/// Calibrated currents for all four channels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DiodeCurrents {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
}

/// Compute calibrated currents from raw data.
pub fn compute_currents(raw: &RawDiodeData, gain: usize, cal: &CalibrationData) -> DiodeCurrents {
    DiodeCurrents {
        a: diode_current(raw.a, gain, 0, cal),
        b: diode_current(raw.b, gain, 1, cal),
        c: diode_current(raw.c, gain, 2, cal),
        d: diode_current(raw.d, gain, 3, cal),
    }
}

/// Total beam intensity (sum of all four channels).
pub fn total_current(currents: &DiodeCurrents) -> f64 {
    currents.a + currents.b + currents.c + currents.d
}

/// Beam position from four diode currents.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BeamPosition {
    pub x: f64,
    pub y: f64,
}

/// Compute beam position from diode currents and geometric factors.
///
/// From the original code (empirically discovered mapping):
/// - X = GX * (B - D) / (B + D)
/// - Y = GY * (A - C) / (A + C)
///
/// C (sncqxbpm.st:493-494) divides unguarded, so a zero quadrant sum yields
/// NaN (0/0) or ±Inf (nonzero/0) and a tiny sum a large finite value. We
/// reproduce that IEEE result rather than substituting 0.0, so pos:x / pos:y
/// carry the same on-the-wire value as C under no-beam or cancelling currents.
pub fn compute_position(currents: &DiodeCurrents, gx: f64, gy: f64) -> BeamPosition {
    let bd_sum = currents.b + currents.d;
    let ac_sum = currents.a + currents.c;
    let x = gx * (currents.b - currents.d) / bd_sum;
    let y = gy * (currents.a - currents.c) / ac_sum;
    BeamPosition { x, y }
}

// ---------------------------------------------------------------------------
// Parameter indices
// ---------------------------------------------------------------------------

/// Parameter index constants for the QXBPM driver.
#[derive(Debug, Clone, Copy)]
pub struct QxbpmParams {
    pub current_a: usize,
    pub current_b: usize,
    pub current_c: usize,
    pub current_d: usize,
    pub total_current: usize,
    pub x_pos: usize,
    pub y_pos: usize,
    pub gain: usize,
    pub mode: usize,
    pub low_current: usize,
    /// Write-only trigger: a write runs one [`QxbpmDriver::poll`] inside the
    /// port actor. The poll loop owns only a `PortHandle`, so this is how it
    /// reaches the driver the actor owns — the same route C's `readPoller`
    /// takes through `P_Read` (drvModbusAsyn.cpp:1631-1640).
    pub poll: usize,
}

impl QxbpmParams {
    /// Create all parameters on the given port driver base.
    fn create(base: &mut PortDriverBase) -> AsynResult<Self> {
        Ok(Self {
            current_a: base.create_param("CURRENT_A", ParamType::Float64)?,
            current_b: base.create_param("CURRENT_B", ParamType::Float64)?,
            current_c: base.create_param("CURRENT_C", ParamType::Float64)?,
            current_d: base.create_param("CURRENT_D", ParamType::Float64)?,
            total_current: base.create_param("TOTAL_CURRENT", ParamType::Float64)?,
            x_pos: base.create_param("X_POS", ParamType::Float64)?,
            y_pos: base.create_param("Y_POS", ParamType::Float64)?,
            gain: base.create_param("GAIN", ParamType::Int32)?,
            mode: base.create_param("MODE", ParamType::Int32)?,
            low_current: base.create_param("LOW_CURRENT", ParamType::Int32)?,
            poll: base.create_param("POLL", ParamType::Int32)?,
        })
    }
}

// ---------------------------------------------------------------------------
// SimQxbpm — in-memory BPM simulation
// ---------------------------------------------------------------------------

/// In-memory simulation of a quad X-ray BPM.
///
/// Generates simulated diode currents based on a configurable beam position.
/// The position is converted back to four diode currents using the inverse
/// of the position formula:
///   X = GX * (B - D) / (B + D)
///   Y = GY * (A - C) / (A + C)
pub struct SimQxbpm {
    /// Simulated beam X position.
    pub x_pos: f64,
    /// Simulated beam Y position.
    pub y_pos: f64,
    /// Raw count of a diode pair on the most sensitive range (gain 0).
    pub base_intensity: f64,
    /// Current gain range.
    pub gain: usize,
    /// Signal mode: 0=Single, 1=Average, 2=Window.
    pub mode: i32,
    /// Geometric scaling factor for X.
    pub gx: f64,
    /// Geometric scaling factor for Y.
    pub gy: f64,
    /// Calibration data.
    pub calibration: CalibrationData,
    /// Low current threshold.
    pub low_current_threshold: u32,
}

impl SimQxbpm {
    pub fn new(x_pos: f64, y_pos: f64) -> Self {
        Self {
            x_pos,
            y_pos,
            base_intensity: 50000.0,
            gain: 0,
            mode: 0,
            gx: DEFAULT_GX,
            gy: DEFAULT_GY,
            calibration: default_calibration(),
            low_current_threshold: DEFAULT_LOW_CURRENT_RAW,
        }
    }

    /// Generate simulated raw diode data from the current beam position.
    ///
    /// From the position equations:
    ///   X = GX * (B - D) / (B + D)  =>  B/D = (GX + X) / (GX - X)
    ///   Y = GY * (A - C) / (A + C)  =>  A/C = (GY + Y) / (GY - Y)
    ///
    /// We use a base intensity to scale the diode readings. A real
    /// QxBPM's count tracks current over the range's full scale, so the
    /// same beam reads fewer counts on a less sensitive range.
    pub fn generate_raw(&self) -> RawDiodeData {
        let range = FULL_SCALE_AMPS[self.gain.min(NUM_GAINS - 1)];
        let half = self.base_intensity * FULL_SCALE_AMPS[0] / range;

        // Compute B/D ratio from X position
        let x_clamped = self.x_pos.clamp(-self.gx + 0.01, self.gx - 0.01);
        let bd_ratio = (self.gx + x_clamped) / (self.gx - x_clamped);
        let d_val = half / (1.0 + bd_ratio);
        let b_val = half - d_val;

        // Compute A/C ratio from Y position
        let y_clamped = self.y_pos.clamp(-self.gy + 0.01, self.gy - 0.01);
        let ac_ratio = (self.gy + y_clamped) / (self.gy - y_clamped);
        let c_val = half / (1.0 + ac_ratio);
        let a_val = half - c_val;

        RawDiodeData {
            a: a_val.max(0.0) as u32,
            b: b_val.max(0.0) as u32,
            c: c_val.max(0.0) as u32,
            d: d_val.max(0.0) as u32,
        }
    }

    /// Process the simulated raw data through calibration and compute position.
    pub fn read(&self) -> SimQxbpmReading {
        let raw = self.generate_raw();
        let currents = compute_currents(&raw, self.gain, &self.calibration);
        let position = compute_position(&currents, self.gx, self.gy);
        let total = total_current(&currents);
        let low = raw.a < self.low_current_threshold
            && raw.b < self.low_current_threshold
            && raw.c < self.low_current_threshold
            && raw.d < self.low_current_threshold;

        SimQxbpmReading {
            currents,
            x_pos: position.x,
            y_pos: position.y,
            total,
            low_current: low,
        }
    }
}

impl Default for SimQxbpm {
    fn default() -> Self {
        Self::new(0.0, 0.0)
    }
}

/// A reading from the simulated QXBPM.
#[derive(Debug, Clone)]
pub struct SimQxbpmReading {
    pub currents: DiodeCurrents,
    pub x_pos: f64,
    pub y_pos: f64,
    pub total: f64,
    pub low_current: bool,
}

// ---------------------------------------------------------------------------
// QxbpmDriver — asyn port driver
// ---------------------------------------------------------------------------

/// Asyn port driver for the Oxford Quad X-ray BPM.
///
/// Uses a `SimQxbpm` for in-memory simulation or can be extended for real serial I/O.
/// A poll loop periodically reads the simulation and updates parameter readbacks.
pub struct QxbpmDriver {
    base: PortDriverBase,
    params: QxbpmParams,
    sim: Arc<Mutex<SimQxbpm>>,
}

impl QxbpmDriver {
    /// Create a new driver with the given port name and simulation backend.
    pub fn new(port_name: &str, sim: Arc<Mutex<SimQxbpm>>) -> AsynResult<Self> {
        let flags = PortFlags {
            multi_device: false,
            can_block: false,
            destructible: true,
        };
        let mut base = PortDriverBase::new(
            port_name,
            1,
            flags,
            // rs-only simulation port: optics has no C driver for it
            // (sncqxbpm.st talks to the serial port itself).
            InterfaceMask::INT32 | InterfaceMask::FLOAT64 | InterfaceMask::DRV_USER,
            InterfaceMask::INT32 | InterfaceMask::FLOAT64,
        );
        let params = QxbpmParams::create(&mut base)?;

        // Initialize all params to 0
        base.set_float64_param(params.current_a, 0, 0.0)?;
        base.set_float64_param(params.current_b, 0, 0.0)?;
        base.set_float64_param(params.current_c, 0, 0.0)?;
        base.set_float64_param(params.current_d, 0, 0.0)?;
        base.set_float64_param(params.total_current, 0, 0.0)?;
        base.set_float64_param(params.x_pos, 0, 0.0)?;
        base.set_float64_param(params.y_pos, 0, 0.0)?;
        base.set_int32_param(params.gain, 0, 0)?;
        base.set_int32_param(params.mode, 0, 0)?;
        base.set_int32_param(params.low_current, 0, 0)?;

        Ok(Self { base, params, sim })
    }

    /// Get the parameter index set.
    pub fn params(&self) -> &QxbpmParams {
        &self.params
    }

    /// Poll the simulation and update readback parameters.
    pub fn poll(&mut self) -> AsynResult<()> {
        let reading = {
            let sim = self.sim.lock().unwrap();
            sim.read()
        };

        self.base
            .set_float64_param(self.params.current_a, 0, reading.currents.a)?;
        self.base
            .set_float64_param(self.params.current_b, 0, reading.currents.b)?;
        self.base
            .set_float64_param(self.params.current_c, 0, reading.currents.c)?;
        self.base
            .set_float64_param(self.params.current_d, 0, reading.currents.d)?;
        self.base
            .set_float64_param(self.params.total_current, 0, reading.total)?;
        self.base
            .set_float64_param(self.params.x_pos, 0, reading.x_pos)?;
        self.base
            .set_float64_param(self.params.y_pos, 0, reading.y_pos)?;
        self.base.set_int32_param(
            self.params.low_current,
            0,
            if reading.low_current { 1 } else { 0 },
        )?;

        self.base.call_param_callbacks(0)?;
        Ok(())
    }
}

impl asyn_rs::port::PortDriver for QxbpmDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        self.base().check_ready()?;
        let reason = user.reason;
        self.base_mut().set_int32_param(reason, user.addr, value)?;

        if reason == self.params.poll {
            // `poll` publishes the readbacks and fires the callbacks itself.
            return self.poll();
        }
        if reason == self.params.gain {
            let mut sim = self.sim.lock().unwrap();
            sim.gain = value.max(0) as usize;
        } else if reason == self.params.mode {
            let mut sim = self.sim.lock().unwrap();
            sim.mode = value;
        }

        self.base_mut().call_param_callbacks(user.addr)
    }
}

// ---------------------------------------------------------------------------
// Poll loop
// ---------------------------------------------------------------------------

/// Commands sent to the QXBPM poll loop.
#[derive(Debug)]
pub enum QxbpmPollCommand {
    StartPolling,
    Shutdown,
}

/// QXBPM poll loop: periodically triggers a driver poll through the port.
///
/// The driver itself belongs to the port actor once the port is registered, so
/// the loop holds a `PortHandle` and writes [`QxbpmParams::poll`] rather than
/// locking the driver behind the actor's back.
pub struct QxbpmPollLoop {
    cmd_rx: tokio::sync::mpsc::Receiver<QxbpmPollCommand>,
    port: asyn_rs::port_handle::PortHandle,
    poll_reason: usize,
    poll_interval: Duration,
}

impl QxbpmPollLoop {
    pub fn new(
        cmd_rx: tokio::sync::mpsc::Receiver<QxbpmPollCommand>,
        port: asyn_rs::port_handle::PortHandle,
        poll_reason: usize,
        poll_interval: Duration,
    ) -> Self {
        Self {
            cmd_rx,
            port,
            poll_reason,
            poll_interval,
        }
    }

    /// Run the poll loop. Call from a spawned task.
    ///
    /// Starts idle — waits for `StartPolling` before entering the periodic
    /// poll cycle. This avoids CPU load during st.cmd and autosave restore,
    /// matching C EPICS where pollers start after iocInit.
    pub async fn run(mut self) {
        // Wait for StartPolling before entering the active loop.
        match self.cmd_rx.recv().await {
            Some(QxbpmPollCommand::StartPolling) => {}
            Some(QxbpmPollCommand::Shutdown) | None => return,
        }

        // Active polling loop
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(QxbpmPollCommand::StartPolling) => {}
                        Some(QxbpmPollCommand::Shutdown) | None => return,
                    }
                }
                _ = epics_base_rs::runtime::task::sleep(self.poll_interval) => {
                    if self.port.is_closed() {
                        return;
                    }
                    let _ = self.port.write_int32(self.poll_reason, 0, 1).await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// QxbpmHolder — IOC startup integration
// ---------------------------------------------------------------------------

/// Holds the ports created by startup commands.
pub struct QxbpmHolder {
    ports: Mutex<HashMap<String, asyn_rs::port_handle::PortHandle>>,
    poll_senders: Mutex<Vec<tokio::sync::mpsc::Sender<QxbpmPollCommand>>>,
}

impl QxbpmHolder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            ports: Mutex::new(HashMap::new()),
            poll_senders: Mutex::new(Vec::new()),
        })
    }

    /// Start polling on all registered QXBPM drivers.
    /// Call after iocInit to match C EPICS behavior.
    pub fn start_all_polling(&self) {
        for tx in self.poll_senders.lock().unwrap().iter() {
            let _ = tx.try_send(QxbpmPollCommand::StartPolling);
        }
    }

    /// Register a `simQxbpmCreate` iocsh command.
    ///
    /// Usage: `simQxbpmCreate("port", [xPos], [yPos], [pollMs])`
    ///
    /// Creates a SimQxbpm-backed QxbpmDriver with the given port name,
    /// initial beam position, and poll interval.
    pub fn sim_qxbpm_create_command(
        self: &Arc<Self>,
    ) -> epics_base_rs::server::iocsh::registry::CommandDef {
        use epics_base_rs::server::iocsh::registry::*;

        let holder = self.clone();
        CommandDef::new(
            "simQxbpmCreate",
            vec![
                ArgDesc {
                    name: "port",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "xPos",
                    arg_type: ArgType::Double,
                },
                ArgDesc {
                    name: "yPos",
                    arg_type: ArgType::Double,
                },
                ArgDesc {
                    name: "pollMs",
                    arg_type: ArgType::Int,
                },
            ],
            "simQxbpmCreate(port, [xPos], [yPos], [pollMs]) - Create a simulated QXBPM",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = match &args[0] {
                    ArgValue::String(s) => s.to_str()?.to_owned(),
                    _ => return Err("port must be a string".into()),
                };
                let x_pos = match &args[1] {
                    ArgValue::Double(v) => *v,
                    ArgValue::Missing => 0.0,
                    _ => return Err("xPos must be a number".into()),
                };
                let y_pos = match &args[2] {
                    ArgValue::Double(v) => *v,
                    ArgValue::Missing => 0.0,
                    _ => return Err("yPos must be a number".into()),
                };
                let poll_ms = match &args[3] {
                    ArgValue::Int(v) => *v as u64,
                    ArgValue::Missing => 100,
                    _ => return Err("pollMs must be an integer".into()),
                };

                let claim = asyn_rs::asyn_record::claim_port(&port)
                    .map_err(|e| format!("failed to register QXBPM port: {e}"))?;
                let sim = Arc::new(Mutex::new(SimQxbpm::new(x_pos, y_pos)));
                let driver = match QxbpmDriver::new(&port, sim) {
                    Ok(d) => d,
                    Err(e) => return Err(format!("failed to create QxbpmDriver: {e}")),
                };
                let poll_reason = driver.params.poll;

                let (runtime, _actor) = match asyn_rs::runtime::port::create_port_runtime(
                    &claim,
                    driver,
                    asyn_rs::runtime::config::RuntimeConfig::default(),
                ) {
                    Ok(pair) => pair,
                    Err(e) => return Err(format!("failed to start QXBPM port: {e}")),
                };
                let port_handle = runtime.port_handle().clone();
                let owner = ctx.db().scan_control().interrupt_gate();
                if let Err(e) = claim.publish(port_handle.clone(), owner) {
                    runtime.shutdown();
                    return Err(format!("failed to register QXBPM port: {e}"));
                }

                let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
                let poll_loop = QxbpmPollLoop::new(
                    cmd_rx,
                    port_handle.clone(),
                    poll_reason,
                    Duration::from_millis(poll_ms),
                );

                ctx.bridge().spawn(poll_loop.run());

                holder.poll_senders.lock().unwrap().push(cmd_tx);
                holder
                    .ports
                    .lock()
                    .unwrap()
                    .insert(port.clone(), port_handle);
                println!(
                    "simQxbpmCreate: port={port} x={} y={} poll={poll_ms}ms",
                    CFloat(x_pos).g(),
                    CFloat(y_pos).g()
                );
                Ok(CommandOutcome::Continue)
            },
        )
    }

    /// Get a port handle by port name.
    pub fn get_port(&self, port: &str) -> Option<asyn_rs::port_handle::PortHandle> {
        self.ports.lock().unwrap().get(port).cloned()
    }
}

impl Default for QxbpmHolder {
    fn default() -> Self {
        Self {
            ports: Mutex::new(HashMap::new()),
            poll_senders: Mutex::new(Vec::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use asyn_rs::port::PortDriver;

    /// The driver as registration leaves it: its port starts down and the
    /// registration connect brings it up (asynManager.c:523, :2133-2137).
    fn registered(port_name: &str, sim: Arc<Mutex<SimQxbpm>>) -> QxbpmDriver {
        let mut driver = QxbpmDriver::new(port_name, sim).unwrap();
        driver.connect(&AsynUser::default()).unwrap();
        driver
    }

    /// The port serves Int32 and Float64 params and names them through
    /// drvUser; PortDriverBase adds Common as asynPortDriver does.
    #[test]
    fn test_port_masks_are_the_interfaces_it_serves() {
        let driver =
            QxbpmDriver::new("QXBPM_MASKS", Arc::new(Mutex::new(SimQxbpm::new(0.0, 0.0)))).unwrap();
        let served = InterfaceMask::INT32 | InterfaceMask::FLOAT64;
        assert_eq!(driver.base().interrupt_mask(), served);
        assert_eq!(
            driver.base().interface_mask(),
            served | InterfaceMask::DRV_USER | InterfaceMask::COMMON
        );
    }

    #[test]
    fn test_sim_qxbpm_centered() {
        let sim = SimQxbpm::new(0.0, 0.0);
        let reading = sim.read();
        // At center, X and Y should be approximately 0
        assert!(reading.x_pos.abs() < 0.1, "x_pos={}", reading.x_pos);
        assert!(reading.y_pos.abs() < 0.1, "y_pos={}", reading.y_pos);
        assert!(reading.total > 0.0);
    }

    #[test]
    fn test_sim_qxbpm_off_center() {
        let sim = SimQxbpm::new(1.0, -0.5);
        let reading = sim.read();
        // Position should reflect the configured beam position (within rounding)
        assert!(
            reading.x_pos > 0.0,
            "x should be positive, got {}",
            reading.x_pos
        );
        assert!(
            reading.y_pos < 0.0,
            "y should be negative, got {}",
            reading.y_pos
        );
    }

    #[test]
    fn test_sim_qxbpm_currents_positive() {
        let sim = SimQxbpm::new(0.0, 0.0);
        let reading = sim.read();
        assert!(reading.currents.a >= 0.0);
        assert!(reading.currents.b >= 0.0);
        assert!(reading.currents.c >= 0.0);
        assert!(reading.currents.d >= 0.0);
    }

    #[test]
    fn test_sim_qxbpm_raw_generation() {
        let sim = SimQxbpm::new(0.0, 0.0);
        let raw = sim.generate_raw();
        // At center, all four channels should be roughly equal
        let avg = (raw.a + raw.b + raw.c + raw.d) as f64 / 4.0;
        assert!((raw.a as f64 - avg).abs() < avg * 0.1);
        assert!((raw.b as f64 - avg).abs() < avg * 0.1);
        assert!((raw.c as f64 - avg).abs() < avg * 0.1);
        assert!((raw.d as f64 - avg).abs() < avg * 0.1);
    }

    /// The same beam reads the same current on every range that resolves
    /// it: raw counts scale as 1 / full scale (`sncqxbpm.st:54-55`).
    #[test]
    fn test_sim_qxbpm_current_is_independent_of_gain() {
        let mut sim = SimQxbpm::new(0.0, 0.0);
        let at_gain0 = sim.read().currents.a;
        sim.gain = 3;
        let at_gain3 = sim.read().currents.a;
        assert!(
            (at_gain3 - at_gain0).abs() < at_gain0 * 1e-6,
            "gain 0 {at_gain0} A, gain 3 {at_gain3} A"
        );
    }

    #[test]
    fn test_sim_qxbpm_not_low_current() {
        let sim = SimQxbpm::new(0.0, 0.0);
        let reading = sim.read();
        // Base intensity 50000 is well above default threshold 1000
        assert!(!reading.low_current);
    }

    #[test]
    fn test_sim_qxbpm_low_current() {
        let mut sim = SimQxbpm::new(0.0, 0.0);
        sim.base_intensity = 100.0; // below default threshold of 1000
        let reading = sim.read();
        assert!(reading.low_current);
    }

    #[test]
    fn test_qxbpm_driver_poll_updates_params() {
        let sim = Arc::new(Mutex::new(SimQxbpm::new(1.0, -0.5)));
        let mut driver = registered("test_qxbpm", sim);
        driver.poll().unwrap();

        let p = driver.params();
        // Currents should be populated
        let ca = driver.base.get_float64_param(p.current_a, 0).unwrap();
        let cb = driver.base.get_float64_param(p.current_b, 0).unwrap();
        let cc = driver.base.get_float64_param(p.current_c, 0).unwrap();
        let cd = driver.base.get_float64_param(p.current_d, 0).unwrap();
        assert!(ca > 0.0 || cb > 0.0 || cc > 0.0 || cd > 0.0);

        // Position should reflect beam offset
        let x = driver.base.get_float64_param(p.x_pos, 0).unwrap();
        let y = driver.base.get_float64_param(p.y_pos, 0).unwrap();
        assert!(x > 0.0, "x should be positive, got {x}");
        assert!(y < 0.0, "y should be negative, got {y}");

        // Total should be positive
        let total = driver.base.get_float64_param(p.total_current, 0).unwrap();
        assert!(total > 0.0);
    }

    #[test]
    fn test_qxbpm_driver_write_gain() {
        let sim = Arc::new(Mutex::new(SimQxbpm::new(0.0, 0.0)));
        let mut driver = registered("test_qxbpm_gain", sim.clone());

        let p = driver.params;
        let mut user = AsynUser::new(p.gain);
        driver.write_int32(&mut user, 3).unwrap();

        assert_eq!(sim.lock().unwrap().gain, 3);
        assert_eq!(driver.base.get_int32_param(p.gain, 0).unwrap(), 3);
    }

    #[test]
    fn test_qxbpm_driver_write_mode() {
        let sim = Arc::new(Mutex::new(SimQxbpm::new(0.0, 0.0)));
        let mut driver = registered("test_qxbpm_mode", sim.clone());

        let p = driver.params;
        let mut user = AsynUser::new(p.mode);
        driver.write_int32(&mut user, 2).unwrap();

        assert_eq!(sim.lock().unwrap().mode, 2);
        assert_eq!(driver.base.get_int32_param(p.mode, 0).unwrap(), 2);
    }

    #[test]
    fn test_qxbpm_holder_creation() {
        let holder = QxbpmHolder::new();
        assert!(holder.get_port("nonexistent").is_none());
    }

    #[test]
    fn test_sim_qxbpm_gain_change_scales_raw_counts() {
        let mut sim = SimQxbpm::new(0.0, 0.0);
        let raw_g0 = sim.generate_raw().a as f64;
        sim.gain = 3;
        let raw_g3 = sim.generate_raw().a as f64;
        // 7 uA full scale is 20x the 350 nA one.
        assert!(
            (raw_g0 / raw_g3 - 20.0).abs() < 0.01,
            "gain 0 {raw_g0} counts, gain 3 {raw_g3} counts"
        );
    }

    #[test]
    fn calibration_default_trims_nonzero() {
        let cal = default_calibration();
        for g in 0..NUM_GAINS {
            for ch in 0..NUM_CHANNELS {
                let trim = cal.get_trim(g, ch);
                assert!(trim > 0.0, "trim[{g}][{ch}] should be > 0, got {trim}");
            }
        }
    }

    #[test]
    fn calibration_offsets_default_zero() {
        let cal = default_calibration();
        for g in 0..NUM_GAINS {
            for ch in 0..NUM_CHANNELS {
                assert_eq!(cal.get_offset(g, ch), 0);
            }
        }
    }

    #[test]
    fn calibration_set_and_get() {
        let mut cal = CalibrationData::default();
        cal.set_trim(2, 1, 0.42);
        cal.set_offset(2, 1, 500);
        assert!((cal.get_trim(2, 1) - 0.42).abs() < 1e-15);
        assert_eq!(cal.get_offset(2, 1), 500);
    }

    #[test]
    fn calibration_out_of_range() {
        let cal = CalibrationData::default();
        // Should return defaults for out-of-range
        assert!((cal.get_trim(99, 0) - 1.0).abs() < 1e-15);
        assert_eq!(cal.get_offset(99, 0), 0);
    }

    #[test]
    fn diode_current_no_offset() {
        let cal = CalibrationData {
            gain_trim: vec![2.0; NUM_GAINS * NUM_CHANNELS],
            offset: vec![0; NUM_GAINS * NUM_CHANNELS],
        };
        // current = 2.0 * (1000 - 0) = 2000.0
        assert!((diode_current(1000, 0, 0, &cal) - 2000.0).abs() < 1e-9);
    }

    #[test]
    fn diode_current_with_offset() {
        let mut cal = CalibrationData {
            gain_trim: vec![1.0; NUM_GAINS * NUM_CHANNELS],
            offset: vec![0; NUM_GAINS * NUM_CHANNELS],
        };
        cal.set_offset(0, 0, 100);
        // current = 1.0 * (500 - 100) = 400.0
        assert!((diode_current(500, 0, 0, &cal) - 400.0).abs() < 1e-9);
    }

    #[test]
    fn compute_currents_all() {
        let cal = CalibrationData {
            gain_trim: vec![1.0; NUM_GAINS * NUM_CHANNELS],
            offset: vec![0; NUM_GAINS * NUM_CHANNELS],
        };
        let raw = RawDiodeData {
            a: 100,
            b: 200,
            c: 300,
            d: 400,
        };
        let c = compute_currents(&raw, 0, &cal);
        assert!((c.a - 100.0).abs() < 1e-9);
        assert!((c.b - 200.0).abs() < 1e-9);
        assert!((c.c - 300.0).abs() < 1e-9);
        assert!((c.d - 400.0).abs() < 1e-9);
    }

    #[test]
    fn total_current_sum() {
        let c = DiodeCurrents {
            a: 1.0,
            b: 2.0,
            c: 3.0,
            d: 4.0,
        };
        assert!((total_current(&c) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn position_centered() {
        let c = DiodeCurrents {
            a: 1.0,
            b: 1.0,
            c: 1.0,
            d: 1.0,
        };
        let pos = compute_position(&c, 4.5, 4.5);
        assert!((pos.x - 0.0).abs() < 1e-9);
        assert!((pos.y - 0.0).abs() < 1e-9);
    }

    #[test]
    fn position_x_offset() {
        // X = GX * (B-D)/(B+D)
        let c = DiodeCurrents {
            a: 1.0,
            b: 3.0,
            c: 1.0,
            d: 1.0,
        };
        let pos = compute_position(&c, 4.5, 4.5);
        // x = 4.5 * (3-1)/(3+1) = 4.5 * 0.5 = 2.25
        assert!((pos.x - 2.25).abs() < 1e-9);
        assert!((pos.y - 0.0).abs() < 1e-9);
    }

    #[test]
    fn position_y_offset() {
        // Y = GY * (A-C)/(A+C)
        let c = DiodeCurrents {
            a: 4.0,
            b: 1.0,
            c: 2.0,
            d: 1.0,
        };
        let pos = compute_position(&c, 4.5, 4.5);
        // y = 4.5 * (4-2)/(4+2) = 4.5 * 1/3 = 1.5
        assert!((pos.y - 1.5).abs() < 1e-9);
    }

    #[test]
    fn position_zero_denominator_is_nan_like_c() {
        // No beam: both quadrant sums are 0. C divides 0/0 -> NaN; we match.
        let c = DiodeCurrents {
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
        };
        let pos = compute_position(&c, 4.5, 4.5);
        assert!(pos.x.is_nan(), "pos.x = {}", pos.x);
        assert!(pos.y.is_nan(), "pos.y = {}", pos.y);
    }

    #[test]
    fn position_nonzero_over_zero_is_infinite_like_c() {
        // X numerator nonzero, B+D == 0 (cancelling currents) -> C yields +-Inf.
        let c = DiodeCurrents {
            a: 1.0,
            b: 2.0,
            c: 1.0,
            d: -2.0,
        };
        let pos = compute_position(&c, 4.5, 4.5);
        assert!(pos.x.is_infinite(), "pos.x = {}", pos.x);
    }

    #[test]
    fn position_negative_x() {
        // B < D => x < 0
        let c = DiodeCurrents {
            a: 1.0,
            b: 1.0,
            c: 1.0,
            d: 3.0,
        };
        let pos = compute_position(&c, 4.5, 4.5);
        // x = 4.5 * (1-3)/(1+3) = 4.5 * (-0.5) = -2.25
        assert!((pos.x - (-2.25)).abs() < 1e-9);
    }

    #[test]
    fn default_gain_trims_values() {
        let trims = default_gain_trims();
        assert_eq!(trims.len(), NUM_GAINS * NUM_CHANNELS);
        // Gain 0: 350e-9 / 10 / 1e5 = 3.5e-13
        let expected_g0 = 350e-9 / 10.0 / 1e5;
        assert!((trims[0] - expected_g0).abs() < 1e-20);
        // Gain 5: 700e-6 / 10 / 1e5 = 7e-10
        let expected_g5 = 700e-6 / 10.0 / 1e5;
        assert!((trims[5 * NUM_CHANNELS] - expected_g5).abs() < 1e-20);
    }
}
