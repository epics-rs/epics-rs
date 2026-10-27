use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::{AsynMotor, Model1Motor, PortSets};
use epics_base_rs::server::record::Record;
use tokio::sync::mpsc;

use crate::device_support::{AxisSlot, MotorAxis, MotorDeviceSupport};
use crate::poll_loop::{
    Axis, FirstPollPending, Join, Joining, MotorPollLoop, Place, PollCommand, SharedControllerPoll,
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

/// The driver a [`MotorBuilder`] builds its axis on.
enum Driver {
    /// A model-3 or model-2 axis, which its controller's poller polls.
    Polled(Arc<Mutex<dyn AsynMotor>>),
    /// A model-1 axis, which its driver's motor task queries.
    Model1(Arc<Mutex<dyn Model1Motor>>),
}

/// Builder for constructing a complete motor record + device support + poll loop.
pub struct MotorBuilder {
    motor: Driver,
    user: TraceUser,
    timeout: Duration,
    moving_poll_interval: Duration,
    idle_poll_interval: Duration,
    forced_fast_polls: u32,
    poll_channel_capacity: usize,
    configure_record: Option<Box<dyn FnOnce(&mut MotorRecord)>>,
}

impl MotorBuilder {
    /// A model-3 or model-2 axis.
    pub fn new(motor: Arc<Mutex<dyn AsynMotor>>) -> Self {
        Self::on(Driver::Polled(motor))
    }

    /// A model-1 axis; its [`Self::trace_user`] is a [`TraceUser::Model1`].
    pub fn model1(motor: Arc<Mutex<dyn Model1Motor>>) -> Self {
        Self::on(Driver::Model1(motor))
    }

    fn on(motor: Driver) -> Self {
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
        let model1_user = matches!(self.user, TraceUser::Model1 { .. });
        let motor = match self.motor {
            Driver::Polled(motor) => {
                assert!(
                    !model1_user,
                    "a model-1 axis is built with MotorBuilder::model1"
                );
                motor
            }
            Driver::Model1(motor) => {
                assert!(model1_user, "a model-1 axis has a TraceUser::Model1");
                // A model-1 driver's task scans at its moving period alone.
                let mut task = MotorTaskBuilder::new(self.moving_poll_interval);
                let ControllerAxis { axis, poll_cmd_tx } =
                    task.axis_with(motor, self.user, self.timeout, self.poll_channel_capacity);
                return AxisSetup {
                    axis,
                    poll_loop: task.build(),
                    poll_cmd_tx,
                };
            }
        };
        let mut controller = ControllerBuilder::new();
        // A controller of one axis is its port's poller.
        if let TraceUser::Asyn { port, .. } = &self.user {
            controller = controller.port(port);
        }
        let ControllerAxis { axis, poll_cmd_tx } =
            controller.axis_with(motor, self.user, self.timeout, self.poll_channel_capacity);
        AxisSetup {
            axis,
            poll_loop: controller.build(
                self.moving_poll_interval,
                self.idle_poll_interval,
                self.forced_fast_polls,
            ),
            poll_cmd_tx,
        }
    }

    /// One record bound to a fresh axis, past both `init_record` passes as
    /// an IOC's is: `configure_record` is its load, a later put a runtime
    /// put through special().
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
        // init_record never fails for a motor record.
        for pass in [0, 1] {
            let _ = record.init_record(pass);
        }
        MotorSetup {
            record,
            device_support,
            poll_loop,
            poll_cmd_tx,
        }
    }
}

/// An axis's record half and the place its poller takes; the axis of
/// `poll`'s controller at `slot` from now on, `None` while another
/// controller's axis holds it.
fn axis_parts(
    motor: &Arc<Mutex<dyn AsynMotor>>,
    user: TraceUser,
    timeout: Duration,
    capacity: usize,
    slot: Arc<AxisSlot>,
    poll: &SharedControllerPoll,
) -> Option<(MotorAxis, Place)> {
    let disables_ntm = motor.lock().is_ok_and(|m| m.disables_ntm());
    let (axis, place) = MotorAxis::placed(user, motor.clone(), timeout, slot, poll, capacity)?;
    Some((axis.disabling_ntm(disables_ntm), place))
}

