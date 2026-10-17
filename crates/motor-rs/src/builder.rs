use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::AsynMotor;
use tokio::sync::mpsc;

use crate::device_state::AxisSubscribers;
use crate::device_support::{MotorAxis, MotorDeviceSupport};
use crate::poll_loop::{
    Axis, ControllerPoll, Join, Joining, MotorPollLoop, PollCommand, PollerInbox,
};
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

    /// The axis and its poller, with no record bound: a model-1 axis is a
    /// driver of one axis, any other a controller of one.
    pub fn build_axis(mut self) -> AxisSetup {
        self.configure_record = None;
        let model1 = matches!(self.user, TraceUser::Model1 { .. });
        if !model1 {
            let mut controller = ControllerBuilder::new();
            let ControllerAxis { axis, poll_cmd_tx } = controller.axis_with(
                self.motor,
                self.user,
                self.timeout,
                self.poll_channel_capacity,
            );
            return AxisSetup {
                axis,
                poll_loop: controller.build(
                    self.moving_poll_interval,
                    self.idle_poll_interval,
                    self.forced_fast_polls,
                ),
                poll_cmd_tx,
            };
        }
        // A model-1 driver's task scans at its moving period alone.
        let mut task = MotorTaskBuilder::new(self.moving_poll_interval);
        let ControllerAxis { axis, poll_cmd_tx } = task.axis_with(
            self.motor,
            self.user,
            self.timeout,
            self.poll_channel_capacity,
        );
        AxisSetup {
            axis,
            poll_loop: task.build(),
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
#[derive(Default)]
pub struct ControllerBuilder {
    controller_poll: Option<ControllerPoll>,
    axes: ControllerAxes,
}

/// A controller's axis list, which C's poller reads on every pass
/// (asynMotorController.cpp:655-661): an axis added once the poller runs
/// joins it, polled in address order with the rest.
#[derive(Clone)]
pub struct ControllerAxes(Arc<Mutex<Axes>>);

struct Axes {
    list: AxisList,
    /// Each driver axis added, once: a later add of the same one is another
    /// address of the axis already polled.
    opened: Vec<Opened>,
}

struct Opened {
    motor: Arc<Mutex<dyn AsynMotor>>,
    axis: MotorAxis,
    poll_cmd_tx: mpsc::Sender<PollCommand>,
    /// The controller port address the status is mirrored to.
    mirror: Option<(String, i32)>,
}

enum AxisList {
    /// Before the poller starts.
    Pending(Vec<Axis>),
    /// The running poller's.
    Running(mpsc::UnboundedSender<Joining>),
}

impl Default for ControllerAxes {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Axes {
            list: AxisList::Pending(Vec::new()),
            opened: Vec::new(),
        })))
    }
}

impl ControllerAxes {
    /// Add an axis; on a running poller, return once the poller has taken
    /// it, so its status is cached before any record binds it. A `motor`
    /// added before is not polled twice: `user` reaches the same axis.
    pub fn add(
        &self,
        motor: Arc<Mutex<dyn AsynMotor>>,
        user: TraceUser,
    ) -> Result<ControllerAxis, String> {
        self.add_with(motor, user, DEFAULT_TIMEOUT, DEFAULT_POLL_CHANNEL_CAPACITY)
    }

