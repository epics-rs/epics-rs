//! Coarse + fine flexure stage combined motion state machine.
//!
//! Pure Rust port of `flexCombinedMotion.st` — coordinates a coarse motor
//! (e.g. New Focus picomotor) with a fine actuator (e.g. piezo) to achieve
//! precise positioning with deadband retry logic.

/// Operating mode for the combined motion system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexMode {
    /// Normal mode: coarse + fine combined. Re-enter calcDistance after coarse move.
    Normal = 0,
    /// Standard mode: coarse + fine with retry on coarse.
    Standard = 1,
    /// Setup mode: coarse only, piezo stays at home.
    Setup = 2,
    /// Fine-only mode: only the fine motor moves, restricted to limits.
    FineOnly = 3,
}

impl From<i32> for FlexMode {
    fn from(v: i32) -> Self {
        match v {
            0 => FlexMode::Normal,
            1 => FlexMode::Standard,
            2 => FlexMode::Setup,
            3 => FlexMode::FineOnly,
            _ => FlexMode::Normal,
        }
    }
}

/// State of the flex combined motion state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexState {
    Init,
    Idle,
    CheckSetPoint,
    CalcDistance,
    HomeFine,
    MoveCoarse,
    WaitCoarse,
    AfterMove,
    MaybeRetry,
    MoveFine,
    ResetBusy,
    Abort,
    ResetStop,
}

/// Configuration for the flex combined motion system.
#[derive(Debug, Clone)]
pub struct FlexConfig {
    /// Prefix for the virtual motor PVs.
    pub prefix: String,
    /// Virtual motor name (e.g. "m1").
    pub motor: String,
    /// Capacitance sensor PV prefix (e.g. "cap1").
    pub cap: String,
    /// Fine motor PV prefix (e.g. "pi:c0:m1").
    pub fine_motor: String,
    /// Coarse motor PV prefix (e.g. "nf:c0:m1").
    pub coarse_motor: String,
}

impl FlexConfig {
    pub fn new(p: &str, m: &str, c: &str, fm: &str, cm: &str) -> Self {
        Self {
            prefix: p.to_string(),
            motor: m.to_string(),
            cap: c.to_string(),
            fine_motor: fm.to_string(),
            coarse_motor: cm.to_string(),
        }
    }

    pub fn set_point_pv(&self) -> String {
        format!("{}{}:setPoint", self.prefix, self.motor)
    }

    pub fn mode_pv(&self) -> String {
        format!("{}{}:mode", self.prefix, self.motor)
    }

    pub fn deadband_pv(&self) -> String {
        format!("{}{}:deadband", self.prefix, self.motor)
    }

    pub fn retries_pv(&self) -> String {
        format!("{}{}:retries", self.prefix, self.motor)
    }

    pub fn max_retries_pv(&self) -> String {
        format!("{}{}:maxRetries", self.prefix, self.motor)
    }

    pub fn pos_monitor_pv(&self) -> String {
        format!("{}{}:pos", self.prefix, self.cap)
    }

    pub fn fine_val_pv(&self) -> String {
        format!("{}{}.VAL", self.prefix, self.fine_motor)
    }

    pub fn fine_rbv_pv(&self) -> String {
        format!("{}{}.RBV", self.prefix, self.fine_motor)
    }

    pub fn coarse_rlv_pv(&self) -> String {
        format!("{}{}.RLV", self.prefix, self.coarse_motor)
    }

    pub fn coarse_rbv_pv(&self) -> String {
        format!("{}{}.RBV", self.prefix, self.coarse_motor)
    }

    pub fn coarse_mres_pv(&self) -> String {
        format!("{}{}.MRES", self.prefix, self.coarse_motor)
    }

    pub fn coarse_dmov_pv(&self) -> String {
        format!("{}{}.DMOV", self.prefix, self.coarse_motor)
    }

    pub fn coarse_stop_pv(&self) -> String {
        format!("{}{}.STOP", self.prefix, self.coarse_motor)
    }

    pub fn upper_limit_pv(&self) -> String {
        format!("{}{}:upperLimit", self.prefix, self.motor)
    }

