//! Delay-and-Do state machine — native Rust port of `delayDo.st`.
//!
//! Implements a state machine that waits for a standby condition,
//! monitors an active condition, and after the active condition
//! clears (with a configurable delay), triggers an action.
//!
//! # State Machine
//!
//! ```text
//!   init ──► idle ◄──────────────────────────┐
//!            │  ▲                              │
//!            │  └── maybeStandby ◄── disable  │
//!            ▼                       ▲        │
//!         standby ──► maybeWait ──► waiting ──► action
//!            ▲            │           │
//!            │            ▼           ▼
//!            │          idle       active ──► waiting
//!            └──────────────────────┘
//! ```

use std::time::{Duration, Instant};

/// States of the delay-do state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayDoState {
    Init,
    Disable,
    MaybeStandby,
    Idle,
    Standby,
    MaybeWait,
    Active,
    Waiting,
    Action,
}

impl DelayDoState {
    /// True where the state's `when` clauses are exhaustive, so SNL leaves it
    /// on the same evaluation that entered it rather than waiting for a new
    /// event: `init`'s `pvConnectCount() == pvAssignCount()`
    /// (`delayDo.st:35`), `maybeStandby`'s standby/active/!standby triple
    /// (`:58-74`), `maybeWait`'s active/efTest/!efTest triple (`:121-138`),
    /// and `action`'s bare `when ()` (`:201`). A runner must keep stepping
    /// while this holds.
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            DelayDoState::Init
                | DelayDoState::MaybeStandby
                | DelayDoState::MaybeWait
                | DelayDoState::Action
        )
    }

    /// True where entering the state writes its name to `{P}{R}:state`.
    /// `maybeStandby` and `maybeWait` are the two exceptions, and
    /// deliberately so — "the state doesn't last long enough"
    /// (`delayDo.st:51-52`, `:112-113`) is why their `PVPUTSTR` calls are
    /// commented out upstream.
    pub fn is_published(self) -> bool {
        !matches!(self, DelayDoState::MaybeStandby | DelayDoState::MaybeWait)
    }
}

impl std::fmt::Display for DelayDoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DelayDoState::Init => write!(f, "init"),
            DelayDoState::Disable => write!(f, "disable"),
            DelayDoState::MaybeStandby => write!(f, "maybeStandby"),
            DelayDoState::Idle => write!(f, "idle"),
            DelayDoState::Standby => write!(f, "standby"),
            DelayDoState::MaybeWait => write!(f, "maybeWait"),
            DelayDoState::Active => write!(f, "active"),
            DelayDoState::Waiting => write!(f, "waiting"),
            DelayDoState::Action => write!(f, "action"),
        }
    }
}

/// Input signals for the delay-do state machine.
///
/// Each `_changed` field is a *monitor event*, not a level comparison: SNL's
/// `sync` posts the event flag on every monitor callback for the variable,
/// including one that carries the same value (an alarm-only update) or a fall
/// back to zero. Pass `true` on the step a callback arrived, whatever the new
/// value is; [`DelayDoController::step`] latches it exactly as SNL does.
#[derive(Debug, Clone, Copy)]
pub struct DelayDoInputs {
    /// Enable/disable control
    pub enable: bool,
    /// A monitor event arrived on "enable" this step (SNL `enable_mon`)
    pub enable_changed: bool,
    /// Standby condition
    pub standby: bool,
    /// A monitor event arrived on "standby" this step (SNL `standby_mon`)
    pub standby_changed: bool,
    /// Active condition
    pub active: bool,
    /// A monitor event arrived on "active" this step (SNL `active_mon`)
    pub active_changed: bool,
}

/// An SNL event flag (`evflag`), latched.
///
/// STD-12: `EvFlag(_VAR_)` (`seqPVmacros.h:131-134`) expands to
/// `monitor _VAR_; evflag _VAR_##_mon; sync _VAR_ _VAR_##_mon`, so the flag is
/// raised by *any* monitor event on the variable and stays raised until an
/// `efTestAndClear` or `efClear` evaluates — across state transitions included.
/// Reading a per-step "changed" input directly models an edge instead, which
/// drops every event arriving in a state whose clauses do not test that flag,
/// and keeps a stale one wherever C would have cleared it. Both directions were
/// observable in the ported transition table, so all three flags go through
/// this type rather than the cited one alone.
#[derive(Debug, Clone, Copy, Default)]
struct EventFlag(bool);

impl EventFlag {
    /// SNL `sync`: a monitor callback arrived, whatever value it carried.
    fn sync(&mut self, monitor_event: bool) {
        self.0 |= monitor_event;
    }

