use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus};
use asyn_rs::user::AsynUser;
use std::time::Instant;

/// Simulated motor for testing.
/// Uses time-based linear interpolation to simulate motion.
pub struct SimMotor {
    position: f64,
    encoder_position: f64,
    target: f64,
    velocity: f64,
    moving: bool,
    move_start: Option<Instant>,
    start_position: f64,
    high_limit: f64,
    low_limit: f64,
    homed: bool,
    powered: bool,
    closed_loop_enabled: bool,
    /// If true, velocity move mode (JOG)
    velocity_mode: bool,
    velocity_direction: bool,
}

impl SimMotor {
    pub fn new() -> Self {
        Self {
            position: 0.0,
            encoder_position: 0.0,
            target: 0.0,
            velocity: 1.0,
            moving: false,
            move_start: None,
            start_position: 0.0,
            high_limit: 1000.0,
            low_limit: -1000.0,
            homed: false,
            powered: true,
            closed_loop_enabled: false,
            velocity_mode: false,
            velocity_direction: true,
        }
    }

    pub fn with_limits(mut self, low: f64, high: f64) -> Self {
        self.low_limit = low;
        self.high_limit = high;
        self
    }

    pub fn with_position(mut self, pos: f64) -> Self {
        self.position = pos;
        self.encoder_position = pos;
        self
    }

    /// Advance the simulation by checking elapsed time.
    fn update(&mut self) {
        if !self.moving {
            return;
        }

        let elapsed = self
            .move_start
            .map(|s| s.elapsed().as_secs_f64())
            .unwrap_or(0.0);

        if self.velocity_mode {
            let dir = if self.velocity_direction { 1.0 } else { -1.0 };
            self.position = self.start_position + dir * self.velocity * elapsed;
            self.encoder_position = self.position;

            // Check hardware limits
            if self.position >= self.high_limit {
                self.position = self.high_limit;
                self.encoder_position = self.position;
                self.moving = false;
            } else if self.position <= self.low_limit {
                self.position = self.low_limit;
                self.encoder_position = self.position;
                self.moving = false;
            }
        } else {
            let distance = (self.target - self.start_position).abs();
            let travel_time = if self.velocity > 0.0 {
                distance / self.velocity
            } else {
                0.0
            };

            if elapsed >= travel_time {
                self.position = self.target;
                self.encoder_position = self.target;
                self.moving = false;
            } else {
                let fraction = elapsed / travel_time;
                self.position =
                    self.start_position + (self.target - self.start_position) * fraction;
                self.encoder_position = self.position;
            }
        }
    }
}

impl Default for SimMotor {
    fn default() -> Self {
        Self::new()
    }
}

impl AsynMotor for SimMotor {
    fn move_absolute(
        &mut self,
        _user: &AsynUser,
        position: f64,
        velocity: f64,
        _acceleration: f64,
    ) -> AsynResult<()> {
        self.target = position;
        self.velocity = velocity.abs().max(0.001);
        self.start_position = self.position;
        self.moving = true;
        self.velocity_mode = false;
        self.move_start = Some(Instant::now());
        Ok(())
    }

    fn home(&mut self, _user: &AsynUser, velocity: f64, forward: bool) -> AsynResult<()> {
        // Simulate homing by moving to 0
        self.target = 0.0;
        self.velocity = velocity.abs().max(0.001);
        self.start_position = self.position;
        self.moving = true;
        self.velocity_mode = false;
        self.move_start = Some(Instant::now());
        self.homed = true;
        let _ = forward;
        Ok(())
    }

    fn stop(&mut self, _user: &AsynUser, _acceleration: f64) -> AsynResult<()> {
        self.update();
        self.moving = false;
        self.target = self.position;
        Ok(())
    }

    fn set_closed_loop(&mut self, _user: &AsynUser, enable: bool) -> AsynResult<()> {
        self.closed_loop_enabled = enable;
        Ok(())
    }

    fn set_position(&mut self, _user: &AsynUser, position: f64) -> AsynResult<()> {
        self.position = position;
        self.encoder_position = position;
        Ok(())
    }

    fn poll(&mut self, _user: &AsynUser) -> AsynResult<MotorStatus> {
        self.update();
        Ok(MotorStatus {
            position: self.position,
            encoder_position: self.encoder_position,
            done: !self.moving,
            moving: self.moving,
            high_limit: self.position >= self.high_limit,
            low_limit: self.position <= self.low_limit,
            home: self.position == 0.0,
            powered: self.powered,
            problem: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_sim_motor_initial_state() {
        let user = AsynUser::new(0);
        let mut motor = SimMotor::new();
        let status = motor.poll(&user).unwrap();
        assert_eq!(status.position, 0.0);
        assert!(status.done);
        assert!(!status.moving);
    }

    #[test]
    fn test_sim_motor_move_completes() {
        let user = AsynUser::new(0);
        let mut motor = SimMotor::new();
        motor.move_absolute(&user, 10.0, 10000.0, 1.0).unwrap(); // very fast
        std::thread::sleep(Duration::from_millis(10));
        let status = motor.poll(&user).unwrap();
        assert!(status.done);
        assert_eq!(status.position, 10.0);
    }

    #[test]
    fn test_sim_motor_stop() {
        let user = AsynUser::new(0);
        let mut motor = SimMotor::new();
        motor.move_absolute(&user, 1000.0, 1.0, 1.0).unwrap(); // slow, long move
        std::thread::sleep(Duration::from_millis(10));
        motor.stop(&user, 1.0).unwrap();
        let status = motor.poll(&user).unwrap();
        assert!(status.done);
        assert!(status.position < 1000.0);
    }

    #[test]
    fn test_sim_motor_set_position() {
        let user = AsynUser::new(0);
        let mut motor = SimMotor::new();
        motor.set_position(&user, 42.0).unwrap();
        let status = motor.poll(&user).unwrap();
        assert_eq!(status.position, 42.0);
    }

    #[test]
    fn test_sim_motor_home() {
        let user = AsynUser::new(0);
        let mut motor = SimMotor::new().with_position(10.0);
        motor.home(&user, 10000.0, true).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let status = motor.poll(&user).unwrap();
        assert!(status.done);
        assert_eq!(status.position, 0.0);
    }
}
