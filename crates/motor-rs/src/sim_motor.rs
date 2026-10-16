use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PidGainKind};
use asyn_rs::user::AsynUser;
use std::time::{Duration, Instant};

mod route;

use route::{Demand, Reroute, Route};

/// C `motorSimTask`'s period: it sleeps DELTA (0.1 s) after each step's
/// work and passes the measured interval to `process`
/// (motorSimDriver.cpp:255-270), 0.10018 s on the reference box. Being
/// over DELTA keeps a step off a route that ends on the DELTA grid, where
/// `routeDemand` would leave a residual velocity and no Done.
const PERIOD: Duration = Duration::from_micros(100_180);

/// Simulated motor: one C motorSim axis (motorSimDriver.cpp). The axis
/// follows `route.c` paths, stepped every 0.1 s as `motorSimTask` steps it,
/// and its status is the parameters the last step set.
pub struct SimMotor {
    route: Route,
    /// `endpoint_`: where the route is heading, physical.
    endpoint: Demand,
    /// `nextpoint_`: where the axis is, physical.
    nextpoint: Demand,
    reroute: Reroute,
    high_limit: f64,
    low_limit: f64,
    /// `home_`, physical.
    home_position: f64,
    /// User minus physical position, moved only by `set_position` (C
    /// motorSimAxis `enc_offset_`).
    offset: f64,
    homing: bool,
    homed: bool,
    /// The controller's `movesDeferred_`.
    defer: bool,
    /// `deferred_position_` while `deferred_move_` is set, user.
    deferred_position: Option<f64>,
    /// The step clock: the last step ran at this instant.
    last_step: Instant,
    /// The axis parameters, as the last step or command wrote them.
    posted: MotorStatus,
    closed_loop_enabled: bool,
    /// MRES/ERES as the record last downloaded it. motorSim ignores it
    /// (asynMotorAxis::setEncoderRatio is a no-op), so the encoder counts
    /// motor steps; in EGU that is the position over this ratio.
    encoder_ratio: f64,
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
        Self::starting_at(0.0, -1000.0, 1000.0)
    }

    /// C motorSimAxis's constructor: at rest at `start`, Amax = Vmax = 1.
    fn starting_at(start: f64, low_limit: f64, high_limit: f64) -> Self {
        let at = Demand {
            t: 0.0,
            p: start,
            v: 0.0,
        };
        let mut motor = Self {
            route: Route::new(at, 1.0, 1.0),
            endpoint: at,
            nextpoint: at,
            reroute: Reroute::Calc,
            high_limit,
            low_limit,
            home_position: 0.0,
            offset: 0.0,
            homing: false,
            homed: false,
            defer: false,
            deferred_position: None,
            last_step: Instant::now(),
            posted: MotorStatus::default(),
            closed_loop_enabled: false,
            encoder_ratio: 1.0,
            profile_positions: Vec::new(),
            profile_readbacks: Vec::new(),
            pid_gains: Vec::new(),
            forwarded_high_limit: None,
            forwarded_low_limit: None,
        };
        motor.post(true);
        motor
    }

    pub fn with_limits(mut self, low: f64, high: f64) -> Self {
        self.low_limit = low;
        self.high_limit = high;
        self.post(self.posted.done);
        self
    }

    pub fn with_position(self, pos: f64) -> Self {
        let mut motor = Self::starting_at(pos, self.low_limit, self.high_limit);
        motor.last_step = self.last_step;
        motor
    }

    /// Run every step `motorSimTask` would have run by `now`, on its own
    /// clock: commands and polls do not move the step instants. Steps at
    /// rest change nothing but the clock, so a long idle is skipped to its
    /// last step.
    fn advance_to(&mut self, now: Instant) {
        let behind = now.saturating_duration_since(self.last_step);
        let steps = (behind.as_nanos() / PERIOD.as_nanos()) as u32;
        if steps == 0 {
            return;
        }
        let delta = PERIOD.as_secs_f64();
        let mut run = steps;
        if self.at_rest() && steps > 1 {
            self.nextpoint.t += delta * f64::from(steps - 1);
            self.route.set_demand(self.nextpoint);
            run = 1;
        }
        for _ in 0..run {
            self.process(delta);
        }
        self.last_step += PERIOD * steps;
    }

    /// Stopped on its endpoint with nothing pending: a step moves only
    /// the clock.
    fn at_rest(&self) -> bool {
        self.reroute == Reroute::Calc
            && !self.homing
            && self.nextpoint.v == 0.0
            && self.endpoint.v == 0.0
            && self.nextpoint.p == self.endpoint.p
            && self.nextpoint.t >= self.endpoint.t
    }

    /// C `motorSimAxis::process` (motorSimDriver.cpp:398-480), less the
    /// post-move delay this driver has no parameter for.
    fn process(&mut self, delta: f64) {
        let lastpos = self.nextpoint.p;
        self.nextpoint.t += delta;
        self.route
            .find(self.reroute, &mut self.endpoint, &mut self.nextpoint);
        self.reroute = Reroute::Calc;

        let home = self.home_position;
        if self.homing && (lastpos - home) * (self.nextpoint.p - home) <= 0.0 {
            // Crossed the home switch: return to it.
            self.homing = false;
            self.homed = true;
            self.reroute = Reroute::New;
            self.endpoint.p = home;
            self.endpoint.v = 0.0;
        }
        let (p, v) = (self.nextpoint.p, self.nextpoint.v);
        if (p > self.high_limit && v > 0.0) || (p < self.low_limit && v < 0.0) {
            if self.homing {
                let _ = self.set_velocity(-self.endpoint.v, 0.0);
            } else {
                self.reroute = Reroute::New;
                self.endpoint.p = if v > 0.0 {
                    self.high_limit
                } else {
                    self.low_limit
                };
                self.endpoint.v = 0.0;
            }
        }
        let done = self.nextpoint.v == 0.0 && self.deferred_position.is_none();
        self.post(done);
    }

    /// The parameters a step writes (motorSimDriver.cpp:470-478).
    fn post(&mut self, done: bool) {
        let p = self.nextpoint.p;
        self.posted = MotorStatus {
            position: p + self.offset,
            encoder_position: (p + self.offset) / self.encoder_ratio,
            velocity: self.nextpoint.v,
            direction: self.nextpoint.v > 0.0,
            done,
            high_limit: p >= self.high_limit,
            // motorStatusHome_ is EA_HOME (bit 7); AtHome and HasEncoder
            // are never set.
            encoder_home: p == self.home_position,
            homed: self.homed,
            moving: !done,
            low_limit: p <= self.low_limit,
            ..MotorStatus::default()
        };
    }

    /// C `motorSimAxis::setVelocity` (motorSimDriver.cpp:316-339): a new
    /// endpoint the ramp from the current velocity reaches, with no end.
    fn set_velocity(&mut self, velocity: f64, acceleration: f64) -> AsynResult<()> {
        let p = self.nextpoint.p;
        if (p > self.high_limit && velocity > 0.0) || (p < self.low_limit && velocity < 0.0) {
            return Err(hard_limit());
        }
        let (mut amax, vmax) = self.route.params();
        if acceleration != 0.0 {
            amax = acceleration.abs();
        }
        // A refused routeSetParams keeps the old Amax; the ramp time still
        // uses the asked-for one, as C's local `pars` does.
        self.route.set_params(amax, vmax);
        let delta_v = velocity - self.nextpoint.v;
        let time = (delta_v / amax).abs();
        self.endpoint.v = velocity;
        self.endpoint.p = p + time * (self.nextpoint.v + 0.5 * delta_v);
        self.reroute = Reroute::New;
        Ok(())
    }

    /// C `motorSimAxis::move` (motorSimDriver.cpp:280-314); `position` is
    /// user, relative already resolved.
    fn start_move(&mut self, position: f64, velocity: f64, acceleration: f64) -> AsynResult<()> {
        let p = self.nextpoint.p;
        if (p >= self.high_limit && position > p) || (p <= self.low_limit && position < p) {
            return Err(hard_limit());
        }
        if self.defer {
            self.deferred_position = Some(position);
        } else {
            self.endpoint.p = position - self.offset;
            self.endpoint.v = 0.0;
        }
        let (mut amax, mut vmax) = self.route.params();
        if velocity != 0.0 {
            vmax = velocity.abs();
        }
        if acceleration != 0.0 {
            amax = acceleration.abs();
        }
        self.route.set_params(amax, vmax);
        Ok(())
    }

    /// `asynMotorController::writeFloat64` clears Done after a move-class
    /// call whatever it returned.
    fn moved(&mut self, status: AsynResult<()>) -> AsynResult<()> {
        self.posted.done = false;
        status
    }
}