    /// SNL `efTest` — read without clearing.
    fn test(self) -> bool {
        self.0
    }

    /// SNL `efTestAndClear` — read and clear. Clears whether or not it was
    /// raised, and only when the clause that names it is actually reached, so
    /// it must stay the left operand of every `&&` that mirrors C's.
    fn test_and_clear(&mut self) -> bool {
        std::mem::replace(&mut self.0, false)
    }

    /// SNL `efClear`.
    fn clear(&mut self) {
        self.0 = false;
    }
}

/// Output actions from the delay-do state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayDoAction {
    /// No action this step.
    None,
    /// Process the action sequence (doSeq).
    ProcessAction,
}

/// The delay-do controller.
pub struct DelayDoController {
    pub state: DelayDoState,
    /// Delay period before triggering the action.
    pub delay_period: Duration,
    /// Whether to resume waiting when re-entering from standby.
    resume_waiting: bool,
    /// SNL `enable_mon` / `standby_mon` / `active_mon`.
    enable_mon: EventFlag,
    standby_mon: EventFlag,
    active_mon: EventFlag,
    /// `waiting`'s armed `delay(delayPeriod)` clause (`delayDo.st:189`):
    /// when the state was entered, and the `delay_period` the clause was
    /// evaluated with AT entry. SNL evaluates a `delay()` argument once, on
    /// entry, so a monitor on `{P}{R}:delay` that lands mid-wait retimes the
    /// NEXT wait and not the one in flight. `Some` exactly in `waiting`.
    wait: Option<(Instant, Duration)>,
}

impl Default for DelayDoController {
    fn default() -> Self {
        Self {
            state: DelayDoState::Init,
            delay_period: Duration::from_secs(0),
            resume_waiting: false,
            enable_mon: EventFlag::default(),
            standby_mon: EventFlag::default(),
            active_mon: EventFlag::default(),
            wait: None,
        }
    }
}

impl DelayDoController {
    pub fn new(delay_secs: f64) -> Self {
        Self {
            delay_period: epics_base_rs::runtime::time::duration_from_secs(delay_secs),
            ..Default::default()
        }
    }

    /// Advance the state machine given current inputs.
    /// Returns the action to take (if any) and the new state.
    pub fn step(&mut self, inputs: &DelayDoInputs) -> (DelayDoAction, DelayDoState) {
        let action;

        // SNL `sync` delivers every monitor event to its flag before any state's
        // clauses run, whatever state the machine is in — that is what makes the
        // flags survive the transient states (`init`, `maybeStandby`,
        // `maybeWait`, `action`) whose clauses name none of them.
        self.enable_mon.sync(inputs.enable_changed);
        self.standby_mon.sync(inputs.standby_changed);
        self.active_mon.sync(inputs.active_changed);

        match self.state {
            DelayDoState::Init => {
                action = DelayDoAction::None;
                self.resume_waiting = false;
                self.state = DelayDoState::Idle;
            }

            DelayDoState::Disable => {
                action = DelayDoAction::None;
                if self.enable_mon.test_and_clear() && inputs.enable {
                    // delayDo.st:49 — only events after re-enabling may act.
                    self.active_mon.clear();
                    self.state = DelayDoState::MaybeStandby;
                }
            }

            DelayDoState::MaybeStandby => {
                action = DelayDoAction::None;
                if inputs.standby {
                    self.state = DelayDoState::Standby;
                } else if inputs.active {
                    self.state = DelayDoState::Active;
                } else {
                    self.state = DelayDoState::Idle;
                }
            }

            DelayDoState::Idle => {
                action = DelayDoAction::None;
                if self.enable_mon.test_and_clear() && !inputs.enable {
                    self.state = DelayDoState::Disable;
                } else if self.standby_mon.test_and_clear() && inputs.standby {
                    self.state = DelayDoState::Standby;
                } else if self.active_mon.test_and_clear() && inputs.active {
                    self.state = DelayDoState::Active;
                }
            }

            DelayDoState::Standby => {
                action = DelayDoAction::None;
                // `standby` names no active_mon clause, so the flag accumulates
                // here — that accumulation is the whole point of `maybeWait`.
                if self.enable_mon.test_and_clear() && !inputs.enable {
                    self.resume_waiting = false;
                    self.state = DelayDoState::Disable;
                } else if self.standby_mon.test_and_clear() && !inputs.standby {
                    self.state = DelayDoState::MaybeWait;
                }
            }

            DelayDoState::MaybeWait => {
                action = DelayDoAction::None;
                if inputs.active {
                    self.state = DelayDoState::Active;
                } else if self.active_mon.test() || self.resume_waiting {
                    // delayDo.st:130-132 — `efTest` then an explicit `efClear`.
                    self.active_mon.clear();
                    self.wait = Some((Instant::now(), self.delay_period));
                    self.state = DelayDoState::Waiting;
                } else {
                    self.state = DelayDoState::Idle;
                }
            }

            DelayDoState::Active => {
                action = DelayDoAction::None;
                if self.enable_mon.test_and_clear() && !inputs.enable {
                    self.state = DelayDoState::Disable;
                } else if self.standby_mon.test_and_clear() && inputs.standby {
                    self.state = DelayDoState::Standby;
                } else if self.active_mon.test_and_clear() && !inputs.active {
                    self.wait = Some((Instant::now(), self.delay_period));
                    self.state = DelayDoState::Waiting;
                }
            }

            DelayDoState::Waiting => {
                if self.enable_mon.test_and_clear() && !inputs.enable {
                    action = DelayDoAction::None;
                    self.state = DelayDoState::Disable;
                    self.wait = None;
                } else if self.standby_mon.test_and_clear() && inputs.standby {
                    action = DelayDoAction::None;
                    self.resume_waiting = true;
                    self.state = DelayDoState::Standby;
                    self.wait = None;
                } else if self.active_mon.test_and_clear() && inputs.active {
                    action = DelayDoAction::None;
                    self.state = DelayDoState::Active;
                    self.wait = None;
                } else if let Some((start, period)) = self.wait {
                    if start.elapsed() >= period {
                        self.resume_waiting = false;
                        self.wait = None;
                        self.state = DelayDoState::Action;
                        action = DelayDoAction::None;
                    } else {
                        action = DelayDoAction::None;
                    }
                } else {
                    action = DelayDoAction::None;
                }
            }

            DelayDoState::Action => {
                action = DelayDoAction::ProcessAction;
                self.state = DelayDoState::Idle;
            }
        }

        (action, self.state)
    }

