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
