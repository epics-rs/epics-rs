use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PidGainKind};
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
    /// Deferred moves support
    deferred: bool,
    deferred_moves: Vec<(f64, f64, f64)>, // (position, velocity, acceleration)
    /// Profile move support
    profile_positions: Vec<f64>,
    profile_readbacks: Vec<f64>,
    /// PID gains forwarded by the record, in arrival order (C
    /// devMotorAsyn motorPGain/motorIGain/motorDGain parameters).
    pub pid_gains: Vec<(PidGainKind, f64)>,
    /// Limits forwarded by the record (C devMotorAsyn motorHighLimit/
    /// motorLowLimit parameters). Stored, not enforced — the sim's
    /// `high_limit`/`low_limit` model independent hardware travel
    /// limits, like asyn drivers that record the parameter without
    /// acting on it.
    pub forwarded_high_limit: Option<f64>,
    pub forwarded_low_limit: Option<f64>,
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
            deferred: false,
            deferred_moves: Vec::new(),
            profile_positions: Vec::new(),
            profile_readbacks: Vec::new(),
            pid_gains: Vec::new(),
            forwarded_high_limit: None,
            forwarded_low_limit: None,
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
        _min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        if self.deferred {
            self.deferred_moves.push((position, velocity, acceleration));
            return Ok(());
        }
        self.target = position;
        self.velocity = velocity.abs().max(0.001);
        self.start_position = self.position;
        self.moving = true;
        self.velocity_mode = false;
        self.move_start = Some(Instant::now());
        Ok(())
    }

    fn move_relative(
        &mut self,
        _user: &AsynUser,
        distance: f64,
        _min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        let target = self.position + distance;
        if self.deferred {
            // Honour deferred mode like move_absolute — queue the resolved
            // absolute target instead of moving immediately.
            self.deferred_moves.push((target, velocity, acceleration));
            return Ok(());
        }
        self.target = target;
        self.velocity = velocity.abs().max(0.001);
        self.start_position = self.position;
        self.moving = true;
        self.velocity_mode = false;
        self.move_start = Some(Instant::now());
        Ok(())
    }

    fn home(
        &mut self,
        _user: &AsynUser,
        _min_velocity: f64,
        velocity: f64,
        _acceleration: f64,
        forward: bool,
    ) -> AsynResult<()> {
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

    fn set_pid_gain(&mut self, _user: &AsynUser, kind: PidGainKind, gain: f64) -> AsynResult<()> {
        self.pid_gains.push((kind, gain));
        Ok(())
    }

    fn set_high_limit(&mut self, _user: &AsynUser, position: f64) -> AsynResult<()> {
        self.forwarded_high_limit = Some(position);
        Ok(())
    }

    fn set_low_limit(&mut self, _user: &AsynUser, position: f64) -> AsynResult<()> {
        self.forwarded_low_limit = Some(position);
        Ok(())
    }

    fn set_deferred_moves(&mut self, _user: &AsynUser, defer: bool) -> AsynResult<()> {
        if defer {
            self.deferred = true;
        } else {
            self.deferred = false;
            // Execute queued moves: use the last one's target position
            if let Some(&(position, velocity, _acceleration)) = self.deferred_moves.last() {
                self.target = position;
                self.velocity = velocity.abs().max(0.001);
                self.start_position = self.position;
                self.moving = true;
                self.velocity_mode = false;
                self.move_start = Some(Instant::now());
            }
            self.deferred_moves.clear();
        }
        Ok(())
    }

    fn set_position(&mut self, _user: &AsynUser, position: f64) -> AsynResult<()> {
        self.position = position;
        self.encoder_position = position;
        Ok(())
    }

    fn move_velocity(
        &mut self,
        _user: &AsynUser,
        _min_velocity: f64,
        velocity: f64,
        _acceleration: f64,
    ) -> AsynResult<()> {
        self.velocity = velocity.abs().max(0.001);
        self.velocity_direction = velocity >= 0.0;
        self.velocity_mode = true;
        self.start_position = self.position;
        self.moving = true;
        self.move_start = Some(Instant::now());
        Ok(())
    }

    fn initialize_profile(&mut self, _user: &AsynUser, max_points: usize) -> AsynResult<()> {
        self.profile_positions = Vec::with_capacity(max_points);
        self.profile_readbacks = Vec::with_capacity(max_points);
        Ok(())
    }

    fn define_profile(&mut self, _user: &AsynUser, positions: &[f64]) -> AsynResult<()> {
        self.profile_positions = positions.to_vec();
        Ok(())
    }

    fn build_profile(&mut self, _user: &AsynUser) -> AsynResult<()> {
        Ok(()) // SimMotor: no-op build
    }

    fn execute_profile(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // Simulate: just move to the last position
        if let Some(&last) = self.profile_positions.last() {
            self.target = last;
            self.velocity = 1.0;
            self.start_position = self.position;
            self.moving = true;
            self.velocity_mode = false;
            self.move_start = Some(Instant::now());
        }
        self.profile_readbacks = self.profile_positions.clone();
        Ok(())
    }

    fn abort_profile(&mut self, _user: &AsynUser) -> AsynResult<()> {
        self.moving = false;
        self.target = self.position;
        Ok(())
    }

    fn readback_profile(&mut self, _user: &AsynUser) -> AsynResult<Vec<f64>> {
        Ok(self.profile_readbacks.clone())
    }

    fn poll(&mut self, _user: &AsynUser) -> AsynResult<MotorStatus> {
        self.update();
        Ok(MotorStatus {
            position: self.position,
            encoder_position: self.encoder_position,
            velocity: if self.moving { self.velocity } else { 0.0 },
            done: !self.moving,
            moving: self.moving,
            high_limit: self.position >= self.high_limit,
            low_limit: self.position <= self.low_limit,
            home: self.position == 0.0,
            // The sim tracks a real encoder image (encoder_position,
            // set_encoder_position), so it reports the encoder present
            // — otherwise the record demotes UEIP=Yes to No on the
            // first poll (C 3671-3675) and the EA_HOME path is dead.
            // The encoder index asserts at the same physical home spot
            // as the switch.
            has_encoder: true,
            encoder_home: self.encoder_position == 0.0,
            powered: self.powered,
            problem: false,
            direction: self.velocity_direction,
            homed: self.homed,
            ..MotorStatus::default()
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
    fn test_sim_motor_reports_encoder_and_home_index() {
        let user = AsynUser::new(0);
        let mut motor = SimMotor::new();
        let at_home = motor.poll(&user).unwrap();
        assert!(at_home.has_encoder, "sim encoder is present");
        assert!(at_home.encoder_home, "encoder index asserts at home");
        assert!(at_home.home);

        motor.move_absolute(&user, 10.0, 0.0, 10000.0, 1.0).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let away = motor.poll(&user).unwrap();
        assert!(away.has_encoder);
        assert!(!away.encoder_home, "index drops off the home position");
        assert!(!away.home);
    }

    #[test]
    fn test_sim_motor_move_completes() {
        let user = AsynUser::new(0);
        let mut motor = SimMotor::new();
        motor.move_absolute(&user, 10.0, 0.0, 10000.0, 1.0).unwrap(); // very fast
        std::thread::sleep(Duration::from_millis(10));
        let status = motor.poll(&user).unwrap();
        assert!(status.done);
        assert_eq!(status.position, 10.0);
    }

    #[test]
    fn test_sim_motor_stop() {
        let user = AsynUser::new(0);
        let mut motor = SimMotor::new();
        motor.move_absolute(&user, 1000.0, 0.0, 1.0, 1.0).unwrap(); // slow, long move
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
        motor.home(&user, 0.0, 10000.0, 1.0, true).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let status = motor.poll(&user).unwrap();
        assert!(status.done);
        assert_eq!(status.position, 0.0);
    }
}
