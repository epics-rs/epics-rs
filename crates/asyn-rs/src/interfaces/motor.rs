//! Motor interface definitions.

use crate::error::AsynResult;
use crate::user::AsynUser;

/// Motor axis status.
/// Fields match the C asynMotorController MotorStatus structure.
///
/// `PartialEq` backs the poll loop's idle change-detection: the loop notifies
/// the record only when a freshly polled status differs from the last one
/// delivered, the analogue of C `asynMotorAxis::callParamCallbacks`
/// (asynMotorAxis.cpp:316-322) firing the generic-pointer status callback only
/// when `statusChanged_` is set. f64 `!=` matches C's field comparisons
/// (`value != status_.position`), including the `NaN != NaN` always-changed
/// case.
#[derive(Debug, Clone, PartialEq)]
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
    /// True if the motor's home switch is active (C asynMotorController
    /// `motorStatusAtHome_`, MSTA bit 3 RA_HOME).
    pub home: bool,
    /// True if the encoder's home signal is active (C asynMotorController
    /// `motorStatusHome_`, MSTA bit 7 EA_HOME). The motor record reads
    /// this one for ATHM when UEIP=Yes.
    pub encoder_home: bool,
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
            encoder_home: false,
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

/// Which closed-loop gain a [`AsynMotor::set_pid_gain`] call addresses.
/// C devMotorAsyn build_trans maps SET_PGAIN/SET_IGAIN/SET_DGAIN onto
/// the motorPGain/motorIGain/motorDGain float64 parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidGainKind {
    Proportional,
    Integral,
    Derivative,
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
    ///
    /// `min_velocity` is the base/start velocity (motor record VBAS, EGU/sec):
    /// canonical `asynMotorAxis::move(position, relative, minVelocity,
    /// maxVelocity, acceleration)` carries it, and the motor record emits it as
    /// `SET_VEL_BASE` ahead of the move (`devMotorAsyn.c` `motorVelBase`).
    fn move_absolute(
        &mut self,
        user: &AsynUser,
        position: f64,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()>;

    /// Move a relative distance.
    fn move_relative(
        &mut self,
        user: &AsynUser,
        distance: f64,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        // Default: convert to absolute using current position from poll
        let status = self.poll(user)?;
        self.move_absolute(
            user,
            status.position + distance,
            min_velocity,
            velocity,
            acceleration,
        )
    }

    /// Move at a constant velocity (jog).
    /// Default implementation uses move_absolute to a very large target.
    /// `min_velocity` is the base velocity (VBAS); canonical
    /// `asynMotorAxis::moveVelocity(minVelocity, maxVelocity, acceleration)`.
    fn move_velocity(
        &mut self,
        user: &AsynUser,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        let target = if velocity >= 0.0 { 1e9 } else { -1e9 };
        self.move_absolute(user, target, min_velocity, velocity.abs(), acceleration)
    }

    /// Start a homing sequence. `min_velocity` is the base velocity (VBAS) and
    /// `acceleration` the home ramp rate; canonical
    /// `asynMotorAxis::home(minVelocity, maxVelocity, acceleration, forwards)`.
    fn home(
        &mut self,
        user: &AsynUser,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
        forward: bool,
    ) -> AsynResult<()>;

    /// Stop motion.
    fn stop(&mut self, user: &AsynUser, acceleration: f64) -> AsynResult<()>;

    /// Set the current position without moving.
    fn set_position(&mut self, user: &AsynUser, position: f64) -> AsynResult<()>;

    /// Enable or disable closed-loop control.
    fn set_closed_loop(&mut self, _user: &AsynUser, _enable: bool) -> AsynResult<()> {
        Ok(())
    }

    /// Set a closed-loop gain coefficient (0.0 ..= 1.0, unitless — the
    /// motor record clamps before calling; C special pidcof,
    /// motorRecord.cc 3003-3026). Only meaningful for controllers that
    /// report gain support.
    fn set_pid_gain(&mut self, _user: &AsynUser, _kind: PidGainKind, _gain: f64) -> AsynResult<()> {
        Ok(())
    }

    /// Set the controller's high soft-travel limit, dial-frame EGU.
    /// C set_dial_highlimit (motorRecord.cc 4236-4277) forwards the
    /// limit so controllers with their own read-only travel limits can
    /// vet it; C's wire value is raw steps (dhlm/mres) with an
    /// MRES-sign register swap — this interface speaks dial EGU like
    /// [`set_position`](Self::set_position), so the dial high limit
    /// arrives here unconditionally and raw-frame drivers fold below.
    fn set_high_limit(&mut self, _user: &AsynUser, _position: f64) -> AsynResult<()> {
        Ok(())
    }

    /// Set the controller's low soft-travel limit, dial-frame EGU.
    /// Mirror of [`set_high_limit`](Self::set_high_limit) (C
    /// set_dial_lowlimit, motorRecord.cc 4287-4328).
    fn set_low_limit(&mut self, _user: &AsynUser, _position: f64) -> AsynResult<()> {
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
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        self.move_absolute(user, position, min_velocity, velocity, acceleration)
    }
}

// Position-compare output (PCO) is intentionally NOT part of this trait: the C
// change adding `enablePCO`/PCO params to the base asynMotorAxis class
// (`05b25c1d`, motor PR #248) is an open, unmerged PR — not upstream API.
// PCO-capable drivers (Aerotech, Newport XPS, Galil, ACSMotion) expose it
// driver-privately (e.g. iocsh commands) until upstream actually merges a base
// interface to mirror.
