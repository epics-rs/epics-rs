use crate::flags::MotorCommand;
use asyn_rs::interfaces::motor::MotorStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Stamped motor status with sequence number for change detection.
#[derive(Debug, Clone)]
pub struct StampedStatus {
    pub seq: u64,
    pub status: MotorStatus,
}

/// Poll loop directive — exclusive enum, not booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PollDirective {
    #[default]
    None,
    Start,
    Stop,
}

/// Delay request with unique ID for stale timer prevention.
#[derive(Debug, Clone)]
pub struct DelayRequest {
    pub id: u64,
    pub duration: Duration,
}

/// Atomic bundle of actions from Record → DeviceSupport.
#[derive(Debug, Default)]
pub struct DeviceActions {
    pub commands: Vec<MotorCommand>,
    pub poll: PollDirective,
    pub schedule_delay: Option<DelayRequest>,
    pub status_refresh: bool,
}

impl DeviceActions {
    /// Fold a newer batch into `self` when a previous batch has not yet been
    /// consumed by DeviceSupport.write(). Commands are appended (FIFO order
    /// preserved); the poll directive takes the newer value unless it is
    /// `None` ("no change"); schedule_delay and status_refresh take the newer
    /// value if present. Prevents a dropped move command when two process()
    /// cycles run without an intervening write().
    pub fn merge_newer(&mut self, newer: DeviceActions) {
        self.commands.extend(newer.commands);
        if newer.poll != PollDirective::None {
            self.poll = newer.poll;
        }
        if newer.schedule_delay.is_some() {
            self.schedule_delay = newer.schedule_delay;
        }
        self.status_refresh |= newer.status_refresh;
    }
}

/// Shared mailbox between MotorRecord, MotorDeviceSupport, and PollLoop.
///
/// Data flow:
///   PollLoop → latest_status, expired_delay_id → Record.process() reads
///   Record.process() → pending_actions → DeviceSupport.write() consumes
#[derive(Debug)]
pub struct MotorDeviceState {
    // PollLoop → Record
    pub latest_status: Option<StampedStatus>,
    pub expired_delay_id: Option<u64>,

    // Record → DeviceSupport
    pub pending_actions: Option<DeviceActions>,
}

impl Default for MotorDeviceState {
    fn default() -> Self {
        Self {
            latest_status: None,
            expired_delay_id: None,
            pending_actions: None,
        }
    }
}

pub type SharedDeviceState = Arc<Mutex<MotorDeviceState>>;

pub fn new_shared_state() -> SharedDeviceState {
    Arc::new(Mutex::new(MotorDeviceState::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_newer_appends_commands_fifo() {
        let mut a = DeviceActions {
            commands: vec![MotorCommand::Stop { acceleration: 1.0 }],
            ..Default::default()
        };
        let b = DeviceActions {
            commands: vec![MotorCommand::Poll],
            ..Default::default()
        };
        a.merge_newer(b);
        assert_eq!(a.commands.len(), 2);
        // earlier batch's command stays first
        assert!(matches!(a.commands[0], MotorCommand::Stop { .. }));
        assert!(matches!(a.commands[1], MotorCommand::Poll));
    }

    #[test]
    fn merge_newer_poll_none_keeps_previous() {
        let mut a = DeviceActions {
            poll: PollDirective::Start,
            ..Default::default()
        };
        let b = DeviceActions {
            poll: PollDirective::None,
            ..Default::default()
        };
        a.merge_newer(b);
        assert_eq!(a.poll, PollDirective::Start);
    }

    #[test]
    fn merge_newer_poll_explicit_overrides() {
        let mut a = DeviceActions {
            poll: PollDirective::Start,
            ..Default::default()
        };
        let b = DeviceActions {
            poll: PollDirective::Stop,
            ..Default::default()
        };
        a.merge_newer(b);
        assert_eq!(a.poll, PollDirective::Stop);
    }

    #[test]
    fn merge_newer_takes_newer_delay_and_refresh() {
        let mut a = DeviceActions::default();
        let b = DeviceActions {
            schedule_delay: Some(DelayRequest {
                id: 7,
                duration: Duration::from_millis(5),
            }),
            status_refresh: true,
            ..Default::default()
        };
        a.merge_newer(b);
        assert_eq!(a.schedule_delay.as_ref().unwrap().id, 7);
        assert!(a.status_refresh);
    }
}