/// An axis of a [`ControllerBuilder`], for a create command to register.
pub struct ControllerAxis {
    pub axis: MotorAxis,
    pub poll_cmd_tx: mpsc::Sender<PollCommand>,
}

impl ControllerAxis {
    fn of(axis: MotorAxis) -> Self {
        Self {
            poll_cmd_tx: axis.poll_cmd_tx().clone(),
            axis,
        }
    }
}

/// The axes of one asynMotorController and the one poller that polls them
/// in turn: C `startPoller`'s thread (asynMotorController.cpp:567-575).
#[derive(Default)]
pub struct ControllerBuilder {
    axes: ControllerAxes,
}

/// A controller's axis list, which C's poller reads on every pass
/// (asynMotorController.cpp:655-661): an axis added once the poller runs
/// joins it, polled in address order with the rest.
#[derive(Clone)]
pub struct ControllerAxes(Arc<Mutex<Axes>>);

struct Axes {
    list: AxisList,
    /// The controller poll, which each axis's UPDATE_STATUS write runs too.
    poll: SharedControllerPoll,
    /// Each driver axis added, once: a later add of the same one is another
    /// address of the axis already polled.
    opened: Vec<Opened>,
}

struct Opened {
    motor: Arc<Mutex<dyn AsynMotor>>,
    axis: MotorAxis,
    /// The controller port address the status is mirrored to, and the axis
    /// there, which a later add at the address returns.
    mirror: Option<((String, i32), MotorAxis)>,
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
            poll: SharedControllerPoll::default(),
            opened: Vec::new(),
        })))
    }
}

/// An axis [`ControllerAxes::queue`] placed or sent to the running poller.
struct Queued {
    added: ControllerAxis,
    /// The motor sent to the running poller, and the signal it took it.
    joined: Option<(Arc<Mutex<dyn AsynMotor>>, std::sync::mpsc::Receiver<()>)>,
}

impl ControllerAxes {
    /// Count a first poll pending on this controller until the guard drops:
    /// a driver's own poller thread (omsBaseController's omsPoller) holds
    /// one over its first pass.
    pub fn first_poll_pending(&self) -> FirstPollPending {
        let axes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        axes.poll.first_poll_pending()
    }

    /// This controller's startup pass if it has not ended, and the
    /// [`FirstPollPending`]s its driver holds. These polls run on the poller
    /// thread, as C's do, so a reader that wants what they set waits for
    /// this to reach 0.
    pub fn first_polls_pending(&self) -> usize {
        let axes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        axes.poll.first_polls_pending()
    }

    /// C `startMoveToHomeThread`: a MOTOR_MOVE_HOME write then runs the
    /// axis's [`AsynMotor::move_to_home_task`] on this controller's
    /// `motorMoveToHome` thread. Without it the write only signals.
    pub fn start_move_to_home_thread(&self) -> std::io::Result<()> {
        let axes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        axes.poll.start_move_to_home_thread()
    }

    /// C `setMovingPollPeriod` / `setIdlePollPeriod` for a poller no port
    /// names, a driver's own thread (pmacAsynCoord.c:981-1031): the periods
    /// given replace the running poller's, then it runs a pass.
    pub fn set_poll_periods(&self, moving: Option<Duration>, idle: Option<Duration>) {
        let axes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(period) = moving {
            axes.poll.set_moving_period(period);
        }
        if let Some(period) = idle {
            axes.poll.set_idle_period(period);
        }
        axes.poll.wakeup();
    }