    pub fn lower_limit_pv(&self) -> String {
        format!("{}{}:lowerLimit", self.prefix, self.motor)
    }

    pub fn home_pos_pv(&self) -> String {
        format!("{}{}:homePos", self.prefix, self.motor)
    }

    pub fn stop_pv(&self) -> String {
        format!("{}{}:stop", self.prefix, self.motor)
    }

    pub fn busy_pv(&self) -> String {
        format!("{}{}:busy", self.prefix, self.motor)
    }
}

/// Pure calculation result for a single step of the combined motion.
#[derive(Debug, Clone)]
pub struct FlexCalcResult {
    /// Requested fine position.
    pub dist_calc: f64,
    /// Coarse displacement to command.
    pub coarse_disp: f64,
    /// Fine position to command.
    pub fine_pos: f64,
    /// Fine home position (clamped to limits).
    pub fine_home: f64,
}

/// Calculate the distance the system needs to move.
///
/// `new_set_point` - desired position
/// `pos_monitor` - current capacitance-sensor position
/// `fine_rbv` - current fine motor readback
pub fn calc_distance(new_set_point: f64, pos_monitor: f64, fine_rbv: f64) -> f64 {
    new_set_point - pos_monitor + fine_rbv
}

/// Clamp a value to the fine motor limits.
pub fn clamp_to_limits(value: f64, lower: f64, upper: f64) -> f64 {
    value.clamp(lower, upper)
}

/// Calculate the fine home position, clamped to limits.
pub fn calc_fine_home(home_pos: f64, lower_limit: f64, upper_limit: f64) -> f64 {
    clamp_to_limits(home_pos, lower_limit, upper_limit)
}

/// Calculate the coarse displacement after homing the fine motor.
pub fn calc_coarse_disp(dist_calc: f64, fine_home: f64) -> f64 {
    dist_calc - fine_home
}

/// Determine the effective deadband (at least coarse motor resolution).
pub fn effective_deadband(deadband: f64, coarse_mres: f64) -> f64 {
    if deadband < coarse_mres {
        coarse_mres
    } else {
        deadband
    }
}

/// Check whether a retry is needed based on position error and deadband.
pub fn needs_retry(pos_error: f64, deadband: f64, coarse_mres: f64) -> bool {
    pos_error.abs() > effective_deadband(deadband, coarse_mres)
}

/// Main flex combined motion controller — pure logic.
#[derive(Debug, Clone)]
pub struct FlexController {
    pub state: FlexState,
    pub mode: FlexMode,
    pub new_set_point: f64,
    pub last_set_point: f64,
    pub pos_monitor: f64,
    pub fine_rbv: f64,
    pub fine_home: f64,
    pub dist_calc: f64,
    pub coarse_disp: f64,
    pub fine_pos: f64,
    pub deadband: f64,
    pub coarse_mres: f64,
    pub max_retries: i32,
    pub num_retries: i32,
    pub upper_limit: f64,
    pub lower_limit: f64,
    pub home_pos: f64,
    pub busy: bool,
}

impl Default for FlexController {
    fn default() -> Self {
        Self {
            state: FlexState::Init,
            mode: FlexMode::Normal,
            new_set_point: 0.0,
            last_set_point: -314159.26, // Unlikely sentinel value
            pos_monitor: 0.0,
            fine_rbv: 0.0,
            fine_home: 0.0,
            dist_calc: 0.0,
            coarse_disp: 0.0,
            fine_pos: 0.0,
            deadband: 0.001,
            coarse_mres: 0.001,
            max_retries: 5,
            num_retries: 0,
            upper_limit: 100.0,
            lower_limit: -100.0,
            home_pos: 0.0,
            busy: false,
        }
    }
}

/// Events driving the flex state machine.
#[derive(Debug, Clone)]
pub enum FlexEvent {
    /// New setpoint received.
    SetPointChanged(f64),
    /// Position monitor updated.
    PosMonitorChanged(f64),
    /// Fine motor readback updated.
    FineRBVChanged(f64),
    /// Coarse motor done moving.
    CoarseDone,
    /// Stop requested.
    Stop,
    /// Mode changed.
    ModeChanged(FlexMode),
    /// Deadband changed.
    DeadbandChanged(f64),
    /// Coarse motor resolution changed.
    CoarseMresChanged(f64),
    /// Limits changed.
    LimitsChanged { upper: f64, lower: f64 },
    /// Home position changed.
    HomePosChanged(f64),
    /// Max retries changed.
    MaxRetriesChanged(i32),
}

