//! Motor interface definitions.

use crate::error::AsynResult;
use crate::user::AsynUser;

/// Motor axis status.
/// Fields match the C asynMotorController MotorStatus structure.
#[derive(Debug, Clone)]
pub struct MotorStatus {
    /// Current position in user coordinates.
    pub position: f64,
    /// Encoder position (if available).
    pub encoder_position: f64,
    /// Current velocity.
    pub velocity: f64,
    /// True if the last move has completed.
    pub done: bool,
    /// True if the motor is currently moving.
    pub moving: bool,
    /// True if a positive (raw) limit switch is active.
    pub high_limit: bool,
    /// True if a negative (raw) limit switch is active.
    pub low_limit: bool,
    /// True if the home switch is active.
    pub home: bool,
    /// True if the motor is powered on / closed-loop is active.
    pub powered: bool,
    /// True if a problem was detected (driver stopped polling).
    pub problem: bool,
    /// Direction of last motion (true = positive).
    pub direction: bool,
    /// True if encoder slip is detected.
    pub slip_stall: bool,
    /// True if communication error was detected.
    pub comms_error: bool,
    /// True if the axis has been homed.
    pub homed: bool,
    /// True if the controller supports closed-loop gain.
    pub gain_support: bool,
    /// True if an encoder is present.
    pub has_encoder: bool,
    /// True if the controller exposes a usable base velocity (VBAS).
    /// epics-modules/motor issue #76. Drivers that ignore VBAS should set
    /// this to `false`; the record-level layer then treats VBAS as 0 in
    /// acceleration math.
    pub vbas_supported: bool,
}

impl Default for MotorStatus {
    fn default() -> Self {
        Self {
            position: 0.0,
            encoder_position: 0.0,
            velocity: 0.0,
            done: true,
            moving: false,
            high_limit: false,
            low_limit: false,
            home: false,
            powered: true,
            problem: false,
            direction: false,
            slip_stall: false,
            comms_error: false,
            homed: false,
            gain_support: false,
            has_encoder: false,
            vbas_supported: true,
        }
    }
}

/// Motor interface trait.
///
/// Provides motor axis control for motor-capable drivers.
///
/// Units: `velocity` is in EGU/sec and `acceleration` is in EGU/sec²
/// (matching C `accEGUfromVelo`). The motor record converts its ACCL
/// (time-to-velocity, sec) field into an EGU/sec² rate before calling.
pub trait AsynMotor: Send + Sync {
    /// Move to an absolute position.
    fn move_absolute(
        &mut self,
        user: &AsynUser,
        position: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()>;

    /// Move a relative distance.
    fn move_relative(
        &mut self,
        user: &AsynUser,
        distance: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        // Default: convert to absolute using current position from poll
        let status = self.poll(user)?;
        self.move_absolute(user, status.position + distance, velocity, acceleration)
    }

    /// Move at a constant velocity (jog).
    /// Default implementation uses move_absolute to a very large target.
    fn move_velocity(
        &mut self,
        user: &AsynUser,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        let target = if velocity >= 0.0 { 1e9 } else { -1e9 };
        self.move_absolute(user, target, velocity.abs(), acceleration)
    }

    /// Start a homing sequence.
    fn home(&mut self, user: &AsynUser, velocity: f64, forward: bool) -> AsynResult<()>;

    /// Stop motion.
    fn stop(&mut self, user: &AsynUser, acceleration: f64) -> AsynResult<()>;

    /// Set the current position without moving.
    fn set_position(&mut self, user: &AsynUser, position: f64) -> AsynResult<()>;

    /// Enable or disable closed-loop control.
    fn set_closed_loop(&mut self, _user: &AsynUser, _enable: bool) -> AsynResult<()> {
        Ok(())
    }

    /// Enable or disable deferred moves for coordinated multi-axis motion.
    fn set_deferred_moves(&mut self, _user: &AsynUser, _defer: bool) -> AsynResult<()> {
        Ok(())
    }

    /// Poll the motor for current status.
    fn poll(&mut self, user: &AsynUser) -> AsynResult<MotorStatus>;

    /// Initialize profile move with maximum number of points.
    fn initialize_profile(&mut self, _user: &AsynUser, _max_points: usize) -> AsynResult<()> {
        Ok(())
    }

    /// Define profile positions for this axis.
    fn define_profile(&mut self, _user: &AsynUser, _positions: &[f64]) -> AsynResult<()> {
        Ok(())
    }

    /// Build the profile trajectory.
    fn build_profile(&mut self, _user: &AsynUser) -> AsynResult<()> {
        Ok(())
    }

    /// Execute the profile move.
    fn execute_profile(&mut self, _user: &AsynUser) -> AsynResult<()> {
        Ok(())
    }

    /// Abort the profile move.
    fn abort_profile(&mut self, _user: &AsynUser) -> AsynResult<()> {
        Ok(())
    }

    /// Read back actual positions after profile execution.
    fn readback_profile(&mut self, _user: &AsynUser) -> AsynResult<Vec<f64>> {
        Ok(vec![])
    }

    /// Move to a named "home" position defined by the controller.
    /// Distinct from `home()` (limit-switch seek): this is an absolute move
    /// to a pre-defined position, typical of model-3 controllers.
    /// C: `a6f64591` + `5f421e9a` (2011-07).
    fn move_to_home(
        &mut self,
        user: &AsynUser,
        position: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        self.move_absolute(user, position, velocity, acceleration)
    }

    /// Enable or disable position-compare output (PCO) on this axis.
    /// C: `05b25c1d` (PR #248) — adds `enablePCO(bool)` to asynMotorAxis.
    /// Drivers that support PCO (Aerotech, Newport XPS, Galil, ACSMotion) override
    /// this; the default is a no-op for axes without PCO hardware.
    fn enable_pco(&mut self, _user: &AsynUser, _enable: bool) -> AsynResult<()> {
        Ok(())
    }

    /// Configure position-compare output parameters. C: `05b25c1d` — five asyn
    /// parameters PCO_START/END/INCREMENT/PULSE_WIDTH/ENABLE. The default does
    /// nothing; PCO-capable drivers override and persist the configuration so
    /// the next `enable_pco(true)` uses these values.
    fn set_pco_config(
        &mut self,
        _user: &AsynUser,
        _start: f64,
        _end: f64,
        _increment: f64,
        _pulse_width_us: f64,
    ) -> AsynResult<()> {
        Ok(())
    }
}
