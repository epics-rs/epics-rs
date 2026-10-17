use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::AsynMotor;
use tokio::sync::mpsc;

use crate::device_state::AxisSubscribers;
use crate::device_support::{MotorAxis, MotorDeviceSupport};
use crate::poll_loop::{Axis, ControllerPoll, MotorPollLoop, PollCommand, PollerInbox};
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

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_POLL_CHANNEL_CAPACITY: usize = 16;

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
            timeout: DEFAULT_TIMEOUT,
            moving_poll_interval: Duration::from_millis(100),
            idle_poll_interval: Duration::from_secs(1),
            forced_fast_polls: 10,
            poll_channel_capacity: DEFAULT_POLL_CHANNEL_CAPACITY,
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

    /// The axis and its poller, with no record bound. A model-1 axis gets
    /// its own `motor_task`; any other is a controller of one axis.
    pub fn build_axis(mut self) -> AxisSetup {
        self.configure_record = None;
        let model1 = matches!(self.user, TraceUser::Model1 { .. });
        if !model1 {
            let mut controller = ControllerBuilder::new(
                self.moving_poll_interval,
                self.idle_poll_interval,
                self.forced_fast_polls,
            );
            let ControllerAxis { axis, poll_cmd_tx } = controller.axis_with(
                self.motor,
                self.user,
                self.timeout,
                self.poll_channel_capacity,
            );
            return AxisSetup {
                axis,
                poll_loop: controller.build(),
                poll_cmd_tx,
            };
        }
        let (axis, inbox, subscribers, poll_cmd_tx) = axis_parts(
            &self.motor,
            self.user,
            self.timeout,
            self.poll_channel_capacity,
        );
        // A model-1 driver's task scans at its moving period alone.
        let poll_loop =
            MotorPollLoop::motor_task(inbox, self.motor, subscribers, self.moving_poll_interval);
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

/// An axis's record half, its poller's inbox and subscribers, and the
/// sender of its poll commands.
fn axis_parts(
    motor: &Arc<Mutex<dyn AsynMotor>>,
    user: TraceUser,
    timeout: Duration,
    capacity: usize,
) -> (
    MotorAxis,
    PollerInbox,
    AxisSubscribers,
    mpsc::Sender<PollCommand>,
) {
    let (poll_cmd_tx, poll_cmd_rx) = mpsc::channel(capacity);
    let (motor_cmd_tx, motor_cmd_rx) = mpsc::unbounded_channel();
    let subscribers = AxisSubscribers::new();
    let disables_ntm = motor.lock().is_ok_and(|m| m.disables_ntm());
    let axis = MotorAxis::new(
        user,
        timeout,
        poll_cmd_tx.clone(),
        motor_cmd_tx,
        subscribers.clone(),
    )
    .disabling_ntm(disables_ntm);
    let inbox = PollerInbox {
        poll: poll_cmd_rx,
        commands: motor_cmd_rx,
    };
    (axis, inbox, subscribers, poll_cmd_tx)
}

/// An axis of a [`ControllerBuilder`], for a create command to register.
pub struct ControllerAxis {
    pub axis: MotorAxis,
    pub poll_cmd_tx: mpsc::Sender<PollCommand>,
}

/// The axes of one asynMotorController and the one poller that polls them
/// in turn: C `startPoller`'s thread (asynMotorController.cpp:567-575).
pub struct ControllerBuilder {
    moving_poll_interval: Duration,
    idle_poll_interval: Duration,
    forced_fast_polls: u32,
    controller_poll: Option<ControllerPoll>,
    axes: Vec<Axis>,
}

impl ControllerBuilder {
    /// C `startPoller(movingPollPeriod, idlePollPeriod, forcedFastPolls)`.
    pub fn new(
        moving_poll_interval: Duration,
        idle_poll_interval: Duration,
        forced_fast_polls: u32,
    ) -> Self {
        Self {
            moving_poll_interval,
            idle_poll_interval,
            forced_fast_polls,
            controller_poll: None,
            axes: Vec::new(),
        }
    }

    /// The driver's `asynMotorController::poll` override, run before the
    /// axes on every pass and on the startup pass.
    pub fn controller_poll(mut self, poll: impl FnMut() + Send + 'static) -> Self {
        self.controller_poll = Some(Box::new(poll));
        self
    }

    /// The next axis, polled after those added before it. `user` is the
    /// axis's `(port, addr)` its trace lines print on.
    pub fn axis(&mut self, motor: Arc<Mutex<dyn AsynMotor>>, user: TraceUser) -> ControllerAxis {
        self.axis_with(motor, user, DEFAULT_TIMEOUT, DEFAULT_POLL_CHANNEL_CAPACITY)
    }

    fn axis_with(
        &mut self,
        motor: Arc<Mutex<dyn AsynMotor>>,
        user: TraceUser,
        timeout: Duration,
        capacity: usize,
    ) -> ControllerAxis {
        let controller = match &user {
            TraceUser::Asyn { port, addr } => Some((port.clone(), *addr)),
            _ => None,
        };
        let (axis, inbox, subscribers, poll_cmd_tx) = axis_parts(&motor, user, timeout, capacity);
        let mut polled = Axis::polled(inbox, motor, subscribers);
        if let Some((port, addr)) = controller {
            polled.mirror_to(&port, addr);
        }
        self.axes.push(polled);
        ControllerAxis { axis, poll_cmd_tx }
    }

    /// The poller, after its startup pass over the axes.
    pub fn build(self) -> MotorPollLoop {
        MotorPollLoop::controller(
            self.axes,
            self.moving_poll_interval,
            self.idle_poll_interval,
            self.forced_fast_polls,
            self.controller_poll,
        )
    }
}