    /// Add an axis; on a running poller, return once the poller has polled
    /// it. A `motor` added before is not polled twice: `user` reaches the
    /// same axis.
    pub fn add(
        &self,
        motor: Arc<Mutex<dyn AsynMotor>>,
        user: TraceUser,
    ) -> Result<ControllerAxis, String> {
        self.add_with(motor, user, DEFAULT_TIMEOUT, DEFAULT_POLL_CHANNEL_CAPACITY)
    }

    /// [`Self::add`] for a driver whose C constructs the axis under
    /// `pC->lock()` (MVP2001Driver.cpp:222-224): `build` runs under the
    /// controller lock, so a pass that comes due meanwhile waits for it and
    /// then polls the new axis.
    pub fn add_built<T, E: From<String>>(
        &self,
        user: TraceUser,
        build: impl FnOnce() -> Result<(Arc<Mutex<dyn AsynMotor>>, T), E>,
    ) -> Result<(ControllerAxis, T), E> {
        let (mut added, done) = self.add_built_all([(user, build)]);
        done?;
        Ok(added.pop().expect("the one build succeeded"))
    }

    /// [`Self::add_built`] for every `(user, build)` under one hold of the
    /// controller lock, as pmacCreateAxes builds its axes
    /// (pmacController.cpp:974-979): no pass polls between two builds. The
    /// axes added, then what stopped the batch: a build's error ends it, the
    /// axes built before it staying added.
    pub fn add_built_all<T, E: From<String>, F>(
        &self,
        builds: impl IntoIterator<Item = (TraceUser, F)>,
    ) -> (Vec<(ControllerAxis, T)>, Result<(), E>)
    where
        F: FnOnce() -> Result<(Arc<Mutex<dyn AsynMotor>>, T), E>,
    {
        let poll = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .poll
            .clone();
        let held = poll.hold();
        let mut queued = Vec::new();
        let mut done = Ok(());
        for (user, build) in builds {
            let queue = build().and_then(|(motor, built)| {
                let q = self.queue(motor, user, DEFAULT_TIMEOUT, DEFAULT_POLL_CHANNEL_CAPACITY)?;
                Ok((q, built))
            });
            match queue {
                Ok(q) => queued.push(q),
                Err(e) => {
                    done = Err(e);
                    break;
                }
            }
        }
        drop(held);
        let mut added = Vec::new();
        for (q, built) in queued {
            match self.confirm(q) {
                Ok(axis) => added.push((axis, built)),
                Err(e) if done.is_ok() => done = Err(e.into()),
                Err(_) => {}
            }
        }
        (added, done)
    }

    fn add_with(
        &self,
        motor: Arc<Mutex<dyn AsynMotor>>,
        user: TraceUser,
        timeout: Duration,
        capacity: usize,
    ) -> Result<ControllerAxis, String> {
        let queued = self.queue(motor, user, timeout, capacity)?;
        self.confirm(queued)
    }

