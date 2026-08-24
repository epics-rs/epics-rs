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
#[derive(Debug, Clone, Copy)]
pub struct DelayDoInputs {
    /// Enable/disable control
    pub enable: bool,
    /// Whether the "enable" signal changed since last step
    pub enable_changed: bool,
    /// Standby condition
    pub standby: bool,
    /// Whether the "standby" signal changed since last step
    pub standby_changed: bool,
    /// Active condition
    pub active: bool,
    /// Whether the "active" signal changed since last step
    pub active_changed: bool,
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
    /// Whether the active signal was seen during standby.
    active_seen: bool,
    /// When the waiting state was entered (for delay timing).
    wait_start: Option<Instant>,
}

impl Default for DelayDoController {
    fn default() -> Self {
        Self {
            state: DelayDoState::Init,
            delay_period: Duration::from_secs(0),
            resume_waiting: false,
            active_seen: false,
            wait_start: None,
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

        match self.state {
            DelayDoState::Init => {
                action = DelayDoAction::None;
                self.resume_waiting = false;
                self.state = DelayDoState::Idle;
            }

            DelayDoState::Disable => {
                action = DelayDoAction::None;
                if inputs.enable_changed && inputs.enable {
                    self.active_seen = false;
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
                if inputs.enable_changed && !inputs.enable {
                    self.state = DelayDoState::Disable;
                } else if inputs.standby_changed && inputs.standby {
                    self.state = DelayDoState::Standby;
                } else if inputs.active_changed && inputs.active {
                    self.state = DelayDoState::Active;
                }
            }

            DelayDoState::Standby => {
                action = DelayDoAction::None;
                if inputs.active_changed && inputs.active {
                    self.active_seen = true;
                }
                if inputs.enable_changed && !inputs.enable {
                    self.resume_waiting = false;
                    self.state = DelayDoState::Disable;
                } else if inputs.standby_changed && !inputs.standby {
                    self.state = DelayDoState::MaybeWait;
                }
            }

            DelayDoState::MaybeWait => {
                action = DelayDoAction::None;
                if inputs.active {
                    self.state = DelayDoState::Active;
                } else if self.active_seen || self.resume_waiting {
                    self.active_seen = false;
                    self.wait_start = Some(Instant::now());
                    self.state = DelayDoState::Waiting;
                } else {
                    self.state = DelayDoState::Idle;
                }
            }

            DelayDoState::Active => {
                action = DelayDoAction::None;
                if inputs.enable_changed && !inputs.enable {
                    self.state = DelayDoState::Disable;
                } else if inputs.standby_changed && inputs.standby {
                    self.state = DelayDoState::Standby;
                } else if inputs.active_changed && !inputs.active {
                    self.wait_start = Some(Instant::now());
                    self.state = DelayDoState::Waiting;
                }
            }

            DelayDoState::Waiting => {
                if inputs.enable_changed && !inputs.enable {
                    action = DelayDoAction::None;
                    self.state = DelayDoState::Disable;
                } else if inputs.standby_changed && inputs.standby {
                    action = DelayDoAction::None;
                    self.resume_waiting = true;
                    self.state = DelayDoState::Standby;
                } else if inputs.active_changed && inputs.active {
                    action = DelayDoAction::None;
                    self.state = DelayDoState::Active;
                } else if let Some(start) = self.wait_start {
                    if start.elapsed() >= self.delay_period {
                        self.resume_waiting = false;
                        self.wait_start = None;
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
}
