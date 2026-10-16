use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::AsynMotor;
use tokio::sync::mpsc;

use crate::device_state::AxisSubscribers;
use crate::device_support::{MotorAxis, MotorDeviceSupport};
use crate::poll_loop::{MotorPollLoop, PollCommand, PollerInbox};
use crate::record::MotorRecord;
use crate::trace::TraceUser;

/// Assembled motor components ready for use.
pub struct MotorSetup {
    pub record: MotorRecord,
    pub device_support: MotorDeviceSupport,
    pub poll_loop: MotorPollLoop,
    pub poll_cmd_tx: mpsc::Sender<PollCommand>,
}

/// An axis and its poller, for a create command to register; records bind
/// it at init through [`MotorAxis::attach`].
pub struct AxisSetup {
    pub axis: MotorAxis,
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
    /// `(port, addr)` on a model-3 or model-2 driver, or
    /// [`TraceUser::Model1`] for a model-1 one.
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

    /// The axis and its poller, with no record bound.
    pub fn build_axis(mut self) -> AxisSetup {
        self.configure_record = None;
        let (poll_cmd_tx, poll_cmd_rx) = mpsc::channel(self.poll_channel_capacity);
        let (motor_cmd_tx, motor_cmd_rx) = mpsc::unbounded_channel();
        let subscribers = AxisSubscribers::new();
        let axis = MotorAxis::new(
            self.user,
            self.timeout,
            poll_cmd_tx.clone(),
            motor_cmd_tx,
            subscribers.clone(),
        );
        let poll_loop = MotorPollLoop::new(
            PollerInbox {
                poll: poll_cmd_rx,
                commands: motor_cmd_rx,
            },
            self.motor,
            subscribers,
            self.moving_poll_interval,
            self.idle_poll_interval,
            self.forced_fast_polls,
        );
        AxisSetup {
            axis,
            poll_loop,
            poll_cmd_tx,
        }
    }

    /// One record bound to a fresh axis.
    pub fn build(mut self) -> MotorSetup {
        let configure_record = self.configure_record.take();
        let AxisSetup {
            axis,
            poll_loop,
            poll_cmd_tx,
        } = self.build_axis();
        let device_support = axis.attach();
        let mut record =
            MotorRecord::new().with_device_state(device_support.device_state().clone());
        if let Some(configure) = configure_record {
            configure(&mut record);
        }
        MotorSetup {
            record,
            device_support,
            poll_loop,
            poll_cmd_tx,
        }
    }
}