    /// Place the axis, or send it to the running poller. The controller
    /// lock, if the caller holds it, must be released before
    /// [`Self::confirm`]: the poller takes it to place the axis.
    fn queue(
        &self,
        motor: Arc<Mutex<dyn AsynMotor>>,
        user: TraceUser,
        timeout: Duration,
        capacity: usize,
    ) -> Result<Queued, String> {
        let slot = crate::devices::claim(&user)?;
        let mirror = match &user {
            TraceUser::Asyn { port, addr } => Some((port.clone(), *addr)),
            _ => None,
        };
        let address = mirror.clone();
        let held = || match &address {
            Some((port, addr)) => {
                format!("motor port {port} address {addr} is another controller's")
            }
            None => "the address is another controller's".to_owned(),
        };
        // A drvMotorAsyn port's callback posts the axis.
        let callback = matches!(user, TraceUser::Model2 { .. });
        let same = |o: &Opened| std::ptr::addr_eq(Arc::as_ptr(&o.motor), Arc::as_ptr(&motor));
        let mut axes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        // Recorded before the poller confirms, so a concurrent add of the
        // same motor finds it.
        let (join, added) = if let Some(opened) = axes.opened.iter_mut().find(|o| same(o)) {
            match (mirror, &opened.mirror) {
                (None, _) if callback => (
                    Join::Callback {
                        motor: motor.clone(),
                    },
                    ControllerAxis::of(opened.axis.at(user)),
                ),
                (None, _) => {
                    return Ok(Queued {
                        added: ControllerAxis::of(opened.axis.at(user)),
                        joined: None,
                    });
                }
                (Some(to), Some((had, at))) if &to == had => {
                    return Ok(Queued {
                        added: ControllerAxis::of(at.at(user)),
                        joined: None,
                    });
                }
                (Some((port, addr)), Some(((had_port, had_addr), _))) => {
                    return Err(format!(
                        "the axis is at port {had_port} address {had_addr}, not {port} {addr}"
                    ));
                }
                (Some((port, addr)), None) => {
                    // The address's queue and records, which the axis there
                    // before leaves to this one.
                    let (at, place) = opened
                        .axis
                        .placed_too(user, slot.unwrap_or_default(), capacity)
                        .ok_or_else(held)?;
                    opened.mirror = Some(((port.clone(), addr), at.clone()));
                    let motor = motor.clone();
                    let poll = axes.poll.clone();
                    (
                        Join::Mirror {
                            motor,
                            port,
                            addr,
                            poll,
                            place,
                        },
                        ControllerAxis::of(at),
                    )
                }
            }
        } else {
            let slot = slot.unwrap_or_default();
            let (axis, place) =
                axis_parts(&motor, user, timeout, capacity, slot, &axes.poll).ok_or_else(held)?;
            let axis = axis.with_controller_poll(axes.poll.clone());
            axes.opened.push(Opened {
                motor: motor.clone(),
                axis: axis.clone(),
                mirror: mirror.clone().map(|address| (address, axis.clone())),
            });
            let mut polled = Axis::polled(place, motor.clone(), axis.params().clone());
            if let Some((port, addr)) = mirror {
                polled.construct_asyn_motor_axis();
                polled.mirror_to(&port, addr, axes.poll.clone());
            }
            if callback {
                polled.post_by_callback();
            }
            (Join::Axis(Box::new(polled)), ControllerAxis::of(axis))
        };
        // Named only once the address is placed: a refused add sets nothing.
        if let Some((port, _)) = &address {
            axes.poll.set_port(port);
        }
        // The axis this one replaces at the address leaves the poller too
        // (`Join::place`).
        if address.is_some() {
            axes.opened
                .retain(|o| same(o) || o.mirror.as_ref().map(|(at, _)| at) != address.as_ref());
        }
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
        Ok(Queued {
            added,
            joined: joined.map(|taken| (motor, taken)),
        })
    }

    /// Wait for the running poller to take a queued axis.
    fn confirm(&self, queued: Queued) -> Result<ControllerAxis, String> {
        if let Some((motor, taken)) = queued.joined
            && taken.recv().is_err()
        {
            // No poller took it: a later add must not get its senders.
            let mut axes = self.0.lock().unwrap_or_else(|e| e.into_inner());
            axes.opened
                .retain(|o| !std::ptr::addr_eq(Arc::as_ptr(&o.motor), Arc::as_ptr(&motor)));
            return Err("the controller's poller has ended".to_string());
        }
        Ok(queued.added)
    }

