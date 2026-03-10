use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::AsynMotor;
use tokio::sync::mpsc;

use crate::device_state::{self, SharedDeviceState};
use crate::device_support::MotorDeviceSupport;
use crate::poll_loop::{MotorPollLoop, PollCommand};
use crate::record::MotorRecord;

/// Assembled motor components ready for use.
pub struct MotorSetup {
    pub record: MotorRecord,
    pub device_support: MotorDeviceSupport,
    pub poll_loop: MotorPollLoop,
    pub poll_cmd_tx: mpsc::Sender<PollCommand>,
}

/// Builder for constructing a complete motor record + device support + poll loop.
pub struct MotorBuilder {
    motor: Arc<Mutex<dyn AsynMotor>>,
    addr: i32,
    timeout: Duration,
    poll_interval: Duration,
    poll_channel_capacity: usize,
    configure_record: Option<Box<dyn FnOnce(&mut MotorRecord)>>,
}

impl MotorBuilder {
    pub fn new(motor: Arc<Mutex<dyn AsynMotor>>) -> Self {
        Self {
            motor,
            addr: 0,
            timeout: Duration::from_secs(1),
            poll_interval: Duration::from_millis(100),
            poll_channel_capacity: 16,
            configure_record: None,
        }
    }

    pub fn addr(mut self, addr: i32) -> Self {
        self.addr = addr;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    pub fn poll_channel_capacity(mut self, capacity: usize) -> Self {
        self.poll_channel_capacity = capacity;
        self
    }

    pub fn configure_record(mut self, f: impl FnOnce(&mut MotorRecord) + 'static) -> Self {
        self.configure_record = Some(Box::new(f));
        self
    }

    pub fn build(self) -> MotorSetup {
        let device_state: SharedDeviceState = device_state::new_shared_state();
        let (poll_cmd_tx, poll_cmd_rx) = mpsc::channel(self.poll_channel_capacity);

        let mut record = MotorRecord::new().with_device_state(device_state.clone());
        if let Some(configure) = self.configure_record {
            configure(&mut record);
        }

        let device_support = MotorDeviceSupport::new(
            self.motor.clone(),
            self.addr,
            self.timeout,
            poll_cmd_tx.clone(),
            device_state.clone(),
        );

        let io_intr_tx = device_support.io_intr_sender();
        let poll_loop = MotorPollLoop::new(
            poll_cmd_rx,
            io_intr_tx,
            self.motor,
            device_state,
            self.poll_interval,
        );

        MotorSetup {
            record,
            device_support,
            poll_loop,
            poll_cmd_tx,
        }
    }
}
