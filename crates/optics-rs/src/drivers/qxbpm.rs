//! Asyn port driver for the Oxford Quad X-ray Beam Position Monitor.
//!
//! Provides `QxbpmDriver` (the port driver with parameter cache and poll loop),
//! `SimQxbpm` (in-memory simulation with configurable beam position and noise),
//! and `QxbpmHolder` (IOC startup command registration).
//!
//! Reuses all protocol/math from [`crate::snl::qxbpm`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriverBase, PortFlags};
use asyn_rs::user::AsynUser;

use crate::snl::qxbpm::{
    CalibrationData, DEFAULT_GX, DEFAULT_GY, DEFAULT_LOW_CURRENT_RAW, DiodeCurrents, RawDiodeData,
    compute_currents, compute_position, default_calibration, total_current,
};

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
    /// Base intensity for current generation.
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
    /// We use a base intensity to scale the diode readings.
    pub fn generate_raw(&self) -> RawDiodeData {
        let half = self.base_intensity;

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
        let mut base = PortDriverBase::new(port_name, 1, flags);
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
        self.base_mut().params.set_int32(reason, user.addr, value)?;

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
    Shutdown,
}

/// QXBPM poll loop: periodically reads the simulation and updates driver parameters.
pub struct QxbpmPollLoop {
    cmd_rx: tokio::sync::mpsc::Receiver<QxbpmPollCommand>,
    driver: Arc<Mutex<QxbpmDriver>>,
    poll_interval: Duration,
}

impl QxbpmPollLoop {
    pub fn new(
        cmd_rx: tokio::sync::mpsc::Receiver<QxbpmPollCommand>,
        driver: Arc<Mutex<QxbpmDriver>>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            cmd_rx,
            driver,
            poll_interval,
        }
    }

    /// Run the poll loop. Call from a spawned task.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(QxbpmPollCommand::Shutdown) | None => return,
                    }
                }
                _ = tokio::time::sleep(self.poll_interval) => {
                    if let Ok(mut driver) = self.driver.lock() {
                        let _ = driver.poll();
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// QxbpmHolder — IOC startup integration
// ---------------------------------------------------------------------------

/// Holds QXBPM driver instances created by startup commands.
pub struct QxbpmHolder {
    drivers: Mutex<HashMap<String, Arc<Mutex<QxbpmDriver>>>>,
}

impl QxbpmHolder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            drivers: Mutex::new(HashMap::new()),
        })
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
                    optional: false,
                },
                ArgDesc {
                    name: "xPos",
                    arg_type: ArgType::Double,
                    optional: true,
                },
                ArgDesc {
                    name: "yPos",
                    arg_type: ArgType::Double,
                    optional: true,
                },
                ArgDesc {
                    name: "pollMs",
                    arg_type: ArgType::Int,
                    optional: true,
                },
            ],
            "simQxbpmCreate(port, [xPos], [yPos], [pollMs]) - Create a simulated QXBPM",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = match &args[0] {
                    ArgValue::String(s) => s.clone(),
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

                let sim = Arc::new(Mutex::new(SimQxbpm::new(x_pos, y_pos)));
                let driver = match QxbpmDriver::new(&port, sim) {
                    Ok(d) => Arc::new(Mutex::new(d)),
                    Err(e) => return Err(format!("failed to create QxbpmDriver: {e}")),
                };

                let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
                let poll_loop =
                    QxbpmPollLoop::new(cmd_rx, driver.clone(), Duration::from_millis(poll_ms));

                ctx.runtime_handle().spawn(poll_loop.run());
                let _ = cmd_tx; // kept alive by poll loop's cmd_rx

                holder.drivers.lock().unwrap().insert(port.clone(), driver);
                println!("simQxbpmCreate: port={port} x={x_pos} y={y_pos} poll={poll_ms}ms");
                Ok(CommandOutcome::Continue)
            },
        )
    }

    /// Get a driver by port name.
    pub fn get_driver(&self, port: &str) -> Option<Arc<Mutex<QxbpmDriver>>> {
        self.drivers.lock().unwrap().get(port).cloned()
    }
}

impl Default for QxbpmHolder {
    fn default() -> Self {
        Self {
            drivers: Mutex::new(HashMap::new()),
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
        let mut driver = QxbpmDriver::new("test_qxbpm", sim).unwrap();
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
        let mut driver = QxbpmDriver::new("test_qxbpm_gain", sim.clone()).unwrap();

        let p = driver.params;
        let mut user = AsynUser::new(p.gain);
        driver.write_int32(&mut user, 3).unwrap();

        assert_eq!(sim.lock().unwrap().gain, 3);
        assert_eq!(driver.base.get_int32_param(p.gain, 0).unwrap(), 3);
    }

    #[test]
    fn test_qxbpm_driver_write_mode() {
        let sim = Arc::new(Mutex::new(SimQxbpm::new(0.0, 0.0)));
        let mut driver = QxbpmDriver::new("test_qxbpm_mode", sim.clone()).unwrap();

        let p = driver.params;
        let mut user = AsynUser::new(p.mode);
        driver.write_int32(&mut user, 2).unwrap();

        assert_eq!(sim.lock().unwrap().mode, 2);
        assert_eq!(driver.base.get_int32_param(p.mode, 0).unwrap(), 2);
    }

    #[test]
    fn test_qxbpm_holder_creation() {
        let holder = QxbpmHolder::new();
        assert!(holder.get_driver("nonexistent").is_none());
    }

    #[test]
    fn test_sim_qxbpm_gain_change_affects_currents() {
        let mut sim = SimQxbpm::new(0.0, 0.0);
        let reading_g0 = sim.read();

        sim.gain = 3;
        let reading_g3 = sim.read();

        // Different gain ranges have different trim factors, so currents should differ.
        // Gain 0 trim = 350e-9 / 10 / 1e5 = 3.5e-13
        // Gain 3 trim = 7e-6 / 10 / 1e5 = 7e-12
        // Both are tiny but distinct, so compare the raw f64 values.
        assert!(
            (reading_g0.currents.a - reading_g3.currents.a).abs() > f64::EPSILON,
            "changing gain should change reported current: g0={}, g3={}",
            reading_g0.currents.a,
            reading_g3.currents.a,
        );
    }
}
