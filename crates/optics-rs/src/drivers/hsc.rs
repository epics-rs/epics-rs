//! Asyn port driver for the XIA HSC-1 slit controller.
//!
//! Provides `HscDriver` (the port driver with parameter cache and poll loop),
//! `SimHsc` (in-memory simulation without serial I/O), and `HscHolder`
//! (IOC startup command registration).
//!
//! Reuses all protocol/math from [`crate::snl::xiahsc`] and [`crate::snl::xia_slit`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriverBase, PortFlags};
use asyn_rs::user::AsynUser;

use crate::snl::xiahsc::{
    blades_from_height_center, blades_from_width_center, h_center_from_blades, height_from_blades,
    v_center_from_blades, width_from_blades,
};

// ---------------------------------------------------------------------------
// Parameter indices
// ---------------------------------------------------------------------------

/// Parameter index constants for the HSC driver.
#[derive(Debug, Clone, Copy)]
pub struct HscParams {
    pub h_gap: usize,
    pub h_center: usize,
    pub v_gap: usize,
    pub v_center: usize,
    pub top: usize,
    pub bottom: usize,
    pub left: usize,
    pub right: usize,
    pub top_rbv: usize,
    pub bottom_rbv: usize,
    pub left_rbv: usize,
    pub right_rbv: usize,
    pub h_gap_rbv: usize,
    pub h_center_rbv: usize,
    pub v_gap_rbv: usize,
    pub v_center_rbv: usize,
    pub busy: usize,
    pub power_level: usize,
}

impl HscParams {
    /// Create all parameters on the given port driver base.
    fn create(base: &mut PortDriverBase) -> AsynResult<Self> {
        Ok(Self {
            h_gap: base.create_param("H_GAP", ParamType::Float64)?,
            h_center: base.create_param("H_CENTER", ParamType::Float64)?,
            v_gap: base.create_param("V_GAP", ParamType::Float64)?,
            v_center: base.create_param("V_CENTER", ParamType::Float64)?,
            top: base.create_param("TOP", ParamType::Float64)?,
            bottom: base.create_param("BOTTOM", ParamType::Float64)?,
            left: base.create_param("LEFT", ParamType::Float64)?,
            right: base.create_param("RIGHT", ParamType::Float64)?,
            top_rbv: base.create_param("TOP_RBV", ParamType::Float64)?,
            bottom_rbv: base.create_param("BOTTOM_RBV", ParamType::Float64)?,
            left_rbv: base.create_param("LEFT_RBV", ParamType::Float64)?,
            right_rbv: base.create_param("RIGHT_RBV", ParamType::Float64)?,
            h_gap_rbv: base.create_param("H_GAP_RBV", ParamType::Float64)?,
            h_center_rbv: base.create_param("H_CENTER_RBV", ParamType::Float64)?,
            v_gap_rbv: base.create_param("V_GAP_RBV", ParamType::Float64)?,
            v_center_rbv: base.create_param("V_CENTER_RBV", ParamType::Float64)?,
            busy: base.create_param("BUSY", ParamType::Int32)?,
            power_level: base.create_param("POWER_LEVEL", ParamType::Int32)?,
        })
    }
}

// ---------------------------------------------------------------------------
// SimHsc — in-memory blade simulation
// ---------------------------------------------------------------------------

/// Simulated blade state for one blade.
#[derive(Debug, Clone)]
struct SimBlade {
    position: f64,
    target: f64,
    velocity: f64,
    moving: bool,
    move_start: Option<Instant>,
    start_position: f64,
}

impl SimBlade {
    fn new() -> Self {
        Self {
            position: 0.0,
            target: 0.0,
            velocity: 5.0,
            moving: false,
            move_start: None,
            start_position: 0.0,
        }
    }

    fn move_to(&mut self, target: f64) {
        self.target = target;
        self.start_position = self.position;
        self.moving = true;
        self.move_start = Some(Instant::now());
    }