    fn add_with(
        &self,
        motor: Arc<Mutex<dyn AsynMotor>>,
        user: TraceUser,
        timeout: Duration,
        capacity: usize,
    ) -> Result<ControllerAxis, String> {
        let mirror = match &user {
            TraceUser::Asyn { port, addr } => Some((port.clone(), *addr)),
            _ => None,
        };
        let same = |o: &Opened| std::ptr::addr_eq(Arc::as_ptr(&o.motor), Arc::as_ptr(&motor));
        let mut axes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        // Recorded before the poller confirms, so a concurrent add of the
        // same motor finds it.
        let (join, added) = if let Some(opened) = axes.opened.iter_mut().find(|o| same(o)) {
            let added = ControllerAxis {
                axis: opened.axis.at(user),
                poll_cmd_tx: opened.poll_cmd_tx.clone(),
            };
            match (mirror, &opened.mirror) {
                (None, _) => return Ok(added),
                (Some(to), Some(had)) if &to == had => return Ok(added),
                (Some((port, addr)), Some((had_port, had_addr))) => {
                    return Err(format!(
                        "the axis is at port {had_port} address {had_addr}, not {port} {addr}"
                    ));
                }
                (Some((port, addr)), None) => {
                    opened.mirror = Some((port.clone(), addr));
                    let motor = motor.clone();
                    (Join::Mirror { motor, port, addr }, added)
                }
            }
        } else {
            let (axis, inbox, subscribers, poll_cmd_tx) =
                axis_parts(&motor, user, timeout, capacity);
            axes.opened.push(Opened {
                motor: motor.clone(),
                axis: axis.at(axis.address().clone()),
                poll_cmd_tx: poll_cmd_tx.clone(),
                mirror: mirror.clone(),
            });
            let mut polled = Axis::polled(inbox, motor.clone(), subscribers);
            if let Some((port, addr)) = mirror {
                polled.mirror_to(&port, addr);
            }
            (
                Join::Axis(Box::new(polled)),
                ControllerAxis { axis, poll_cmd_tx },
            )
        };
        let joined = match &mut axes.list {
            AxisList::Pending(axes) => {
                join.place(axes);
                None
            }
            AxisList::Running(poller) => {
                let (joined, taken) = std::sync::mpsc::sync_channel(1);
                let _ = poller.send(Joining { join, joined });
                Some(taken)
            }
        };
        drop(axes);
        if let Some(taken) = joined
            && taken.recv().is_err()
        {
            // No poller took it: a later add must not get its senders.
            let mut axes = self.0.lock().unwrap_or_else(|e| e.into_inner());
            axes.opened.retain(|o| !same(o));
            return Err("the controller's poller has ended".to_string());
        }
        Ok(added)
    }

    /// The axes so far, and the channel later ones join the poller by.
    fn start(&self) -> (Vec<Axis>, mpsc::UnboundedReceiver<Joining>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut axes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match std::mem::replace(&mut axes.list, AxisList::Running(tx)) {
            AxisList::Pending(axes) => (axes, rx),
            // The builder is consumed by the one start.
            AxisList::Running(_) => unreachable!("a controller's poller starts once"),
        }
    }
}

impl ControllerBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The driver's `asynMotorController::poll` override, run before the
    /// axes on every pass and on the startup pass.
    pub fn controller_poll(mut self, poll: impl FnMut() + Send + 'static) -> Self {
        self.controller_poll = Some(Box::new(poll));
        self
    }

    /// The next axis, polled in address order. `user` is the axis's
    /// `(port, addr)` its trace lines print on.
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
        self.axes
            .add_with(motor, user, timeout, capacity)
            .expect("an axis added before the poller starts is queued")
    }

    /// The controller's axis list, for axes added after [`Self::build`].
    pub fn axes(&self) -> ControllerAxes {
        self.axes.clone()
    }

    /// C `startPoller(movingPollPeriod, idlePollPeriod, forcedFastPolls)`:
    /// the poller, after its startup pass over the axes.
    pub fn build(
        self,
        moving_poll_interval: Duration,
        idle_poll_interval: Duration,
        forced_fast_polls: u32,
    ) -> MotorPollLoop {
        let (axes, joining) = self.axes.start();
        MotorPollLoop::controller(
            axes,
            Some(joining),
            moving_poll_interval,
            idle_poll_interval,
            forced_fast_polls,
            self.controller_poll,
        )
    }
}

/// Every card and signal of one model-1 driver and the one `motor_task`
/// that queries them in turn: C motordrvCom starts one per driver table
/// (motordrvCom.cc:177-300).
pub struct MotorTaskBuilder {
    scan: Duration,
    axes: Vec<Axis>,
}

impl MotorTaskBuilder {
    /// `scan` is the driver's `1 / motor_scan_rate`.
    pub fn new(scan: Duration) -> Self {
        Self {
            scan,
            axes: Vec::new(),
        }
    }