/// Actions the caller should take.
#[derive(Debug, Clone, Default)]
pub struct FlexActions {
    /// Command fine motor to this position.
    pub move_fine: Option<f64>,
    /// Command coarse motor relative displacement.
    pub move_coarse_rel: Option<f64>,
    /// Stop the coarse motor.
    pub stop_coarse: bool,
    /// Set busy flag.
    pub set_busy: Option<bool>,
    /// Write retries count.
    pub write_retries: Option<i32>,
    /// Reset stop PV.
    pub reset_stop: bool,
}

impl FlexController {
    /// Process a setpoint change through the full state machine logic.
    /// Returns the actions to take.
    pub fn process_setpoint(&mut self) -> FlexActions {
        let mut actions = FlexActions::default();

        // Set busy
        self.busy = true;
        actions.set_busy = Some(true);
        self.num_retries = 0;
        actions.write_retries = Some(0);

        // CalcDistance
        self.dist_calc = calc_distance(self.new_set_point, self.pos_monitor, self.fine_rbv);

        match self.mode {
            FlexMode::FineOnly => {
                // Fine-only: clamp to limits
                self.dist_calc = clamp_to_limits(self.dist_calc, self.lower_limit, self.upper_limit);
                self.fine_pos = self.dist_calc;
                actions.move_fine = Some(self.fine_pos);
                // Done
                self.busy = false;
                actions.set_busy = Some(false);
            }

            FlexMode::Setup => {
                // Setup: home fine, move coarse, done
                self.fine_home = calc_fine_home(self.home_pos, self.lower_limit, self.upper_limit);
                actions.move_fine = Some(self.fine_home);
                self.coarse_disp = calc_coarse_disp(self.dist_calc, self.fine_home);
                actions.move_coarse_rel = Some(self.coarse_disp);
                self.state = FlexState::WaitCoarse;
            }

            FlexMode::Normal | FlexMode::Standard => {
                if self.dist_calc >= self.upper_limit || self.dist_calc <= self.lower_limit {
                    // Out of fine range: home fine, move coarse
                    self.fine_home =
                        calc_fine_home(self.home_pos, self.lower_limit, self.upper_limit);
                    actions.move_fine = Some(self.fine_home);
                    self.coarse_disp = calc_coarse_disp(self.dist_calc, self.fine_home);
                    actions.move_coarse_rel = Some(self.coarse_disp);
                    self.state = FlexState::WaitCoarse;
                } else {
                    // Within fine range: just move fine
                    self.fine_pos = self.dist_calc;
                    actions.move_fine = Some(self.fine_pos);
                    self.busy = false;
                    actions.set_busy = Some(false);
                }
            }
        }

        actions
    }