    fn update(&mut self) {
        if !self.moving {
            return;
        }
        let elapsed = self
            .move_start
            .map(|s| s.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let distance = (self.target - self.start_position).abs();
        let travel_time = if self.velocity > 0.0 {
            distance / self.velocity
        } else {
            0.0
        };
        if elapsed >= travel_time {
            self.position = self.target;
            self.moving = false;
        } else {
            let fraction = elapsed / travel_time;
            self.position = self.start_position + (self.target - self.start_position) * fraction;
        }
    }
}

/// In-memory simulation of a 4-blade slit system.
///
/// Stores four blade positions and moves them with time-based interpolation,
/// analogous to `SimMotor` in motor-rs.
pub struct SimHsc {
    top: SimBlade,
    bottom: SimBlade,
    left: SimBlade,
    right: SimBlade,
    power_level: i32,
}

impl SimHsc {
    pub fn new() -> Self {
        Self {
            top: SimBlade::new(),
            bottom: SimBlade::new(),
            left: SimBlade::new(),
            right: SimBlade::new(),
            power_level: 0,
        }
    }

    /// Move individual blades to target positions.
    pub fn move_blades(&mut self, top: f64, bottom: f64, left: f64, right: f64) {
        self.top.move_to(top);
        self.bottom.move_to(bottom);
        self.left.move_to(left);
        self.right.move_to(right);
    }

    /// Move horizontal blades based on gap and center.
    pub fn move_h_gap_center(&mut self, gap: f64, center: f64) {
        let (left, right) = blades_from_width_center(gap, center);
        self.left.move_to(left);
        self.right.move_to(right);
    }

    /// Move vertical blades based on gap and center.
    pub fn move_v_gap_center(&mut self, gap: f64, center: f64) {
        let (top, bottom) = blades_from_height_center(gap, center);
        self.top.move_to(top);
        self.bottom.move_to(bottom);
    }

    /// Advance the simulation.
    pub fn update(&mut self) {
        self.top.update();
        self.bottom.update();
        self.left.update();
        self.right.update();
    }

    /// Whether any blade is still moving.
    pub fn is_moving(&self) -> bool {
        self.top.moving || self.bottom.moving || self.left.moving || self.right.moving
    }

    /// Current blade positions (top, bottom, left, right).
    pub fn positions(&self) -> (f64, f64, f64, f64) {
        (
            self.top.position,
            self.bottom.position,
            self.left.position,
            self.right.position,
        )
    }

    /// Set power level.
    pub fn set_power_level(&mut self, level: i32) {
        self.power_level = level.clamp(0, 2);
    }

    /// Get power level.
    pub fn power_level(&self) -> i32 {
        self.power_level
    }
}

impl Default for SimHsc {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// HscDriver — asyn port driver
// ---------------------------------------------------------------------------

/// Asyn port driver for the XIA HSC-1 slit controller.
///
/// Uses a `SimHsc` for in-memory simulation or can be extended for real serial I/O.
/// A poll loop periodically reads positions from the sim and updates parameter readbacks.
pub struct HscDriver {
    base: PortDriverBase,
    params: HscParams,
    sim: Arc<Mutex<SimHsc>>,
}

impl HscDriver {
    /// Create a new driver with the given port name and simulation backend.
    pub fn new(port_name: &str, sim: Arc<Mutex<SimHsc>>) -> AsynResult<Self> {
        let flags = PortFlags {
            multi_device: false,
            can_block: false,
            destructible: true,
        };
        let mut base = PortDriverBase::new(port_name, 1, flags);
        let params = HscParams::create(&mut base)?;

        // Initialize all params to 0
        base.set_float64_param(params.h_gap, 0, 0.0)?;
        base.set_float64_param(params.h_center, 0, 0.0)?;
        base.set_float64_param(params.v_gap, 0, 0.0)?;
        base.set_float64_param(params.v_center, 0, 0.0)?;
        base.set_float64_param(params.top, 0, 0.0)?;
        base.set_float64_param(params.bottom, 0, 0.0)?;
        base.set_float64_param(params.left, 0, 0.0)?;
        base.set_float64_param(params.right, 0, 0.0)?;
        base.set_float64_param(params.top_rbv, 0, 0.0)?;
        base.set_float64_param(params.bottom_rbv, 0, 0.0)?;
        base.set_float64_param(params.left_rbv, 0, 0.0)?;
        base.set_float64_param(params.right_rbv, 0, 0.0)?;
        base.set_float64_param(params.h_gap_rbv, 0, 0.0)?;
        base.set_float64_param(params.h_center_rbv, 0, 0.0)?;
        base.set_float64_param(params.v_gap_rbv, 0, 0.0)?;
        base.set_float64_param(params.v_center_rbv, 0, 0.0)?;
        base.set_int32_param(params.busy, 0, 0)?;
        base.set_int32_param(params.power_level, 0, 0)?;

        Ok(Self { base, params, sim })
    }