    /// SNL `when ( delay( delayPeriod ) )` (`delayDo.st:189`) — how long a
    /// runner must wait before re-evaluating, or `None` where no clause is
    /// time-based and monitors are the only wake-up. `wait` is set on every
    /// entry to `waiting` and cleared on every exit from it, so this answers
    /// `Some` exactly in the one state that has a delay clause.
    pub fn delay_remaining(&self) -> Option<Duration> {
        let (start, period) = self.wait?;
        Some(period.saturating_sub(start.elapsed()))
    }
}

// ---------------------------------------------------------------------------
// Runner — binds the state machine above to `delayDo.db`
// ---------------------------------------------------------------------------

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::{DbChannel, DbMultiMonitor, alloc_origin};

/// The `P` and `R` macros of
/// `program delayDo("name=delayDo,P=xxx:,R=delayDo1")` (`delayDo.st:1`).
#[derive(Debug, Clone)]
pub struct DelayDoConfig {
    pub prefix: String,
    pub record: String,
}

impl DelayDoConfig {
    pub fn new(prefix: &str, record: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
            record: record.to_string(),
        }
    }

    /// `{P}{R}:<leaf>` — the shape every `PV(...)` line in `delayDo.st` uses
    /// (`:22-28`), and the one `delayDo.db` declares its records with.
    pub fn pv(&self, leaf: &str) -> String {
        format!("{}{}:{}", self.prefix, self.record, leaf)
    }
}

/// `DEBUG_PRINT(level, msg)` (`seqPVmacros.h:231-236`) — printed only while
/// `{P}{R}:debug` is at or above the level, with the program name in the
/// header as `DEBUG_PRINT_HEADER` writes it.
fn debug_print(debug_flag: i32, level: i32, msg: &str) {
    if debug_flag >= level {
        println!("<delayDo.st,{level},delayDo> {msg}");
    }
}