    /// Handle coarse motor completion.
    pub fn handle_coarse_done(&mut self) -> FlexActions {
        let mut actions = FlexActions::default();

        match self.mode {
            FlexMode::Normal => {
                // Re-enter calcDistance in case long move didn't reach target
                self.dist_calc =
                    calc_distance(self.new_set_point, self.pos_monitor, self.fine_rbv);

                if self.dist_calc >= self.upper_limit || self.dist_calc <= self.lower_limit {
                    self.fine_home =
                        calc_fine_home(self.home_pos, self.lower_limit, self.upper_limit);
                    actions.move_fine = Some(self.fine_home);
                    self.coarse_disp = calc_coarse_disp(self.dist_calc, self.fine_home);
                    actions.move_coarse_rel = Some(self.coarse_disp);
                    self.state = FlexState::WaitCoarse;
                } else {
                    self.fine_pos = self.new_set_point - self.pos_monitor + self.fine_rbv;
                    actions.move_fine = Some(self.fine_pos);
                    self.busy = false;
                    actions.set_busy = Some(false);
                    self.state = FlexState::Idle;
                }
            }

            FlexMode::Standard => {
                // Check if retry is needed
                let pos_error = self.new_set_point - self.pos_monitor;
                let act_db = effective_deadband(self.deadband, self.coarse_mres);

                if self.num_retries >= self.max_retries {
                    // Give up, move fine to final position
                    self.fine_pos = self.new_set_point - self.pos_monitor + self.fine_rbv;
                    actions.move_fine = Some(self.fine_pos);
                    self.busy = false;
                    actions.set_busy = Some(false);
                    self.state = FlexState::Idle;
                } else if pos_error.abs() > act_db {
                    // Retry: move coarse again
                    self.dist_calc = pos_error + self.fine_home;
                    self.num_retries += 1;
                    actions.write_retries = Some(self.num_retries);
                    self.coarse_disp = calc_coarse_disp(self.dist_calc, self.fine_home);
                    actions.move_coarse_rel = Some(self.coarse_disp);
                    self.state = FlexState::WaitCoarse;
                } else {
                    // Within deadband, do final fine move
                    self.dist_calc =
                        calc_distance(self.new_set_point, self.pos_monitor, self.fine_rbv);
                    if self.dist_calc >= self.upper_limit || self.dist_calc <= self.lower_limit {
                        // Still out of range, home fine
                        self.fine_home =
                            calc_fine_home(self.home_pos, self.lower_limit, self.upper_limit);
                        actions.move_fine = Some(self.fine_home);
                        self.coarse_disp = calc_coarse_disp(self.dist_calc, self.fine_home);
                        actions.move_coarse_rel = Some(self.coarse_disp);
                        self.state = FlexState::WaitCoarse;
                    } else {
                        self.fine_pos = self.dist_calc;
                        actions.move_fine = Some(self.fine_pos);
                        self.busy = false;
                        actions.set_busy = Some(false);
                        self.state = FlexState::Idle;
                    }
                }
            }

            FlexMode::Setup => {
                // In setup mode, check retry
                let pos_error = self.new_set_point - self.pos_monitor;
                let act_db = effective_deadband(self.deadband, self.coarse_mres);

                if self.num_retries >= self.max_retries || pos_error.abs() <= act_db {
                    self.busy = false;
                    actions.set_busy = Some(false);
                    self.state = FlexState::Idle;
                } else {
                    self.dist_calc = pos_error + self.fine_home;
                    self.num_retries += 1;
                    actions.write_retries = Some(self.num_retries);
                    self.coarse_disp = calc_coarse_disp(self.dist_calc, self.fine_home);
                    actions.move_coarse_rel = Some(self.coarse_disp);
                    self.state = FlexState::WaitCoarse;
                }
            }

            FlexMode::FineOnly => {
                // Should not happen, but handle gracefully
                self.busy = false;
                actions.set_busy = Some(false);
                self.state = FlexState::Idle;
            }
        }

        actions
    }

    /// Handle stop request.
    pub fn handle_stop(&mut self) -> FlexActions {
        let mut actions = FlexActions { stop_coarse: true, reset_stop: true, ..Default::default() };
        self.busy = false;
        actions.set_busy = Some(false);
        self.state = FlexState::Idle;
        actions
    }

    /// Process an event and return actions.
    pub fn step(&mut self, event: FlexEvent) -> FlexActions {
        match event {
            FlexEvent::SetPointChanged(sp) => {
                self.new_set_point = sp;
                self.last_set_point = sp;
                self.process_setpoint()
            }

            FlexEvent::PosMonitorChanged(pos) => {
                self.pos_monitor = pos;
                FlexActions::default()
            }

            FlexEvent::FineRBVChanged(rbv) => {
                self.fine_rbv = rbv;
                FlexActions::default()
            }

            FlexEvent::CoarseDone => self.handle_coarse_done(),

            FlexEvent::Stop => self.handle_stop(),

            FlexEvent::ModeChanged(mode) => {
                self.mode = mode;
                FlexActions::default()
            }

            FlexEvent::DeadbandChanged(db) => {
                self.deadband = db;
                FlexActions::default()
            }

            FlexEvent::CoarseMresChanged(mres) => {
                self.coarse_mres = mres;
                FlexActions::default()
            }

            FlexEvent::LimitsChanged { upper, lower } => {
                self.upper_limit = upper;
                self.lower_limit = lower;
                FlexActions::default()
            }

            FlexEvent::HomePosChanged(hp) => {
                self.home_pos = hp;
                FlexActions::default()
            }

            FlexEvent::MaxRetriesChanged(mr) => {
                self.max_retries = mr;
                FlexActions::default()
            }
        }
    }
}