    /// The next axis, queried after those added before it. `user` is its
    /// [`TraceUser::Model1`] card and signal.
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
        let (axis, inbox, subscribers, poll_cmd_tx) = axis_parts(&motor, user, timeout, capacity);
        self.axes.push(Axis::task(inbox, motor, subscribers));
        ControllerAxis { axis, poll_cmd_tx }
    }

    /// The task, after its startup pass over the axes.
    pub fn build(self) -> MotorPollLoop {
        MotorPollLoop::motor_task(self.axes, self.scan)
    }
}

#[cfg(test)]
mod tests {
    use std::thread::ThreadId;
    use std::time::Instant;

    use asyn_rs::error::AsynResult;
    use asyn_rs::interfaces::motor::{MotorStatus, PollFailure};
    use asyn_rs::user::AsynUser;

    use super::*;
    use crate::flags::MotorCommand;

    type Log = Arc<Mutex<Vec<(&'static str, ThreadId, Instant)>>>;

    const SLOW: Duration = Duration::from_millis(40);

    /// A card that never ends its move; each poll is logged, then takes
    /// `delay`.
    struct Card {
        name: &'static str,
        log: Log,
        delay: Duration,
    }

    impl AsynMotor for Card {
        fn move_absolute(
            &mut self,
            _: &AsynUser,
            _: f64,
            _: f64,
            _: f64,
            _: f64,
        ) -> AsynResult<()> {
            Ok(())
        }
        fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
            Ok(())
        }
        fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
            Ok(())
        }
        fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
            Ok(())
        }
        fn poll_failure(&self) -> PollFailure {
            PollFailure::PROBLEM_AND_COMMS_ERROR
        }
        fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
            let polled = (self.name, std::thread::current().id(), Instant::now());
            self.log.lock().unwrap().push(polled);
            std::thread::sleep(self.delay);
            Ok(MotorStatus {
                done: false,
                moving: true,
                ..MotorStatus::default()
            })
        }
    }

    #[test]
    fn one_task_queries_every_card_of_its_driver_in_turn() {
        let log = Log::default();
        let mut task = MotorTaskBuilder::new(Duration::from_millis(5));
        let axes: Vec<ControllerAxis> = [("C0", SLOW), ("C1", Duration::ZERO)]
            .into_iter()
            .enumerate()
            .map(|(card, (name, delay))| {
                let card_dev = Card {
                    name,
                    log: log.clone(),
                    delay,
                };
                let user = TraceUser::Model1 {
                    device: "devTestTask",
                    card: card as i32,
                    signal: 0,
                };
                task.axis(Arc::new(Mutex::new(card_dev)), user)
            })
            .collect();
        let task = task.build();
        log.lock().unwrap().clear();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let running = tokio::spawn(task.run());
            for ControllerAxis { axis, .. } in &axes {
                axis.send(MotorCommand::MoveAbsolute {
                    position: 1.0,
                    min_velocity: 0.0,
                    velocity: 1.0,
                    acceleration: 1.0,
                });
            }
            tokio::time::sleep(SLOW * 6).await;
            axes[0]
                .poll_cmd_tx
                .send(PollCommand::Shutdown)
                .await
                .unwrap();
            running.await.unwrap();
        });
        let log = log.lock().unwrap().clone();
        let names: Vec<_> = log.iter().map(|(name, ..)| *name).collect();
        assert!(names.len() >= 4, "{names:?}");
        assert!(
            log.iter().all(|(_, thread, _)| *thread == log[0].1),
            "every card is queried on the one task thread"
        );
        // The held moves are queried card by card, each wake: card 1's
        // query waits out card 0's slow one.
        let first = names.iter().position(|name| *name == "C0").unwrap();
        for pair in log[first..].as_chunks::<2>().0 {
            assert_eq!((pair[0].0, pair[1].0), ("C0", "C1"), "{names:?}");
            assert!(pair[1].2 - pair[0].2 >= SLOW);
        }
    }

    /// Logs each poll by name; never moves.
    struct Named(&'static str, Arc<Mutex<Vec<&'static str>>>);

    impl AsynMotor for Named {
        fn move_absolute(
            &mut self,
            _: &AsynUser,
            _: f64,
            _: f64,
            _: f64,
            _: f64,
        ) -> AsynResult<()> {
            Ok(())
        }
        fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
            Ok(())
        }
        fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
            Ok(())
        }
        fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
            Ok(())
        }
        fn poll_failure(&self) -> PollFailure {
            PollFailure::PROBLEM_AND_COMMS_ERROR
        }
        fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
            self.1.lock().unwrap().push(self.0);
            Ok(MotorStatus::default())
        }
    }

    /// C motorUpdateStatus_ runs `pC->poll()` before `pAxis->poll`
    /// (asynMotorController.cpp:217-221), whether a ForcePoll or a record's
    /// GET_INFO asks for it.
    #[test]
    fn an_update_status_polls_the_controller_before_the_axis() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let controller_log = log.clone();
        let mut controller = ControllerBuilder::new()
            .controller_poll(move || controller_log.lock().unwrap().push("C"));
        let ControllerAxis { axis, poll_cmd_tx } = controller.axis(
            Arc::new(Mutex::new(Named("A", log.clone()))),
            TraceUser::Asyn {
                port: String::new(),
                addr: 0,
            },
        );
        // No timed pass within the test.
        let idle = Duration::from_secs(60);
        let poller = controller.build(idle, idle, 0);
        log.lock().unwrap().clear();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let settle = Duration::from_millis(50);
        let (forced, info) = rt.block_on(async {
            let running = tokio::spawn(poller.run());
            tokio::time::sleep(settle).await;
            poll_cmd_tx.send(PollCommand::ForcePoll).await.unwrap();
            tokio::time::sleep(settle).await;
            let forced = std::mem::take(&mut *log.lock().unwrap());
            axis.send(MotorCommand::Poll);
            tokio::time::sleep(settle).await;
            let info = std::mem::take(&mut *log.lock().unwrap());
            poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
            running.await.unwrap();
            (forced, info)
        });
        assert_eq!(forced, ["C", "A"], "ForcePoll");
        assert_eq!(info, ["C", "A"], "GET_INFO");
    }

    /// drvMotorAsyn and devMotorSim both open a drvMotorSim axis, and
    /// motorProcTask still processes it once a pass (drvMotorSim.c:657-680).
    #[test]
    fn a_driver_axis_added_at_two_addresses_is_polled_once() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let shared: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Named("A", log.clone())));
        let mut controller = ControllerBuilder::new();
        let model2 = TraceUser::Model2 {
            port: "P".into(),
            addr: 0,
        };
        let interface = TraceUser::MotorInterface {
            device: "Sim",
            driver: "sim",
            card: 0,
            signal: 0,
        };
        let first = controller.axis(shared.clone(), model2.clone());
        let second = controller.axis(shared, interface.clone());
        let _b = controller.axis(Arc::new(Mutex::new(Named("B", log.clone()))), model2);
        assert_eq!(second.axis.address(), &interface);
        let idle = Duration::from_secs(60);
        let poller = controller.build(idle, idle, 0);
        assert_eq!(std::mem::take(&mut *log.lock().unwrap()), ["A", "B"]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let polled = rt.block_on(async {
            let running = tokio::spawn(poller.run());
            second
                .poll_cmd_tx
                .send(PollCommand::ForcePoll)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let polled = std::mem::take(&mut *log.lock().unwrap());
            first.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
            running.await.unwrap();
            polled
        });
        assert_eq!(polled, ["A"], "the second address reaches the one axis");
    }

    /// An add the poller never took fails, and a later add of the same motor
    /// fails too rather than reach no poller.
    #[test]
    fn an_add_to_an_ended_poller_fails_every_time() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut controller = ControllerBuilder::new();
        let user = TraceUser::Model2 {
            port: "P".into(),
            addr: 0,
        };
        let first = controller.axis(Arc::new(Mutex::new(Named("A", log.clone()))), user.clone());
        let axes = controller.axes();
        let idle = Duration::from_secs(60);
        let poller = controller.build(idle, idle, 0);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let running = tokio::spawn(poller.run());
            first.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
            running.await.unwrap();
        });
        let late: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Named("B", log)));
        for attempt in 0..2 {
            assert!(
                axes.add(late.clone(), user.clone()).is_err(),
                "add {attempt}"
            );
        }
    }
}