    /// Get the parameter index set.
    pub fn params(&self) -> &HscParams {
        &self.params
    }

    /// Poll the simulation and update readback parameters.
    pub fn poll(&mut self) -> AsynResult<()> {
        let (top, bottom, left, right, moving, power_level) = {
            let mut sim = self.sim.lock().unwrap();
            sim.update();
            let (t, b, l, r) = sim.positions();
            (t, b, l, r, sim.is_moving(), sim.power_level())
        };

        self.base.set_float64_param(self.params.top_rbv, 0, top)?;
        self.base
            .set_float64_param(self.params.bottom_rbv, 0, bottom)?;
        self.base.set_float64_param(self.params.left_rbv, 0, left)?;
        self.base
            .set_float64_param(self.params.right_rbv, 0, right)?;

        let h_gap = width_from_blades(left, right);
        let h_center = h_center_from_blades(left, right);
        let v_gap = height_from_blades(top, bottom);
        let v_center = v_center_from_blades(top, bottom);

        self.base
            .set_float64_param(self.params.h_gap_rbv, 0, h_gap)?;
        self.base
            .set_float64_param(self.params.h_center_rbv, 0, h_center)?;
        self.base
            .set_float64_param(self.params.v_gap_rbv, 0, v_gap)?;
        self.base
            .set_float64_param(self.params.v_center_rbv, 0, v_center)?;

        self.base
            .set_int32_param(self.params.busy, 0, if moving { 1 } else { 0 })?;
        self.base
            .set_int32_param(self.params.power_level, 0, power_level)?;

        self.base.call_param_callbacks(0)?;
        Ok(())
    }

    /// Handle a write to a Float64 parameter.
    fn handle_float64_write(&mut self, reason: usize, value: f64) -> AsynResult<()> {
        let p = &self.params;

        if reason == p.h_gap {
            let center = self.base.get_float64_param(p.h_center, 0).unwrap_or(0.0);
            let mut sim = self.sim.lock().unwrap();
            sim.move_h_gap_center(value, center);
        } else if reason == p.h_center {
            let gap = self.base.get_float64_param(p.h_gap, 0).unwrap_or(0.0);
            let mut sim = self.sim.lock().unwrap();
            sim.move_h_gap_center(gap, value);
        } else if reason == p.v_gap {
            let center = self.base.get_float64_param(p.v_center, 0).unwrap_or(0.0);
            let mut sim = self.sim.lock().unwrap();
            sim.move_v_gap_center(value, center);
        } else if reason == p.v_center {
            let gap = self.base.get_float64_param(p.v_gap, 0).unwrap_or(0.0);
            let mut sim = self.sim.lock().unwrap();
            sim.move_v_gap_center(gap, value);
        } else if reason == p.top || reason == p.bottom || reason == p.left || reason == p.right {
            // Individual blade write: read all four targets
            let top_val = if reason == p.top {
                value
            } else {
                self.base.get_float64_param(p.top, 0).unwrap_or(0.0)
            };
            let bottom_val = if reason == p.bottom {
                value
            } else {
                self.base.get_float64_param(p.bottom, 0).unwrap_or(0.0)
            };
            let left_val = if reason == p.left {
                value
            } else {
                self.base.get_float64_param(p.left, 0).unwrap_or(0.0)
            };
            let right_val = if reason == p.right {
                value
            } else {
                self.base.get_float64_param(p.right, 0).unwrap_or(0.0)
            };
            let mut sim = self.sim.lock().unwrap();
            sim.move_blades(top_val, bottom_val, left_val, right_val);
        }

        Ok(())
    }
}

impl asyn_rs::port::PortDriver for HscDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        self.base().check_ready()?;
        let reason = user.reason;
        self.base_mut()
            .params
            .set_float64(reason, user.addr, value)?;
        self.handle_float64_write(reason, value)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        self.base().check_ready()?;
        let reason = user.reason;
        self.base_mut().params.set_int32(reason, user.addr, value)?;