/// Async entry point — runs the flex combined motion state machine against live PVs.
pub async fn run(config: FlexConfig) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use epics_base_rs::types::EpicsValue;
    use epics_ca_rs::client::{CaChannel, CaClient};
    use tokio::select;
    use tokio::time::{sleep, Duration};

    async fn get_f64(ch: &CaChannel) -> f64 {
        match ch.get().await {
            Ok((_, val)) => val.to_f64().unwrap_or(0.0),
            Err(_) => 0.0,
        }
    }
    async fn get_i32(ch: &CaChannel) -> i32 {
        match ch.get().await {
            Ok((_, val)) => val.to_f64().unwrap_or(0.0) as i32,
            Err(_) => 0,
        }
    }
    async fn put_f64(ch: &CaChannel, v: f64) {
        let _ = ch.put(&EpicsValue::Double(v)).await;
    }
    async fn put_i32(ch: &CaChannel, v: i32) {
        let _ = ch.put(&EpicsValue::Long(v)).await;
    }

    let client = CaClient::new().await?;

    // Connect PVs
    let ch_set_point = client.create_channel(&config.set_point_pv());
    let ch_mode = client.create_channel(&config.mode_pv());
    let ch_deadband = client.create_channel(&config.deadband_pv());
    let ch_retries = client.create_channel(&config.retries_pv());
    let ch_max_retries = client.create_channel(&config.max_retries_pv());
    let ch_pos_monitor = client.create_channel(&config.pos_monitor_pv());
    let ch_fine_val = client.create_channel(&config.fine_val_pv());
    let ch_fine_rbv = client.create_channel(&config.fine_rbv_pv());
    let ch_coarse_rlv = client.create_channel(&config.coarse_rlv_pv());
    let ch_coarse_mres = client.create_channel(&config.coarse_mres_pv());
    let ch_coarse_dmov = client.create_channel(&config.coarse_dmov_pv());
    let ch_coarse_stop = client.create_channel(&config.coarse_stop_pv());
    let ch_upper_limit = client.create_channel(&config.upper_limit_pv());
    let ch_lower_limit = client.create_channel(&config.lower_limit_pv());
    let ch_home_pos = client.create_channel(&config.home_pos_pv());
    let ch_stop = client.create_channel(&config.stop_pv());
    let ch_busy = client.create_channel(&config.busy_pv());

    // Subscriptions
    let mut sub_set_point = ch_set_point.subscribe().await?;
    let mut sub_pos_monitor = ch_pos_monitor.subscribe().await?;
    let mut sub_fine_rbv = ch_fine_rbv.subscribe().await?;
    let mut sub_coarse_dmov = ch_coarse_dmov.subscribe().await?;
    let mut sub_stop = ch_stop.subscribe().await?;
    let mut sub_mode = ch_mode.subscribe().await?;

    // Initialize controller
    #[allow(clippy::field_reassign_with_default)]
    let mut ctrl = {
        let mut ctrl = FlexController::default();
        ctrl.mode = FlexMode::from(get_i32(&ch_mode).await);
        ctrl.deadband = { let v = get_f64(&ch_deadband).await; if v > 0.0 { v } else { 0.001 } };
        ctrl.coarse_mres = { let v = get_f64(&ch_coarse_mres).await; if v > 0.0 { v } else { 0.001 } };
        ctrl.max_retries = { let v = get_i32(&ch_max_retries).await; if v > 0 { v } else { 5 } };
        ctrl.upper_limit = { let v = get_f64(&ch_upper_limit).await; if v != 0.0 { v } else { 100.0 } };
        ctrl.lower_limit = { let v = get_f64(&ch_lower_limit).await; if v != 0.0 { v } else { -100.0 } };
        ctrl.home_pos = get_f64(&ch_home_pos).await;
        ctrl.pos_monitor = get_f64(&ch_pos_monitor).await;
        ctrl.fine_rbv = get_f64(&ch_fine_rbv).await;
        ctrl
    };

    put_i32(&ch_busy, 0).await;

    tracing::info!(
        "flex_combined_motion running for {}{}",
        config.prefix,
        config.motor
    );

    loop {
        let event: Option<FlexEvent> = select! {
            Some(Ok(val)) = sub_set_point.recv() => {
                Some(FlexEvent::SetPointChanged(val.to_f64().unwrap_or(0.0)))
            }
            Some(Ok(val)) = sub_pos_monitor.recv() => {
                Some(FlexEvent::PosMonitorChanged(val.to_f64().unwrap_or(0.0)))
            }
            Some(Ok(val)) = sub_fine_rbv.recv() => {
                Some(FlexEvent::FineRBVChanged(val.to_f64().unwrap_or(0.0)))
            }
            Some(Ok(val)) = sub_coarse_dmov.recv() => {
                let dmov = val.to_f64().unwrap_or(0.0) as i32;
                if dmov == 1 { Some(FlexEvent::CoarseDone) } else { None }
            }
            Some(Ok(val)) = sub_stop.recv() => {
                let v = val.to_f64().unwrap_or(0.0) as i32;
                if v != 0 { Some(FlexEvent::Stop) } else { None }
            }
            Some(Ok(val)) = sub_mode.recv() => {
                let m = val.to_f64().unwrap_or(0.0) as i32;
                Some(FlexEvent::ModeChanged(FlexMode::from(m)))
            }
        };

        if let Some(ev) = event {
            let actions = ctrl.step(ev);

            if let Some(fine) = actions.move_fine {
                put_f64(&ch_fine_val, fine).await;
            }
            if let Some(coarse) = actions.move_coarse_rel {
                put_f64(&ch_coarse_rlv, coarse).await;
            }
            if actions.stop_coarse {
                sleep(Duration::from_millis(150)).await;
                put_i32(&ch_coarse_stop, 1).await;
            }
            if let Some(b) = actions.set_busy {
                put_i32(&ch_busy, if b { 1 } else { 0 }).await;
            }
            if let Some(r) = actions.write_retries {
                put_f64(&ch_retries, r as f64).await;
            }
            if actions.reset_stop {
                put_i32(&ch_stop, 0).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calc_distance() {
        let d = calc_distance(10.0, 5.0, 2.0);
        // 10 - 5 + 2 = 7
        assert!((d - 7.0).abs() < 1e-10);
    }

    #[test]
    fn test_clamp_to_limits() {
        assert_eq!(clamp_to_limits(5.0, -10.0, 10.0), 5.0);
        assert_eq!(clamp_to_limits(15.0, -10.0, 10.0), 10.0);
        assert_eq!(clamp_to_limits(-15.0, -10.0, 10.0), -10.0);
    }

    #[test]
    fn test_calc_fine_home() {
        assert_eq!(calc_fine_home(0.0, -10.0, 10.0), 0.0);
        assert_eq!(calc_fine_home(15.0, -10.0, 10.0), 10.0);
        assert_eq!(calc_fine_home(-15.0, -10.0, 10.0), -10.0);
    }

    #[test]
    fn test_calc_coarse_disp() {
        assert!((calc_coarse_disp(10.0, 2.0) - 8.0).abs() < 1e-10);
    }

    #[test]
    fn test_effective_deadband() {
        assert_eq!(effective_deadband(0.01, 0.001), 0.01);
        assert_eq!(effective_deadband(0.0001, 0.001), 0.001);
    }

    #[test]
    fn test_needs_retry() {
        assert!(needs_retry(0.1, 0.01, 0.001));
        assert!(!needs_retry(0.005, 0.01, 0.001));
        // Below deadband but above mres
        assert!(!needs_retry(0.005, 0.01, 0.001));
        // Below mres
        assert!(!needs_retry(0.0005, 0.0001, 0.001));
    }

    #[test]
    fn test_flex_mode_from_i32() {
        assert_eq!(FlexMode::from(0), FlexMode::Normal);
        assert_eq!(FlexMode::from(1), FlexMode::Standard);
        assert_eq!(FlexMode::from(2), FlexMode::Setup);
        assert_eq!(FlexMode::from(3), FlexMode::FineOnly);
        assert_eq!(FlexMode::from(42), FlexMode::Normal);
    }

    #[test]
    fn test_process_setpoint_fine_only() {
        let mut ctrl = FlexController::default();
        ctrl.mode = FlexMode::FineOnly;
        ctrl.upper_limit = 10.0;
        ctrl.lower_limit = -10.0;
        ctrl.pos_monitor = 5.0;
        ctrl.fine_rbv = 2.0;
        ctrl.new_set_point = 8.0;

        let actions = ctrl.process_setpoint();
        assert!(actions.move_fine.is_some());
        let fine = actions.move_fine.unwrap();
        // dist_calc = 8 - 5 + 2 = 5, which is within limits
        assert!((fine - 5.0).abs() < 1e-10);
        assert_eq!(actions.set_busy, Some(false)); // Completed immediately
    }

    #[test]
    fn test_process_setpoint_fine_only_clamped() {
        let mut ctrl = FlexController::default();
        ctrl.mode = FlexMode::FineOnly;
        ctrl.upper_limit = 3.0;
        ctrl.lower_limit = -3.0;
        ctrl.pos_monitor = 0.0;
        ctrl.fine_rbv = 0.0;
        ctrl.new_set_point = 10.0;

        let actions = ctrl.process_setpoint();
        let fine = actions.move_fine.unwrap();
        assert_eq!(fine, 3.0); // Clamped to upper limit
    }

    #[test]
    fn test_process_setpoint_within_range() {
        let mut ctrl = FlexController::default();
        ctrl.mode = FlexMode::Standard;
        ctrl.upper_limit = 10.0;
        ctrl.lower_limit = -10.0;
        ctrl.pos_monitor = 5.0;
        ctrl.fine_rbv = 2.0;
        ctrl.new_set_point = 8.0;

        let actions = ctrl.process_setpoint();
        // dist_calc = 8 - 5 + 2 = 5, within limits
        assert!(actions.move_fine.is_some());
        assert!(actions.move_coarse_rel.is_none());
        assert_eq!(actions.set_busy, Some(false));
    }

    #[test]
    fn test_process_setpoint_out_of_range() {
        let mut ctrl = FlexController::default();
        ctrl.mode = FlexMode::Standard;
        ctrl.upper_limit = 3.0;
        ctrl.lower_limit = -3.0;
        ctrl.home_pos = 0.0;
        ctrl.pos_monitor = 0.0;
        ctrl.fine_rbv = 0.0;
        ctrl.new_set_point = 100.0;

        let actions = ctrl.process_setpoint();
        // dist_calc = 100, out of [-3, 3] range
        assert!(actions.move_fine.is_some()); // Home fine
        assert!(actions.move_coarse_rel.is_some());
        assert_eq!(ctrl.state, FlexState::WaitCoarse);
    }

    #[test]
    fn test_handle_coarse_done_standard_within_deadband() {
        let mut ctrl = FlexController::default();
        ctrl.mode = FlexMode::Standard;
        ctrl.upper_limit = 10.0;
        ctrl.lower_limit = -10.0;
        ctrl.deadband = 0.1;
        ctrl.coarse_mres = 0.01;
        ctrl.new_set_point = 5.0;
        ctrl.pos_monitor = 4.99; // Within deadband
        ctrl.fine_rbv = 0.0;
        ctrl.fine_home = 0.0;

        let actions = ctrl.handle_coarse_done();
        // Should do final fine move and become idle
        assert!(actions.move_fine.is_some());
        assert_eq!(actions.set_busy, Some(false));
    }

    #[test]
    fn test_handle_coarse_done_standard_retry() {
        let mut ctrl = FlexController::default();
        ctrl.mode = FlexMode::Standard;
        ctrl.upper_limit = 10.0;
        ctrl.lower_limit = -10.0;
        ctrl.deadband = 0.01;
        ctrl.coarse_mres = 0.001;
        ctrl.max_retries = 5;
        ctrl.num_retries = 0;
        ctrl.new_set_point = 5.0;
        ctrl.pos_monitor = 4.0; // 1.0 error > 0.01 deadband
        ctrl.fine_rbv = 0.0;
        ctrl.fine_home = 0.0;

        let actions = ctrl.handle_coarse_done();
        assert!(actions.move_coarse_rel.is_some());
        assert_eq!(ctrl.num_retries, 1);
        assert_eq!(ctrl.state, FlexState::WaitCoarse);
    }

    #[test]
    fn test_handle_coarse_done_max_retries() {
        let mut ctrl = FlexController::default();
        ctrl.mode = FlexMode::Standard;
        ctrl.upper_limit = 10.0;
        ctrl.lower_limit = -10.0;
        ctrl.deadband = 0.01;
        ctrl.max_retries = 3;
        ctrl.num_retries = 3; // Already at max
        ctrl.new_set_point = 5.0;
        ctrl.pos_monitor = 4.0;
        ctrl.fine_rbv = 0.0;

        let actions = ctrl.handle_coarse_done();
        assert!(actions.move_fine.is_some()); // Final fine positioning
        assert_eq!(actions.set_busy, Some(false));
    }

    #[test]
    fn test_handle_stop() {
        let mut ctrl = FlexController::default();
        ctrl.busy = true;

        let actions = ctrl.handle_stop();
        assert!(actions.stop_coarse);
        assert!(actions.reset_stop);
        assert_eq!(actions.set_busy, Some(false));
        assert!(!ctrl.busy);
    }

    #[test]
    fn test_step_setpoint_event() {
        let mut ctrl = FlexController::default();
        ctrl.mode = FlexMode::FineOnly;
        ctrl.upper_limit = 10.0;
        ctrl.lower_limit = -10.0;
        ctrl.pos_monitor = 0.0;
        ctrl.fine_rbv = 0.0;

        let actions = ctrl.step(FlexEvent::SetPointChanged(5.0));
        assert!(actions.move_fine.is_some());
        assert_eq!(ctrl.new_set_point, 5.0);
    }

    #[test]
    fn test_step_pos_monitor_event() {
        let mut ctrl = FlexController::default();
        let actions = ctrl.step(FlexEvent::PosMonitorChanged(3.14));
        assert_eq!(ctrl.pos_monitor, 3.14);
        assert!(actions.move_fine.is_none());
    }

    #[test]
    fn test_step_mode_event() {
        let mut ctrl = FlexController::default();
        let _ = ctrl.step(FlexEvent::ModeChanged(FlexMode::Setup));
        assert_eq!(ctrl.mode, FlexMode::Setup);
    }

    #[test]
    fn test_setup_mode_coarse_only() {
        let mut ctrl = FlexController::default();
        ctrl.mode = FlexMode::Setup;
        ctrl.upper_limit = 10.0;
        ctrl.lower_limit = -10.0;
        ctrl.home_pos = 0.0;
        ctrl.pos_monitor = 0.0;
        ctrl.fine_rbv = 0.0;
        ctrl.new_set_point = 5.0;

        let actions = ctrl.process_setpoint();
        // Setup mode: home fine to home_pos, then coarse
        assert!(actions.move_fine.is_some());
        assert_eq!(actions.move_fine.unwrap(), 0.0); // Home pos
        assert!(actions.move_coarse_rel.is_some());
    }

    #[test]
    fn test_normal_mode_reenter_calc_distance() {
        let mut ctrl = FlexController::default();
        ctrl.mode = FlexMode::Normal;
        ctrl.upper_limit = 3.0;
        ctrl.lower_limit = -3.0;
        ctrl.new_set_point = 5.0;
        ctrl.pos_monitor = 4.8; // After coarse move, nearly there
        ctrl.fine_rbv = 0.0;

        let actions = ctrl.handle_coarse_done();
        // dist_calc = 5.0 - 4.8 + 0 = 0.2, within [-3, 3] now
        assert!(actions.move_fine.is_some());
        assert!((actions.move_fine.unwrap() - 0.2).abs() < 1e-10);
        assert_eq!(actions.set_busy, Some(false));
    }
}
