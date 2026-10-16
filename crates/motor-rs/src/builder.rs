use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::AsynMotor;
use tokio::sync::mpsc;

use crate::device_state::{self, SharedDeviceState};
use crate::device_support::MotorDeviceSupport;
use crate::poll_loop::{MotorPollLoop, PollCommand, PollerInbox};
use crate::record::MotorRecord;
use crate::trace::{MotorTrace, TraceUser};

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
    user: TraceUser,
    timeout: Duration,
    moving_poll_interval: Duration,
    idle_poll_interval: Duration,
    forced_fast_polls: u32,
    poll_channel_capacity: usize,
    configure_record: Option<Box<dyn FnOnce(&mut MotorRecord)>>,
}

impl MotorBuilder {
    pub fn new(motor: Arc<Mutex<dyn AsynMotor>>) -> Self {
        Self {
            motor,
            user: TraceUser::Asyn {
                port: String::new(),
                addr: 0,
            },
            timeout: Duration::from_secs(1),
            moving_poll_interval: Duration::from_millis(100),
            idle_poll_interval: Duration::from_secs(1),
            forced_fast_polls: 10,
            poll_channel_capacity: 16,
            configure_record: None,
        }
    }

    /// The asyn user the axis's trace lines print on: C's `pasynUser`
    /// `(port, addr)`, or [`TraceUser::Model1`] for a model-1 driver.
    pub fn trace_user(mut self, user: TraceUser) -> Self {
        self.user = user;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.moving_poll_interval = interval;
        self.idle_poll_interval = interval;
        self
    }

    pub fn moving_poll_interval(mut self, interval: Duration) -> Self {
        self.moving_poll_interval = interval;
        self
    }

    pub fn idle_poll_interval(mut self, interval: Duration) -> Self {
        self.idle_poll_interval = interval;
        self
    }

    pub fn forced_fast_polls(mut self, count: u32) -> Self {
        self.forced_fast_polls = count;
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

        let (motor_cmd_tx, motor_cmd_rx) = mpsc::unbounded_channel();
        let trace = Arc::new(MotorTrace::with_user(self.user));
        let device_support = MotorDeviceSupport::new(
            trace.clone(),
            self.timeout,
            poll_cmd_tx.clone(),
            motor_cmd_tx,
            device_state.clone(),
        );

        let io_intr_tx = device_support.io_intr_sender();
        let poll_loop = MotorPollLoop::new(
            PollerInbox {
                poll: poll_cmd_rx,
                commands: motor_cmd_rx,
            },
            io_intr_tx,
            self.motor,
            device_state,
            self.moving_poll_interval,
            self.idle_poll_interval,
            self.forced_fast_polls,
        )
        .with_trace(trace);

        MotorSetup {
            record,
            device_support,
            poll_loop,
            poll_cmd_tx,
        }
    }
}