        if reason == self.params.power_level {
            let mut sim = self.sim.lock().unwrap();
            sim.set_power_level(value);
        }

        self.base_mut().call_param_callbacks(user.addr)
    }
}

// ---------------------------------------------------------------------------
// Poll loop
// ---------------------------------------------------------------------------

/// Commands sent to the HSC poll loop.
#[derive(Debug)]
pub enum HscPollCommand {
    Shutdown,
}

/// HSC poll loop: periodically reads the simulation and updates driver parameters.
pub struct HscPollLoop {
    cmd_rx: tokio::sync::mpsc::Receiver<HscPollCommand>,
    driver: Arc<Mutex<HscDriver>>,
    poll_interval: Duration,
}

impl HscPollLoop {
    pub fn new(
        cmd_rx: tokio::sync::mpsc::Receiver<HscPollCommand>,
        driver: Arc<Mutex<HscDriver>>,
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
                        Some(HscPollCommand::Shutdown) | None => return,
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
// HscHolder — IOC startup integration
// ---------------------------------------------------------------------------

/// Holds HSC driver instances created by startup commands.
pub struct HscHolder {
    drivers: Mutex<HashMap<String, Arc<Mutex<HscDriver>>>>,
}

impl HscHolder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            drivers: Mutex::new(HashMap::new()),
        })
    }

    /// Register a `simHscCreate` iocsh command.
    ///
    /// Usage: `simHscCreate("port", [pollMs])`
    ///
    /// Creates a SimHsc-backed HscDriver with the given port name and poll interval,
    /// spawns the poll loop on the tokio runtime.
    pub fn sim_hsc_create_command(
        self: &Arc<Self>,
    ) -> epics_base_rs::server::iocsh::registry::CommandDef {
        use epics_base_rs::server::iocsh::registry::*;

        let holder = self.clone();
        CommandDef::new(
            "simHscCreate",
            vec![
                ArgDesc {
                    name: "port",
                    arg_type: ArgType::String,
                    optional: false,
                },
                ArgDesc {
                    name: "pollMs",
                    arg_type: ArgType::Int,
                    optional: true,
                },
            ],
            "simHscCreate(port, [pollMs]) - Create a simulated HSC-1 slit controller",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = match &args[0] {
                    ArgValue::String(s) => s.clone(),
                    _ => return Err("port must be a string".into()),
                };
                let poll_ms = match &args[1] {
                    ArgValue::Int(v) => *v as u64,
                    ArgValue::Missing => 100,
                    _ => return Err("pollMs must be an integer".into()),
                };

                let sim = Arc::new(Mutex::new(SimHsc::new()));
                let driver = match HscDriver::new(&port, sim) {
                    Ok(d) => Arc::new(Mutex::new(d)),
                    Err(e) => return Err(format!("failed to create HscDriver: {e}")),
                };

                let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
                let poll_loop =
                    HscPollLoop::new(cmd_rx, driver.clone(), Duration::from_millis(poll_ms));

                ctx.runtime_handle().spawn(poll_loop.run());
                let _ = cmd_tx; // kept alive by poll loop's cmd_rx

                holder.drivers.lock().unwrap().insert(port.clone(), driver);
                println!("simHscCreate: port={port} poll={poll_ms}ms");
                Ok(CommandOutcome::Continue)
            },
        )
    }

    /// Get a driver by port name.
    pub fn get_driver(&self, port: &str) -> Option<Arc<Mutex<HscDriver>>> {
        self.drivers.lock().unwrap().get(port).cloned()
    }
}