fn hard_limit() -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: String::new(),
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
        self.advance_to(Instant::now());
        let status = self.start_move(position, velocity, acceleration);
        self.moved(status)
    }

    fn move_relative(
        &mut self,
        _user: &AsynUser,
        distance: f64,
        _min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        self.advance_to(Instant::now());
        let position = distance + self.endpoint.p + self.offset;
        let status = self.start_move(position, velocity, acceleration);
        self.moved(status)
    }

    /// C `motorSimAxis::home`: run toward the home switch until a step
    /// crosses it.
    fn home(
        &mut self,
        _user: &AsynUser,
        _min_velocity: f64,
        velocity: f64,
        acceleration: f64,
        forward: bool,
    ) -> AsynResult<()> {
        self.advance_to(Instant::now());
        let status = self.set_velocity(if forward { velocity } else { -velocity }, acceleration);
        self.homing = true;
        self.homed = false;
        self.moved(status)
    }

    fn stop(&mut self, _user: &AsynUser, acceleration: f64) -> AsynResult<()> {
        self.advance_to(Instant::now());
        let _ = self.set_velocity(0.0, acceleration);
        self.deferred_position = None;
        Ok(())
    }

    fn set_encoder_ratio(&mut self, _user: &AsynUser, ratio: f64) -> AsynResult<()> {
        if ratio.is_finite() && ratio != 0.0 {
            self.encoder_ratio = ratio;
            self.post(self.posted.done);
        }
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

    /// C `motorSimController::writeInt32` for `motorDeferMoves_` and
    /// `processDeferredMoves` (motorSimDriver.cpp:155-212).
    fn set_deferred_moves(&mut self, _user: &AsynUser, defer: bool) -> AsynResult<()> {
        self.advance_to(Instant::now());
        let mut status = Ok(());
        if !defer && self.defer {
            if let Some(position) = self.deferred_position {
                let p = self.nextpoint.p;
                if (p >= self.high_limit && position > p) || (p <= self.low_limit && position < p) {
                    status = Err(hard_limit());
                } else {
                    self.endpoint.p = position - self.offset;
                    self.endpoint.v = 0.0;
                    self.posted.done = false;
                    self.deferred_position = None;
                }
            }
        }
        self.defer = defer;
        status
    }

    fn set_position(&mut self, _user: &AsynUser, position: f64) -> AsynResult<()> {
        self.advance_to(Instant::now());
        self.offset = position - self.nextpoint.p;
        // asynMotorController::writeFloat64 stores the parameter first.
        self.posted.position = position;
        Ok(())
    }

    fn move_velocity(
        &mut self,
        _user: &AsynUser,
        _min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        self.advance_to(Instant::now());
        let status = self.set_velocity(velocity, acceleration);
        self.moved(status)
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
        self.advance_to(Instant::now());
        if let Some(&last) = self.profile_positions.last() {
            self.endpoint.p = last - self.offset;
            self.endpoint.v = 0.0;
            self.posted.done = false;
        }
        self.profile_readbacks = self.profile_positions.clone();
        Ok(())
    }

    fn abort_profile(&mut self, _user: &AsynUser) -> AsynResult<()> {
        self.advance_to(Instant::now());
        let _ = self.set_velocity(0.0, 0.0);
        Ok(())
    }

    fn readback_profile(&mut self, _user: &AsynUser) -> AsynResult<Vec<f64>> {
        Ok(self.profile_readbacks.clone())
    }

    /// C `motorSimAxis::poll` does nothing: the status is what the steps
    /// posted.
    fn poll(&mut self, _user: &AsynUser) -> AsynResult<MotorStatus> {
        self.advance_to(Instant::now());
        Ok(self.posted.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `motor` `n` steps of `motorSimTask` on.
    fn step(motor: &mut SimMotor, n: u32) -> MotorStatus {
        let now = motor.last_step + PERIOD * n;
        motor.advance_to(now);
        motor.posted.clone()
    }

    fn user() -> AsynUser {
        AsynUser::new(0)
    }

    #[test]
    fn test_sim_motor_initial_state() {
        let mut motor = SimMotor::new();
        let status = motor.poll(&user()).unwrap();
        assert_eq!(status.position, 0.0);
        assert!(status.done);
        assert!(!status.moving);
        // motorSimDriver.cpp's poll never sets motorStatusPowerOn_: MSTA 2.
        assert!(!status.powered);
    }

    #[test]
    fn test_sim_motor_reports_home_on_encoder_home_bit_only() {
        let mut motor = SimMotor::new();
        let at_home = motor.poll(&user()).unwrap();
        assert!(at_home.encoder_home);
        assert!(!at_home.home && !at_home.has_encoder && !at_home.direction);

        motor
            .move_absolute(&user(), 10.0, 0.0, 10.0, 100.0)
            .unwrap();
        let away = step(&mut motor, 1);
        assert!(!away.encoder_home && !away.home);
    }

    /// Where a 5 EGU/s² ramp from rest is after one step.
    fn ramp_after_one_step() -> f64 {
        let t = PERIOD.as_secs_f64();
        0.5 * 5.0 * t * t
    }

    /// A unit move at VELO 1, ACCL 0.2 (5 EGU/s²) is the 1.2 s trapezoid:
    /// done on the twelfth step, accelerating on the first.
    #[test]
    fn test_sim_motor_move_follows_the_ramp() {
        let mut motor = SimMotor::new();
        motor.move_absolute(&user(), 1.0, 0.0, 1.0, 5.0).unwrap();
        assert!(!motor.posted.done, "the move clears Done at once");
        let first = step(&mut motor, 1);
        assert!(
            (first.position - ramp_after_one_step()).abs() < 1e-12,
            "{first:?}"
        );
        assert!(first.direction && first.moving);
        let before_end = step(&mut motor, 10);
        assert!(!before_end.done, "{before_end:?}");
        let end = step(&mut motor, 1);
        assert!(end.done && !end.moving, "{end:?}");
        assert!((end.position - 1.0).abs() < 1e-12, "{end:?}");
    }

    #[test]
    fn test_sim_motor_stop_ramps_down() {
        let mut motor = SimMotor::new();
        motor.move_absolute(&user(), 1000.0, 0.0, 1.0, 5.0).unwrap();
        step(&mut motor, 5);
        motor.stop(&user(), 5.0).unwrap();
        let ramping = step(&mut motor, 1);
        assert!(!ramping.done, "{ramping:?}");
        let stopped = step(&mut motor, 2);
        assert!(stopped.done, "{stopped:?}");
        // 0.2 s up (0.1), 0.3 s at 1 (0.3), 0.2 s down (0.1), less the
        // steps' overrun of 0.1 s.
        assert!((stopped.position - 0.5).abs() < 2e-3, "{stopped:?}");
    }

    #[test]
    fn test_sim_motor_set_position() {
        let mut motor = SimMotor::new();
        motor.set_position(&user(), 42.0).unwrap();
        let status = motor.poll(&user()).unwrap();
        assert_eq!(status.position, 42.0);
        // The switches stay physical, as motorSimAxis::setPosition moves
        // only enc_offset_.
        assert!(status.encoder_home);

        motor
            .move_absolute(&user(), 52.0, 0.0, 100.0, 1000.0)
            .unwrap();
        let status = step(&mut motor, 3);
        assert_eq!(status.position, 52.0);
        assert!(!status.encoder_home);
    }

    /// C home: run at HVEL until a step crosses the switch, then route back
    /// to it.
    #[test]
    fn test_sim_motor_home_crosses_the_switch_and_returns() {
        let mut motor = SimMotor::new().with_position(1.0);
        motor.home(&user(), 0.0, 1.0, 5.0, false).unwrap();
        let mut status = step(&mut motor, 1);
        let mut steps = 1;
        while !status.done {
            status = step(&mut motor, 1);
            steps += 1;
            assert!(steps < 100, "never homed: {status:?}");
        }
        assert_eq!(status.position, 0.0);
        assert!(status.encoder_home && status.homed);
    }

    /// A forward home with the switch behind runs into the hard limit and
    /// reverses.
    #[test]
    fn test_sim_motor_home_reverses_at_the_limit() {
        let mut motor = SimMotor::new().with_limits(-2.0, 2.0).with_position(1.0);
        motor.home(&user(), 0.0, 1.0, 5.0, true).unwrap();
        let mut status = step(&mut motor, 1);
        let mut peak = status.position;
        for _ in 0..200 {
            if status.done {
                break;
            }
            status = step(&mut motor, 1);
            peak = peak.max(status.position);
        }
        assert!(status.done && status.homed, "{status:?}");
        assert_eq!(status.position, 0.0);
        assert!(peak > 2.0, "went past the limit before reversing: {peak}");
    }

    /// A long idle is skipped, not stepped, and leaves the axis where it was.
    #[test]
    fn test_sim_motor_idle_is_skipped() {
        let mut motor = SimMotor::new().with_position(3.0);
        let status = step(&mut motor, 1_000_000);
        assert_eq!(status.position, 3.0);
        motor.move_absolute(&user(), 4.0, 0.0, 1.0, 5.0).unwrap();
        let first = step(&mut motor, 1);
        assert!(
            (first.position - 3.0 - ramp_after_one_step()).abs() < 1e-9,
            "{first:?}"
        );
    }
}