    /// The axes so far, and the channel later ones join the poller by.
    fn start(
        &self,
    ) -> (
        Vec<Axis>,
        mpsc::UnboundedReceiver<Joining>,
        SharedControllerPoll,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut axes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let poll = axes.poll.clone();
        match std::mem::replace(&mut axes.list, AxisList::Running(tx)) {
            AxisList::Pending(axes) => (axes, rx, poll),
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
    /// axes on every pass and on the startup pass. What it returns is what
    /// it set on its own port, which the poller applies once it returns.
    pub fn controller_poll<R: Into<PortSets>>(
        self,
        mut poll: impl FnMut() -> R + Send + 'static,
    ) -> Self {
        let axes = self.axes.0.lock().unwrap_or_else(|e| e.into_inner());
        axes.poll.set(Box::new(move || poll().into()));
        drop(axes);
        self
    }

    /// The controller port this poller is, so setMovingPollPeriod and
    /// setIdlePollPeriod reach it before any axis on the port names it.
    pub fn port(self, port: &str) -> Self {
        let axes = self.axes.0.lock().unwrap_or_else(|e| e.into_inner());
        axes.poll.set_port(port);
        drop(axes);
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
        let (axes, joining, controller_poll) = self.axes.start();
        MotorPollLoop::controller(
            axes,
            Some(joining),
            moving_poll_interval,
            idle_poll_interval,
            forced_fast_polls,
            controller_poll,
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
    pub fn axis(&mut self, motor: Arc<Mutex<dyn Model1Motor>>, user: TraceUser) -> ControllerAxis {
        self.axis_with(motor, user, DEFAULT_TIMEOUT, DEFAULT_POLL_CHANNEL_CAPACITY)
    }

    fn axis_with(
        &mut self,
        motor: Arc<Mutex<dyn Model1Motor>>,
        user: TraceUser,
        timeout: Duration,
        capacity: usize,
    ) -> ControllerAxis {
        let polled: Arc<Mutex<dyn AsynMotor>> = motor.clone();
        let poll = SharedControllerPoll::default();
        let (axis, place) = axis_parts(&polled, user, timeout, capacity, Arc::default(), &poll)
            .expect("a new slot is no controller's");
        self.axes.push(Axis::task(place, motor));
        ControllerAxis::of(axis)
    }

    /// The task over the axes.
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
    use crate::controller_port::WriteValue;
    use crate::flags::MotorCommand;

    type Log = Arc<Mutex<Vec<(&'static str, ThreadId, Instant)>>>;

    const SLOW: Duration = Duration::from_millis(40);

    /// Waits for `log` to hold `n` polls.
    async fn logged<T>(log: &Mutex<Vec<T>>, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while log.lock().unwrap().len() < n {
            assert!(Instant::now() < deadline, "{n} polls never ran");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// A card that never ends its move; each poll is logged, then takes
    /// `delay`.
    struct Card {
        name: &'static str,
        log: Log,
        delay: Duration,
    }

    impl Model1Motor for Card {
        fn take_init_status(&mut self) -> MotorStatus {
            MotorStatus::default()
        }
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
        fn move_relative(
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
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            let polled = (self.name, std::thread::current().id(), Instant::now());
            self.log.lock().unwrap().push(polled);
            std::thread::sleep(self.delay);
            *status = MotorStatus {
                done: false,
                moving: true,
                ..MotorStatus::default()
            };
            Ok(())
        }
    }

    /// drvMotorSim's report dumps the axis's paramLib (drvMotorSim.c:186,
    /// paramLib.c:299-317): what a record's write stored there reads back by
    /// port and address.
    #[test]
    fn a_model2_axis_s_stored_parameters_read_by_port_and_address() {
        let mut controller = ControllerBuilder::new();
        let card: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Card {
            name: "P",
            log: Log::default(),
            delay: Duration::ZERO,
        }));
        let user = TraceUser::Model2 {
            port: "builderParams2".into(),
            addr: 0,
        };
        let ControllerAxis { axis, .. } = controller.axis(card.clone(), user);
        let record_half = axis.at(axis.address().clone());
        let handle =
            crate::poll_loop::drv_motor_asyn_stand_in::port("builderParams2", card, record_half);
        crate::devices::register_drv_motor_asyn_port("builderParams2", 1, handle).unwrap();
        axis.send(MotorCommand::SetHighLimit { position: 2.5 });
        crate::devices::register_axis(axis).unwrap();
        controller.build(SLOW, SLOW, 0).run_queued_commands();

        let params = crate::devices::model2_params("builderParams2", 0).unwrap();
        assert_eq!(params.get(4), Some(WriteValue::Float64(2.5)));
        let mut dump = String::new();
        params.dump(&mut dump);
        let mut want = String::from("Number of parameters is: 28\n");
        for i in 0..28 {
            want += &match i {
                4 => format!("Parameter {i} is a double, value 2.500000\n"),
                _ => format!("Parameter {i} is undefined\n"),
            };
        }
        assert_eq!(dump, want);
        assert!(crate::devices::model2_params("builderParams2", 1).is_none());
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
        // Both moves are queued before the task's first wake drains them. A
        // move landing after it is queried a wake later, in C too
        // (motordrvCom.cc:241-263).
        for ControllerAxis { axis, .. } in &axes {
            axis.send(MotorCommand::MoveAbsolute {
                position: 1.0,
                min_velocity: 0.0,
                velocity: 1.0,
                acceleration: 1.0,
            });
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let running = tokio::spawn(task.start());
            logged(&log, 4).await;
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
        fn move_relative(
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
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            self.1.lock().unwrap().push(self.0);
            *status = MotorStatus::default();
            Ok(())
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
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        // Past the polls waited for, time for any more.
        let settle = Duration::from_millis(50);
        let (forced, info) = rt.block_on(async {
            let running = tokio::spawn(poller.start());
            logged(&log, 2).await;
            tokio::time::sleep(settle).await;
            // The startup pass.
            log.lock().unwrap().clear();
            poll_cmd_tx.send(PollCommand::ForcePoll).await.unwrap();
            logged(&log, 2).await;
            tokio::time::sleep(settle).await;
            let forced = std::mem::take(&mut *log.lock().unwrap());
            axis.send(MotorCommand::Poll);
            logged(&log, 2).await;
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
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let polled = rt.block_on(async {
            let running = tokio::spawn(poller.start());
            logged(&log, 2).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let startup = std::mem::take(&mut *log.lock().unwrap());
            assert_eq!(startup, ["A", "B"], "the startup pass");
            second
                .poll_cmd_tx
                .send(PollCommand::ForcePoll)
                .await
                .unwrap();
            logged(&log, 1).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let polled = std::mem::take(&mut *log.lock().unwrap());
            first.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
            running.await.unwrap();
            polled
        });
        assert_eq!(polled, ["A"], "the second address reaches the one axis");
    }

    /// pmacAsynCoord.c:981-1031: a period set on the running poller wakes
    /// it, and the waits after that pass are the new period's.
    #[test]
    fn periods_set_on_a_running_poller_are_the_next_waits() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut controller = ControllerBuilder::new();
        let ControllerAxis { poll_cmd_tx, .. } = controller.axis(
            Arc::new(Mutex::new(Named("A", log.clone()))),
            TraceUser::Asyn {
                port: String::new(),
                addr: 0,
            },
        );
        let axes = controller.axes();
        let hour = Duration::from_secs(3600);
        let poller = controller.build(hour, hour, 1);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let passes = |n: usize| {
            let log = log.clone();
            async move {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while log.lock().unwrap().len() < n && std::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                log.lock().unwrap().len()
            }
        };
        let ms = Duration::from_millis(10);
        let (moving, idle) = rt.block_on(async {
            let running = tokio::spawn(poller.start());
            assert_eq!(passes(1).await, 1, "the startup pass");
            // The woken pass, then its one forced fast poll.
            axes.set_poll_periods(Some(ms), None);
            let moving = passes(3).await;
            tokio::time::sleep(10 * ms).await;
            let moving = moving.max(log.lock().unwrap().len());
            axes.set_poll_periods(None, Some(ms));
            let idle = passes(moving + 4).await - moving;
            poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
            running.await.unwrap();
            (moving, idle)
        });
        assert_eq!(moving, 3);
        assert_eq!(idle, 4);
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
            let running = tokio::spawn(poller.start());
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