impl Default for HscHolder {
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
    fn test_sim_hsc_initial_state() {
        let sim = SimHsc::new();
        let (t, b, l, r) = sim.positions();
        assert_eq!(t, 0.0);
        assert_eq!(b, 0.0);
        assert_eq!(l, 0.0);
        assert_eq!(r, 0.0);
        assert!(!sim.is_moving());
    }

    #[test]
    fn test_sim_hsc_move_completes() {
        let mut sim = SimHsc::new();
        // Set a very high velocity so the move completes quickly
        sim.top.velocity = 100000.0;
        sim.bottom.velocity = 100000.0;
        sim.left.velocity = 100000.0;
        sim.right.velocity = 100000.0;

        sim.move_blades(1.0, 2.0, 3.0, 4.0);
        std::thread::sleep(Duration::from_millis(10));
        sim.update();

        let (t, b, l, r) = sim.positions();
        assert_eq!(t, 1.0);
        assert_eq!(b, 2.0);
        assert_eq!(l, 3.0);
        assert_eq!(r, 4.0);
        assert!(!sim.is_moving());
    }

    #[test]
    fn test_sim_hsc_gap_center_horizontal() {
        let mut sim = SimHsc::new();
        sim.left.velocity = 100000.0;
        sim.right.velocity = 100000.0;

        // gap=2.0, center=0.5 => left = 2.0/2 - 0.5 = 0.5, right = 2.0/2 + 0.5 = 1.5
        sim.move_h_gap_center(2.0, 0.5);
        std::thread::sleep(Duration::from_millis(10));
        sim.update();

        let (_, _, l, r) = sim.positions();
        assert!((l - 0.5).abs() < 1e-9);
        assert!((r - 1.5).abs() < 1e-9);
    }