/// Run `delayDo.st` against the records `delayDo.db` loaded.
///
/// The state machine above is pure; this is the half that gives it PVs. It
/// owns the two things SNL's runtime owns and the controller cannot: when to
/// evaluate, and what to write.
///
/// **When to evaluate.** SNL leaves a state whose `when` clauses are
/// exhaustive on the same evaluation that entered it, so a stable state is
/// reached by stepping while [`DelayDoState::is_transient`] holds. It then
/// blocks — on monitors alone, or on whichever of a monitor and the armed
/// `delay(delayPeriod)` clause comes first. `{P}{R}:delay` and `{P}{R}:debug`
/// appear in no `when` condition, only inside action blocks, so a monitor on
/// either updates the runner and does NOT re-evaluate: an evaluation runs
/// `efTestAndClear` on the flags its clauses name, and one triggered by a
/// variable no clause tests would consume an event that had not been acted
/// on.
///
/// One deviation, and it is loud rather than silent: a PV that `delayDo.db`
/// never loaded leaves C in `init` forever, because `pvConnectCount()` never
/// reaches `pvAssignCount()` (`:35`). Here it is an error return, so the
/// caller's `eprintln!` names it.
pub async fn run(
    config: DelayDoConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let origin = alloc_origin();

    // `PV(..., EvFlag)` and `PV(..., Monitor)` — the five assigned inputs.
    let pv_enable = config.pv("enable");
    let pv_standby = config.pv("standbyCalc");
    let pv_active = config.pv("activeCalc");
    let pv_delay = config.pv("delay");
    let pv_debug = config.pv("debug");
    let monitored = vec![
        pv_enable.clone(),
        pv_standby.clone(),
        pv_active.clone(),
        pv_delay.clone(),
        pv_debug.clone(),
    ];

    let ch_enable = DbChannel::with_origin(&db, &pv_enable, origin);
    let ch_standby = DbChannel::with_origin(&db, &pv_standby, origin);
    let ch_active = DbChannel::with_origin(&db, &pv_active, origin);
    let ch_delay = DbChannel::with_origin(&db, &pv_delay, origin);
    let ch_debug = DbChannel::with_origin(&db, &pv_debug, origin);

    // `PV(..., NoMon)` — the two outputs. `doSeq` is assigned to the field
    // `.PROC` (`:27`), so the put processes the sseq rather than setting a
    // value on it.
    let ch_state = DbChannel::with_origin(&db, &config.pv("state"), origin);
    let ch_doseq = DbChannel::with_origin(&db, &format!("{}.PROC", config.pv("doSeq")), origin);

    let mut monitor = DbMultiMonitor::new_filtered(&db, &monitored, origin).await;
    if monitor.sub_count() != monitored.len() {
        return Err(format!(
            "delayDo: {} of the {} PVs it assigns are not in the database ({})",
            monitored.len() - monitor.sub_count(),
            monitored.len(),
            monitored.join(", ")
        )
        .into());
    }

    let mut debug_flag = ch_debug.get_i32().await;
    let mut ctrl = DelayDoController::new(ch_delay.get_f64().await);
    // SNL's `monitor` delivers the variable's current value at connect, so the
    // machine starts on levels rather than on zeroes. `enable` is a `short`
    // and the two calcs are `int` (`:23-25`), and C truncates on the way in —
    // a calc result of 0.5 is a false `standby`, not a true one.
    let mut inputs = DelayDoInputs {
        enable: ch_enable.get_i16().await != 0,
        enable_changed: false,
        standby: ch_standby.get_i32().await != 0,
        standby_changed: false,
        active: ch_active.get_i32().await != 0,
        active_changed: false,
    };

    loop {
        loop {
            let previous = ctrl.state;
            let (action, state) = ctrl.step(&inputs);
            // One evaluation consumes the monitor events it was given; a
            // transient state's re-evaluation is not a second arrival.
            inputs.enable_changed = false;
            inputs.standby_changed = false;
            inputs.active_changed = false;

            if action == DelayDoAction::ProcessAction {
                // `PVPUT(doSeq, 1)` before `PVPUTSTR(seqState, "idle")`
                // (`:204-206`) — the sseq runs, then the state PV catches up.
                let _ = ch_doseq.put_i32_process(1).await;
            }
            if state != previous {
                debug_print(debug_flag, 3, &format!("{previous} -> {state}"));
                if state.is_published() {
                    let _ = ch_state.put_string_process(&state.to_string()).await;
                }
            }
            if !state.is_transient() {
                break;
            }
        }

        loop {
            let woken = match ctrl.delay_remaining() {
                Some(remaining) => tokio::time::timeout(remaining, monitor.wait_change())
                    .await
                    .ok(),
                None => Some(monitor.wait_change().await),
            };
            let Some((pv, value)) = woken else {
                // The armed `delay(delayPeriod)` clause came first.
                break;
            };
            if pv == pv_enable {
                inputs.enable = (value as i16) != 0;
                inputs.enable_changed = true;
                break;
            } else if pv == pv_standby {
                inputs.standby = (value as i32) != 0;
                inputs.standby_changed = true;
                break;
            } else if pv == pv_active {
                inputs.active = (value as i32) != 0;
                inputs.active_changed = true;
                break;
            } else if pv == pv_delay {
                ctrl.delay_period = epics_base_rs::runtime::time::duration_from_secs(value);
            } else if pv == pv_debug {
                debug_flag = value as i32;
            }
        }
    }
}
