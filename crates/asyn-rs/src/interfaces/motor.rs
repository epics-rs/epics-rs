//! Motor interface definitions.

use crate::error::AsynResult;
use crate::user::AsynUser;

/// Motor axis status.
#[derive(Debug, Clone)]
pub struct MotorStatus {
    /// Current position in user coordinates.
    pub position: f64,
    /// Encoder position (if available).
    pub encoder_position: f64,
    /// True if the last move has completed.
    pub done: bool,
    /// True if the motor is currently moving.
    pub moving: bool,
    /// True if a positive limit switch is active.
    pub high_limit: bool,
    /// True if a negative limit switch is active.
    pub low_limit: bool,
    /// True if the home switch is active.
    pub home: bool,
    /// True if the motor is powered on.
    pub powered: bool,
    /// True if a problem was detected.
    pub problem: bool,
}

impl Default for MotorStatus {
    fn default() -> Self {
        Self {
            position: 0.0,
            encoder_position: 0.0,
            done: true,
            moving: false,
            high_limit: false,
            low_limit: false,
            home: false,
            powered: true,
            problem: false,
        }
    }
}

/// Motor interface trait.
///
/// Provides motor axis control for motor-capable drivers.
pub trait AsynMotor: Send + Sync {
    /// Move to an absolute position.
    fn move_absolute(
        &mut self,
        user: &AsynUser,
        position: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()>;

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

    /// Poll the motor for current status.
    fn poll(&mut self, user: &AsynUser) -> AsynResult<MotorStatus>;
}