    #[test]
    fn test_sim_hsc_gap_center_vertical() {
        let mut sim = SimHsc::new();
        sim.top.velocity = 100000.0;
        sim.bottom.velocity = 100000.0;

        // gap=4.0, center=1.0 => top = 4.0/2 + 1.0 = 3.0, bottom = 4.0/2 - 1.0 = 1.0
        sim.move_v_gap_center(4.0, 1.0);
        std::thread::sleep(Duration::from_millis(10));
        sim.update();

        let (t, b, _, _) = sim.positions();
        assert!((t - 3.0).abs() < 1e-9);
        assert!((b - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_sim_hsc_power_level() {
        let mut sim = SimHsc::new();
        assert_eq!(sim.power_level(), 0);
        sim.set_power_level(2);
        assert_eq!(sim.power_level(), 2);
        sim.set_power_level(5); // clamped to 2
        assert_eq!(sim.power_level(), 2);
        sim.set_power_level(-1); // clamped to 0
        assert_eq!(sim.power_level(), 0);
    }

    #[test]
    fn test_hsc_driver_poll_updates_readbacks() {
        let sim = Arc::new(Mutex::new(SimHsc::new()));

        // Set up initial positions directly
        {
            let mut s = sim.lock().unwrap();
            s.top.position = 3.0;
            s.bottom.position = 1.0;
            s.left.position = 0.5;
            s.right.position = 1.5;
        }

        let mut driver = HscDriver::new("test_hsc", sim).unwrap();
        driver.poll().unwrap();

        let p = driver.params();
        assert_eq!(driver.base.get_float64_param(p.top_rbv, 0).unwrap(), 3.0);
        assert_eq!(driver.base.get_float64_param(p.bottom_rbv, 0).unwrap(), 1.0);
        assert_eq!(driver.base.get_float64_param(p.left_rbv, 0).unwrap(), 0.5);
        assert_eq!(driver.base.get_float64_param(p.right_rbv, 0).unwrap(), 1.5);

        // Check computed readbacks: width = left + right = 2.0
        assert!((driver.base.get_float64_param(p.h_gap_rbv, 0).unwrap() - 2.0).abs() < 1e-9);
        // h_center = (right - left) / 2 = 0.5
        assert!((driver.base.get_float64_param(p.h_center_rbv, 0).unwrap() - 0.5).abs() < 1e-9);
        // height = top + bottom = 4.0
        assert!((driver.base.get_float64_param(p.v_gap_rbv, 0).unwrap() - 4.0).abs() < 1e-9);
        // v_center = (top - bottom) / 2 = 1.0
        assert!((driver.base.get_float64_param(p.v_center_rbv, 0).unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_hsc_driver_write_h_gap() {
        let sim = Arc::new(Mutex::new(SimHsc::new()));
        let mut driver = HscDriver::new("test_hsc_gap", sim.clone()).unwrap();

        // Write H_CENTER first, then H_GAP
        let p = driver.params;
        driver.base.set_float64_param(p.h_center, 0, 0.5).unwrap();
        let mut user = AsynUser::new(p.h_gap);
        driver.write_float64(&mut user, 2.0).unwrap();

        // Sim should have been commanded to move
        {
            let s = sim.lock().unwrap();
            // left target = 2.0/2 - 0.5 = 0.5
            assert!((s.left.target - 0.5).abs() < 1e-9);
            // right target = 2.0/2 + 0.5 = 1.5
            assert!((s.right.target - 1.5).abs() < 1e-9);
        }
    }

    #[test]
    fn test_hsc_driver_write_individual_blades() {
        let sim = Arc::new(Mutex::new(SimHsc::new()));
        let mut driver = HscDriver::new("test_hsc_blades", sim.clone()).unwrap();

        let p = driver.params;
        // Pre-set the target params so individual blade writes pick them up
        driver.base.set_float64_param(p.top, 0, 1.0).unwrap();
        driver.base.set_float64_param(p.bottom, 0, 2.0).unwrap();
        driver.base.set_float64_param(p.left, 0, 3.0).unwrap();

        // Write RIGHT blade
        let mut user = AsynUser::new(p.right);
        driver.write_float64(&mut user, 4.0).unwrap();

        {
            let s = sim.lock().unwrap();
            assert!((s.top.target - 1.0).abs() < 1e-9);
            assert!((s.bottom.target - 2.0).abs() < 1e-9);
            assert!((s.left.target - 3.0).abs() < 1e-9);
            assert!((s.right.target - 4.0).abs() < 1e-9);
        }
    }

    #[test]
    fn test_hsc_driver_write_power_level() {
        let sim = Arc::new(Mutex::new(SimHsc::new()));
        let mut driver = HscDriver::new("test_hsc_pwr", sim.clone()).unwrap();

        let p = driver.params;
        let mut user = AsynUser::new(p.power_level);
        driver.write_int32(&mut user, 2).unwrap();

        assert_eq!(sim.lock().unwrap().power_level(), 2);
        assert_eq!(driver.base.get_int32_param(p.power_level, 0).unwrap(), 2);
    }

    #[test]
    fn test_hsc_driver_busy_flag() {
        let sim = Arc::new(Mutex::new(SimHsc::new()));
        let mut driver = HscDriver::new("test_hsc_busy", sim.clone()).unwrap();

        // Initially not moving
        driver.poll().unwrap();
        assert_eq!(
            driver.base.get_int32_param(driver.params.busy, 0).unwrap(),
            0
        );

        // Start a slow move
        {
            let mut s = sim.lock().unwrap();
            s.left.velocity = 0.001; // very slow
            s.left.move_to(100.0);
        }

        driver.poll().unwrap();
        assert_eq!(
            driver.base.get_int32_param(driver.params.busy, 0).unwrap(),
            1
        );
    }

    #[test]
    fn test_hsc_holder_creation() {
        let holder = HscHolder::new();
        assert!(holder.get_driver("nonexistent").is_none());
    }

    #[test]
    fn test_sim_blade_partial_move() {
        let mut blade = SimBlade::new();
        blade.velocity = 1.0; // 1 mm/s
        blade.move_to(10.0);
        // Immediately after starting, position should still be near 0
        blade.update();
        assert!(blade.position < 10.0);
        assert!(blade.moving);
    }
}
