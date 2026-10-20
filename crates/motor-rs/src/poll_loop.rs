use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::interfaces::motor::{
    AsynMotor, Model1Motor, MotorStatus, PollFailure, PortSet, PortSets, StatusWrite,
};
use asyn_rs::port_handle::{ForbidBlocking, forbid_blocking};
use asyn_rs::trace::TraceMask;
use asyn_rs::user::AsynUser;
use epics_base_rs::runtime::task::{
    StackSizeClass, ThreadPriority, block_on_sync, sleep_background, spawn_dedicated_thread,
};
use epics_libcom_rs::cfmt::CFloat;
use tokio::sync::{mpsc, oneshot};

use crate::controller_port::{FinishedWrite, WriteValue};
use crate::device_state::*;
use crate::flags::{MotorCommand, PidGainKind, Speed, StoredSpeeds};
use crate::trace::MotorTrace;

/// Queue of what reaches an axis's poller thread from outside it.
///
/// Unbounded like the asyn request queue C `devMotorAsyn::build_trans` feeds,
/// so a record pass never waits on the driver.
pub type MotorCommandSender = mpsc::UnboundedSender<ToAxis>;

/// What reaches an axis's poller, in the order sent.
#[derive(Debug)]
pub enum ToAxis {
    /// A record pass's commands, for the poller to run.
    Batch(CommandBatch),
    /// What a controller-port write already ran on the port thread set on
    /// the axis, for the poller to post.
    Written(Written),
    /// A model-2 driver's parameter callback: the status drvMotorAsyn's
    /// `intCallback` hands every record on the axis (drvMotorAsyn.c:822-920).
    Callback(MotorStatus),
}

impl From<CommandBatch> for ToAxis {
    fn from(batch: CommandBatch) -> Self {
        Self::Batch(batch)
    }
}

impl From<Written> for ToAxis {
    fn from(written: Written) -> Self {
        Self::Written(written)
    }
}

/// One record pass's commands, with the record's trace: C's asynUser that
/// `build_trans` queued them on, which `asynCallback` prints through and
/// whose record a finished move processes. `record` is `None` for a
/// harness's batch, which processes no record.
#[derive(Debug)]
pub struct CommandBatch {
    pub trace: Arc<MotorTrace>,
    pub record: Option<RecordId>,
    pub commands: Vec<MotorCommand>,
}

/// A driver call a controller-port write made on the port thread: what it
/// set on the axis, which C's `pAxis->callParamCallbacks()` posts before
/// the write returns (asynMotorController.cpp:243, :417).
#[derive(Debug)]
pub struct Written {
    write: AxisWrite,
    /// Auto power switched the axis on before the move.
    powered: bool,
}

impl Written {
    pub(crate) fn new(write: AxisWrite, powered: bool) -> Self {
        Self { write, powered }
    }
}

/// Commands sent to the poll loop. The axis polls while any bound record
/// wants it, so one record settling cannot starve another of posts.
#[derive(Debug)]
pub enum PollCommand {
    /// Poll now, posting even when nothing changed: C `motorUpdateStatus_`
    /// (asynMotorController.cpp:217-222). The timed polls never stop: C's
    /// `asynMotorPoller` has no request that halts them (:615-696).
    ForcePoll,
    /// C `wakeupPoller` from outside the poller, as a driver's own thread
    /// sends after commanding a move: a pass now, with the forced fast
    /// polls reloaded.
    Wakeup,
    /// A driver thread's own status sets and `callParamCallbacks` on the
    /// axis, as OMS's `omsPoller` makes: posted only if it changed the last
    /// status (`statusChanged_`, asynMotorAxis.cpp:316-322). The driver is
    /// not polled and no fast polls are reloaded. Send with `blocking_send`.
    Post(MotorStatus),
    Shutdown,
}

/// The poller's two queues: poll control, and the driver commands a record
/// pass queued (drained first, so a ForcePoll sees their effect).
pub struct PollerInbox {
    pub poll: mpsc::Receiver<PollCommand>,
    pub commands: mpsc::UnboundedReceiver<ToAxis>,
}

/// An address's queue and the records bound there, as the axis there takes
/// them. The queue is the address's, as C queues a record's request on the
/// port and its port thread finds the axis when it runs it
/// (asynMotorController.cpp:532-537): an axis placed at the address takes
/// over what the one before it left.
pub(crate) struct Place(Arc<PlaceState>);

struct PlaceState {
    queue: Mutex<AxisQueue>,
    records: AxisSubscribers,
}

struct AxisQueue {
    inbox: PollerInbox,
    /// Batches [`Axis::take_posts`] took off `inbox`, to run ahead of
    /// anything still on it.
    batches: std::collections::VecDeque<CommandBatch>,
}

/// The [`Place`] the axes at an address hold, gone with the last of them.
#[derive(Default)]
pub(crate) struct PlaceRef(std::sync::Weak<PlaceState>);

impl Place {
    pub(crate) fn new(inbox: PollerInbox, records: AxisSubscribers) -> Self {
        Self(Arc::new(PlaceState {
            queue: Mutex::new(AxisQueue {
                inbox,
                batches: std::collections::VecDeque::new(),
            }),
            records,
        }))
    }

    pub(crate) fn downgrade(&self) -> PlaceRef {
        PlaceRef(Arc::downgrade(&self.0))
    }

    fn records(&self) -> &AxisSubscribers {
        &self.0.records
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, AxisQueue> {
        self.0.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl PlaceRef {
    pub(crate) fn upgrade(&self) -> Option<Place> {
        self.0.upgrade().map(Place)
    }
}

/// Motor poll loop — one per controller, polling its axes in turn and
/// posting each to every record bound to it.
///
/// It is the only caller of the drivers after `init`: polls and the records'
/// commands all run on its own `motorPoller` OS thread (see [`Self::start`]),
/// as C runs one `asynMotorPoller` per controller on an epicsThread
/// (asynMotorController.cpp:615-695), so a driver blocked on a dead serial
/// line or TCP peer never stalls the IOC's async executor, and a slow axis
/// delays its controller's other axes as in C.
pub struct MotorPollLoop {
    axes: Vec<Axis>,
    /// Axes added to the controller once the poller runs.
    joining: Option<mpsc::UnboundedReceiver<Joining>>,
    schedule: Schedule,
    controller_poll: SharedControllerPoll,
}

/// An add to a running poller, and the reply once it has taken it.
pub(crate) struct Joining {
    pub(crate) join: Join,
    pub(crate) joined: std::sync::mpsc::SyncSender<()>,
}

/// What an add puts in a controller's axis list.
pub(crate) enum Join {
    /// A driver axis not added before.
    Axis(Box<Axis>),
    /// A controller port address for the axis of `motor`, added before
    /// with none: its status is mirrored there, and it takes `place`.
    Mirror {
        motor: Arc<Mutex<dyn AsynMotor>>,
        port: String,
        addr: i32,
        poll: SharedControllerPoll,
        place: Place,
    },
    /// A drvMotorAsyn port's open of the axis of `motor`, added before by
    /// another user: from now on only the driver's callback posts it.
    Callback { motor: Arc<Mutex<dyn AsynMotor>> },
}

impl Join {
    /// Apply the add to `axes`, kept in address order.
    pub(crate) fn place(self, axes: &mut Vec<Axis>) {
        let of = |motor: &Arc<Mutex<dyn AsynMotor>>| {
            axes.iter()
                .position(|a| std::ptr::addr_eq(Arc::as_ptr(&a.motor), Arc::as_ptr(motor)))
        };
        let axis = match self {
            Self::Axis(axis) => *axis,
            Self::Mirror {
                motor,
                port,
                addr,
                poll,
                place,
            } => {
                let Some(at) = of(&motor) else {
                    return;
                };
                let mut axis = axes.remove(at);
                axis.mirror_to(&port, addr, poll);
                axis.take(place);
                axis
            }
            Self::Callback { motor } => {
                if let Some(at) = of(&motor) {
                    axes[at].post_by_callback();
                }
                return;
            }
        };
        // C's poller polls only `pAxes_` (asynMotorController.cpp:651-652),
        // where a later axis at the address replaced the earlier one.
        if axis.controller.is_some() {
            axes.retain(|a| a.controller != axis.controller);
        }
        let at = axes.partition_point(|a| a.order() <= axis.order());
        axes.insert(at, axis);
    }
}

/// C `asynMotorController::poll`, run before the axes on every pass,
/// returning what it set on its own port as C's sets under the controller
/// lock.
pub type ControllerPoll = Box<dyn FnMut() -> PortSets + Send>;

/// A controller's [`ControllerPoll`], shared by its poller and the port
/// thread's UPDATE_STATUS write, each of which runs it before an axis poll,
/// and the controller's lock.
#[derive(Clone, Default)]
pub(crate) struct SharedControllerPoll {
    state: Arc<Mutex<ControllerPollState>>,
    lock: Arc<Mutex<()>>,
    move_to_home: Arc<MoveToHomeEvent>,
    queue: Arc<Mutex<PortQueue>>,
    /// C `pollEventId_`, a binary event: a wakeup with no pass waiting is
    /// kept for the next wait.
    wake: Arc<tokio::sync::Notify>,
    first_polls: FirstPolls,
}

/// A controller's [`FirstPollPending`]s, each one listed in the process's.
#[derive(Clone)]
struct FirstPolls(Arc<std::sync::atomic::AtomicUsize>);

fn every_first_polls() -> &'static Mutex<Vec<std::sync::Weak<std::sync::atomic::AtomicUsize>>> {
    static EVERY: std::sync::OnceLock<Mutex<Vec<std::sync::Weak<std::sync::atomic::AtomicUsize>>>> =
        std::sync::OnceLock::new();
    EVERY.get_or_init(Default::default)
}

impl Default for FirstPolls {
    fn default() -> Self {
        let count = Arc::default();
        let mut every = every_first_polls()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        every.retain(|c| c.strong_count() > 0);
        every.push(Arc::downgrade(&count));
        Self(count)
    }
}

/// What was set on the controller port's parameters off its thread, in the
/// order set. Only the port's thread applies it, first thing each time it
/// takes the controller lock, so a write or poll there lands after
/// everything set under the lock before it, as C's sets land in lock order.
#[derive(Default)]
struct PortQueue {
    sets: Vec<(Option<i32>, Vec<PortSet>)>,
    /// A request to apply `sets` is on the port's queue.
    flush_queued: bool,
}

/// C `moveToHomeId_` and `moveToHomeAxis_`: a binary event and the one
/// axis the last MOTOR_MOVE_HOME named.
#[derive(Default)]
struct MoveToHomeEvent {
    axis: Mutex<Option<i32>>,
    signalled: std::sync::Condvar,
}

/// C's controller lock, held across a poller pass (asynMotorController.cpp:
/// 644-658) and every driver call, so a port write never lands between the
/// controller poll and the axis polls that read what it read. No blocking
/// request to the controller port may be made under it: the port's thread
/// may be waiting on it.
pub(crate) struct ControllerHeld<'a> {
    _guard: std::sync::MutexGuard<'a, ()>,
}

#[derive(Default)]
struct ControllerPollState {
    poll: Option<ControllerPoll>,
    /// The controller port, known once an axis on it is added.
    port: Option<String>,
    /// C `movingPollPeriod_` and `idlePollPeriod_`, which the poller reads
    /// at the end of each pass and setMovingPollPeriod / setIdlePollPeriod
    /// change at runtime.
    periods: PollPeriods,
}

#[derive(Clone, Copy, Default)]
struct PollPeriods {
    moving: Duration,
    idle: Duration,
}

impl SharedControllerPoll {
    fn state(&self) -> std::sync::MutexGuard<'_, ControllerPollState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `other` is this controller's.
    pub(crate) fn is(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// Count a first poll pending on this controller until the guard drops.
    pub(crate) fn first_poll_pending(&self) -> FirstPollPending {
        FirstPollPending::begin(&self.first_polls.0)
    }

    /// The [`FirstPollPending`]s this controller holds.
    pub(crate) fn first_polls_pending(&self) -> usize {
        self.first_polls.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Take the controller's lock.
    pub(crate) fn hold(&self) -> ControllerHeld<'_> {
        ControllerHeld {
            _guard: self.lock.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }

    pub(crate) fn set(&self, poll: ControllerPoll) {
        self.state().poll = Some(poll);
    }

    /// C `asynMotorController::wakeupPoller`: the poller runs a pass now,
    /// whether or not the controller has an axis yet.
    pub(crate) fn wakeup(&self) {
        self.wake.notify_one();
    }

    /// C `asynMotorController::setMovingPollPeriod`, less its wakeupPoller.
    pub(crate) fn set_moving_period(&self, period: Duration) {
        self.state().periods.moving = period;
    }

    /// C `asynMotorController::setIdlePollPeriod`, less its wakeupPoller.
    pub(crate) fn set_idle_period(&self, period: Duration) {
        self.state().periods.idle = period;
    }

    /// The controller port the queued sets go to; the startup pass can run
    /// before any axis names it. This is also C `startPoller` on `port`:
    /// setMovingPollPeriod, setIdlePollPeriod and the port's first-poll
    /// count find this poller by `port`, whoever named it.
    pub(crate) fn set_port(&self, port: &str) {
        let mut state = self.state();
        if state.port.is_some() {
            return;
        }
        state.port = Some(port.to_owned());
        drop(state);
        crate::devices::link_controller_poll(port, self);
        let mut queue = self.queue();
        if !queue.sets.is_empty() {
            self.flush(port, &mut queue);
        }
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, PortQueue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// C's `setXxxParam` / `callParamCallbacks` on the controller port from
    /// off its thread. An axis's status or wakeup goes to the poller at
    /// once; a parameter set joins the queue the port's thread applies, so
    /// nothing here waits on that thread, which may be waiting on the lock.
    pub(crate) fn post_sets(&self, axis: Option<i32>, sets: PortSets) {
        let port = self.state().port.clone();
        let mut queued = Vec::new();
        for set in sets {
            match (&port, set) {
                (Some(port), set @ (PortSet::Status { .. } | PortSet::WakeupPoller)) => {
                    apply_axis_set(port, set)
                }
                (_, set) => queued.push(set),
            }
        }
        if queued.is_empty() {
            return;
        }
        let mut queue = self.queue();
        queue.sets.push((axis, queued));
        if let Some(port) = &port {
            self.flush(port, &mut queue);
        }
    }

    /// Queue on the port's thread a request that takes the lock and applies
    /// the queued sets, unless one is queued already.
    fn flush(&self, port: &str, queue: &mut PortQueue) {
        if queue.flush_queued {
            return;
        }
        let Some(handle) = crate::devices::controller_port_handle(port) else {
            return;
        };
        let poll = self.clone();
        let call = asyn_rs::request::DriverCall::new(move |driver| {
            let held = poll.hold();
            poll.drain_on_port(&held, driver.base_mut());
        });
        let op = asyn_rs::request::RequestOp::WithDriver(call);
        queue.flush_queued = handle.try_submit(op, AsynUser::default()).is_ok();
    }

    /// Apply on the port's thread, under the lock, what was set off it.
    pub(crate) fn drain_on_port(
        &self,
        _: &ControllerHeld<'_>,
        base: &mut asyn_rs::port::PortDriverBase,
    ) {
        let sets = {
            let mut queue = self.queue();
            queue.flush_queued = false;
            std::mem::take(&mut queue.sets)
        };
        for (axis, sets) in sets {
            crate::controller_port::apply_port_sets(base, axis, sets);
        }
    }

    /// Run the poll on the controller port's own thread and return its
    /// sets, for the port to apply on its base.
    pub(crate) fn run(&self, _: &ControllerHeld<'_>) -> PortSets {
        let mut state = self.state();
        let _forbid = state.port.clone().map(|p| forbid_blocking(&p));
        state.poll.as_mut().map(|poll| poll()).unwrap_or_default()
    }

    /// Run the poll on the poller, its sets queued for the port.
    fn run_on_poller(&self, _: &ControllerHeld<'_>) {
        let mut state = self.state();
        let forbid = state.port.clone().map(|p| forbid_blocking(&p));
        let Some(poll) = state.poll.as_mut() else {
            return;
        };
        let sets = poll();
        drop(forbid);
        drop(state);
        self.post_sets(None, sets);
    }

    /// C `moveToHomeAxis_ = axis; epicsEventSignal(moveToHomeId_)`; with no
    /// moveToHome thread nothing waits on it.
    pub(crate) fn signal_move_to_home(&self, axis: i32) {
        *self
            .move_to_home
            .axis
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(axis);
        self.move_to_home.signalled.notify_one();
    }

    /// C `asynMotorController::startMoveToHomeThread`
    /// (asynMotorController.cpp:703-710).
    pub(crate) fn start_move_to_home_thread(&self) -> std::io::Result<()> {
        let poll = self.clone();
        std::thread::Builder::new()
            .name("motorMoveToHome".into())
            .spawn(move || {
                loop {
                    poll.move_to_home();
                }
            })
            .map(drop)
    }

    /// One wake of C's `asynMotorMoveToHome` (asynMotorController.cpp:
    /// 723-742): the axis's task is taken under the controller lock and run
    /// without it, as C runs `doMoveToHome`.
    fn move_to_home(&self) {
        let event = &self.move_to_home;
        let mut slot = event.axis.lock().unwrap_or_else(|e| e.into_inner());
        let addr = loop {
            if let Some(addr) = slot.take() {
                break addr;
            }
            slot = event
                .signalled
                .wait(slot)
                .unwrap_or_else(|e| e.into_inner());
        };
        drop(slot);
        let Some(port) = self.state().port.clone() else {
            return;
        };
        let Some(axis) = crate::devices::axis_at(&port, addr) else {
            return;
        };
        let task = {
            let held = self.hold();
            let Some(mut motor) = lock_axis(axis.motor(), Some(&port), &held) else {
                return;
            };
            motor.move_to_home_task(axis.referencing_mode_move())
        };
        let trace = MotorTrace::with_user(axis.address().clone());
        let Some(task) = task else {
            trace.print(
                TraceMask::ERROR,
                0,
                format_args!("asynMotorAxis:doMoveToHome: Axis={addr} no implementation\n"),
            );
            return;
        };
        let under_lock = |sets: PortSets| {
            let _held = self.hold();
            self.post_sets(Some(addr), sets);
        };
        if task(&under_lock).is_err() {
            trace.print_self(
                TraceMask::ERROR,
                format_args!(
                    "asynMotorController:asynMotorMoveToHome: move to home failed in asynMotorController::asynMotorMoveToHome. Axis number={addr}\n"
                ),
            );
        }
    }
}

/// A [`PortSet`] on an axis's status or its poller: C's
/// `pAxis->setIntegerParam(motorStatusXxx_)` + `pAxis->callParamCallbacks()`
/// on axis `addr` of port `port`, or `epicsEventSignal(pollEventId_)`.
pub(crate) fn apply_axis_set(port: &str, set: PortSet) {
    match set {
        PortSet::Status { addr, field } => {
            if let Some(axis) = crate::devices::axis_at(port, addr) {
                let write = AxisWrite {
                    fields: vec![field],
                    ..AxisWrite::default()
                };
                axis.post_written(Written::new(write, false));
            }
        }
        PortSet::WakeupPoller => {
            for axis in crate::devices::axes_of(port) {
                axis.wakeup();
            }
        }
        PortSet::Set { .. } | PortSet::Axis { .. } | PortSet::Callbacks(_) => {}
    }
}

/// An axis's lock, during which the thread may make no blocking request to
/// the controller port: the port's own writes take the lock, so a driver
/// hands its steps on the port to the owner through
/// [`AsynMotor::take_port_sets`].
pub(crate) struct AxisLock<'a> {
    motor: std::sync::MutexGuard<'a, dyn AsynMotor + 'static>,
    _forbid: Option<ForbidBlocking>,
}

impl std::ops::Deref for AxisLock<'_> {
    type Target = dyn AsynMotor + 'static;
    fn deref(&self) -> &Self::Target {
        &*self.motor
    }
}

impl std::ops::DerefMut for AxisLock<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.motor
    }
}

/// Lock `motor`, an axis of controller port `port`, under its controller's
/// lock; `None` if poisoned.
pub(crate) fn lock_axis<'a>(
    motor: &'a Mutex<dyn AsynMotor + 'static>,
    port: Option<&str>,
    _: &ControllerHeld<'_>,
) -> Option<AxisLock<'a>> {
    let forbid = port.map(forbid_blocking);
    Some(AxisLock {
        motor: motor.lock().ok()?,
        _forbid: forbid,
    })
}

mod polled {
    use super::{ControllerHeld, SharedControllerPoll};

    /// Proof the controller poll has run, its lock held until this drops: C
    /// polls an axis only after `pC->poll()`, on a poller pass
    /// (asynMotorController.cpp:652-656) and in `motorUpdateStatus_`
    /// (:217-221). Only [`after`] makes one.
    pub(super) struct Polled<'a>(ControllerHeld<'a>, &'a SharedControllerPoll);

    impl<'a> Polled<'a> {
        pub(super) fn held(&self) -> &ControllerHeld<'a> {
            &self.0
        }

        /// The controller whose lock this holds.
        pub(super) fn controller(&self) -> &'a SharedControllerPoll {
            self.1
        }
    }

    /// Take the controller's lock and run its poll, for the axis polls that
    /// follow it.
    pub(super) fn after(poll: &SharedControllerPoll) -> Polled<'_> {
        let held = poll.hold();
        poll.run_on_poller(&held);
        Polled(held, poll)
    }
}
use polled::Polled;

/// When the poll loop runs a pass.
enum Schedule {
    /// C `asynMotorPoller` (asynMotorController.cpp:615-695): every axis,
    /// at the controller's moving or idle period, fast for a while after a
    /// wakeup.
    Poller {
        forced_fast_polls: u32,
        remaining: u32,
    },
    /// C motordrvCom `motor_task` (motordrvCom.cc:180-262), a scan period
    /// after the last wake while a move is held.
    MotorTask { scan: Duration },
}

/// One axis of the loop and the records bound to it.
pub(crate) struct Axis {
    /// An add to the running poller, counted until its first poll.
    pub(crate) first_poll: Option<FirstPollPending>,
    /// Its own address's, then the controller port address it was added
    /// at later; each address's records hold the status the poller set.
    places: Vec<Place>,
    motor: Arc<Mutex<dyn AsynMotor>>,
    pacing: Pacing,
    /// C `statusChanged_`: a set changed the status since the last post, so
    /// the next post goes out even if it equals `last_status`.
    status_changed: bool,
    /// How many of `places`, from the first, have had a post since the axis
    /// took them. The rest are C's asynMotorAxis constructed at the address,
    /// `statusChanged_` raised there alone (asynMotorAxis.cpp:29): the next
    /// `callParamCallbacks` posts them whatever it reads.
    posted: usize,
    /// A model-2 driver's parameter library, shared with its [`MotorAxis`]
    /// for the driver's report.
    ///
    /// [`MotorAxis`]: crate::device_support::MotorAxis
    params: Model2Params,
    /// The speeds a model-1 card keeps, or devMotorSim's `min_vel` /
    /// `max_vel` / `acc` (devMotorSim.c:70-72, 251-259), which the record's
    /// speed writes store. 0 until one is: the private struct is calloc'd.
    speeds: StoredSpeeds,
    /// The io_intr pulses owed for processes the callbacks queued.
    pulses: Pulses,
    posts: Posts,
    /// A move ran: C `wakeupPoller`.
    woken: bool,
    /// The controller port and axis whose parameters mirror the status.
    controller: Option<(String, i32)>,
    /// The controller whose queue the mirror's sets go through.
    controller_poll: SharedControllerPoll,
    /// The status word last set in the controller port's parameters.
    mirrored_word: Option<u32>,
    /// [`AsynMotor::param_owned_msta_bits`]: bit params the driver owns.
    owned_bits: u32,
    power: Power,
}

/// C `asynMotorAxis`'s auto-power flags, `wasMovingFlag_`, `disableFlag_`
/// and `lastEndOfMoveTime_`, as one state.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Power {
    Idle,
    /// A move ran or was seen: `wasMovingFlag_`.
    Moving,
    /// The move ended under auto power at this time: `disableFlag_`.
    Ended(Instant),
}

/// Who posts an axis's status to the records bound to it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Posts {
    /// The poll loop, from its polls and the writes it runs.
    Poller,
    /// The driver's parameter callback, on every call: drvMotorAsyn's
    /// `intCallback` calls devMotorAsyn's `statusCallback`, which processes
    /// the record, whatever changed (devMotorAsyn.c:755-762). The poll loop
    /// posts nothing of its own.
    Callback,
}

/// How the loop's pass treats the axis.
enum Pacing {
    /// Polled on every pass of [`Schedule::Poller`].
    Poller,
    /// C motordrvCom `motor_task`: only the held move (`motor_motion`, with
    /// its `no_motion_count`) is queried, on every wake. An idle axis is
    /// never polled.
    MotorTask {
        held: Option<Held>,
        /// The driver's [`AsynMotor::no_motion_test`].
        no_motion_test: bool,
    },
}

/// motor_task's `motor_motion`: the held move's `no_motion_count`, and the
/// record that sent it, by its place, with that record's CDIR as the driver
/// last left it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Held {
    count: u32,
    mover: Option<((usize, RecordId), bool)>,
}

/// The record's speed write `command` on an asynMotorController axis: the
/// port's own `writeFloat64`, whose base stores the parameter the next
/// move reads (asynMotorController.cpp:289, :297-300).
fn write_controller_speed(trace: &MotorTrace, command: &MotorCommand) {
    let Some(port) = crate::devices::controller_port_handle(trace.port()) else {
        return;
    };
    let Some((function, WriteValue::Float64(value))) = c_write(command) else {
        return;
    };
    if let Err(e) = port.write_float64_blocking(function as usize, trace.addr(), value) {
        dev_motor_asyn_reply(trace, command, function, Some(&e.message_display()));
    }
}

/// `command` on the speeds an asynMotorController axis moves on: its
/// port's parameters, only those the command runs on. asyn's
/// paramList::getDouble zeroes an unset one before it fails.
fn on_controller_speeds(trace: &MotorTrace, command: &MotorCommand) -> Option<MotorCommand> {
    use crate::controller_port::index;
    let params = crate::devices::controller_params(trace.port())?;
    let (command, addr) = (command.clone(), trace.addr());
    params.read(move |base| {
        command.on_speeds(|speed| {
            let function = match speed {
                Speed::MinVelocity => index::VEL_BASE,
                Speed::Velocity => index::VELOCITY,
                Speed::Acceleration => index::ACCEL,
            };
            base.get_float64_param(function, addr).unwrap_or(0.0)
        })
    })
}

/// [`Axis::speeds_for`]'s verdict on a record command.
enum Speeds {
    /// A speed write the store took; no driver call follows.
    Stored,
    /// A move or stop on the stored speeds.
    Resolved(MotorCommand),
}

/// A `motorPoller` thread [`MotorPollLoop::start`] started; awaiting it
/// waits for the thread to end. Dropping it leaves the thread running.
pub struct PollerThread(Option<oneshot::Receiver<()>>);

impl std::future::Future for PollerThread {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        match &mut self.0 {
            Some(done) => std::pin::Pin::new(done).poll(cx).map(|_| ()),
            None => std::task::Poll::Ready(()),
        }
    }
}

/// A controller's poller whose startup pass has not ended, or an axis added
/// to its running poller and not yet polled, counted on that controller until
/// this drops: after the pass, or with the poller or axis that never got
/// there. A driver whose own thread polls (omsBaseController's omsPoller)
/// holds one over its first pass.
#[must_use]
pub struct FirstPollPending(Arc<std::sync::atomic::AtomicUsize>);

impl FirstPollPending {
    fn begin(count: &Arc<std::sync::atomic::AtomicUsize>) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for FirstPollPending {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The sum of every live controller's
/// [`crate::builder::ControllerAxes::first_polls_pending`] in this process:
/// for a reader that ran a shell command and cannot name the controllers it
/// started or added to.
pub fn first_polls_pending_in_process() -> usize {
    let every = every_first_polls()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    every
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .map(|c| c.load(std::sync::atomic::Ordering::SeqCst))
        .sum()
}

/// C `epicsThreadCreate`'s report of a refused `pthread_create`
/// (osdThread.c:675, `checkStatusOnce`); the WIN32 port prints nothing.
fn thread_create_refused(e: &std::io::Error) {
    #[cfg(unix)]
    if let Some(errno) = e.raw_os_error() {
        // SAFETY: `strerror` returns a NUL-terminated string valid until the
        // next `strerror` call; it is copied out at once.
        let text = unsafe { std::ffi::CStr::from_ptr(libc::strerror(errno)) }.to_string_lossy();
        epics_base_rs::stderr_println!("pthread_create error {text}");
    }
    #[cfg(not(unix))]
    let _ = e;
}

impl Axis {
    /// An axis polled on every pass of a [`Schedule::Poller`] loop, keeping
    /// its model-2 parameters in `params`, the table its `MotorAxis` reads.
    pub(crate) fn polled(
        place: Place,
        motor: Arc<Mutex<dyn AsynMotor>>,
        params: Model2Params,
    ) -> Self {
        Self::with_pacing(place, motor, params, Pacing::Poller)
    }

    /// A model-1 axis, whose held move a [`Schedule::MotorTask`] queries.
    pub(crate) fn task(place: Place, motor: Arc<Mutex<dyn Model1Motor>>) -> Self {
        let no_motion_test = motor.lock().map_or(true, |m| m.no_motion_test());
        let status = motor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_init_status();
        let pacing = Pacing::MotorTask {
            held: None,
            no_motion_test,
        };
        let mut axis = Self::with_pacing(place, motor, Model2Params::default(), pacing);
        axis.store_init_status(status);
        axis
    }

    fn with_pacing(
        place: Place,
        motor: Arc<Mutex<dyn AsynMotor>>,
        params: Model2Params,
        pacing: Pacing,
    ) -> Self {
        // C `status_` from construction: all 0 (asynMotorAxis.cpp:46), then
        // whatever the driver's constructor set. readGenericPointer returns
        // it before any poll (asynMotorController.cpp:505-525); the
        // constructor's callParamCallbacks had no record to call back.
        let mut status = MotorStatus::default();
        if let Ok(mut motor) = motor.lock() {
            for write in motor.take_status_writes() {
                write.apply(&mut status);
                let (reason, value) = model2_param(write);
                params.set(reason, value);
            }
        }
        place.records().store(status.clone());
        place.records().set_status(status);
        Self {
            places: vec![place],
            motor,
            pacing,
            status_changed: false,
            posted: 1,
            params,
            speeds: StoredSpeeds::default(),
            pulses: Pulses::new(),
            posts: Posts::Poller,
            woken: false,
            controller: None,
            controller_poll: SharedControllerPoll::default(),
            mirrored_word: None,
            owned_bits: 0,
            power: Power::Idle,
            first_poll: None,
        }
    }

    /// The axis's `delay` parameter when its MOTOR_POWER_AUTO_ONOFF is 1,
    /// read as C's getIntegerParam / getDoubleParam do: 0 when unset.
    fn auto_power(&self, delay: usize) -> Option<Duration> {
        use crate::controller_port::index;
        let (port, addr) = self.controller.as_ref()?;
        let addr = *addr;
        let secs = crate::devices::controller_params(port)?.read(move |base| {
            let on = base.get_int32_param(index::POWER_AUTO_ONOFF, addr);
            (on.unwrap_or(0) == 1).then(|| base.get_float64_param(delay, addr).unwrap_or(0.0))
        })??;
        Some(Duration::try_from_secs_f64(secs).unwrap_or_default())
    }

    /// One axis of C `asynMotorPoller`'s pass (asynMotorController.cpp:
    /// 652-683): the poll, then an ended move under auto power powers the
    /// axis off once MOTOR_POWER_OFF_DELAY has passed.
    fn poller_poll(&mut self, polled: &Polled, off_delay: Option<Duration>) -> bool {
        let moving = self.poll_and_notify(false, polled);
        self.power = match self.power {
            _ if moving => Power::Moving,
            Power::Moving if off_delay.is_some() => Power::Ended(Instant::now()),
            power => power,
        };
        if let (Power::Ended(at), Some(delay)) = (self.power, off_delay)
            && !moving
            && at.elapsed() >= delay
        {
            if let Some(mut motor) = lock_axis(&self.motor, self.own_port(), polled.held()) {
                let _ = motor.set_closed_loop(&AsynUser::new(0), false);
            }
            self.power = Power::Idle;
        }
        moving
    }

    /// Leave posting the axis to the driver's parameter callback: a
    /// drvMotorAsyn port opened it.
    pub(crate) fn post_by_callback(&mut self) {
        self.posts = Posts::Callback;
    }

    /// The status the poller last set, what its polls write into. An
    /// autonomous poll posts only a status that differs from it, unless
    /// `status_changed` ([`Self::notify`]).
    fn last_status(&self) -> MotorStatus {
        self.places[0].records().status()
    }

    /// The controller port this axis mirrors its status to.
    fn own_port(&self) -> Option<&str> {
        self.controller.as_ref().map(|(port, _)| port.as_str())
    }

    /// C `asynMotorAxis`'s constructor raises `statusChanged_`
    /// (asynMotorAxis.cpp:29) and a driver constructor ending in
    /// `callParamCallbacks` lowers it again.
    pub(crate) fn construct_asyn_motor_axis(&mut self) {
        let posted = self
            .motor
            .lock()
            .is_ok_and(|motor| motor.constructor_calls_param_callbacks());
        self.posted = if posted { self.places.len() } else { 0 };
    }

    /// Mirror each status into axis `addr` of controller port `port`, whose
    /// lock and queue are `poll`'s.
    pub(crate) fn mirror_to(&mut self, port: &str, addr: i32, poll: SharedControllerPoll) {
        self.controller = Some((port.to_owned(), addr));
        poll.set_port(port);
        self.controller_poll = poll;
        self.mirrored_word = None;
        if let Ok(motor) = self.motor.lock() {
            self.owned_bits = motor.param_owned_msta_bits();
        }
        // What the constructor set, which the port's parameters hold before
        // the first poll.
        let status = self.last_status();
        self.hold(&status);
    }

    /// Take `place` as well: its queue, and its records' cache from now on;
    /// the next status is posted there whatever it is.
    fn take(&mut self, place: Place) {
        place.records().store(self.last_status());
        place.records().set_status(self.last_status());
        self.places.push(place);
    }

    /// The next message on the axis's queues, a set-aside batch ahead of
    /// its queue's own, and the place it came from.
    fn next_message(&self) -> Option<(usize, ToAxis)> {
        self.places.iter().enumerate().find_map(|(at, place)| {
            let mut queue = place.queue();
            let message = match queue.batches.pop_front() {
                Some(batch) => batch.into(),
                None => queue.inbox.commands.try_recv().ok()?,
            };
            Some((at, message))
        })
    }

    fn run_queued_commands(&mut self, controller_poll: &SharedControllerPoll) {
        // A batch's driver calls take the queue's posts, setting aside the
        // batches behind them.
        while let Some((place, message)) = self.next_message() {
            self.receive(place, message, controller_poll);
        }
        for io_intr in self.pulses.drain(..) {
            let _ = io_intr.try_send(());
        }
    }

    /// Send the owed pulses a queue has room for, under the controller's
    /// lock as C's statusCallback processes the record under it
    /// (devMotorAsyn.c:756-764); the rest stay owed.
    fn offer_pulses(&mut self) {
        let mut owed = std::mem::take(&mut self.pulses).into_iter();
        for io_intr in owed.by_ref() {
            if let Err(tokio::sync::mpsc::error::TrySendError::Full(())) = io_intr.try_send(()) {
                self.pulses.push(io_intr);
                break;
            }
        }
        self.pulses.extend(owed);
    }

    /// Send the owed pulses, waiting on a full queue as C's callback waits
    /// in `dbProcess`.
    async fn send_pulses(&mut self) {
        for io_intr in std::mem::take(&mut self.pulses) {
            let _ = io_intr.send(()).await;
        }
    }

    /// Take one message from the queue of place `place`.
    fn receive(&mut self, place: usize, message: ToAxis, controller_poll: &SharedControllerPoll) {
        match message {
            ToAxis::Batch(batch) => self.execute(place, &batch, controller_poll),
            ToAxis::Written(written) => self.take_written(written),
            ToAxis::Callback(status) => self.post(status),
        }
    }

    /// Post what the queue holds beside the record passes, which wait their
    /// turn: what controller-port writes set on the axis and the driver's
    /// callbacks. Run before a poll under the controller lock, as each write
    /// queued it inside its own lock region and C's write posts before it
    /// unlocks (asynMotorController.cpp:243, :417); and after each driver
    /// call, whose callbacks C makes inside the call.
    fn take_posts(&mut self) {
        for at in 0..self.places.len() {
            loop {
                let message = self.places[at].queue().inbox.commands.try_recv();
                match message {
                    Ok(ToAxis::Written(written)) => self.take_written(written),
                    Ok(ToAxis::Callback(status)) => self.post(status),
                    Ok(ToAxis::Batch(batch)) => self.places[at].queue().batches.push_back(batch),
                    Err(_) => break,
                }
            }
        }
    }

    /// Post what a controller-port write set on the axis.
    fn take_written(&mut self, Written { write, powered }: Written) {
        if powered {
            self.power = Power::Moving;
        }
        self.post_writes(write);
    }

    /// Run a record's commands, posting each write's status change; a
    /// finished move then processes the record (C `asynCallback`), and a
    /// move wakes the poller (asynMotorController.cpp:303, :320, :336,
    /// :355). The axis's lock is held only across each driver call: a port
    /// call made under it would wait on a port thread waiting on the lock.
    fn execute(
        &mut self,
        place: usize,
        batch: &CommandBatch,
        controller_poll: &SharedControllerPoll,
    ) {
        let motor = self.motor.clone();
        let records = self.places[place].records().clone();
        let drv_port = batch
            .trace
            .is_model2()
            .then(|| crate::devices::drv_motor_asyn_port_handle(batch.trace.port()))
            .flatten();
        let port = PortCtx::of(&batch.trace);
        let mut cdir = None;
        let mut accel = None;
        for command in &batch.commands {
            if let MotorCommand::MoveDirection { forward } = *command {
                cdir = Some(forward);
                continue;
            }
            // devMotorSim's GET_INFO only sets needUpdate (devMotorSim.c
            // build_trans): no driver call, no process.
            if let (MotorCommand::Poll, Some(record)) = (command, batch.record)
                && batch.trace.is_motor_interface()
            {
                records.get_info(record);
                continue;
            }
            if batch.record.is_some() {
                async_callback_flow(&batch.trace, command);
            }
            if let Some(drv) = &drv_port
                && let Some(write) = drv_motor_asyn_write(command)
            {
                // The port's write takes the axis lock itself.
                let result = write_drv_motor_asyn(drv, &batch.trace, write);
                let Ok(mut locked) = motor.lock() else {
                    return;
                };
                let axis_write =
                    drvset_axis_write(&mut *locked, command, Some(write), result.is_ok());
                drop(locked);
                self.post_writes(axis_write);
                self.take_posts();
                let failure = result.err().map(|e| e.message_display().to_string());
                dev_motor_asyn_reply(
                    &batch.trace,
                    command,
                    write.0,
                    failure.as_ref().map(|e| e as &dyn std::fmt::Display),
                );
                if command.is_move()
                    && let Some(record) = batch.record
                {
                    self.pulses.extend(records.move_done(record, command));
                }
                continue;
            }
            let stored;
            // The port route already moves on the port's own parameters.
            let command = match batch
                .record
                .and_then(|_| self.speeds_for(&batch.trace, command, &mut accel))
            {
                Some(Speeds::Stored) => continue,
                Some(Speeds::Resolved(c)) => {
                    stored = c;
                    &stored
                }
                None => command,
            };
            // C writeFloat64 powers the axis on before a move or home and
            // waits out the delay under the lock (asynMotorController.cpp:
            // 291-295, :308-312, :325-329, :342-346).
            let on_delay = (command.is_move()
                && !matches!(command, MotorCommand::SetPosition { .. }))
            .then(|| self.auto_power(crate::controller_port::index::POWER_ON_DELAY))
            .flatten();
            if on_delay.is_some() {
                self.power = Power::Moving;
            }
            let trace = &batch.trace;
            let mut poisoned = false;
            execute_command(
                trace,
                command,
                &port,
                controller_poll,
                |held, c, quiet| {
                    let own = trace.is_controller().then(|| trace.port());
                    if matches!(command, MotorCommand::Poll) {
                        controller_poll.run_on_poller(held);
                    }
                    let Some(mut motor) = lock_axis(&motor, own, held) else {
                        poisoned = true;
                        return None;
                    };
                    if let Some(delay) = on_delay {
                        let _ = motor.set_closed_loop(&AsynUser::new(0), true);
                        std::thread::sleep(delay);
                    }
                    Some(drive(&mut *motor, trace, command, c, quiet, &records))
                },
                &mut |write| self.post_writes(write),
            );
            if poisoned {
                return;
            }
            self.take_posts();
            if !command.is_move() {
                continue;
            }
            if let Some(record) = batch.record {
                self.pulses.extend(records.move_done(record, command));
            }
            if !matches!(command, MotorCommand::SetPosition { .. }) {
                match &mut self.pacing {
                    // drvMotorSim's motorProcTask keeps its period: nothing
                    // wakes it (drvMotorSim.c:660-690). A drvMotorAsyn port
                    // wakes its poller itself.
                    Pacing::Poller => self.woken = batch.trace.is_controller(),
                    // A MOTION or VELOCITY message replaces the held move
                    // and clears its count (motordrvCom.cc:415-437).
                    Pacing::MotorTask { held, .. } => {
                        *held = Some(Held {
                            count: 0,
                            mover: batch.record.map(|record| (place, record)).zip(cdir),
                        })
                    }
                }
            }
        }
    }

    /// What the axis's speed store makes of a record's `command`: a speed
    /// write is stored, and a move or stop runs on the stored speeds, as
    /// asynMotorController and devMotorSim read them
    /// (asynMotorController.cpp:208, :297-350; devMotorSim.c:251-296). A
    /// model-2 axis's are its port's.
    /// On a model-1 card a SET_* only appends to the message string: the
    /// card keeps the velocities a later move leaves unwritten, as a
    /// backlash leg's SET_VEL_BASE (motorRecord.cc do_work). Its
    /// acceleration is the SET_ACCEL written since the last move, `accel`,
    /// or 0 when none was: the record skips SET_ACCEL at acc <= 0 and a
    /// driver appends its acceleration only above 0 (motorRecord.cc:874).
    fn speeds_for(
        &mut self,
        trace: &MotorTrace,
        command: &MotorCommand,
        accel: &mut Option<f64>,
    ) -> Option<Speeds> {
        if trace.is_model1() {
            if let MotorCommand::SetAccel { acceleration } = *command {
                *accel = Some(acceleration);
            }
            if command.store_speed(&mut self.speeds) {
                return Some(Speeds::Stored);
            }
            return command.runs_on_speeds().then(|| {
                let speeds = StoredSpeeds {
                    acceleration: accel.take().unwrap_or(0.0),
                    ..self.speeds
                };
                Speeds::Resolved(command.on_speeds(|speed| speeds.get(speed)))
            });
        }
        if trace.is_controller() {
            if command.is_speed_write() {
                write_controller_speed(trace, command);
                return Some(Speeds::Stored);
            }
            if !command.runs_on_speeds() {
                return None;
            }
            return on_controller_speeds(trace, command).map(Speeds::Resolved);
        }
        if !trace.is_motor_interface() {
            return None;
        }
        if command.store_speed(&mut self.speeds) {
            return None;
        }
        command
            .runs_on_speeds()
            .then(|| Speeds::Resolved(command.on_speeds(|speed| self.speeds.get(speed))))
    }

    /// C `asynMotorAxis::callParamCallbacks` after a write (asynMotorAxis.cpp:
    /// 316-322): `write.fields` land on the last posted status, which is
    /// posted only if one of them changed it or a poll left `statusChanged_`
    /// raised. A devMotorSim axis's parameter posts the status when it
    /// changed the parameter: paramLib flags only a new type or value
    /// (paramLib.c:112-118, 141-147), and `callCallback` calls back only
    /// then (:252-275). A drvMotorAsyn axis's driver posts its own
    /// ([`Posts::Callback`]).
    fn post_writes(&mut self, write: AxisWrite) {
        let param_changed = write
            .params
            .into_iter()
            .fold(false, |changed, (reason, value)| {
                self.params.set(reason, value) | changed
            });
        if let Some(polled) = write.polled {
            self.publish_info(polled);
            return;
        }
        let mut status = self.last_status();
        // A later set putting it back does not lower statusChanged_, nor
        // does a write that sets nothing lower one a poll left raised.
        let mut changed = self.status_changed || param_changed;
        for field in write.fields {
            let before = status.clone();
            field.apply(&mut status);
            changed |= status != before;
        }
        if changed {
            self.publish(status);
            return;
        }
        self.post_unposted(status);
    }

    /// The poll loop's own post of `status`, which an axis its driver's
    /// callback posts never makes.
    fn publish(&mut self, status: MotorStatus) {
        if self.posts == Posts::Poller {
            self.post(status);
        }
    }

    /// Post `status` to every bound record's `statusCallback` under a new
    /// sequence number.
    fn post(&mut self, status: MotorStatus) {
        self.post_from(0, status);
    }

    /// C `asynMotorAxis::callParamCallbacks` with `statusChanged_` lowered:
    /// `status` is posted only to the places not posted since taken.
    fn post_unposted(&mut self, status: MotorStatus) {
        if self.posts == Posts::Poller && self.posted < self.places.len() {
            self.post_from(self.posted, status);
        } else {
            self.call_param_callbacks();
        }
    }

    /// [`Self::post`] to the records of `places[from..]`.
    fn post_from(&mut self, from: usize, status: MotorStatus) {
        self.hold(&status);
        self.status_changed = false;
        for place in &self.places[from..] {
            let pulses = place.records().post(status.clone());
            self.pulses.extend(pulses);
        }
        self.posted = self.places.len();
    }

    /// Make `status` the axis's: C `asynMotorAxis::setDoubleParam` /
    /// `setIntegerParam` of each of its values on the controller port's axis
    /// address, then `callParamCallbacks` (asynMotorAxis.cpp:250-322).
    /// MOTOR_STATUS is the whole word on every set: C writes it as 0 when a
    /// non-status integer is set (`status` stays 0 at :253, :267), a bug.
    fn hold(&mut self, status: &MotorStatus) {
        self.stage(status);
        self.call_param_callbacks();
    }

    /// [`Self::hold`]'s sets without its `callParamCallbacks`, as a C poll
    /// leaves them when it returns early.
    fn stage(&mut self, status: &MotorStatus) {
        for place in &self.places {
            place.records().set_status(status.clone());
        }
        if self.controller.is_none() {
            return;
        }
        use crate::controller_port::index;
        use asyn_rs::param::ParamValue;
        let word = crate::flags::MstaFlags::from_status(status).bits();
        let mut updates = Vec::from([
            (index::POSITION, ParamValue::Float64(status.position)),
            (
                index::ENCODER_POSITION,
                ParamValue::Float64(status.encoder_position),
            ),
            (index::ACT_VELOCITY, ParamValue::Float64(status.velocity)),
            (index::STATUS, ParamValue::Int32(word as i32)),
        ]);
        // C sets a MOTOR_STATUS_* bit only when a driver calls
        // setIntegerParam on it. The word here is the driver's whole status,
        // so only a bit that changed is one it set to something new; any
        // other keeps what was set last, such as pmac's COMMS_ERROR. A bit
        // the driver sets on its params itself is never mirrored.
        let changed = self.mirrored_word.map_or(!0, |last| last ^ word) & !self.owned_bits;
        self.mirrored_word = Some(word);
        updates.extend((0..15).filter(|bit| changed >> bit & 1 != 0).map(|bit| {
            let value = ((word >> bit) & 1) as i32;
            (index::STATUS_DIRECTION + bit, ParamValue::Int32(value))
        }));
        self.set_params(updates);
    }

    /// C `setXxxParam` on the axis: a read sees each value at once, and the
    /// next [`Self::call_param_callbacks`] posts it.
    fn set_params(&self, updates: Vec<(usize, asyn_rs::param::ParamValue)>) {
        if updates.is_empty() {
            return;
        }
        let Some((_, addr)) = &self.controller else {
            return;
        };
        let mut sets = PortSets::default();
        for (reason, value) in updates {
            sets.set(*addr, reason, value);
        }
        self.controller_poll.post_sets(None, sets);
    }

    /// C's `callParamCallbacks` on the axis with no status to set: posts
    /// whatever was set on it since the last one, such as a
    /// [`WriteHook`](crate::controller_port::WriteHook)'s sets.
    fn call_param_callbacks(&self) {
        let Some((_, addr)) = &self.controller else {
            return;
        };
        let mut sets = PortSets::default();
        sets.call_param_callbacks(*addr);
        self.controller_poll.post_sets(None, sets);
    }

    /// The status a failed poll leaves in the parameter library: what it
    /// wrote with the bits `failure` raises set. Whether it posts is
    /// [`Self::poll_and_notify`]'s change gate, so a failure raising no new
    /// bit posts only on a forced poll, as C's `motorUpdateStatus_` path
    /// forces `statusChanged_` (asynMotorController.cpp:217-221) while the
    /// background poller ignores the poll status (:658).
    fn failed_poll_status(&mut self, failed: FailedPoll) -> MotorStatus {
        // The bits are the driver's own sets after what it wrote, so C's
        // status_ changes only if the two together differ from the last.
        let mut left = failed.left;
        left.problem |= failed.bits.problem;
        left.comms_error |= failed.bits.comms_error;
        let last = self.last_status();
        if last != left {
            self.stage(&left);
            self.status_changed |= changes_status(&last, &left);
        }
        left
    }

    /// The status the driver's `motor_init` read with `set_status`
    /// ([`Model1Motor::take_init_status`]), before any record initialises,
    /// so init reads it. No record is bound yet, so it only fills the cache.
    fn store_init_status(&mut self, status: MotorStatus) {
        if status.moving {
            self.power = Power::Moving;
        }
        self.hold(&status);
        for place in &self.places {
            place.records().store(status.clone());
        }
    }

    fn poll_result(&mut self, polled_by: &Polled) -> Result<MotorStatus, FailedPoll> {
        self.take_posts();
        let user = AsynUser::new(0);
        // An `Err` is the C driver poll bailing to its error label, which
        // raises what `poll_failure` names. A poisoned driver mutex has no C
        // counterpart; it raises both bits. A forced poll still posts either
        // way, so a `STUP=BUSY` refresh cannot latch (record/status_update.rs
        // :48-58). C's poller ignores the poll status
        // (asynMotorController.cpp:658), so a failure prints nothing.
        let Some(mut motor) = lock_axis(&self.motor, self.own_port(), polled_by.held()) else {
            return Err(FailedPoll::poisoned(self.last_status()));
        };
        let polled = poll_driver(&mut *motor, &user, self.last_status());
        let port_sets = motor.take_port_sets();
        // A model-2 driver's update sets its paramLib
        // (drvMotorSim.c:646-653); the poll's status posts them.
        for write in motor.take_status_writes() {
            let (reason, value) = model2_param(write);
            self.params.set(reason, value);
        }
        drop(motor);
        if let Some((_, addr)) = &self.controller {
            polled_by.controller().post_sets(Some(*addr), port_sets);
        }
        self.take_posts();
        polled
    }

    /// C motordrvCom `query_axis` (motordrvCom.cc:270-347) on the held
    /// move: the driver's status is called back while the position moves,
    /// at a limit in the direction of travel, on done and on a problem,
    /// and the last three end the move with RA_DONE set. Every pass is
    /// called back unless the driver returned early (`set_status`'s RETRY,
    /// [`PollFailure::NONE`]) or the position sat still under the driver's
    /// [`AsynMotor::no_motion_test`]. Returns the move
    /// the axis holds after the pass.
    fn query_held(&mut self, mut held: Held, polled: &Polled) -> Option<Held> {
        let status = match self.poll_in_motion(&mut held.mover, polled) {
            Ok(status) => status,
            Err(failed) if !failed.bits.raises() => {
                self.failed_poll_status(failed);
                return Some(held);
            }
            Err(failed) => self.failed_poll_status(failed),
        };
        let tested = matches!(
            self.pacing,
            Pacing::MotorTask {
                no_motion_test: true,
                ..
            }
        );
        let still = tested && self.last_status().position == status.position;
        held.count = if still { held.count + 1 } else { 0 };
        let ls_active = if status.direction {
            status.high_limit
        } else {
            status.low_limit
        };
        if ls_active || status.done || status.problem {
            self.publish(MotorStatus {
                done: true,
                ..status
            });
            return None;
        }
        if held.count == 0 {
            self.publish(status);
        }
        Some(held)
    }

    /// The held move's driver poll, handing the driver its record's CDIR; a
    /// CDIR the driver changed is written into the record before the pass
    /// calls it back, as C's `set_status` writes `mr->cdir`.
    fn poll_in_motion(
        &mut self,
        mover: &mut Option<((usize, RecordId), bool)>,
        polled: &Polled,
    ) -> Result<MotorStatus, FailedPoll> {
        let Some(((place, record), cdir)) = mover else {
            return self.poll_result(polled);
        };
        self.take_posts();
        let before = *cdir;
        let mut status = self.last_status();
        let polled = match lock_axis(&self.motor, self.own_port(), polled.held()) {
            Some(mut motor) => match motor.poll_in_motion(&AsynUser::new(0), cdir, &mut status) {
                Ok(()) => Ok(status),
                Err(_) => Err(FailedPoll::of(&*motor, status)),
            },
            None => Err(FailedPoll::poisoned(status)),
        };
        if *cdir != before {
            self.places[*place]
                .records()
                .set_driver_cdir(*record, *cdir);
        }
        self.take_posts();
        polled
    }

    /// A GET_INFO's post, made even for an unchanged status: C
    /// motorUpdateStatus_ forces statusChanged_ (asynMotorController.cpp:
    /// 217-222). motor_task's INFO reports RA_DONE as whether no move is
    /// held, whatever the driver read (motordrvCom.cc:470-481).
    fn publish_info(&mut self, polled: Result<MotorStatus, FailedPoll>) {
        let mut status = polled.unwrap_or_else(|failure| self.failed_poll_status(failure));
        if let Pacing::MotorTask { held, .. } = self.pacing {
            status.done = held.is_none();
        }
        self.publish(status);
    }

    /// One wake of `motor_task`, before it processes the messages: query
    /// the held move, if any (motordrvCom.cc:244-254).
    fn query_task(&mut self, polled: &Polled) {
        let Pacing::MotorTask {
            held: Some(held), ..
        } = self.pacing
        else {
            return;
        };
        let next = self.query_held(held, polled);
        if let Pacing::MotorTask { held, .. } = &mut self.pacing {
            *held = next;
        }
    }

    /// Poll and post; returns whether the axis is moving. On a failed poll
    /// that is the `moving` the driver left: the last known one when it
    /// bails early, as C's `pAxis->poll(&moving)` leaves `*moving`, or the
    /// one it wrote past the failure (phytronAxisMotor.cpp:1868-1869).
    ///
    /// `force` distinguishes the two C poll paths. A forced poll (the analogue
    /// of C `motorUpdateStatus_` forcing `statusChanged_=1`, asynMotorController
    /// .cpp:217-222) always bumps the sequence and pulses io_intr, so the
    /// record re-processes even on a stationary axis. An autonomous (timed) poll passes
    /// `force=false` and notifies only when the polled status differs from the
    /// last one delivered — the analogue of C's `statusChanged_` gate, so a
    /// settled, unchanging axis does not re-post DIFF/RDIF every idle period.
    fn poll_and_notify(&mut self, force: bool, polled: &Polled) -> bool {
        let (status, moving) = match self.poll_result(polled) {
            Ok(status) => {
                let moving = status.moving;
                (status, moving)
            }
            Err(failed) => {
                // A driver returning before callParamCallbacks posts nothing,
                // whatever it left set, and C's poller adds none
                // (asynMotorController.cpp:658).
                let quiet = !failed.bits.raises() && !force;
                let status = self.failed_poll_status(failed);
                let moving = status.moving;
                if quiet {
                    return moving;
                }
                (status, moving)
            }
        };
        self.notify(status, force);
        moving
    }

    /// C `asynMotorAxis::callParamCallbacks` with `status` set: posted to
    /// the records when `force`d or when it changed the last one.
    fn notify(&mut self, status: MotorStatus, force: bool) {
        let last = self.last_status();
        if !force && !self.status_changed && !changes_status(&last, &status) {
            // Autonomous idle poll, status unchanged → no record pass (C posts
            // nothing when statusChanged_ stays 0). A *repeated* comms failure
            // lands here too, and correctly so: the COMM_ERR flag is already
            // set, no parameter changed, so C's `statusChanged_` stays 0 and
            // `asynMotorAxis::callParamCallbacks` posts nothing
            // (asynMotorAxis.cpp:316-322). The first failure — the one that
            // flips the flag — always differs from the last status and posts.
            // Its pC_->callParamCallbacks still posts what was set meanwhile,
            // and a place taken since the last post has the status.
            if last.moving != status.moving {
                // The pace alone, for the next poll to start from.
                self.stage(&status);
            }
            self.post_unposted(status);
            return;
        }
        self.publish(status);
    }

    /// Where the axis falls in the controller's address order; an axis of
    /// no port polls after those of one, in the order added.
    fn order(&self) -> (bool, i32) {
        match &self.controller {
            Some((_, addr)) => (false, *addr),
            None => (true, 0),
        }
    }

    fn holds_move(&self) -> bool {
        matches!(self.pacing, Pacing::MotorTask { held: Some(_), .. })
    }
}

impl MotorPollLoop {
    pub fn new(
        inbox: PollerInbox,
        motor: Arc<Mutex<dyn AsynMotor>>,
        subscribers: AxisSubscribers,
        moving_poll_interval: Duration,
        idle_poll_interval: Duration,
        forced_fast_polls: u32,
    ) -> Self {
        Self::controller(
            vec![Axis::polled(
                Place::new(inbox, subscribers),
                motor,
                Model2Params::default(),
            )],
            None,
            moving_poll_interval,
            idle_poll_interval,
            forced_fast_polls,
            SharedControllerPoll::default(),
        )
    }

    /// One poller over a controller's `axes`, polled in index order after
    /// `controller_poll`.
    pub(crate) fn controller(
        axes: Vec<Axis>,
        joining: Option<mpsc::UnboundedReceiver<Joining>>,
        moving_poll_interval: Duration,
        idle_poll_interval: Duration,
        forced_fast_polls: u32,
        controller_poll: SharedControllerPoll,
    ) -> Self {
        controller_poll.state().periods = PollPeriods {
            moving: moving_poll_interval,
            idle: idle_poll_interval,
        };
        let schedule = Schedule::Poller {
            forced_fast_polls,
            remaining: 0,
        };
        Self::assembled(axes, joining, schedule, controller_poll)
    }

    /// A model-1 driver's task over its `axes`, scanning the held moves
    /// every `scan`. C motor_task has no startup pass: each axis's status is
    /// the one its `motor_init` read ([`Axis::task`]).
    pub(crate) fn motor_task(axes: Vec<Axis>, scan: Duration) -> Self {
        let none = SharedControllerPoll::default();
        Self::assembled(axes, None, Schedule::MotorTask { scan }, none)
    }

    /// The loop over `axes` in address order. A poller's startup pass runs
    /// on its own thread ([`Self::serve`]), as C's does.
    fn assembled(
        mut axes: Vec<Axis>,
        joining: Option<mpsc::UnboundedReceiver<Joining>>,
        schedule: Schedule,
        controller_poll: SharedControllerPoll,
    ) -> Self {
        axes.sort_by_key(Axis::order);
        Self {
            axes,
            joining,
            schedule,
            controller_poll,
        }
    }

    /// An add to the running controller. C's poller runs one startup pass;
    /// a later axis is first polled on the next pass, and a record bound
    /// before reads what its constructor set.
    fn join(&mut self, Joining { join, joined }: Joining) {
        join.place(&mut self.axes);
        let _ = joined.send(());
    }

    /// The axes added before a pass took the controller lock: C's are in
    /// its list when it walks it (asynMotorController.cpp:648-658).
    fn join_from(joining: &mut Option<mpsc::UnboundedReceiver<Joining>>, axes: &mut Vec<Axis>) {
        while let Some(Joining { join, joined }) =
            joining.as_mut().and_then(|rx| rx.try_recv().ok())
        {
            join.place(axes);
            let _ = joined.send(());
        }
    }

    /// The poller thread's startup pass, on the calling thread: for a
    /// harness standing in for the poller thread, whose first act it is.
    pub fn run_startup_pass(&mut self) {
        if self.is_motor_task() {
            return;
        }
        let moving = self.poll_axes(true);
        for axis in &mut self.axes {
            for io_intr in axis.pulses.drain(..) {
                let _ = io_intr.try_send(());
            }
        }
        self.next_pass(moving);
    }

    /// Run the commands queued so far on the calling thread: for a harness
    /// standing in for the poller thread. The IOC path is [`Self::start`]'s
    /// thread.
    pub fn run_queued_commands(&mut self) {
        for axis in &mut self.axes {
            axis.run_queued_commands(&self.controller_poll);
        }
    }

    /// Run a batch of axis `index`'s commands and send what they posted, as
    /// C's write ends in `pAxis->callParamCallbacks()` before its
    /// `wakeupPoller` (asynMotorController.cpp:317-320).
    async fn execute(&mut self, index: usize, place: usize, message: ToAxis) {
        let axis = &mut self.axes[index];
        axis.receive(place, message, &self.controller_poll);
        axis.send_pulses().await;
    }

    fn is_motor_task(&self) -> bool {
        matches!(self.schedule, Schedule::MotorTask { .. })
    }

    /// One wake of `motor_task`, before it processes the messages: query
    /// each held move (motordrvCom.cc:244-254), its callback queued as its
    /// query returns (:280-318).
    async fn query_task(&mut self) {
        let polled = polled::after(&self.controller_poll);
        for axis in &mut self.axes {
            axis.query_task(&polled);
            axis.offer_pulses();
        }
        drop(polled);
        self.send_pulses().await;
    }

    /// The pulses a pass still owes, in axis order, once the controller's
    /// lock is released: the record processing that frees a full queue may
    /// wait on a port write that waits on the lock.
    async fn send_pulses(&mut self) {
        for axis in &mut self.axes {
            axis.send_pulses().await;
        }
    }

    /// `motor_task`'s next timed wake, a scan period after `tick` while a
    /// move is held, and none while idle (motordrvCom.cc:216-238).
    fn task_due(&self, tick: Instant) -> Option<Instant> {
        match self.schedule {
            Schedule::MotorTask { scan } if self.axes.iter().any(Axis::holds_move) => {
                Some(tick + scan)
            }
            _ => None,
        }
    }

    /// One pass of C `asynMotorPoller` (asynMotorController.cpp:632-695):
    /// the controller poll, then every axis in turn under the controller's
    /// lock, its change posted to its records before the next axis polls as
    /// `pAxis->poll` ends in callParamCallbacks, and return when the next
    /// timed pass is due. `woken` is a `wakeupPoller`, which reloads the
    /// forced fast polls.
    async fn poller_pass(&mut self, woken: bool) -> Option<Instant> {
        let moving = self.poll_axes(woken);
        self.send_pulses().await;
        self.next_pass(moving)
    }

    /// [`Self::poller_pass`]'s polls under the controller lock, each axis's
    /// pulses offered; whether any axis moves.
    fn poll_axes(&mut self, woken: bool) -> bool {
        if woken {
            self.reload_forced_fast_polls();
        }
        // Read on the port's thread, which may be waiting on the lock.
        let off_delays: Vec<_> = self
            .axes
            .iter()
            .map(|axis| {
                let delay = axis.auto_power(crate::controller_port::index::POWER_OFF_DELAY);
                (Arc::as_ptr(&axis.motor), delay)
            })
            .collect();
        let polled = polled::after(&self.controller_poll);
        // C's pass walks pAxes_ under the lock (asynMotorController.cpp:
        // 658), so it polls an axis added while it waited for the lock. One
        // that joins here has not moved, so no off delay applies yet.
        Self::join_from(&mut self.joining, &mut self.axes);
        let mut moving = false;
        for axis in &mut self.axes {
            let off_delay = off_delays
                .iter()
                .find(|(motor, _)| std::ptr::addr_eq(*motor, Arc::as_ptr(&axis.motor)))
                .and_then(|(_, delay)| *delay);
            moving |= axis.poller_poll(&polled, off_delay);
            axis.first_poll = None;
            axis.offer_pulses();
        }
        drop(polled);
        moving
    }

    fn reload_forced_fast_polls(&mut self) {
        if let Schedule::Poller {
            forced_fast_polls,
            remaining,
            ..
        } = &mut self.schedule
        {
            *remaining = *forced_fast_polls;
        }
    }

    /// A timed wake of either task.
    async fn timed_pass(&mut self) -> Option<Instant> {
        if !self.is_motor_task() {
            return self.poller_pass(false).await;
        }
        let tick = Instant::now();
        self.query_task().await;
        self.task_due(tick)
    }

    /// C's timeout choice (:686-694); a zero period waits on the event alone
    /// (:633-634).
    fn next_pass(&mut self, moving: bool) -> Option<Instant> {
        let Schedule::Poller { remaining, .. } = &mut self.schedule else {
            return None;
        };
        let periods = self.controller_poll.state().periods;
        let timeout = if *remaining > 0 {
            *remaining -= 1;
            periods.moving
        } else if moving {
            periods.moving
        } else {
            periods.idle
        };
        (!timeout.is_zero()).then(|| Instant::now() + timeout)
    }

    /// Start the poll loop on a dedicated `motorPoller` thread, which serves
    /// until Shutdown. The thread exists when this returns, as C's does when
    /// `startPoller` returns, so no executor has to run first; it parks
    /// between polls on the background timer's timers.
    pub fn start(self) -> PollerThread {
        let startup = (!self.is_motor_task()).then(|| self.controller_poll.first_poll_pending());
        let (done_tx, done_rx) = oneshot::channel::<()>();
        // C startPoller (asynMotorController.cpp:572-575).
        let spawned = spawn_dedicated_thread(
            "motorPoller".into(),
            ThreadPriority::Low,
            StackSizeClass::Medium,
            move || {
                let _ = block_on_sync(self.serve(startup));
                drop(done_tx);
            },
        );
        match spawned {
            Ok(_) => PollerThread(Some(done_rx)),
            Err(e) => {
                thread_create_refused(&e);
                PollerThread(None)
            }
        }
    }

    /// The next queued message of any axis: every axis's driver commands
    /// first, so a ForcePoll sees their effect.
    fn inbox(
        axes: &mut [Axis],
        joining: &mut Option<mpsc::UnboundedReceiver<Joining>>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Wake> {
        use std::task::Poll;
        for (index, axis) in axes.iter().enumerate() {
            for (at, place) in axis.places.iter().enumerate() {
                let mut queue = place.queue();
                if let Some(batch) = queue.batches.pop_front() {
                    return Poll::Ready(Wake::Batch(index, at, batch.into()));
                }
                if let Poll::Ready(Some(message)) = queue.inbox.commands.poll_recv(cx) {
                    return Poll::Ready(Wake::Batch(index, at, message));
                }
            }
        }
        for (index, axis) in axes.iter().enumerate() {
            for place in &axis.places {
                if let Poll::Ready(cmd) = place.queue().inbox.poll.poll_recv(cx) {
                    return Poll::Ready(Wake::Command(index, cmd));
                }
            }
        }
        if let Some(rx) = joining {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(join)) => return Poll::Ready(Wake::Join(Box::new(join))),
                // The axis list is gone: no axis can join any more.
                Poll::Ready(None) => *joining = None,
                Poll::Pending => {}
            }
        }
        Poll::Pending
    }

    /// The pass a wakeup runs, and when the next is due.
    async fn wakeup_pass(&mut self) -> Option<Instant> {
        if self.is_motor_task() {
            let tick = Instant::now();
            self.query_task().await;
            self.task_due(tick)
        } else {
            self.poller_pass(true).await
        }
    }

    /// The poller's first pass, which C's `wakeupPoller` at its start makes
    /// a woken one (asynMotorController.cpp:630). It runs on the poller's
    /// thread, so whether a record initialises before or after it is the
    /// threads' timing, as in C.
    pub(crate) async fn startup_pass(&mut self) -> Option<Instant> {
        if self.is_motor_task() {
            return None;
        }
        self.poller_pass(true).await
    }

    async fn serve(mut self, startup: Option<FirstPollPending>) {
        let mut due = self.startup_pass().await;
        drop(startup);
        loop {
            for axis in &mut self.axes {
                axis.send_pulses().await;
            }
            // Only a poller pass moves `due`: C's motorUpdateStatus_ and
            // non-move writes run on the port thread, beside the poller's
            // running event wait.
            let wait = due.map(|at| at.saturating_duration_since(Instant::now()));
            let woken = self.controller_poll.wake.clone();
            let wake = tokio::select! {
                biased;
                wake = std::future::poll_fn(|cx| Self::inbox(&mut self.axes, &mut self.joining, cx)) => wake,
                _ = woken.notified() => Wake::Wakeup,
                _ = sleep_background(wait.unwrap_or_default()), if wait.is_some() => Wake::Due,
            };
            match wake {
                Wake::Batch(index, place, batch) if self.is_motor_task() => {
                    // One wake drains the driver's whole queue after the
                    // query (motordrvCom.cc:255, :351).
                    let tick = Instant::now();
                    self.query_task().await;
                    self.execute(index, place, batch).await;
                    for axis in &mut self.axes {
                        axis.run_queued_commands(&self.controller_poll);
                    }
                    due = self.task_due(tick);
                }
                Wake::Batch(index, place, batch) => {
                    self.execute(index, place, batch).await;
                    if std::mem::take(&mut self.axes[index].woken) {
                        due = self.poller_pass(true).await;
                    }
                }
                Wake::Command(index, cmd) => match cmd {
                    Some(PollCommand::ForcePoll) if self.is_motor_task() => {
                        let tick = Instant::now();
                        self.query_task().await;
                        let polled = polled::after(&self.controller_poll);
                        let axis = &mut self.axes[index];
                        let status = axis.poll_result(&polled);
                        axis.publish_info(status);
                        due = self.task_due(tick);
                    }
                    // C motorUpdateStatus_ (asynMotorController.cpp:217-221).
                    Some(PollCommand::ForcePoll) => {
                        let polled = polled::after(&self.controller_poll);
                        self.axes[index].poll_and_notify(true, &polled);
                    }
                    Some(PollCommand::Wakeup) => due = self.wakeup_pass().await,
                    Some(PollCommand::Post(status)) => self.axes[index].notify(status, false),
                    // C shutdownPoller ends the controller's one poller.
                    Some(PollCommand::Shutdown) | None => return,
                },
                Wake::Join(join) => self.join(*join),
                Wake::Wakeup => due = self.wakeup_pass().await,
                Wake::Due => {
                    due = self.timed_pass().await;
                }
            }
        }
    }
}

/// What woke the poller thread.
enum Wake {
    /// A message on axis `.0`'s place `.1`.
    Batch(usize, usize, ToAxis),
    Command(usize, Option<PollCommand>),
    Join(Box<Joining>),
    /// [`SharedControllerPoll::wakeup`].
    Wakeup,
    Due,
}

/// C `asynMotorController`'s parameter index for `cmd` (createParam order,
/// asynMotorController.cpp:57-129) and the value written to it.
/// The profile functions are the controller's, whose port prints their
/// write; the axis calls behind them are direct driver calls in C.
pub(crate) fn c_write(cmd: &MotorCommand) -> Option<(i32, WriteValue)> {
    use crate::controller_port::index as c;
    use WriteValue::{Float64, Int32};
    let (function, value) = match *cmd {
        MotorCommand::MoveRelative { distance, .. } => (c::MOVE_REL, Float64(distance)),
        MotorCommand::MoveAbsolute { position, .. } => (c::MOVE_ABS, Float64(position)),
        MotorCommand::MoveVelocity { velocity, .. }
        | MotorCommand::JogVelocity { velocity, .. } => (c::MOVE_VEL, Float64(velocity)),
        MotorCommand::Home { forward, .. } => (c::HOME, Float64(f64::from(u8::from(forward)))),
        // devMotorAsyn's STOP_AXIS leaves `ivalue` at 0.
        MotorCommand::Stop { .. } => (c::STOP, Int32(0)),
        MotorCommand::SetPosition { position } | MotorCommand::InitPosition { position } => {
            (c::POSITION, Float64(position))
        }
        MotorCommand::SetEncoderPosition { position } => (c::ENCODER_POSITION, Float64(position)),
        MotorCommand::DeferMoves { defer } => (c::DEFER_MOVES, Int32(defer.into())),
        MotorCommand::SetEncoderRatio { .. } | MotorCommand::InitEncoderRatio { .. } => {
            (c::ENCODER_RATIO, Float64(cmd.encoder_ratio()?))
        }
        MotorCommand::MoveToHome => (c::MOVE_TO_HOME, Int32(1)),
        MotorCommand::SetPidGain { kind, gain } => match kind {
            PidGainKind::Proportional => (c::PGAIN, Float64(gain)),
            PidGainKind::Integral => (c::IGAIN, Float64(gain)),
            PidGainKind::Derivative => (c::DGAIN, Float64(gain)),
        },
        MotorCommand::SetHighLimit { position } => (c::HIGH_LIMIT, Float64(position)),
        MotorCommand::SetLowLimit { position } => (c::LOW_LIMIT, Float64(position)),
        MotorCommand::SetClosedLoop { enable } => (c::CLOSED_LOOP, Int32(enable.into())),
        MotorCommand::SetVelocity { velocity } => (c::VELOCITY, Float64(velocity)),
        MotorCommand::SetVelBase { velocity } => (c::VEL_BASE, Float64(velocity)),
        MotorCommand::SetAccel { acceleration } => (c::ACCEL, Float64(acceleration)),
        MotorCommand::Poll => (c::UPDATE_STATUS, Int32(0)),
        MotorCommand::ProfileInitialize { .. }
        | MotorCommand::ProfileDefine { .. }
        | MotorCommand::ProfileBuild
        | MotorCommand::ProfileExecute
        | MotorCommand::ProfileAbort
        | MotorCommand::ProfileReadback
        | MotorCommand::OutParm { .. }
        | MotorCommand::Primitive { .. }
        | MotorCommand::MoveStrings { .. }
        | MotorCommand::MoveDirection { .. }
        | MotorCommand::RecordFields(_)
        | MotorCommand::Driver(_) => return None,
    };
    Some((function as i32, value))
}

/// drvMotorAsyn's `motorCommand` for `cmd` (drvMotorAsyn.c:76-104), the
/// reason a model-2 write carries. Profiles and the encoder position, which
/// only the controller port sends, have none.
fn drv_motor_asyn_reason(cmd: &MotorCommand) -> Option<i32> {
    Some(match *cmd {
        MotorCommand::SetPosition { .. } | MotorCommand::InitPosition { .. } => 0,
        MotorCommand::SetEncoderRatio { .. } | MotorCommand::InitEncoderRatio { .. } => 2,
        MotorCommand::SetLowLimit { .. } => 3,
        MotorCommand::SetHighLimit { .. } => 4,
        MotorCommand::SetPidGain { kind, .. } => match kind {
            PidGainKind::Proportional => 5,
            PidGainKind::Integral => 6,
            PidGainKind::Derivative => 7,
        },
        MotorCommand::SetClosedLoop { .. } => 8,
        MotorCommand::DeferMoves { .. } => 26,
        MotorCommand::MoveToHome => 27,
        MotorCommand::SetVelocity { .. } => 100,
        MotorCommand::SetVelBase { .. } => 101,
        MotorCommand::SetAccel { .. } => 102,
        MotorCommand::MoveRelative { .. } => 103,
        MotorCommand::MoveAbsolute { .. } => 104,
        MotorCommand::MoveVelocity { .. } | MotorCommand::JogVelocity { .. } => 105,
        MotorCommand::Home { .. } => 106,
        MotorCommand::Stop { .. } => 107,
        MotorCommand::Poll => 109,
        MotorCommand::SetEncoderPosition { .. }
        | MotorCommand::ProfileInitialize { .. }
        | MotorCommand::ProfileDefine { .. }
        | MotorCommand::ProfileBuild
        | MotorCommand::ProfileExecute
        | MotorCommand::ProfileAbort
        | MotorCommand::ProfileReadback
        | MotorCommand::OutParm { .. }
        | MotorCommand::Primitive { .. }
        | MotorCommand::MoveStrings { .. }
        | MotorCommand::MoveDirection { .. }
        | MotorCommand::RecordFields(_)
        | MotorCommand::Driver(_) => return None,
    })
}

/// Whether drvMotorAsyn hands `reason` to the driver's `setDouble` /
/// `setInteger` (drvMotorAsyn.c:613-623, 735-745), which store it as a
/// parameter. Position is a status field, posted through it.
fn is_driver_param(reason: i32) -> bool {
    (1..100).contains(&reason)
}

/// The drvMotorAsyn reason and value of a record's `cmd`: what devMotorAsyn
/// writes to a model-2 port.
fn drv_motor_asyn_write(cmd: &MotorCommand) -> Option<(i32, WriteValue)> {
    let (_, value) = c_write(cmd)?;
    Some((drv_motor_asyn_reason(cmd)?, value))
}

/// devMotorAsyn's `pasynFloat64->write` / `pasynInt32->write` of `write` to
/// the model-2 port on the record's axis (devMotorAsyn.c:689-708).
fn write_drv_motor_asyn(
    port: &asyn_rs::port_handle::PortHandle,
    trace: &MotorTrace,
    (reason, value): (i32, WriteValue),
) -> asyn_rs::error::AsynResult<()> {
    let reason = reason as usize;
    match value {
        WriteValue::Float64(v) => port.write_float64_blocking(reason, trace.addr(), v),
        WriteValue::Int32(v) => port.write_int32_blocking(reason, trace.addr(), v),
    }
}

/// The position a SET_POSITION or encoder-position write sets.
fn position_write(cmd: &MotorCommand) -> Option<StatusWrite> {
    match *cmd {
        MotorCommand::SetPosition { position } | MotorCommand::InitPosition { position } => {
            Some(StatusWrite::Position(position))
        }
        MotorCommand::SetEncoderPosition { position } => {
            Some(StatusWrite::EncoderPosition(position))
        }
        _ => None,
    }
}

/// The DONE=0 a move sets.
fn move_start_write(cmd: &MotorCommand) -> Option<StatusWrite> {
    matches!(
        cmd,
        MotorCommand::MoveAbsolute { .. }
            | MotorCommand::MoveRelative { .. }
            | MotorCommand::MoveVelocity { .. }
            | MotorCommand::JogVelocity { .. }
            | MotorCommand::Home { .. }
    )
    .then_some(StatusWrite::Done(false))
}

/// What a `motorAxisDrvSET_t` driver's write `c` of `cmd` set: its own
/// status fields, then the position or DONE=0 a write that succeeded sets,
/// and the parameter it stored, which it stores only on success
/// (drvMotorSim.c:364-368, 424-428).
fn drvset_axis_write(
    motor: &mut dyn AsynMotor,
    cmd: &MotorCommand,
    c: Option<(i32, WriteValue)>,
    ok: bool,
) -> AxisWrite {
    let mut fields = motor.take_status_writes();
    if ok {
        fields.extend(position_write(cmd).or(move_start_write(cmd)));
    }
    let param = c.filter(|&(reason, _)| ok && is_driver_param(reason));
    AxisWrite {
        params: fields
            .iter()
            .map(|&w| model2_param(w))
            .chain(param)
            .collect(),
        fields,
        polled: None,
    }
}

/// The paramLib parameter a model-2 driver's `write` sets, by its
/// `motorAxisParam_t` index (motor_interface.h:278-307).
fn model2_param(write: StatusWrite) -> (i32, WriteValue) {
    let bit = |v: bool| WriteValue::Int32(v.into());
    match write {
        StatusWrite::Position(v) => (0, WriteValue::Float64(v)),
        StatusWrite::EncoderPosition(v) => (9, WriteValue::Float64(v)),
        StatusWrite::Direction(v) => (11, bit(v)),
        StatusWrite::Done(v) => (12, bit(v)),
        StatusWrite::HighHardLimit(v) => (13, bit(v)),
        StatusWrite::HomeSignal(v) => (14, bit(v)),
        StatusWrite::EncoderHome(v) => (18, bit(v)),
        StatusWrite::PowerOn(v) => (16, bit(v)),
        StatusWrite::Problem(v) => (20, bit(v)),
        StatusWrite::Moving(v) => (21, bit(v)),
        StatusWrite::CommsError(v) => (23, bit(v)),
        StatusWrite::LowHardLimit(v) => (24, bit(v)),
        StatusWrite::Homed(v) => (25, bit(v)),
        StatusWrite::HasEncoder(v) => (19, bit(v)),
        StatusWrite::GainSupport(v) => (22, bit(v)),
    }
}

/// What one command wrote on the axis.
#[derive(Debug, Default)]
pub struct AxisWrite {
    /// The status fields it set.
    pub fields: Vec<StatusWrite>,
    /// The model-2 parameters it stored, by drvMotorAsyn reason: the
    /// status fields the driver set and the parameter the write carried.
    params: Vec<(i32, WriteValue)>,
    /// The driver poll a GET_INFO ran.
    polled: Option<Result<MotorStatus, FailedPoll>>,
}

/// Whether `status` changes C's `status_` from `last`: every field but
/// `moving`, which is poll's `*moving` out-argument, not a parameter.
fn changes_status(last: &MotorStatus, status: &MotorStatus) -> bool {
    *last
        != MotorStatus {
            moving: last.moving,
            ..status.clone()
        }
}

/// A failed driver poll: the bits its error label raises and the status it
/// wrote before bailing.
#[derive(Debug, Clone)]
struct FailedPoll {
    bits: PollFailure,
    left: MotorStatus,
}

impl FailedPoll {
    /// A poisoned driver mutex has no C counterpart; it raises both bits
    /// over `left`, which no poll wrote.
    fn poisoned(left: MotorStatus) -> Self {
        Self {
            bits: PollFailure::PROBLEM_AND_COMMS_ERROR,
            left,
        }
    }

    fn of(motor: &dyn AsynMotor, left: MotorStatus) -> Self {
        Self {
            bits: motor.poll_failure(),
            left,
        }
    }
}

/// The driver's poll into `status`, an `Err` raising what its
/// `poll_failure` names over what it wrote.
fn poll_driver(
    motor: &mut dyn AsynMotor,
    user: &AsynUser,
    mut status: MotorStatus,
) -> Result<MotorStatus, FailedPoll> {
    match motor.poll(user, &mut status) {
        Ok(()) => Ok(status),
        Err(_) => Err(FailedPoll::of(motor, status)),
    }
}

/// Whether a failed `cmd` prints C `devMotorAsyn::asynCallback`'s write line
/// (devMotorAsyn.c:661-713); `GET_INFO` has no failure print.
fn sent_by_dev_motor_asyn(cmd: &MotorCommand) -> bool {
    !matches!(cmd, MotorCommand::Poll) && dev_motor_asyn_command(cmd).is_some()
}

/// devMotorAsyn's `motorCommand` (devMotorAsyn.c:101-121) for a record's
/// `cmd`, which `build_trans` queues for `asynCallback`. Deferred moves,
/// move-to-home and profiles reach the C driver through other records;
/// init_controller's writes are synchronous.
fn dev_motor_asyn_command(cmd: &MotorCommand) -> Option<i32> {
    Some(match *cmd {
        MotorCommand::MoveAbsolute { .. } => 0,
        MotorCommand::MoveRelative { .. } => 1,
        MotorCommand::MoveVelocity { .. } | MotorCommand::JogVelocity { .. } => 2,
        MotorCommand::Home { .. } => 3,
        MotorCommand::Stop { .. } => 4,
        MotorCommand::SetVelocity { .. } => 5,
        MotorCommand::SetVelBase { .. } => 6,
        MotorCommand::SetAccel { .. } => 7,
        MotorCommand::SetPosition { .. } => 8,
        MotorCommand::SetEncoderRatio { .. } => 10,
        MotorCommand::SetPidGain { kind, .. } => match kind {
            PidGainKind::Proportional => 11,
            PidGainKind::Integral => 12,
            PidGainKind::Derivative => 13,
        },
        MotorCommand::SetHighLimit { .. } => 14,
        MotorCommand::SetLowLimit { .. } => 15,
        MotorCommand::SetClosedLoop { .. } => 16,
        MotorCommand::Poll => 18,
        _ => return None,
    })
}

/// C devMotorAsyn's `motorAsynMessage` (devMotorAsyn.c:125-130), whose
/// address and size `asynCallback` prints.
#[repr(C)]
struct MotorAsynMessage {
    command: i32,
    interface: i32,
    ivalue: i32,
    dvalue: f64,
}

impl MotorAsynMessage {
    /// The message `build_trans` queues for a record's `cmd`: both values
    /// zeroed, then the one its interface takes set.
    fn of(cmd: &MotorCommand) -> Option<Self> {
        let command = dev_motor_asyn_command(cmd)?;
        let (_, value) = c_write(cmd)?;
        Some(match value {
            WriteValue::Int32(ivalue) => Self {
                command,
                interface: 0,
                ivalue,
                dvalue: 0.0,
            },
            WriteValue::Float64(dvalue) => Self {
                command,
                interface: 1,
                ivalue: 0,
                dvalue,
            },
        })
    }
}

/// The motorRecord `build_trans` calls (motor_cmnd, value) behind a record's
/// `cmd`, the last one the call that queues it. A move is its MOVE_ABS,
/// MOVE_REL or HOME_FOR/HOME_REV and then GO; HOME's value is `hpos = 0`
/// (motorRecord.cc:2048).
fn build_trans_calls(cmd: &MotorCommand) -> Vec<(i32, f64)> {
    const GO: (i32, f64) = (8, 0.0);
    match *cmd {
        MotorCommand::MoveAbsolute { position, .. } => vec![(0, position), GO],
        MotorCommand::MoveRelative { distance, .. } => vec![(1, distance), GO],
        MotorCommand::Home { forward, .. } => vec![(if forward { 2 } else { 3 }, 0.0), GO],
        MotorCommand::SetPosition { position } => vec![(4, position)],
        MotorCommand::SetVelBase { velocity } => vec![(5, velocity)],
        MotorCommand::SetVelocity { velocity } => vec![(6, velocity)],
        MotorCommand::SetAccel { acceleration } => vec![(7, acceleration)],
        MotorCommand::SetEncoderRatio { pulses } => vec![(9, pulses[0])],
        MotorCommand::Poll => vec![(10, 0.0)],
        MotorCommand::Stop { .. } => vec![(11, 0.0)],
        MotorCommand::MoveVelocity { velocity, .. } => vec![(12, velocity)],
        MotorCommand::JogVelocity { velocity, .. } => vec![(21, velocity)],
        MotorCommand::SetPidGain { kind, gain } => vec![(
            match kind {
                PidGainKind::Proportional => 13,
                PidGainKind::Integral => 14,
                PidGainKind::Derivative => 15,
            },
            gain,
        )],
        // special CNEN passes `temp_dbl = cnen` (motorRecord.cc:3032-3037).
        MotorCommand::SetClosedLoop { enable } => {
            vec![(if enable { 16 } else { 17 }, f64::from(u8::from(enable)))]
        }
        MotorCommand::SetHighLimit { position } => vec![(19, position)],
        MotorCommand::SetLowLimit { position } => vec![(20, position)],
        _ => Vec::new(),
    }
}

/// devMotorAsyn `build_trans`'s FLOW lines for a record's `commands`
/// (devMotorAsyn.c:497-499, :634-637), sent with the record's `pact`. A
/// record in COMM_ALARM prints each call's first line and queues nothing
/// (:530-531).
pub(crate) fn build_trans_flow(
    trace: &MotorTrace,
    commands: &[MotorCommand],
    pact: bool,
    comm_alarm: bool,
) {
    if trace.is_model1() {
        return;
    }
    let reason = trace.update_status_reason();
    for cmd in commands {
        let calls = build_trans_calls(cmd);
        for &(motor_cmnd, value) in &calls {
            trace.print(
                TraceMask::FLOW,
                reason,
                format_args!(
                    "devMotorAsyn::build_trans: {} motor_cmnd={motor_cmnd}, pact={}, value={:.6}\n",
                    trace.record(),
                    u8::from(pact),
                    CFloat(value),
                ),
            );
        }
        if calls.is_empty() || comm_alarm {
            continue;
        }
        let Some(msg) = MotorAsynMessage::of(cmd) else {
            continue;
        };
        // C's format has no separator after the size.
        trace.print(
            TraceMask::FLOW,
            reason,
            format_args!(
                "devAsynMotor::build_trans: calling queueRequest, pmsg={:p}, sizeof(*pmsg)={}pmsg->command={}, pmsg->interface={}, pmsg->dvalue={:.6}\n",
                &msg,
                std::mem::size_of::<MotorAsynMessage>(),
                msg.command,
                msg.interface,
                CFloat(msg.dvalue),
            ),
        );
    }
}

/// C `asynCallback`'s opening line for a record's `cmd`
/// (devMotorAsyn.c:668-674), on the reason the port's write carries.
fn async_callback_flow(trace: &MotorTrace, cmd: &MotorCommand) {
    if trace.is_model1() {
        return;
    }
    let (Some(msg), Some((function, _))) = (MotorAsynMessage::of(cmd), c_write(cmd)) else {
        return;
    };
    let reason = if trace.is_model2() {
        drv_motor_asyn_reason(cmd).unwrap_or(0)
    } else {
        function
    };
    trace.print(
        TraceMask::FLOW,
        reason,
        format_args!(
            "devMotorAsyn::asynCallback: {} pmsg={:p}, sizeof(*pmsg)={}, pmsg->command={},pmsg->interface={}, pmsg->ivalue={}, pmsg->dvalue={:.6}, pasynUser->reason={reason}\n",
            trace.record(),
            &msg,
            std::mem::size_of::<MotorAsynMessage>(),
            msg.command,
            msg.interface,
            msg.ivalue,
            CFloat(msg.dvalue),
        ),
    );
}

/// Run `commands` against the driver in order. Only the poller thread calls
/// this outside tests, so a blocking driver stalls nothing but that thread.
///
/// Each command prints what C's driver layer prints for it — on a model-3
/// axis `asynMotorController::writeFloat64` / `writeInt32`
/// (asynMotorController.cpp:181-427), on a model-2 axis drvMotorAsyn's
/// (drvMotorAsyn.c:575-735) — and then devMotorAsyn's `asynCallback` print,
/// on the record's asyn user.
///
/// After each command `on_write` gets the status fields that write set: on
/// an `asynMotorController` axis the position SET_POSITION stores before
/// `setPosition` (asynMotorController.cpp:289) and the DONE=0 every move
/// sets after it (:301, :318, :334, :353), around whatever the driver set
/// itself ([`AsynMotor::take_status_writes`]). A model-2 driver sets the
/// position and DONE=0 only once the write succeeded, after its own
/// fields (drvMM4000Asyn.c:388, :485, :518; drvEnsembleAsyn.cc:539), and
/// the parameter the write stored. A [`WriteHook`]'s sets after a write
/// land in the controller port's parameters once `on_write` has the write:
/// the next `callParamCallbacks` on the axis posts them, as C's does.
///
/// [`WriteHook`]: crate::controller_port::WriteHook
pub fn execute_commands(
    motor: &mut dyn AsynMotor,
    trace: &MotorTrace,
    commands: &[MotorCommand],
    on_write: &mut dyn FnMut(AxisWrite),
) {
    let port = PortCtx::of(trace);
    let axis = crate::devices::axis_at(trace.port(), trace.addr());
    let poll = match &axis {
        Some(axis) if trace.is_controller() => axis.controller_poll().clone(),
        _ => SharedControllerPoll::default(),
    };
    let records = axis.map(|axis| axis.records().clone()).unwrap_or_default();
    for cmd in commands {
        execute_command(
            trace,
            cmd,
            &port,
            &poll,
            |_, c, quiet| Some(drive(&mut *motor, trace, cmd, c, quiet, &records)),
            on_write,
        );
    }
}

/// The controller port a record's commands reach beside the driver: the
/// subclass's [`WriteHook`] and whether the base sets parameters there.
/// Neither off a controller axis.
///
/// [`WriteHook`]: crate::controller_port::WriteHook
#[derive(Default)]
struct PortCtx {
    hook: Option<Arc<dyn crate::controller_port::WriteHook>>,
    params: bool,
}

impl PortCtx {
    fn of(trace: &MotorTrace) -> Self {
        if !trace.is_controller() {
            return Self::default();
        }
        Self {
            hook: crate::devices::write_hook(trace.port()),
            params: true,
        }
    }
}

/// The base function a record's `cmd` reaches on `trace`'s axis and the
/// value it writes, and whether the base prints none of its lines: a
/// drvMotorAsyn port, or a subclass branch that skips the base.
pub(crate) fn base_call(
    trace: &MotorTrace,
    hook: Option<&Arc<dyn crate::controller_port::WriteHook>>,
    cmd: &MotorCommand,
) -> (Option<(i32, WriteValue)>, bool) {
    // A motorAxisDrvSET_t driver, behind drvMotorAsyn or devMotorSim.
    let c = if trace.is_model2() || trace.is_motor_interface() {
        drv_motor_asyn_write(cmd)
    } else {
        c_write(cmd)
    };
    let quiet = trace.is_model2()
        || hook.is_some_and(|hook| c.is_some_and(|(function, _)| hook.handles(function as usize)));
    (c, quiet)
}

/// One record command under `poll`'s controller lock, as C's whole
/// `writeFloat64` runs under it: the subclass's check, the driver call
/// `drive` makes under the axis's lock, then, with the axis's lock
/// released, the port's sets, the post, the base's closing line, the
/// subclass's code after it and devMotorAsyn's reply. A GET_INFO's `drive`
/// runs the controller poll first, as C `motorUpdateStatus_` runs
/// `pC->poll()` (asynMotorController.cpp:217-221). `drive` returns `None`
/// when the lock is poisoned: nothing more runs.
fn execute_command(
    trace: &MotorTrace,
    cmd: &MotorCommand,
    port: &PortCtx,
    poll: &SharedControllerPoll,
    drive: impl FnOnce(&ControllerHeld<'_>, Option<(i32, WriteValue)>, bool) -> Option<Driven>,
    on_write: &mut dyn FnMut(AxisWrite),
) {
    if port.params {
        poll.set_port(trace.port());
    }
    let held = poll.hold();
    let axis = trace.addr();
    let store = |delta: crate::controller_port::ParamDelta| {
        if !port.params {
            return;
        }
        let mut sets = PortSets::default();
        for (function, value) in delta.into_values() {
            sets.set(axis, function, value);
        }
        poll.post_sets(Some(axis), sets);
    };
    let hook = port.hook.as_ref();
    let (c, quiet) = base_call(trace, hook, cmd);
    let function = c.map_or(0, |(function, _)| function);
    let refusal = match (hook, c) {
        (Some(hook), Some((function, _))) => hook.check(axis, function as usize).err(),
        _ => None,
    };
    if let (Some(e), Some(hook), Some((function, value))) = (refusal, hook, c) {
        // The base never runs: nothing is written and it prints nothing.
        let status = Err(e);
        let outcome = hook.after_write(&FinishedWrite::new(
            axis,
            function as usize,
            value,
            &status,
            &|mask, args| trace.print(mask, function, args),
        ));
        on_write(AxisWrite::default());
        store(outcome.delta);
        let status = outcome.status.unwrap_or(status);
        let failure = status.as_ref().err().map(|e| e.message_display());
        dev_motor_asyn_reply(
            trace,
            cmd,
            function,
            failure.as_ref().map(|e| e as &dyn std::fmt::Display),
        );
        return;
    }
    let Some(Driven {
        result,
        write,
        port_sets,
    }) = drive(&held, c, quiet)
    else {
        return;
    };
    if port.params {
        let mut sets = PortSets::default();
        if let Some((function, value)) = c {
            // The base sets the parameter whatever the driver returns
            // (asynMotorController.cpp:204, :289); its callParamCallbacks
            // closes the write.
            let value = match value {
                WriteValue::Float64(v) => asyn_rs::param::ParamValue::Float64(v),
                WriteValue::Int32(v) => asyn_rs::param::ParamValue::Int32(v),
            };
            sets.set(axis, function as usize, value);
            sets.call_param_callbacks(axis);
        }
        // The poll's or the write's own sets, as C's paramLib.
        sets.append(port_sets);
        poll.post_sets(Some(axis), sets);
    }
    on_write(write);

    // The base closing line, which neither a drvMotorAsyn port nor a
    // subclass branch that skips the base prints.
    if let (false, Some((_, value))) = (quiet, c) {
        let status = match &result {
            Ok(()) => 0,
            Err(e) => e.c_status_code(),
        };
        let print = |iface: &str, value: std::fmt::Arguments<'_>| {
            if status != 0 {
                trace.print(
                    TraceMask::ERROR,
                    function,
                    format_args!(
                        "asynMotorController:{iface} error, status={status} axis={axis}, function={function}, value={value}\n"
                    ),
                );
            } else {
                trace.print(
                    TraceMask::IO_DRIVER,
                    function,
                    format_args!(
                        "asynMotorController:{iface}:: axis={axis}, function={function}, value={value}\n"
                    ),
                );
            }
        };
        match value {
            WriteValue::Float64(v) => print("writeFloat64", format_args!("{:.6}", CFloat(v))),
            WriteValue::Int32(v) => print("writeInt32", format_args!("{v}")),
        }
    }

    // What the subclass's write returns instead of the base's status.
    let mut returned = None;
    if let (Some(hook), Some((function, value))) = (hook, c) {
        let outcome = hook.after_write(&FinishedWrite::new(
            axis,
            function as usize,
            value,
            &result,
            &|mask, args| trace.print(mask, function, args),
        ));
        store(outcome.delta);
        returned = outcome.status;
    }

    let failure = match &returned {
        Some(status) => status
            .as_ref()
            .err()
            .map(|e| e.message_display().to_string()),
        // C axis methods get no record asynUser, so a failure leaves its
        // errorMessage as createAsynUser made it: empty.
        None => result.as_ref().err().map(|_| String::new()),
    };
    dev_motor_asyn_reply(
        trace,
        cmd,
        function,
        failure.as_ref().map(|e| e as &dyn std::fmt::Display),
    );
}

/// What one driver call left, taken under the axis's lock and applied once
/// it is released.
pub(crate) struct Driven {
    pub(crate) result: asyn_rs::error::AsynResult<()>,
    /// What it set on the axis, for the poller to post.
    pub(crate) write: AxisWrite,
    /// [`AsynMotor::take_port_sets`].
    pub(crate) port_sets: PortSets,
}

/// The driver call behind `cmd`, with the flow lines C's base prints beside
/// it unless `quiet`, on the axis's lock. It reaches no port, so the lock is
/// never held across a port call: one would wait on a port thread that
/// waits on the lock for a direct write. `c` is the base function and value;
/// a GET_INFO polls into the status `records` hold.
pub(crate) fn drive(
    motor: &mut dyn AsynMotor,
    trace: &MotorTrace,
    cmd: &MotorCommand,
    c: Option<(i32, WriteValue)>,
    quiet: bool,
    records: &AxisSubscribers,
) -> Driven {
    let user = AsynUser::new(0);
    let port = trace.port();
    let axis = trace.addr();
    let controller = trace.is_controller();
    let drvset = trace.is_model2() || trace.is_motor_interface();
    let function = c.map_or(0, |(function, _)| function);
    // asynMotorController's own flow lines.
    let flow = |args: std::fmt::Arguments<'_>| {
        if !quiet {
            trace.print(TraceMask::FLOW, function, args)
        }
    };
    let mut polled = None;
    let result = match *cmd {
        MotorCommand::MoveAbsolute {
            position,
            min_velocity,
            velocity,
            acceleration,
        } => {
            let r = motor.move_absolute(&user, position, min_velocity, velocity, acceleration);
            flow(format_args!(
                "asynMotorController:writeFloat64: Set driver {port}, axis {axis} move absolute to {:.6}, base velocity={:.6}, velocity={:.6}, acceleration={:.6}\n",
                CFloat(position),
                CFloat(min_velocity),
                CFloat(velocity),
                CFloat(acceleration),
            ));
            r
        }
        MotorCommand::MoveRelative {
            distance,
            min_velocity,
            velocity,
            acceleration,
        } => {
            let r = motor.move_relative(&user, distance, min_velocity, velocity, acceleration);
            flow(format_args!(
                "asynMotorController:writeFloat64: Set driver {port}, axis {axis} move relative by {:.6}, base velocity={:.6}, velocity={:.6}, acceleration={:.6}\n",
                CFloat(distance),
                CFloat(min_velocity),
                CFloat(velocity),
                CFloat(acceleration),
            ));
            r
        }
        MotorCommand::MoveVelocity {
            min_velocity,
            velocity,
            acceleration,
        }
        | MotorCommand::JogVelocity {
            min_velocity,
            velocity,
            acceleration,
        } => {
            let r = motor.move_velocity(&user, min_velocity, velocity, acceleration);
            flow(format_args!(
                "asynMotorController:writeFloat64: Set port {port}, axis {axis} move with velocity of {:.6}, acceleration={:.6}\n",
                CFloat(velocity),
                CFloat(acceleration),
            ));
            r
        }
        MotorCommand::Home {
            forward,
            min_velocity,
            velocity,
            acceleration,
        } => {
            let r = motor.home(&user, min_velocity, velocity, acceleration, forward);
            let way = if forward { "FORWARDS" } else { "REVERSE" };
            flow(format_args!(
                "asynMotorController:writeFloat64: Set driver {port}, axis {axis} to home {way}, base velocity={:.6}, velocity={:.6}, acceleration={:.6}\n",
                CFloat(min_velocity),
                CFloat(velocity),
                CFloat(acceleration),
            ));
            r
        }
        MotorCommand::Stop { acceleration } => motor.stop(&user, acceleration),
        MotorCommand::SetPosition { position } | MotorCommand::InitPosition { position } => {
            let r = motor.set_position(&user, position);
            flow(format_args!(
                "asynMotorController:writeFloat64: Set driver {port}, axis {axis} to position={:.6}\n",
                CFloat(position)
            ));
            r
        }
        MotorCommand::SetEncoderPosition { position } => {
            let r = motor.set_encoder_position(&user, position);
            flow(format_args!(
                "asynMotorController:writeFloat64: Set driver {port}, axis {axis} to encoder position={:.6}\n",
                CFloat(position)
            ));
            r
        }
        MotorCommand::SetEncoderRatio { .. } | MotorCommand::InitEncoderRatio { .. } => {
            let ratio = cmd.encoder_ratio().unwrap_or_default();
            let r = motor.set_encoder_ratio(&user, ratio);
            flow(format_args!(
                "asynMotorController:writeFloat64: Set driver {port}, axis {axis} encoder ratio={:.6}\n",
                CFloat(ratio)
            ));
            r
        }
        MotorCommand::SetClosedLoop { enable } => motor.set_closed_loop(&user, enable),
        MotorCommand::Driver(ref call) => call.run(motor, &user),
        MotorCommand::DeferMoves { defer } => motor.set_deferred_moves(&user, defer),
        MotorCommand::ProfileDefine { ref positions } => motor.define_profile(&user, positions),
        MotorCommand::ProfileBuild => motor.build_profile(&user),
        MotorCommand::ProfileExecute => motor.execute_profile(&user),
        MotorCommand::ProfileAbort => motor.abort_profile(&user),
        MotorCommand::ProfileReadback => motor.readback_profile(&user).map(|_| ()),
        // The caller signals the moveToHome thread, which calls the driver
        // (asynMotorController.cpp:236-242).
        MotorCommand::MoveToHome => {
            flow(format_args!(
                "asynMotorController:writeInt32:: Starting a move to home for axis {axis}\n"
            ));
            Ok(())
        }
        MotorCommand::SetPidGain { kind, gain } => {
            let r = motor.set_pid_gain(&user, kind, gain);
            let what = match kind {
                PidGainKind::Proportional => "proportional",
                PidGainKind::Integral => "integral",
                PidGainKind::Derivative => "derivative",
            };
            flow(format_args!(
                "asynMotorController:writeFloat64: Set driver {port}, axis {axis} {what} gain={:.6}\n",
                CFloat(gain)
            ));
            r
        }
        MotorCommand::SetHighLimit { position } | MotorCommand::SetLowLimit { position } => {
            let high = matches!(cmd, MotorCommand::SetHighLimit { .. });
            let r = if high {
                motor.set_high_limit(&user, position)
            } else {
                motor.set_low_limit(&user, position)
            };
            let which = if high { "high" } else { "low" };
            flow(format_args!(
                "asynMotorController:writeFloat64: Set driver {port}, axis {axis} {which} limit={:.6}\n",
                CFloat(position)
            ));
            r
        }
        MotorCommand::ProfileInitialize { max_points } => {
            motor.initialize_profile(&user, max_points)
        }
        MotorCommand::OutParm { ref parm } => motor.set_out_parm(&user, parm.as_bytes()),
        MotorCommand::Primitive { ref init } => motor.primitive(&user, init.as_bytes()),
        MotorCommand::MoveStrings { ref prem, ref post } => {
            motor.set_move_strings(&user, prem.as_bytes(), post.clone())
        }
        MotorCommand::RecordFields(ref fields) => motor.set_record_fields(&user, fields),
        // The motor task holds it with the move it precedes.
        MotorCommand::MoveDirection { .. } => Ok(()),
        // GET_INFO polls inside the write, before its closing line
        // (asynMotorController.cpp:217-222; drvMotorAsyn.c:617-619).
        MotorCommand::Poll => {
            if drvset {
                motor.force_callback(&user);
            }
            let mut status = records.status();
            let r = motor.poll(&user, &mut status);
            polled = Some(match &r {
                Ok(()) => Ok(status),
                Err(_) => Err(FailedPoll::of(&*motor, status)),
            });
            // drvMotorAsyn returns forceCallback's status, not the poll's.
            let succeeds = matches!(&polled, Some(Err(f)) if f.bits.succeeds);
            r.or_else(|e| {
                if controller && !succeeds {
                    Err(e)
                } else {
                    Ok(())
                }
            })
        }
        // The base only stores them; devMotorSim's store is the
        // poller's.
        MotorCommand::SetVelocity { .. }
        | MotorCommand::SetVelBase { .. }
        | MotorCommand::SetAccel { .. } => Ok(()),
    };
    let port_sets = motor.take_port_sets();
    let write = if controller {
        let mut fields = Vec::from_iter(position_write(cmd));
        fields.extend(motor.take_status_writes());
        fields.extend(move_start_write(cmd));
        AxisWrite {
            fields,
            ..AxisWrite::default()
        }
    } else if drvset {
        drvset_axis_write(motor, cmd, c, result.is_ok())
    } else {
        // Model 1: motordrvCom `process_messages` sends a MOTION or
        // VELOCITY message with no callback (motordrvCom.cc:396-438);
        // only `query_axis` on the held move calls the record back.
        let _ = motor.take_status_writes();
        AxisWrite::default()
    };
    Driven {
        result,
        write: AxisWrite { polled, ..write },
        port_sets,
    }
}

/// An axis the port answers for but never created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingAxis {
    /// `asynMotorController::getAxis` is NULL: `asynError` with the
    /// `errorMessage` left empty (asynMotorController.cpp:505-513, 552-556).
    Controller,
    /// drvMotorAsyn's refusal (drvMotorAsyn.c:584-597, 683-695, 762-775):
    /// an axis number past `numAxes`, or a slot whose `open` never succeeded.
    DrvMotorAsyn { invalid: bool },
}

impl MissingAxis {
    /// The `errorMessage` `method` leaves for axis `addr`.
    pub fn message(self, method: &str, addr: i32) -> String {
        match self {
            MissingAxis::Controller => String::new(),
            MissingAxis::DrvMotorAsyn { invalid } => {
                let what = if invalid { "Invalid" } else { "Uninitialised" };
                format!("drvMotorAsyn::{method} {what} axis {addr}")
            }
        }
    }
}

/// [`execute_commands`] for a [`MissingAxis`]: every write fails before the
/// driver prints anything, and devMotorAsyn reports it as for any failure.
pub fn refuse_commands(trace: &MotorTrace, missing: MissingAxis, commands: &[MotorCommand]) {
    let model2 = trace.is_model2();
    for command in commands {
        let c = c_write(command);
        let function = if model2 {
            drv_motor_asyn_reason(command)
        } else {
            c.map(|(function, _)| function)
        };
        let method = match c {
            Some((_, WriteValue::Int32(_))) => "writeInt32",
            _ => "writeFloat64",
        };
        let message = missing.message(method, trace.addr());
        async_callback_flow(trace, command);
        dev_motor_asyn_reply(trace, command, function.unwrap_or(0), Some(&message));
    }
}

/// devMotorAsyn's own line for a finished `cmd`: `init_controller`'s report
/// of its two synchronous writes (devMotorAsyn.c:187-235), or `asynCallback`'s
/// write failure (:700-707) carrying the driver's `errorMessage`.
fn dev_motor_asyn_reply(
    trace: &MotorTrace,
    cmd: &MotorCommand,
    function: i32,
    failure: Option<&dyn std::fmt::Display>,
) {
    let record = trace.record();
    let (what, value) = match *cmd {
        MotorCommand::InitEncoderRatio { ratio } => ("encoder ratio", ratio),
        MotorCommand::InitPosition { position } => ("position", position),
        _ => {
            if let Some(e) = failure.filter(|_| sent_by_dev_motor_asyn(cmd)) {
                trace.print(
                    TraceMask::ERROR,
                    function,
                    format_args!(
                        "devMotorAsyn::asynCallback: {record} pasyn{{Float64,Int32}}->write returned {e}\n"
                    ),
                );
            }
            return;
        }
    };
    let reason = trace.status_reason();
    match failure {
        None => trace.print(
            TraceMask::FLOW,
            reason,
            format_args!(
                "devMotorAsyn::init_controller, {record} set {what} to {:.6}\n",
                CFloat(value)
            ),
        ),
        Some(_) => trace.print(
            TraceMask::ERROR,
            reason,
            format_args!(
                "devMotorAsyn::init_controller, {record} failed to set {what} to {:.6}\n",
                CFloat(value)
            ),
        ),
    }
}

/// A model-2 port for tests: drvMotorAsyn's writes (drvMotorAsyn.c:576-752)
/// on one axis's driver.
#[cfg(test)]
pub(crate) mod drv_motor_asyn_stand_in {
    use std::sync::{Arc, Mutex};

    use asyn_rs::error::AsynResult;
    use asyn_rs::interfaces::motor::AsynMotor;
    use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
    use asyn_rs::port_handle::PortHandle;
    use asyn_rs::registry::claim_port;
    use asyn_rs::runtime::config::RuntimeConfig;
    use asyn_rs::runtime::port::create_port_runtime;
    use asyn_rs::user::AsynUser;

    use crate::device_support::MotorAxis;
    use crate::flags::StoredSpeeds;

    struct Port {
        base: PortDriverBase,
        motor: Arc<Mutex<dyn AsynMotor>>,
        axis: MotorAxis,
        speeds: StoredSpeeds,
    }

    impl PortDriver for Port {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
            let s = self.speeds;
            let mut motor = self.motor.lock().unwrap();
            match user.reason {
                107 => motor.stop(user, s.acceleration),
                106 => {
                    let r =
                        motor.home(user, s.min_velocity, s.velocity, s.acceleration, value != 0);
                    self.axis.wakeup();
                    r
                }
                109 => {
                    motor.force_callback(user);
                    self.axis.force_poll();
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
            let s = self.speeds;
            match user.reason {
                100 => self.speeds.velocity = value,
                101 => self.speeds.min_velocity = value,
                102 => self.speeds.acceleration = value,
                reason @ 103..=106 => {
                    let mut motor = self.motor.lock().unwrap();
                    let (min, max, acc) = (s.min_velocity, s.velocity, s.acceleration);
                    let r = match reason {
                        103 => motor.move_relative(user, value, min, max, acc),
                        104 => motor.move_absolute(user, value, min, max, acc),
                        105 => motor.move_velocity(user, min, value, acc),
                        _ => motor.home(user, min, max, acc, value != 0.0),
                    };
                    self.axis.wakeup();
                    return r;
                }
                _ => {}
            }
            Ok(())
        }
    }

    /// Model-2 port `port` with `motor` on axis 0, whose poller is `axis`'s.
    pub(crate) fn port(
        port: &str,
        motor: Arc<Mutex<dyn AsynMotor>>,
        axis: MotorAxis,
    ) -> PortHandle {
        let claim = claim_port(port).unwrap();
        let driver = Port {
            base: PortDriverBase::new(
                port,
                1,
                PortFlags::default(),
                asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
                asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
            ),
            motor,
            axis,
            speeds: StoredSpeeds::default(),
        };
        let (runtime, _actor) =
            create_port_runtime(&claim, driver, RuntimeConfig::default()).unwrap();
        runtime.port_handle().clone()
    }
}

#[cfg(test)]
mod tests {
    use asyn_rs::error::{AsynError, AsynResult, AsynStatus};

    use super::*;

    /// Opens `gate` on `handle`'s port and returns once the asyn-boot-flush
    /// that runs has posted, so it cannot post a test's sets late: the
    /// staged MOVE_REL posts only through that flush.
    fn open_past_boot_flush(
        handle: &asyn_rs::port_handle::PortHandle,
        gate: &epics_base_rs::runtime::interrupt_accept::InterruptGate,
    ) {
        use crate::controller_port::index;
        let set = asyn_rs::request::ParamSetValue::new(
            index::MOVE_REL,
            0,
            asyn_rs::param::ParamValue::Float64(-1.0),
        );
        handle.set_params_blocking(0, vec![set]).unwrap();
        let (_sub, mut rx) = handle
            .interrupts()
            .register_interrupt_user(
                asyn_rs::interrupt::InterruptFilter {
                    reason: Some(index::MOVE_REL),
                    addr: Some(0),
                    ..Default::default()
                },
                1,
            )
            .unwrap();
        gate.set(true);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let flushed = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await
        });
        assert!(
            matches!(flushed, Ok(Some(_))),
            "the boot flush: {flushed:?}"
        );
    }

    /// A pass on the test thread, which has no runtime to starve.
    fn now<F: std::future::Future>(pass: F) -> F::Output {
        block_on_sync(pass).expect("a test thread can block")
    }
    use crate::flags::MstaFlags;
    use crate::trace::TraceUser;

    /// A drvMotorSim axis: `created` are its constructor's sets
    /// (drvMotorSim.c:739), `updated` the sets of each update (:646-653).
    struct Sim {
        writes: Vec<StatusWrite>,
        updated: Vec<StatusWrite>,
    }

    impl AsynMotor for Sim {
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
            self.writes.extend(self.updated.iter().copied());
            *status = MotorStatus {
                done: true,
                ..MotorStatus::default()
            };
            Ok(())
        }
        fn take_status_writes(&mut self) -> Vec<StatusWrite> {
            std::mem::take(&mut self.writes)
        }
    }

    /// A started poller on `motor` at axis 0 of model-2 port `port`, and the
    /// axis's record half.
    fn model2_axis(
        port: &str,
        motor: Arc<Mutex<dyn AsynMotor>>,
    ) -> (MotorPollLoop, crate::device_support::MotorAxis) {
        let mut controller = crate::builder::ControllerBuilder::new();
        let user = TraceUser::Model2 {
            port: port.into(),
            addr: 0,
        };
        let axis = controller.axis(motor.clone(), user).axis;
        let record_half = axis.at(axis.address().clone());
        let handle = super::drv_motor_asyn_stand_in::port(port, motor, record_half.clone());
        crate::devices::register_drv_motor_asyn_port(port, 1, handle).unwrap();
        crate::devices::register_axis(axis).unwrap();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        poll_loop.run_startup_pass();
        (poll_loop, record_half)
    }

    /// [`model2_axis`] on a [`Sim`], and the axis's parameter library.
    fn model2_sim(
        port: &str,
        created: Vec<StatusWrite>,
        updated: Vec<StatusWrite>,
    ) -> (
        MotorPollLoop,
        crate::device_support::MotorAxis,
        Model2Params,
    ) {
        let sim = Sim {
            writes: created,
            updated,
        };
        let (poll_loop, record_half) = model2_axis(port, Arc::new(Mutex::new(sim)));
        let params = crate::devices::model2_params(port, 0).unwrap();
        (poll_loop, record_half, params)
    }

    /// A record bound to the first axis of `poll_loop` through `trace`,
    /// past the gate, and its io_intr pulses.
    fn bind_accepting(
        poll_loop: &MotorPollLoop,
        trace: &Arc<MotorTrace>,
    ) -> (RecordId, SharedDeviceState, mpsc::Receiver<()>) {
        let state = new_shared_state();
        state.lock().unwrap().accept();
        let (io_intr_tx, io_intr) = mpsc::channel(8);
        let record =
            poll_loop.axes[0].places[0]
                .records()
                .attach(state.clone(), io_intr_tx, trace.clone());
        (record, state, io_intr)
    }

    /// devMotorAsyn's statusCallback processes the record on every callback
    /// drvMotorAsyn's intCallback makes, a gain set's included
    /// (devMotorAsyn.c:755-762); the poll loop posts nothing of its own.
    #[test]
    fn every_model2_callback_processes_the_record_and_a_poll_posts_none() {
        let motor = EarlyReturn {
            polls: [Ok(at(1.0)), Ok(at(2.0))].into(),
            limit: 0.0,
        };
        let (mut poll_loop, axis) = model2_axis("PLM2CALLBACK", Arc::new(Mutex::new(motor)));
        let trace = Arc::new(MotorTrace::with_user(axis.address().clone()));
        let (_, state, mut io_intr) = bind_accepting(&poll_loop, &trace);
        now(poll_loop.poller_pass(true));
        assert!(
            io_intr.try_recv().is_err(),
            "a poll that moved the axis posted"
        );
        let poster = axis.status_poster().unwrap();
        let status = at(2.0);
        for _ in 0..2 {
            poster.post(status.clone());
            poll_loop.run_queued_commands();
            assert!(io_intr.try_recv().is_ok(), "the callback processed nothing");
            state.lock().unwrap().begin_callback_pass();
        }
    }

    /// A driver whose move calls back from inside the write, as a C model-2
    /// driver's move sets motorAxisDone 0 and calls back (drvMotorSim.c:
    /// 463-464).
    struct CallsBack {
        poster: Arc<Mutex<Option<crate::device_support::StatusPoster>>>,
        moving: MotorStatus,
    }

    impl AsynMotor for CallsBack {
        fn move_absolute(
            &mut self,
            _: &AsynUser,
            _: f64,
            _: f64,
            _: f64,
            _: f64,
        ) -> AsynResult<()> {
            if let Some(poster) = &*self.poster.lock().unwrap() {
                poster.post(self.moving.clone());
            }
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
            *status = at(1.0);
            Ok(())
        }
    }

    /// devMotorAsyn's asynCallback: a callback inside the move's write only
    /// copies while the move is pending, and the write's end processes the
    /// record once with it (devMotorAsyn.c:717-728, :755-762).
    #[test]
    fn a_model2_move_processes_its_record_once_with_what_the_write_called_back() {
        let poster = Arc::new(Mutex::new(None));
        let moving = MotorStatus {
            done: false,
            moving: true,
            ..at(1.0)
        };
        let motor = CallsBack {
            poster: poster.clone(),
            moving: moving.clone(),
        };
        let (mut poll_loop, axis) = model2_axis("PLM2MOVECB", Arc::new(Mutex::new(motor)));
        *poster.lock().unwrap() = axis.status_poster();
        let trace = Arc::new(MotorTrace::with_user(axis.address().clone()));
        let (record, state, mut io_intr) = bind_accepting(&poll_loop, &trace);
        state.lock().unwrap().move_queued();
        let batch = CommandBatch {
            trace,
            record: Some(record),
            commands: vec![MotorCommand::MoveAbsolute {
                position: 2.0,
                min_velocity: 0.1,
                velocity: 1.0,
                acceleration: 1.0,
            }],
        };
        let controller_poll = poll_loop.controller_poll.clone();
        poll_loop.axes[0].execute(0, &batch, &controller_poll);
        poll_loop.axes[0].offer_pulses();
        assert!(io_intr.try_recv().is_ok());
        assert!(io_intr.try_recv().is_err(), "processed more than once");
        let mut ds = state.lock().unwrap();
        ds.begin_callback_pass();
        assert_eq!(ds.latest_status.as_ref().unwrap().status, moving);
    }

    #[test]
    fn a_model2_driver_s_update_sets_land_in_its_parameter_library() {
        let updated = vec![
            StatusWrite::Position(3.0),
            StatusWrite::EncoderPosition(4.0),
            StatusWrite::Direction(true),
            StatusWrite::Done(true),
            StatusWrite::HighHardLimit(false),
            StatusWrite::HomeSignal(true),
            StatusWrite::EncoderHome(true),
            StatusWrite::Moving(false),
            StatusWrite::LowHardLimit(true),
        ];
        let (mut poll_loop, _axis, params) = model2_sim("PLM2UPDATE", Vec::new(), updated);
        now(poll_loop.poller_pass(false));
        let want = [
            (0, WriteValue::Float64(3.0)),
            (9, WriteValue::Float64(4.0)),
            (11, WriteValue::Int32(1)),
            (12, WriteValue::Int32(1)),
            (13, WriteValue::Int32(0)),
            (14, WriteValue::Int32(1)),
            (18, WriteValue::Int32(1)),
            (21, WriteValue::Int32(0)),
            (24, WriteValue::Int32(1)),
        ];
        for (reason, value) in want {
            assert_eq!(params.get(reason), Some(value), "parameter {reason}");
        }
    }

    /// drvMotorSim's report walks its axes by card and axis (`drv.pFirst`,
    /// drvMotorSim.c:178-190) whoever opened them: a devMotorSim open alone
    /// keeps the paramLib its update sets.
    #[test]
    fn a_motor_interface_axis_s_parameter_library_reads_by_driver_card_and_signal() {
        let mut controller = crate::builder::ControllerBuilder::new();
        let sim = Sim {
            writes: Vec::new(),
            updated: vec![StatusWrite::Position(3.0)],
        };
        let user = TraceUser::MotorInterface {
            device: "PLMIDev",
            driver: "PLMIDrv",
            card: 1,
            signal: 2,
        };
        let axis = controller.axis(Arc::new(Mutex::new(sim)), user).axis;
        crate::devices::register_axis(axis).unwrap();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        now(poll_loop.poller_pass(false));
        let params = crate::devices::motor_interface_params("PLMIDrv", 1, 2).unwrap();
        assert_eq!(params.get(0), Some(WriteValue::Float64(3.0)));
        assert!(crate::devices::motor_interface_params("PLMIDrv", 1, 1).is_none());
        assert!(crate::devices::motor_interface_params("PLMIDrv", 0, 2).is_none());
        assert!(crate::devices::motor_interface_params("other", 1, 2).is_none());
    }

    #[test]
    fn a_model2_driver_s_create_position_is_in_its_parameter_library() {
        let created = vec![StatusWrite::Position(1.5)];
        let (_poll_loop, _axis, params) = model2_sim("PLM2CREATE", created, Vec::new());
        assert_eq!(params.get(0), Some(WriteValue::Float64(1.5)));
    }

    #[test]
    fn a_model2_move_start_stores_done_off() {
        let updated = vec![StatusWrite::Done(true)];
        let (mut poll_loop, axis, params) = model2_sim("PLM2MOVE", Vec::new(), updated);
        assert_eq!(params.get(12), Some(WriteValue::Int32(1)));
        axis.send(MotorCommand::MoveAbsolute {
            position: 2.0,
            min_velocity: 0.1,
            velocity: 1.0,
            acceleration: 1.0,
        });
        poll_loop.run_queued_commands();
        assert_eq!(params.get(12), Some(WriteValue::Int32(0)));
    }

    /// A motor recording the speeds of each absolute move and stop.
    struct Speeds(Vec<(f64, f64, f64)>);

    impl AsynMotor for Speeds {
        fn move_absolute(
            &mut self,
            _: &AsynUser,
            _: f64,
            min_velocity: f64,
            velocity: f64,
            acceleration: f64,
        ) -> AsynResult<()> {
            self.0.push((min_velocity, velocity, acceleration));
            Ok(())
        }
        fn move_relative(
            &mut self,
            _: &AsynUser,
            _: f64,
            _min_velocity: f64,
            _velocity: f64,
            _acceleration: f64,
        ) -> AsynResult<()> {
            Ok(())
        }
        fn poll_failure(&self) -> PollFailure {
            PollFailure::NONE
        }
        fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
            Ok(())
        }
        fn stop(&mut self, _: &AsynUser, acceleration: f64) -> AsynResult<()> {
            self.0.push((0.0, 0.0, acceleration));
            Ok(())
        }
        fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
            Ok(())
        }
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            *status = MotorStatus::default();
            Ok(())
        }
    }

    /// A model-2 axis's speeds are its port's (drvMotorAsyn.c:722-733): the
    /// record's speed writes reach the port, and a move and a stop run on
    /// what the port stored, not on the command's own speeds.
    #[test]
    fn a_model2_move_runs_on_the_speeds_its_port_stored() {
        let motor = Arc::new(Mutex::new(Speeds(Vec::new())));
        let (mut poll_loop, axis) = model2_axis("PLM2SPEEDS", motor.clone());
        axis.send(MotorCommand::SetVelBase { velocity: 0.5 });
        axis.send(MotorCommand::SetVelocity { velocity: 3.0 });
        axis.send(MotorCommand::SetAccel { acceleration: 0.25 });
        axis.send(MotorCommand::MoveAbsolute {
            position: 2.0,
            min_velocity: 9.0,
            velocity: 9.0,
            acceleration: 9.0,
        });
        axis.send(MotorCommand::Stop { acceleration: 9.0 });
        poll_loop.run_queued_commands();
        assert_eq!(
            motor.lock().unwrap().0,
            [(0.5, 3.0, 0.25), (0.0, 0.0, 0.25)]
        );
    }

    /// Answers each poll from the script in turn; `None` is `set_status`
    /// returning early (RETRY).
    struct Script(std::collections::VecDeque<Option<MotorStatus>>);

    type Held = Option<super::Held>;

    /// At rest at 0 when its card's `motor_init` read it.
    impl Model1Motor for Script {
        fn take_init_status(&mut self) -> MotorStatus {
            MotorStatus::default()
        }
    }

    impl AsynMotor for Script {
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
            PollFailure::NONE
        }
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            *status = self.0.pop_front().flatten().ok_or(AsynError::Status {
                status: AsynStatus::Error,
                message: "retry".into(),
            })?;
            Ok(())
        }
    }

    fn at(position: f64) -> MotorStatus {
        MotorStatus {
            position,
            done: false,
            moving: true,
            direction: true,
            ..MotorStatus::default()
        }
    }

    const SCAN: Duration = Duration::from_secs(1);

    /// pmac's shape: each write sets COMMS_ERROR, here to the write's count.
    #[derive(Default)]
    struct CountingHook(std::sync::atomic::AtomicI32);

    impl crate::controller_port::WriteHook for CountingHook {
        fn after_write(&self, _: &FinishedWrite<'_>) -> crate::controller_port::WriteOutcome {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let mut outcome = crate::controller_port::WriteOutcome::default();
            outcome
                .delta
                .set_int32(crate::controller_port::index::STATUS_COMMS_ERROR, n);
            outcome
        }
    }

    struct Hooked;

    impl crate::controller_port::ControllerParams for Hooked {
        fn write_hook(&self) -> Option<Arc<dyn crate::controller_port::WriteHook>> {
            Some(Arc::new(CountingHook::default()))
        }
    }

    type Gate = Arc<epics_base_rs::runtime::interrupt_accept::InterruptGate>;

    /// Moving while `moving` is; each poll sets HIGH_LIMIT to `limit`.
    struct PollSets {
        moving: Arc<std::sync::atomic::AtomicBool>,
        limit: Arc<Mutex<f64>>,
    }

    impl AsynMotor for PollSets {
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
            let moving = self.moving.load(std::sync::atomic::Ordering::SeqCst);
            *status = MotorStatus {
                moving,
                done: !moving,
                ..MotorStatus::default()
            };
            Ok(())
        }
        fn take_port_sets(&mut self) -> PortSets {
            let limit = *self.limit.lock().unwrap();
            let mut sets = PortSets::default();
            sets.axis(
                crate::controller_port::index::HIGH_LIMIT,
                asyn_rs::param::ParamValue::Float64(limit),
            );
            sets
        }
    }

    /// Polls `polls` in turn; an `Err` returns early having left that status
    /// and set HIGH_LIMIT to `limit`.
    /// A C constructor that bailed on a dead line: PROBLEM|COMM_ERR set,
    /// and every poll bails the same way (smarActMCSMotorDriver.cpp:386-389).
    struct CtorBailed(Vec<StatusWrite>);

    impl AsynMotor for CtorBailed {
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
        fn poll(&mut self, _: &AsynUser, _: &mut MotorStatus) -> AsynResult<()> {
            Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "dead line".into(),
            })
        }
        fn take_status_writes(&mut self) -> Vec<StatusWrite> {
            std::mem::take(&mut self.0)
        }
        fn constructor_calls_param_callbacks(&self) -> bool {
            true
        }
    }

    /// What a C constructor set stays in the status a failed poll posts:
    /// phytronAxis's HasEncoder (phytronAxisMotor.cpp:1352) under the
    /// poll's PROBLEM, MSTA 0x300.
    #[test]
    fn a_failed_poll_keeps_what_the_constructor_set() {
        let mut controller = crate::builder::ControllerBuilder::new();
        let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(CtorBailed(vec![
            StatusWrite::HasEncoder(true),
            StatusWrite::GainSupport(true),
        ])));
        let user = TraceUser::Asyn {
            port: String::new(),
            addr: 0,
        };
        let axis = controller.axis(motor, user).axis;
        let record = axis.attach();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        now(poll_loop.poller_pass(false));
        let status = poll_loop.axes[0].last_status();
        assert!(status.has_encoder && status.gain_support && status.problem);
        let init = record.device_state().lock().unwrap().init_status();
        assert!(init.has_encoder && init.gain_support, "{init:?}");
    }

    /// C readGenericPointer reads the parameters the constructor set
    /// (asynMotorController.cpp:505-525), so a record bound to an axis
    /// created after the poller started reads PROBLEM|COMM_ERR at init, and
    /// a poll that sets them again changes nothing and posts nothing
    /// (asynMotorAxis.cpp:316-322).
    #[test]
    fn a_record_bound_to_a_joined_axis_reads_what_its_constructor_set() {
        let controller = crate::builder::ControllerBuilder::new();
        let axes = controller.axes();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(CtorBailed(vec![
            StatusWrite::Problem(true),
            StatusWrite::CommsError(true),
        ])));
        let user = TraceUser::Asyn {
            port: String::new(),
            addr: 0,
        };
        let add = std::thread::spawn(move || axes.add(motor, user));
        let joining = poll_loop.joining.as_mut().unwrap().blocking_recv().unwrap();
        poll_loop.join(joining);
        let axis = add.join().unwrap().unwrap().axis;
        let record = axis.attach();
        let state = record.device_state().clone();
        let init = state.lock().unwrap().init_status();
        assert!(init.problem && init.comms_error, "{init:?}");
        let seq = poll_loop.axes[0].places[0].records().stamp();
        now(poll_loop.poller_pass(false));
        assert_eq!(
            poll_loop.axes[0].places[0].records().stamp(),
            seq,
            "the poll posted"
        );
        assert_eq!(state.lock().unwrap().update_values(), None);
    }

    /// Counts its polls, each at `.1`.
    struct PollsAt(Arc<Mutex<u32>>, f64);

    impl AsynMotor for PollsAt {
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
            PollFailure::NONE
        }
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            *self.0.lock().unwrap() += 1;
            *status = at(self.1);
            Ok(())
        }
    }

    /// C's asynMotorAxis constructor sets `pC->pAxes_[axisNo]`
    /// (asynMotorAxis.cpp:46) and the poller polls only that array
    /// (asynMotorController.cpp:651-652): an axis created at an address
    /// already held replaces the one there, for the records and the poller.
    #[test]
    fn a_second_axis_at_an_address_replaces_the_first() {
        use crate::controller_port::index;
        let port = "replacedAxis";
        let handle =
            crate::devices::register_controller_port(&asyn_rs::registry::open_owner(), port, 1, ())
                .unwrap();
        let user = || TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        };
        let first_polls = Arc::new(Mutex::new(0));
        let mut controller = crate::builder::ControllerBuilder::new();
        let first: Arc<Mutex<dyn AsynMotor>> =
            Arc::new(Mutex::new(PollsAt(first_polls.clone(), 1.0)));
        crate::devices::register_axis(controller.axis(first.clone(), user()).axis).unwrap();
        let axes = controller.axes();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        now(poll_loop.poller_pass(false));

        let second_polls = Arc::new(Mutex::new(0));
        let second: Arc<Mutex<dyn AsynMotor>> =
            Arc::new(Mutex::new(PollsAt(second_polls.clone(), 2.0)));
        let add = {
            let (axes, second) = (axes.clone(), second.clone());
            std::thread::spawn(move || axes.add(second, user()))
        };
        let joining = poll_loop.joining.as_mut().unwrap().blocking_recv().unwrap();
        poll_loop.join(joining);
        crate::devices::register_axis(add.join().unwrap().unwrap().axis).unwrap();
        let polled = *first_polls.lock().unwrap();
        now(poll_loop.poller_pass(false));
        assert_eq!(
            *first_polls.lock().unwrap(),
            polled,
            "the replaced axis polled"
        );
        assert_eq!(*second_polls.lock().unwrap(), 1);
        assert_eq!(
            handle.read_float64_blocking(index::POSITION, 0).unwrap(),
            2.0
        );
        let bound = crate::devices::axis_at(port, 0).unwrap();
        assert!(std::ptr::addr_eq(
            Arc::as_ptr(bound.motor()),
            Arc::as_ptr(&second)
        ));

        // The replaced one created again replaces the second in turn.
        let add = std::thread::spawn(move || axes.add(first, user()));
        let joining = poll_loop.joining.as_mut().unwrap().blocking_recv().unwrap();
        poll_loop.join(joining);
        crate::devices::register_axis(add.join().unwrap().unwrap().axis).unwrap();
        now(poll_loop.poller_pass(false));
        assert_eq!(*first_polls.lock().unwrap(), polled + 1);
        assert_eq!(*second_polls.lock().unwrap(), 1, "the replaced axis polled");
    }

    /// Logs its moves, polling at the last one, or `.1` before any.
    struct MovesAt(Arc<Mutex<Vec<f64>>>, f64);

    impl AsynMotor for MovesAt {
        fn move_absolute(
            &mut self,
            _: &AsynUser,
            position: f64,
            _: f64,
            _: f64,
            _: f64,
        ) -> AsynResult<()> {
            self.0.lock().unwrap().push(position);
            Ok(())
        }
        fn move_relative(
            &mut self,
            _: &AsynUser,
            _distance: f64,
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
            PollFailure::NONE
        }
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            *status = at(self.0.lock().unwrap().last().copied().unwrap_or(self.1));
            Ok(())
        }
    }

    /// A motor record bound through devMotorAsyn at `@asyn(port,addr)`,
    /// past init and the gate, and its io_intr pulses.
    fn bind_motor_record(
        port: &str,
        addr: i32,
    ) -> (
        crate::record::MotorRecord,
        Box<dyn epics_base_rs::server::device_support::DeviceSupport>,
        mpsc::Receiver<()>,
    ) {
        use epics_base_rs::server::ioc_app::DeviceSupportContext;
        let out = format!("@asyn({port},{addr})");
        let context = DeviceSupportContext {
            record_type: "motor",
            dtyp: "asynMotor",
            inp: "",
            out: &out,
        };
        let mut support = crate::devices::device_support_factory()(&context).unwrap();
        let mut record = crate::record::MotorRecord::new();
        support.init(&mut record).unwrap();
        support.bind_dev_sup_gate(&asyn_rs::registry::open_owner());
        let pulses = support.io_intr_receiver().unwrap();
        (record, support, pulses)
    }

    fn move_to(position: f64) -> MotorCommand {
        MotorCommand::MoveAbsolute {
            position,
            min_velocity: 0.0,
            velocity: 1.0,
            acceleration: 1.0,
        }
    }

    /// devMotorAsyn's requests wait on the port's queue and reach the axis
    /// at the address when the port thread takes them (asynMotorController.
    /// cpp:532-537): a move a record queued before the axis there was
    /// replaced drives the new one.
    #[test]
    fn a_move_queued_before_a_replacement_drives_the_new_axis() {
        let port = "queuedAcrossReplace";
        crate::devices::register_controller_port(&asyn_rs::registry::open_owner(), port, 1, ())
            .unwrap();
        let user = || TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        };
        let moves: [Arc<Mutex<Vec<f64>>>; 2] = Default::default();
        let motor = |i: usize| -> Arc<Mutex<dyn AsynMotor>> {
            Arc::new(Mutex::new(MovesAt(moves[i].clone(), 1.0)))
        };
        let mut controller = crate::builder::ControllerBuilder::new();
        crate::devices::register_axis(controller.axis(motor(0), user()).axis).unwrap();
        let axes = controller.axes();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        now(poll_loop.poller_pass(false));
        let (mut record, _support, _pulses) = bind_motor_record(port, 0);
        poll_loop.run_queued_commands();

        record.send_special(vec![move_to(5.0)]);
        let add = {
            let (axes, motor) = (axes.clone(), motor(1));
            std::thread::spawn(move || axes.add(motor, user()))
        };
        let joining = poll_loop.joining.as_mut().unwrap().blocking_recv().unwrap();
        poll_loop.join(joining);
        crate::devices::register_axis(add.join().unwrap().unwrap().axis).unwrap();
        poll_loop.run_queued_commands();
        assert!(moves[0].lock().unwrap().is_empty());
        assert_eq!(*moves[1].lock().unwrap(), [5.0]);
    }

    /// A motor first added under a model-2 user, then at a controller port
    /// address, is the axis at that address: the records bound there before
    /// drive it, the moves they queued before it joined included, and take
    /// its posts.
    #[test]
    fn a_motor_added_later_at_a_controller_address_serves_the_records_there() {
        let port = "laterAddress";
        crate::devices::register_controller_port(&asyn_rs::registry::open_owner(), port, 1, ())
            .unwrap();
        let user = || TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        };
        let moves: [Arc<Mutex<Vec<f64>>>; 2] = Default::default();
        let first: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(MovesAt(moves[0].clone(), 1.0)));
        let opened: Arc<Mutex<dyn AsynMotor>> =
            Arc::new(Mutex::new(MovesAt(moves[1].clone(), 2.0)));
        let mut controller = crate::builder::ControllerBuilder::new();
        let model2 = TraceUser::Model2 {
            port: "laterAddressModel2".into(),
            addr: 0,
        };
        let poster = controller
            .axis(opened.clone(), model2)
            .axis
            .status_poster()
            .unwrap();
        crate::devices::register_axis(controller.axis(first, user()).axis).unwrap();
        let axes = controller.axes();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        now(poll_loop.poller_pass(false));
        let (mut record, _support, mut pulses) = bind_motor_record(port, 0);
        poll_loop.run_queued_commands();

        record.send_special(vec![move_to(7.0)]);
        let add = std::thread::spawn(move || axes.add(opened, user()));
        let joining = poll_loop.joining.as_mut().unwrap().blocking_recv().unwrap();
        poll_loop.join(joining);
        crate::devices::register_axis(add.join().unwrap().unwrap().axis).unwrap();
        record.send_special(vec![move_to(8.0)]);
        poll_loop.run_queued_commands();
        assert!(moves[0].lock().unwrap().is_empty());
        assert_eq!(*moves[1].lock().unwrap(), [7.0, 8.0]);
        while pulses.try_recv().is_ok() {}
        poster.post(at(8.0));
        poll_loop.run_queued_commands();
        assert!(pulses.try_recv().is_ok(), "the axis posted no record there");
    }

    /// A motor added later at a controller port address is C's asynMotorAxis
    /// constructed there, `statusChanged_` raised (asynMotorAxis.cpp:29): its
    /// next poll posts the records bound there whatever it reads, and its
    /// first user's records no more than before.
    #[test]
    fn a_motor_added_later_at_a_controller_address_posts_there_from_its_next_poll() {
        use epics_base_rs::server::device_support::DeviceSupport;
        let port = "laterAddressPosts";
        crate::devices::register_controller_port(&asyn_rs::registry::open_owner(), port, 1, ())
            .unwrap();
        let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Unchanged));
        let mut controller = crate::builder::ControllerBuilder::new();
        let first = controller.axis(motor.clone(), interface_user()).axis;
        let axes = controller.axes();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        now(poll_loop.poller_pass(false));
        let mut own = first.attach();
        own.bind_dev_sup_gate(&asyn_rs::registry::open_owner());
        let mut own_pulses = own.io_intr_receiver().unwrap();
        let (support, mut pulses) = attach_at(port, 0);
        let state = support.device_state().clone();

        replace(&mut poll_loop, &axes, motor, port, 0);
        assert!(pulses.try_recv().is_err(), "the join posted");
        now(poll_loop.poller_pass(false));
        assert!(
            pulses.try_recv().is_ok(),
            "the next poll posted nothing there"
        );
        assert!(
            own_pulses.try_recv().is_err(),
            "the first user's records posted"
        );
        let mut ds = state.lock().unwrap();
        ds.begin_callback_pass();
        assert_eq!(ds.update_values(), Some(MotorStatus::default()));
        drop(ds);
        now(poll_loop.poller_pass(false));
        assert!(
            pulses.try_recv().is_err(),
            "an unchanged second poll posted"
        );
    }

    /// C's port is one asynMotorController, whose poller alone polls its
    /// `pAxes_` (asynMotorController.cpp:651-652): another controller's add
    /// at an address an axis holds is refused, so no two pollers drain one
    /// address's queue, and leaves that controller's own port unnamed.
    #[test]
    fn an_add_at_an_address_another_poller_holds_is_refused() {
        let port = "otherPollerAddress";
        crate::devices::register_controller_port(&asyn_rs::registry::open_owner(), port, 1, ())
            .unwrap();
        let user = || TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        };
        let moves: [Arc<Mutex<Vec<f64>>>; 2] = Default::default();
        let motor = |i: usize| -> Arc<Mutex<dyn AsynMotor>> {
            Arc::new(Mutex::new(MovesAt(moves[i].clone(), 1.0)))
        };
        let mut first = crate::builder::ControllerBuilder::new();
        crate::devices::register_axis(first.axis(motor(0), user()).axis).unwrap();
        let mut first_loop = first.build(SCAN, SCAN, 0);
        let second = crate::builder::ControllerBuilder::new();
        assert!(second.axes().add(motor(1), user()).is_err());
        let own = "otherPollerAddressOwn";
        let own_port =
            crate::devices::register_controller_port(&asyn_rs::registry::open_owner(), own, 1, ())
                .unwrap();
        let at_own = TraceUser::Asyn {
            port: own.into(),
            addr: 0,
        };
        second.axes().add(motor(1), at_own).unwrap();
        let mut second_loop = second.build(SCAN, SCAN, 0);
        second_loop.run_startup_pass();
        assert_eq!(
            own_port
                .read_float64_blocking(crate::controller_port::index::POSITION, 0)
                .unwrap(),
            1.0
        );

        let (mut record, _support, _pulses) = bind_motor_record(port, 0);
        record.send_special(vec![move_to(5.0)]);
        second_loop.run_queued_commands();
        first_loop.run_queued_commands();
        assert!(moves[1].lock().unwrap().is_empty());
        assert_eq!(*moves[0].lock().unwrap(), [5.0]);
    }

    /// Reads what its constructor left: the default status.
    struct Unchanged;

    impl AsynMotor for Unchanged {
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
            PollFailure::NONE
        }
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            *status = MotorStatus::default();
            Ok(())
        }
    }

    /// A record's mailbox bound at `@asyn(port,addr)` past the gate, and its
    /// io_intr pulses.
    fn attach_at(
        port: &str,
        addr: i32,
    ) -> (
        crate::device_support::MotorDeviceSupport,
        mpsc::Receiver<()>,
    ) {
        use epics_base_rs::server::device_support::DeviceSupport;
        let user = TraceUser::Asyn {
            port: port.into(),
            addr,
        };
        let slot = crate::devices::claim(&user).unwrap().unwrap();
        let mut support = slot.attach(user, Duration::ZERO);
        support.bind_dev_sup_gate(&asyn_rs::registry::open_owner());
        let pulses = support.io_intr_receiver().unwrap();
        (support, pulses)
    }

    /// Axis `motor` replacing the one at `port` address `addr` on the
    /// running `poll_loop`.
    fn replace(
        poll_loop: &mut MotorPollLoop,
        axes: &crate::builder::ControllerAxes,
        motor: Arc<Mutex<dyn AsynMotor>>,
        port: &str,
        addr: i32,
    ) {
        let user = TraceUser::Asyn {
            port: port.into(),
            addr,
        };
        let add = {
            let axes = axes.clone();
            std::thread::spawn(move || axes.add(motor, user))
        };
        let joining = poll_loop.joining.as_mut().unwrap().blocking_recv().unwrap();
        poll_loop.join(joining);
        crate::devices::register_axis(add.join().unwrap().unwrap().axis).unwrap();
    }

    /// The stamps a record's needUpdate compares are its address's: an axis
    /// that replaces another there stamps past every status the records
    /// read, so its first post is new to them.
    #[test]
    fn a_replacement_never_reissues_a_stamp_its_records_read() {
        let port = "stampsAcrossReplace";
        crate::devices::register_controller_port(&asyn_rs::registry::open_owner(), port, 1, ())
            .unwrap();
        let user = TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        };
        let motor = |at: f64| -> Arc<Mutex<dyn AsynMotor>> {
            Arc::new(Mutex::new(MovesAt(Arc::default(), at)))
        };
        let mut controller = crate::builder::ControllerBuilder::new();
        crate::devices::register_axis(controller.axis(motor(1.0), user).axis).unwrap();
        let axes = controller.axes();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        now(poll_loop.poller_pass(false));
        let (support, mut pulses) = attach_at(port, 0);
        let state = support.device_state().clone();
        assert_eq!(
            state.lock().unwrap().update_values().map(|s| s.position),
            Some(1.0)
        );

        replace(&mut poll_loop, &axes, motor(2.0), port, 0);
        now(poll_loop.poller_pass(false));
        assert!(pulses.try_recv().is_ok(), "the first poll posted nothing");
        let mut ds = state.lock().unwrap();
        ds.begin_callback_pass();
        assert_eq!(ds.update_values().map(|s| s.position), Some(2.0));
    }

    /// C's asynMotorAxis constructor calls no record back
    /// (asynMotorAxis.cpp:31-68) and leaves `statusChanged_` raised: an axis
    /// placed at an address with records bound posts nothing until its first
    /// poll, which posts whatever it reads.
    #[test]
    fn a_new_axis_at_a_live_address_posts_first_from_its_first_poll() {
        let port = "firstPollPosts";
        crate::devices::register_controller_port(&asyn_rs::registry::open_owner(), port, 1, ())
            .unwrap();
        let user = TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        };
        let first: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(MovesAt(Arc::default(), 1.0)));
        let mut controller = crate::builder::ControllerBuilder::new();
        crate::devices::register_axis(controller.axis(first, user).axis).unwrap();
        let axes = controller.axes();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        now(poll_loop.poller_pass(false));
        let (support, mut pulses) = attach_at(port, 0);
        let state = support.device_state().clone();
        assert!(state.lock().unwrap().update_values().is_some());

        replace(
            &mut poll_loop,
            &axes,
            Arc::new(Mutex::new(Unchanged)),
            port,
            0,
        );
        assert!(pulses.try_recv().is_err(), "the constructor posted");
        assert!(
            state.lock().unwrap().update_values().is_none(),
            "the constructor set needUpdate"
        );
        now(poll_loop.poller_pass(false));
        assert!(pulses.try_recv().is_ok(), "the first poll posted nothing");
        let mut ds = state.lock().unwrap();
        ds.begin_callback_pass();
        assert_eq!(ds.update_values(), Some(MotorStatus::default()));
        drop(ds);
        now(poll_loop.poller_pass(false));
        assert!(
            pulses.try_recv().is_err(),
            "an unchanged second poll posted"
        );
    }

    /// devMotorAsyn reaches its axis by `getAxis(pasynUser)` on each request
    /// (asynMotorController.cpp:532-537), and its statusCallback is on the
    /// port's interrupt list at the address (devMotorAsyn.c:378-384): a
    /// record drives, and takes the posts of, the axis at its address when
    /// it sends, whether bound before that axis replaced another or where no
    /// axis was yet.
    #[test]
    fn a_record_reaches_the_axis_at_its_address_when_it_sends() {
        let port = "axisByAddress";
        crate::devices::register_controller_port(&asyn_rs::registry::open_owner(), port, 2, ())
            .unwrap();
        let user = |addr| TraceUser::Asyn {
            port: port.into(),
            addr,
        };
        let moves: [Arc<Mutex<Vec<f64>>>; 3] = Default::default();
        let motor = |i: usize| -> Arc<Mutex<dyn AsynMotor>> {
            Arc::new(Mutex::new(MovesAt(moves[i].clone(), i as f64 + 1.0)))
        };
        let mut controller = crate::builder::ControllerBuilder::new();
        crate::devices::register_axis(controller.axis(motor(0), user(0)).axis).unwrap();
        let axes = controller.axes();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        now(poll_loop.poller_pass(false));

        // One record at the first axis, one where no axis is yet.
        let mut bound = [bind_motor_record(port, 0), bind_motor_record(port, 1)];
        for (addr, i) in [(0, 1), (1, 2)] {
            let add = {
                let (axes, motor) = (axes.clone(), motor(i));
                std::thread::spawn(move || axes.add(motor, user(addr)))
            };
            let joining = poll_loop.joining.as_mut().unwrap().blocking_recv().unwrap();
            poll_loop.join(joining);
            crate::devices::register_axis(add.join().unwrap().unwrap().axis).unwrap();
        }
        now(poll_loop.poller_pass(false));
        for (addr, (_, _, pulses)) in bound.iter_mut().enumerate() {
            assert!(pulses.try_recv().is_ok(), "address {addr} posted no record");
        }
        for (addr, (record, _, _)) in bound.iter_mut().enumerate() {
            record.send_special(vec![move_to(10.0 + addr as f64)]);
        }
        poll_loop.run_queued_commands();
        assert_eq!(*moves[1].lock().unwrap(), [10.0]);
        assert_eq!(*moves[2].lock().unwrap(), [11.0]);
    }

    /// C's asynMotorAxis constructor refuses an address past the
    /// controller's axes before it sets `pAxes_` (asynMotorAxis.cpp:40-44):
    /// an add its port refuses never joins the poller.
    #[test]
    fn an_axis_its_port_refuses_never_joins_the_poller() {
        let port = "refusedAxis";
        crate::devices::register_controller_port(&asyn_rs::registry::open_owner(), port, 1, ())
            .unwrap();
        let polls = Arc::new(Mutex::new(0));
        let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(PollsAt(polls.clone(), 1.0)));
        let controller = crate::builder::ControllerBuilder::new();
        let axes = controller.axes();
        let past = TraceUser::Asyn {
            port: port.into(),
            addr: 1,
        };
        assert!(axes.add(motor.clone(), past).is_err());
        let model2 = TraceUser::Model2 {
            port: port.into(),
            addr: 0,
        };
        assert!(axes.add(motor, model2).is_err());
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        poll_loop.run_startup_pass();
        assert_eq!(*polls.lock().unwrap(), 0);
    }

    /// A stop whose driver sets `writes` on the axis.
    struct SetsOnStop {
        writes: Vec<StatusWrite>,
        set: Vec<StatusWrite>,
    }

    impl AsynMotor for SetsOnStop {
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
            self.set = self.writes.clone();
            Ok(())
        }
        fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
            Ok(())
        }
        fn poll_failure(&self) -> PollFailure {
            PollFailure::NONE
        }
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            *status = MotorStatus::default();
            Ok(())
        }
        fn take_status_writes(&mut self) -> Vec<StatusWrite> {
            std::mem::take(&mut self.set)
        }
    }

    /// Each set that changes C's status_ raises statusChanged_, so a write
    /// setting a bit and clearing it again posts the status it left
    /// (asynMotorAxis.cpp:250-264, :316-322).
    #[test]
    fn a_write_changing_the_status_and_back_posts() {
        let stop = |writes: Vec<StatusWrite>| {
            let mut poll_loop = poller_on(Arc::new(Mutex::new(SetsOnStop {
                writes,
                set: Vec::new(),
            })));
            let seq = poll_loop.axes[0].places[0].records().stamp();
            let trace = Arc::new(MotorTrace::with_user(TraceUser::Asyn {
                port: String::new(),
                addr: 0,
            }));
            now(poll_loop.execute(
                0,
                0,
                ToAxis::from(CommandBatch {
                    trace,
                    record: None,
                    commands: vec![MotorCommand::Stop { acceleration: 1.0 }],
                }),
            ));
            poll_loop.axes[0].places[0].records().stamp() - seq
        };
        assert_eq!(stop(vec![StatusWrite::Done(false)]), 0);
        assert_eq!(
            stop(vec![StatusWrite::Done(true), StatusWrite::Done(false)]),
            1
        );
    }

    /// C's first pass polls every axis in the list when it takes the lock
    /// (asynMotorController.cpp:648-658), so an axis created before the
    /// poller thread got there is polled by the startup pass.
    #[test]
    fn an_axis_added_before_the_startup_pass_is_polled_by_it() {
        let controller = crate::builder::ControllerBuilder::new();
        let axes = controller.axes();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        let motor = EarlyReturn {
            polls: [Ok(at(3.0))].into(),
            limit: 0.0,
        };
        let user = TraceUser::Asyn {
            port: String::new(),
            addr: 0,
        };
        let add = std::thread::spawn(move || axes.add(Arc::new(Mutex::new(motor)), user));
        while poll_loop.joining.as_ref().unwrap().is_empty() {
            std::thread::sleep(Duration::from_millis(1));
        }
        poll_loop.run_startup_pass();
        let status = poll_loop.axes.first().map(|a| a.last_status());
        if let Ok(joining) = poll_loop.joining.as_mut().unwrap().try_recv() {
            poll_loop.join(joining);
        }
        add.join().unwrap().unwrap();
        assert_eq!(status.map(|s| s.position), Some(3.0));
    }

    struct EarlyReturn {
        polls: std::collections::VecDeque<Result<MotorStatus, MotorStatus>>,
        limit: f64,
    }

    impl AsynMotor for EarlyReturn {
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
            PollFailure::NONE
        }
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            self.limit += 1.0;
            let polled = self.polls.pop_front().unwrap();
            *status = polled.clone().unwrap_or_else(|left| left);
            polled.map(|_| ()).map_err(|_| AsynError::Status {
                status: AsynStatus::Error,
                message: "failed".into(),
            })
        }
        fn take_port_sets(&mut self) -> PortSets {
            let mut sets = PortSets::default();
            sets.axis(
                crate::controller_port::index::HIGH_LIMIT,
                asyn_rs::param::ParamValue::Float64(self.limit),
            );
            sets
        }
    }

    /// C's poller makes no callParamCallbacks of its own (asynMotorController
    /// .cpp:658): a poll returning before its own leaves what it set, status
    /// and parameters alike, to the next one.
    #[test]
    fn a_poll_returning_early_leaves_its_sets_to_the_next_callback() {
        use crate::controller_port::index;
        let port = "PLEARLY";
        let gate = epics_base_rs::runtime::interrupt_accept::InterruptGate::new();
        let handle = crate::devices::register_controller_port(&gate, port, 1, ()).unwrap();
        let motor = EarlyReturn {
            polls: [Ok(at(0.0)), Ok(at(0.0)), Err(at(2.0)), Ok(at(2.0))].into(),
            limit: 0.0,
        };
        let mut controller = crate::builder::ControllerBuilder::new();
        let user = TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        };
        let axis = controller.axis(Arc::new(Mutex::new(motor)), user).axis;
        crate::devices::register_axis(axis).unwrap();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        poll_loop.run_startup_pass();
        open_past_boot_flush(&handle, &gate);
        let read = |reason| handle.read_float64_blocking(reason, 0).unwrap();
        let (_sub, mut limits) = handle
            .interrupts()
            .register_interrupt_user(
                asyn_rs::interrupt::InterruptFilter {
                    reason: Some(index::HIGH_LIMIT),
                    addr: Some(0),
                    ..Default::default()
                },
                16,
            )
            .unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        // The mailbox keeps the latest post; none within the wait is `None`.
        let mut posted = || {
            rt.block_on(async {
                tokio::time::timeout(std::time::Duration::from_millis(50), limits.recv()).await
            })
            .ok()
            .flatten()
            .map(|iv| match iv.value {
                asyn_rs::param::ParamValue::Float64(v) => v,
                other => panic!("{other:?}"),
            })
        };
        // After the startup poll's.
        now(poll_loop.poller_pass(false));
        assert_eq!(read(index::HIGH_LIMIT), 2.0);
        assert_eq!(read(index::POSITION), 0.0);
        assert!(posted().is_some());
        now(poll_loop.poller_pass(false));
        assert_eq!(
            read(index::POSITION),
            2.0,
            "C getDoubleParam reads the status it left"
        );
        assert_eq!(
            read(index::HIGH_LIMIT),
            3.0,
            "C getDoubleParam reads the failed poll's own"
        );
        assert_eq!(posted(), None, "and no callback posts it");
        now(poll_loop.poller_pass(false));
        assert_eq!(read(index::HIGH_LIMIT), 4.0);
        assert_eq!(read(index::POSITION), 2.0);
        assert_eq!(posted(), Some(4.0));
    }

    /// A poll's or a write's own parameters post with its status when it
    /// changed, and in the bare callParamCallbacks when it did not
    /// (phytronAxisMotor.cpp:2084-2088, asynMotorController.cpp:319).
    #[test]
    fn a_poll_or_write_s_parameters_post_whether_or_not_its_status_changed() {
        use crate::controller_port::index;
        let port = "PLPOLLSETS";
        let gate = epics_base_rs::runtime::interrupt_accept::InterruptGate::new();
        let handle = crate::devices::register_controller_port(&gate, port, 1, ()).unwrap();
        let moving = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let limit = Arc::new(Mutex::new(1.0));
        let motor = PollSets {
            moving: Arc::clone(&moving),
            limit: Arc::clone(&limit),
        };
        let mut controller = crate::builder::ControllerBuilder::new();
        let user = TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        };
        let axis = controller.axis(Arc::new(Mutex::new(motor)), user).axis;
        crate::devices::register_axis(axis).unwrap();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        let read = || handle.read_float64_blocking(index::HIGH_LIMIT, 0).unwrap();

        *limit.lock().unwrap() = 3.0;
        now(poll_loop.poller_pass(false));
        assert_eq!(read(), 3.0, "unchanged status");
        *limit.lock().unwrap() = 4.0;
        moving.store(true, std::sync::atomic::Ordering::SeqCst);
        now(poll_loop.poller_pass(false));
        assert_eq!(read(), 4.0, "changed status");
        *limit.lock().unwrap() = 5.0;
        let trace = Arc::new(MotorTrace::with_user(TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        }));
        let write = |command| {
            ToAxis::from(CommandBatch {
                trace: Arc::clone(&trace),
                record: None,
                commands: vec![command],
            })
        };
        now(poll_loop.execute(0, 0, write(MotorCommand::Poll)));
        assert_eq!(read(), 5.0, "GET_INFO");
        *limit.lock().unwrap() = 6.0;
        now(poll_loop.execute(0, 0, write(MotorCommand::Stop { acceleration: 1.0 })));
        assert_eq!(read(), 6.0, "write leaving the status");
        *limit.lock().unwrap() = 7.0;
        let move_to = MotorCommand::MoveAbsolute {
            position: 1.0,
            min_velocity: 0.0,
            velocity: 1.0,
            acceleration: 1.0,
        };
        now(poll_loop.execute(0, 0, write(move_to)));
        assert_eq!(read(), 7.0, "write changing the status");
    }

    /// Records its closed-loop calls and moves; moving while `moving` is.
    struct Powered {
        calls: Arc<Mutex<Vec<&'static str>>>,
        moving: Arc<std::sync::atomic::AtomicBool>,
    }

    impl AsynMotor for Powered {
        fn move_absolute(
            &mut self,
            _: &AsynUser,
            _: f64,
            _: f64,
            _: f64,
            _: f64,
        ) -> AsynResult<()> {
            self.calls.lock().unwrap().push("move");
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
        fn set_closed_loop(&mut self, _: &AsynUser, enable: bool) -> AsynResult<()> {
            self.calls
                .lock()
                .unwrap()
                .push(if enable { "on" } else { "off" });
            Ok(())
        }
        fn poll_failure(&self) -> PollFailure {
            PollFailure::PROBLEM_AND_COMMS_ERROR
        }
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            let moving = self.moving.load(std::sync::atomic::Ordering::SeqCst);
            *status = MotorStatus {
                moving,
                done: !moving,
                ..MotorStatus::default()
            };
            Ok(())
        }
    }

    struct PowerRig {
        poll_loop: MotorPollLoop,
        calls: Arc<Mutex<Vec<&'static str>>>,
        moving: Arc<std::sync::atomic::AtomicBool>,
        _handle: asyn_rs::port_handle::PortHandle,
        _gate: Gate,
    }

    impl PowerRig {
        /// Axis 0 of controller port `port`, MOTOR_POWER_AUTO_ONOFF `auto`
        /// and MOTOR_POWER_OFF_DELAY `off_delay` seconds.
        fn new(port: &str, auto: i32, off_delay: f64) -> Self {
            Self::started(port, auto, off_delay, false)
        }

        /// Auto power on with no delay, the axis moving at the startup pass.
        fn moving_at_startup(port: &str) -> Self {
            Self::started(port, 1, 0.0, true)
        }

        fn started(port: &str, auto: i32, off_delay: f64, moving_at_startup: bool) -> Self {
            use crate::controller_port::index;
            let gate = epics_base_rs::runtime::interrupt_accept::InterruptGate::new();
            let handle = crate::devices::register_controller_port(&gate, port, 1, ()).unwrap();
            let calls = Arc::default();
            let moving = Arc::new(std::sync::atomic::AtomicBool::new(moving_at_startup));
            let motor = Powered {
                calls: Arc::clone(&calls),
                moving: Arc::clone(&moving),
            };
            let mut controller = crate::builder::ControllerBuilder::new();
            let user = TraceUser::Asyn {
                port: port.into(),
                addr: 0,
            };
            let axis = controller.axis(Arc::new(Mutex::new(motor)), user).axis;
            crate::devices::register_axis(axis).unwrap();
            handle
                .write_int32_blocking(index::POWER_AUTO_ONOFF, 0, auto)
                .unwrap();
            handle
                .write_float64_blocking(index::POWER_OFF_DELAY, 0, off_delay)
                .unwrap();
            let mut poll_loop = controller.build(SCAN, SCAN, 0);
            poll_loop.run_startup_pass();
            Self {
                poll_loop,
                calls,
                moving,
                _handle: handle,
                _gate: gate,
            }
        }

        /// A move written to the port, C `writeFloat64(motorMoveAbs_)`.
        fn move_axis(&mut self, port: &str) {
            let trace = Arc::new(MotorTrace::with_user(TraceUser::Asyn {
                port: port.into(),
                addr: 0,
            }));
            now(self.poll_loop.execute(
                0,
                0,
                ToAxis::from(CommandBatch {
                    trace,
                    record: None,
                    commands: vec![MotorCommand::MoveAbsolute {
                        position: 5.0,
                        min_velocity: 0.0,
                        velocity: 1.0,
                        acceleration: 1.0,
                    }],
                }),
            ));
        }

        /// A poller pass with the axis `moving`.
        fn pass(&mut self, moving: bool) {
            self.moving
                .store(moving, std::sync::atomic::Ordering::SeqCst);
            now(self.poll_loop.poller_pass(false));
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    // C writeFloat64 powers the axis on before a move, and the poller powers
    // it off once the move ends (asynMotorController.cpp:291-295, :655-683).
    #[test]
    fn auto_power_powers_on_before_a_move_and_off_after_it_ends() {
        let mut rig = PowerRig::new("autoPowerOnOff", 1, 0.0);
        rig.move_axis("autoPowerOnOff");
        assert_eq!(rig.calls(), ["on", "move"]);
        rig.pass(true);
        assert_eq!(rig.calls(), ["on", "move"]);
        rig.pass(false);
        assert_eq!(rig.calls(), ["on", "move", "off"]);
        rig.pass(false);
        assert_eq!(rig.calls(), ["on", "move", "off"]);
    }

    #[test]
    fn auto_power_off_waits_for_the_off_delay_after_the_move_ends() {
        let mut rig = PowerRig::new("autoPowerDelay", 1, 0.05);
        rig.pass(true);
        rig.pass(false);
        assert!(rig.calls().is_empty());
        std::thread::sleep(Duration::from_millis(60));
        rig.pass(false);
        assert_eq!(rig.calls(), ["off"]);
    }

    // The startup pass marks a moving axis as C's poller does, whatever the
    // auto power then (asynMotorController.cpp:660-662).
    #[test]
    fn an_axis_moving_at_startup_powers_off_when_its_move_ends() {
        let mut rig = PowerRig::moving_at_startup("autoPowerStartup");
        rig.pass(false);
        assert_eq!(rig.calls(), ["off"]);
    }

    #[test]
    fn without_auto_power_the_axis_power_is_left_alone() {
        let mut rig = PowerRig::new("autoPowerUnset", 0, 0.0);
        rig.move_axis("autoPowerUnset");
        rig.pass(true);
        rig.pass(false);
        assert_eq!(rig.calls(), ["move"]);
    }

    /// A poller on axis 0 of a [`Hooked`] controller port `port`, at rest,
    /// whose later polls answer from `script`, and the port's interruptAccept,
    /// still closed.
    fn hooked_poller(
        port: &str,
        script: Vec<MotorStatus>,
    ) -> (MotorPollLoop, asyn_rs::port_handle::PortHandle, Gate) {
        let gate = epics_base_rs::runtime::interrupt_accept::InterruptGate::new();
        let handle = crate::devices::register_controller_port(&gate, port, 1, Hooked).unwrap();
        let (_poll_tx, poll) = mpsc::channel(1);
        let (_tx, commands) = mpsc::unbounded_channel();
        let script = std::iter::once(MotorStatus::default()).chain(script);
        let motor: Arc<Mutex<dyn AsynMotor>> =
            Arc::new(Mutex::new(Script(script.map(Some).collect())));
        let mut poll_loop = MotorPollLoop::new(
            PollerInbox { poll, commands },
            motor,
            AxisSubscribers::new(),
            SCAN,
            SCAN,
            0,
        );
        poll_loop.run_startup_pass();
        let poll = poll_loop.controller_poll.clone();
        poll_loop.axes[0].mirror_to(port, 0, poll);
        (poll_loop, handle, gate)
    }

    fn write_stop(poll_loop: &mut MotorPollLoop, port: &str) {
        now(poll_loop.execute(
            0,
            0,
            ToAxis::from(CommandBatch {
                trace: Arc::new(MotorTrace::new(port, 0)),
                record: None,
                commands: vec![MotorCommand::Stop { acceleration: 1.0 }],
            }),
        ));
    }

    #[test]
    fn a_velocity_move_is_jog_and_a_jvel_retune_jog_velocity() {
        let jog = MotorCommand::MoveVelocity {
            min_velocity: 0.0,
            velocity: -3.0,
            acceleration: 1.0,
        };
        assert_eq!(build_trans_calls(&jog), [(12, -3.0)]);
        let retune = MotorCommand::JogVelocity {
            min_velocity: 0.0,
            velocity: -3.0,
            acceleration: 1.0,
        };
        assert_eq!(build_trans_calls(&retune), [(21, -3.0)]);
    }

    #[test]
    fn a_hook_set_status_bit_stays_until_the_driver_changes_it() {
        let comms_error = MotorStatus {
            comms_error: true,
            ..at(2.0)
        };
        let script = vec![at(1.0), comms_error, at(3.0)];
        let (mut poll_loop, handle, _) = hooked_poller("PLHOOKBIT", script);
        write_stop(&mut poll_loop, "PLHOOKBIT");
        now(poll_loop.poller_pass(false));
        assert_eq!(comms(&handle), Some(1), "a new status leaves it");
        now(poll_loop.poller_pass(false));
        now(poll_loop.poller_pass(false));
        assert_eq!(comms(&handle), Some(0), "the driver's own clear");
    }

    /// IMS's shape: COMMS_ERROR is set on the controller's params by the
    /// driver (ImsMDrivePlusMotorController.cpp:403-406), never from MSTA.
    struct OwnsComms(Script);

    impl AsynMotor for OwnsComms {
        fn move_absolute(
            &mut self,
            u: &AsynUser,
            p: f64,
            a: f64,
            b: f64,
            c: f64,
        ) -> AsynResult<()> {
            self.0.move_absolute(u, p, a, b, c)
        }
        fn move_relative(
            &mut self,
            u: &AsynUser,
            p: f64,
            a: f64,
            b: f64,
            c: f64,
        ) -> AsynResult<()> {
            self.0.move_relative(u, p, a, b, c)
        }
        fn home(&mut self, u: &AsynUser, a: f64, b: f64, c: f64, f: bool) -> AsynResult<()> {
            self.0.home(u, a, b, c, f)
        }
        fn stop(&mut self, u: &AsynUser, a: f64) -> AsynResult<()> {
            self.0.stop(u, a)
        }
        fn set_position(&mut self, u: &AsynUser, p: f64) -> AsynResult<()> {
            self.0.set_position(u, p)
        }
        fn poll_failure(&self) -> PollFailure {
            self.0.poll_failure()
        }
        fn poll(&mut self, u: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            self.0.poll(u, status)
        }
        fn param_owned_msta_bits(&self) -> u32 {
            crate::flags::MstaFlags::COMM_ERR.bits()
        }
    }

    #[test]
    fn a_driver_owned_status_bit_is_never_mirrored_from_the_status() {
        use crate::controller_port::index::STATUS_COMMS_ERROR;
        use asyn_rs::param::ParamValue;
        use asyn_rs::request::ParamSetValue;
        let port = "PLOWNBIT";
        let gate = epics_base_rs::runtime::interrupt_accept::InterruptGate::new();
        let handle = crate::devices::register_controller_port(&gate, port, 1, ()).unwrap();
        let set = ParamSetValue::new(STATUS_COMMS_ERROR, 0, ParamValue::Int32(1));
        handle.set_params_and_notify_blocking(0, vec![set]).unwrap();
        let (_poll_tx, poll) = mpsc::channel(1);
        let (_tx, commands) = mpsc::unbounded_channel();
        let cleared = MotorStatus {
            comms_error: false,
            ..at(2.0)
        };
        let script = [MotorStatus::default(), at(1.0), cleared].map(Some);
        let motor: Arc<Mutex<dyn AsynMotor>> =
            Arc::new(Mutex::new(OwnsComms(Script(script.into_iter().collect()))));
        let mut poll_loop = MotorPollLoop::new(
            PollerInbox { poll, commands },
            motor,
            AxisSubscribers::new(),
            SCAN,
            SCAN,
            0,
        );
        let poll = poll_loop.controller_poll.clone();
        poll_loop.axes[0].mirror_to(port, 0, poll);
        now(poll_loop.poller_pass(false));
        assert_eq!(comms(&handle), Some(1), "the first post leaves it");
        now(poll_loop.poller_pass(false));
        assert_eq!(comms(&handle), Some(1));
    }

    fn comms(handle: &asyn_rs::port_handle::PortHandle) -> Option<i32> {
        handle
            .read_int32_blocking(crate::controller_port::index::STATUS_COMMS_ERROR, 0)
            .ok()
    }

    /// Axis 0's COMMS_ERROR callbacks on controller port `handle` from the
    /// moment `gate` opened and the port's one-shot flush ran. That flush is
    /// on its own thread, as C's `callbackThread::run` (asynPortDriver.cpp:
    /// 923-937), so it would post whatever was set by the time it got there.
    fn comms_posts(
        handle: &asyn_rs::port_handle::PortHandle,
        gate: &Gate,
    ) -> (
        asyn_rs::interrupt::InterruptSubscription,
        asyn_rs::interrupt::InterruptReceiver,
    ) {
        use crate::controller_port::index::STATUS_COMMS_ERROR;
        // A marker only that flush can post.
        let marker = asyn_rs::request::ParamSetValue::new(
            STATUS_COMMS_ERROR,
            0,
            asyn_rs::param::ParamValue::Int32(0),
        );
        handle.set_params_blocking(0, vec![marker]).unwrap();
        // A callParamCallbacks already queued (the startup pass's flush,
        // register_axis's) must run while the gate is shut, or its post is
        // taken for the gate's flush, which then lands after the test's write.
        settle(handle);
        let (sub, mut rx) = handle
            .interrupts()
            .register_interrupt_user(
                asyn_rs::interrupt::InterruptFilter {
                    reason: Some(STATUS_COMMS_ERROR),
                    addr: Some(0),
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        gate.set(true);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let flushed =
            rt.block_on(async { tokio::time::timeout(Duration::from_secs(10), rx.recv()).await });
        assert!(
            matches!(
                flushed,
                Ok(Some(asyn_rs::interrupt::InterruptValue {
                    value: asyn_rs::param::ParamValue::Int32(0),
                    ..
                }))
            ),
            "the one-shot flush: {flushed:?}"
        );
        (sub, rx)
    }

    /// The port's actor has run everything sent to it so far.
    fn settle(handle: &asyn_rs::port_handle::PortHandle) {
        handle
            .with_driver_blocking::<crate::controller_port::ControllerPort<Hooked>, _>(|_| ())
            .unwrap();
    }

    /// The COMMS_ERROR last posted since the last look, if one was.
    fn posted(
        handle: &asyn_rs::port_handle::PortHandle,
        rx: &mut asyn_rs::interrupt::InterruptReceiver,
    ) -> Option<i32> {
        settle(handle);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let iv = rt.block_on(async { tokio::time::timeout(Duration::ZERO, rx.recv()).await });
        match iv.ok()??.value {
            asyn_rs::param::ParamValue::Int32(v) => Some(v),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_hook_set_waits_for_the_next_poll_pass() {
        let (mut poll_loop, handle, gate) =
            hooked_poller("PLHOOKPASS", vec![MotorStatus::default()]);
        let (_sub, mut rx) = comms_posts(&handle, &gate);
        write_stop(&mut poll_loop, "PLHOOKPASS");
        assert_eq!(posted(&handle, &mut rx), None, "set, not yet called back");
        assert_eq!(comms(&handle), Some(1), "set in the parameter library");
        now(poll_loop.poller_pass(false));
        assert_eq!(posted(&handle, &mut rx), Some(1));
    }

    /// A driver whose move waits on the test and leaves OWN=1 on the port,
    /// and whose poll leaves OWN=2.
    struct Racing {
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        own: i32,
    }

    impl AsynMotor for Racing {
        fn move_absolute(
            &mut self,
            _: &AsynUser,
            _: f64,
            _: f64,
            _: f64,
            _: f64,
        ) -> AsynResult<()> {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            self.own = 1;
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
            self.own = 2;
            *status = MotorStatus::default();
            Ok(())
        }
        fn take_port_sets(&mut self) -> PortSets {
            let mut sets = PortSets::default();
            let own = crate::controller_port::BASE_PARAMS.len();
            sets.axis(own, asyn_rs::param::ParamValue::Int32(self.own));
            sets
        }
    }

    struct Own;

    impl crate::controller_port::ControllerParams for Own {
        fn create_params(&mut self, base: &mut asyn_rs::port::PortDriverBase) -> AsynResult<()> {
            base.create_param("OWN", asyn_rs::param::ParamType::Int32)?;
            Ok(())
        }
    }

    /// A port poll that waits on the lock a poller's write holds runs after
    /// the write's sets land, as C's lock orders them.
    #[test]
    fn a_port_poll_waiting_on_a_poller_write_lands_after_its_sets() {
        let port = "PLWRITERACE";
        let gate = epics_base_rs::runtime::interrupt_accept::InterruptGate::new();
        let handle = crate::devices::register_controller_port(&gate, port, 1, Own).unwrap();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Racing {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            own: 0,
        }));
        let mut controller = crate::builder::ControllerBuilder::new();
        let user = TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        };
        crate::devices::register_axis(controller.axis(motor, user).axis).unwrap();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        let writer = std::thread::spawn(move || {
            let command = MotorCommand::MoveAbsolute {
                position: 1.0,
                min_velocity: 0.0,
                velocity: 1.0,
                acceleration: 1.0,
            };
            now(poll_loop.execute(
                0,
                0,
                ToAxis::from(CommandBatch {
                    trace: Arc::new(MotorTrace::new(port, 0)),
                    record: None,
                    commands: vec![command],
                }),
            ));
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let update = {
            let handle = handle.clone();
            std::thread::spawn(move || {
                handle.write_int32_blocking(crate::controller_port::index::UPDATE_STATUS, 0, 1)
            })
        };
        // The port's thread is now waiting on the write's lock.
        std::thread::sleep(Duration::from_millis(200));
        release.send(()).unwrap();
        writer.join().unwrap();
        update.join().unwrap().unwrap();
        let own = crate::controller_port::BASE_PARAMS.len();
        assert_eq!(handle.read_int32_blocking(own, 0).unwrap(), 2);
    }

    #[test]
    fn a_hook_set_rides_the_next_write_callback() {
        let (mut poll_loop, handle, gate) =
            hooked_poller("PLHOOKWRITE", vec![MotorStatus::default()]);
        let (_sub, mut rx) = comms_posts(&handle, &gate);
        write_stop(&mut poll_loop, "PLHOOKWRITE");
        write_stop(&mut poll_loop, "PLHOOKWRITE");
        assert_eq!(
            posted(&handle, &mut rx),
            Some(1),
            "the second write's posts the first's"
        );
        now(poll_loop.poller_pass(false));
        assert_eq!(posted(&handle, &mut rx), Some(2));
    }

    #[test]
    fn a_hook_set_rides_the_next_port_write_callback() {
        let (mut poll_loop, handle, gate) =
            hooked_poller("PLHOOKPORT", vec![MotorStatus::default()]);
        let (motor_tx, _commands) = mpsc::unbounded_channel();
        let (poll_tx, _polls) = mpsc::channel(1);
        crate::devices::register_axis(crate::device_support::MotorAxis::new(
            TraceUser::Asyn {
                port: "PLHOOKPORT".into(),
                addr: 0,
            },
            Arc::new(Mutex::new(Script(Default::default()))),
            SCAN,
            poll_tx,
            motor_tx,
            AxisSubscribers::new(),
        ))
        .unwrap();
        let (_sub, mut rx) = comms_posts(&handle, &gate);
        write_stop(&mut poll_loop, "PLHOOKPORT");
        assert_eq!(posted(&handle, &mut rx), None);
        handle
            .write_float64_blocking(crate::controller_port::index::REC_RESOLUTION, 0, 0.5)
            .unwrap();
        assert_eq!(
            posted(&handle, &mut rx),
            Some(1),
            "the port's callParamCallbacks posts the axis write's"
        );
    }

    /// A model-1 axis at rest at 0 whose driver answers from `script`, and
    /// the script's handle to count the polls left.
    fn model1_task(script: Vec<Option<MotorStatus>>) -> (MotorPollLoop, Arc<Mutex<Script>>) {
        let script = Arc::new(Mutex::new(Script(script.into())));
        (model1_task_on(script.clone()), script)
    }

    fn model1_task_on(motor: Arc<Mutex<dyn Model1Motor>>) -> MotorPollLoop {
        let (_poll_tx, poll) = mpsc::channel(1);
        let (_tx, commands) = mpsc::unbounded_channel();
        let inbox = PollerInbox { poll, commands };
        let axis = Axis::task(Place::new(inbox, AxisSubscribers::new()), motor);
        let poll_loop = MotorPollLoop::motor_task(vec![axis], SCAN);
        poll_loop.axes[0].places[0]
            .records()
            .set_status(MotorStatus::default());
        poll_loop
    }

    /// Answers each poll from its list; an `Err` fails the poll raising
    /// those bits, having left that status.
    struct Failing {
        polls: std::collections::VecDeque<Result<MotorStatus, (PollFailure, Option<MotorStatus>)>>,
        failure: PollFailure,
    }

    /// At rest at 0 when its card's `motor_init` read it.
    impl Model1Motor for Failing {
        fn take_init_status(&mut self) -> MotorStatus {
            MotorStatus::default()
        }
    }

    impl AsynMotor for Failing {
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
            self.failure
        }
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            match self.polls.pop_front().unwrap() {
                Ok(polled) => {
                    *status = polled;
                    Ok(())
                }
                Err((failure, left)) => {
                    self.failure = failure;
                    if let Some(left) = left {
                        *status = left;
                    }
                    Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "failed".into(),
                    })
                }
            }
        }
    }

    fn failing_poller(
        polls: Vec<Result<MotorStatus, (PollFailure, Option<MotorStatus>)>>,
    ) -> MotorPollLoop {
        let polls = std::iter::once(Ok(MotorStatus::default())).chain(polls);
        poller_on(Arc::new(Mutex::new(Failing {
            polls: polls.collect(),
            failure: PollFailure::NONE,
        })))
    }

    /// An asyn poll returning before callParamCallbacks posts nothing, but
    /// what it set leaves `statusChanged_` set: the next poll posts even an
    /// equal status (asynMotorAxis.cpp:255-264, :316-322).
    #[test]
    fn a_status_left_by_an_early_return_posts_with_the_next_poll() {
        let fresh = MotorStatus {
            home: true,
            ..MotorStatus::default()
        };
        let mut poll_loop = failing_poller(vec![
            Err((PollFailure::NONE, Some(fresh.clone()))),
            Ok(fresh.clone()),
        ]);
        let seq = poll_loop.axes[0].places[0].records().stamp();
        now(poll_loop.poller_pass(false));
        assert_eq!(
            poll_loop.axes[0].places[0].records().stamp(),
            seq,
            "the early return posts nothing"
        );
        now(poll_loop.poller_pass(false));
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq + 1);
        assert_eq!(poll_loop.axes[0].last_status(), fresh);
    }

    /// The write that follows calls back with statusChanged_ still set, so
    /// it posts the status the early return left (asynMotorController.cpp:
    /// 246, asynMotorAxis.cpp:316-322).
    #[test]
    fn a_status_left_by_an_early_return_posts_with_the_next_write() {
        let fresh = MotorStatus {
            home: true,
            ..MotorStatus::default()
        };
        let mut poll_loop = failing_poller(vec![Err((PollFailure::NONE, Some(fresh.clone())))]);
        let seq = poll_loop.axes[0].places[0].records().stamp();
        now(poll_loop.poller_pass(false));
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq);
        let trace = Arc::new(MotorTrace::with_user(TraceUser::Asyn {
            port: String::new(),
            addr: 0,
        }));
        now(poll_loop.execute(
            0,
            0,
            ToAxis::from(CommandBatch {
                trace,
                record: None,
                commands: vec![MotorCommand::Stop { acceleration: 1.0 }],
            }),
        ));
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq + 1);
        assert_eq!(poll_loop.axes[0].last_status(), fresh);
    }

    #[test]
    fn a_left_status_equal_to_the_last_sets_nothing_to_post() {
        let mut poll_loop = failing_poller(vec![
            Err((PollFailure::NONE, Some(MotorStatus::default()))),
            Ok(MotorStatus::default()),
        ]);
        let seq = poll_loop.axes[0].places[0].records().stamp();
        now(poll_loop.poller_pass(false));
        now(poll_loop.poller_pass(false));
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq);
    }

    /// `*moving` is poll's out-argument, not a parameter: a status that
    /// differs from the last only there changes no `status_` and posts
    /// nothing, polled or failed (asynMotorAxis.cpp:255-264, :316-322).
    #[test]
    fn a_status_differing_only_in_the_pace_posts_nothing() {
        let moving = MotorStatus {
            moving: true,
            ..MotorStatus::default()
        };
        let mut poll_loop = failing_poller(vec![
            Ok(moving.clone()),
            Err((PollFailure::PROBLEM, Some(MotorStatus::default()))),
            Err((PollFailure::PROBLEM, Some(moving))),
        ]);
        let stamp = |poll_loop: &MotorPollLoop| poll_loop.axes[0].places[0].records().stamp();
        let seq = stamp(&poll_loop);
        now(poll_loop.poller_pass(false));
        assert_eq!(stamp(&poll_loop), seq);
        assert!(poll_loop.axes[0].last_status().moving);
        now(poll_loop.poller_pass(false));
        assert_eq!(stamp(&poll_loop), seq + 1);
        now(poll_loop.poller_pass(false));
        assert_eq!(stamp(&poll_loop), seq + 1);
        assert!(poll_loop.axes[0].last_status().moving);
    }

    #[test]
    fn a_failure_raising_bits_posts_them_over_what_it_left() {
        let fresh = MotorStatus {
            home: true,
            ..MotorStatus::default()
        };
        let mut poll_loop =
            failing_poller(vec![Err((PollFailure::COMMS_ERROR, Some(fresh.clone())))]);
        let seq = poll_loop.axes[0].places[0].records().stamp();
        now(poll_loop.poller_pass(false));
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq + 1);
        let posted = poll_loop.axes[0].last_status();
        assert!(posted.home && posted.comms_error && !posted.problem);
    }

    /// Sets its status as a C poll its parameters: the high limit on the
    /// first poll; on the next the position and home, then it fails.
    #[derive(Default)]
    struct WritesThenFails {
        polls: u32,
    }

    impl AsynMotor for WritesThenFails {
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
            PollFailure::PROBLEM
        }
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            self.polls += 1;
            if self.polls == 1 {
                status.high_limit = true;
                return Ok(());
            }
            status.position = 5.0;
            status.home = true;
            Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "failed".into(),
            })
        }
    }

    /// A C poll bailing to its error label still calls back what it set
    /// before it (AG_UC.cpp:355-389, asynMotorAxis.cpp:316-322): the post
    /// carries those sets over what the last poll left, and the failure's
    /// bit.
    #[test]
    fn a_failed_poll_posts_what_it_set_before_failing() {
        let mut poll_loop = poller_on(Arc::new(Mutex::new(WritesThenFails::default())));
        let seq = poll_loop.axes[0].places[0].records().stamp();
        now(poll_loop.poller_pass(false));
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq + 1);
        let posted = poll_loop.axes[0].last_status();
        assert_eq!(posted.position, 5.0, "{posted:?}");
        assert!(
            posted.home && posted.high_limit && posted.problem,
            "{posted:?}"
        );
    }

    /// Sets one status field per poll: the high limit on the first, the
    /// position after.
    #[derive(Default)]
    struct FieldWriter {
        polls: u32,
    }

    impl AsynMotor for FieldWriter {
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
            PollFailure::PROBLEM
        }
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            self.polls += 1;
            if self.polls == 1 {
                status.high_limit = true;
            } else {
                status.position = 7.0;
            }
            Ok(())
        }
    }

    /// A controller port's UPDATE_STATUS polls the axis on the port's
    /// thread into the parameters the poller set, C's one param list
    /// (asynMotorController.cpp:217-222).
    #[test]
    fn a_port_update_status_polls_into_what_the_poller_set() {
        let port = "PLPORTBASE";
        let gate = epics_base_rs::runtime::interrupt_accept::InterruptGate::new();
        let handle = crate::devices::register_controller_port(&gate, port, 1, ()).unwrap();
        let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(FieldWriter::default()));
        let mut controller = crate::builder::ControllerBuilder::new();
        let user = TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        };
        crate::devices::register_axis(controller.axis(motor, user).axis).unwrap();
        let mut poll_loop = controller.build(SCAN, SCAN, 0);
        now(poll_loop.poller_pass(false));
        handle
            .write_int32_blocking(crate::controller_port::index::UPDATE_STATUS, 0, 1)
            .unwrap();
        poll_loop.axes[0].take_posts();
        let status = poll_loop.axes[0].last_status();
        assert!(status.high_limit && status.position == 7.0, "{status:?}");
    }

    /// A dead controller's polls each set PROBLEM again and leave the same
    /// status: only the first changes C's status_, so only it posts
    /// (ImsMDrivePlusMotorAxis.cpp:483-501, asynMotorAxis.cpp:258-264).
    #[test]
    fn a_repeated_failure_leaving_the_same_status_posts_once() {
        let left = MotorStatus {
            done: false,
            ..MotorStatus::default()
        };
        let mut poll_loop = failing_poller(vec![
            Err((PollFailure::PROBLEM, Some(left.clone()))),
            Err((PollFailure::PROBLEM, Some(left.clone()))),
            Err((PollFailure::PROBLEM, Some(left))),
        ]);
        let seq = poll_loop.axes[0].places[0].records().stamp();
        now(poll_loop.poller_pass(false));
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq + 1);
        now(poll_loop.poller_pass(false));
        now(poll_loop.poller_pass(false));
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq + 1);
    }

    /// C's startup pass is an ordinary one: the dead controller's first
    /// failure is called back to the not yet bound record and clears
    /// statusChanged_, so the next equal failure posts nothing
    /// (asynMotorController.cpp:630, asynMotorAxis.cpp:316-322).
    #[test]
    fn a_failure_the_startup_pass_called_back_does_not_post_again() {
        let left = || {
            Err((
                PollFailure::PROBLEM,
                Some(MotorStatus {
                    done: false,
                    ..MotorStatus::default()
                }),
            ))
        };
        let mut poll_loop = poller_on(Arc::new(Mutex::new(Failing {
            polls: [left(), left()].into(),
            failure: PollFailure::NONE,
        })));
        let seq = poll_loop.axes[0].places[0].records().stamp();
        assert!(poll_loop.axes[0].last_status().problem);
        now(poll_loop.poller_pass(false));
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq);
    }

    /// C MM3000: MS answers and TP misses (RETRY, nothing posted), then MS
    /// misses (COMM_ERR); the COMM_ERR post carries the RETRY pass's MS bits
    /// from `motor_info->status` (drvMM3000.cc:247-266, :379-381).
    #[test]
    fn a_comm_err_post_carries_what_the_retry_pass_left() {
        let fresh = MotorStatus {
            home: true,
            ..at(0.0)
        };
        let polls = [
            Err((PollFailure::NONE, Some(fresh.clone()))),
            Err((PollFailure::PROBLEM_AND_COMMS_ERROR, None)),
        ];
        let mut poll_loop = model1_task_on(Arc::new(Mutex::new(Failing {
            polls: polls.into(),
            failure: PollFailure::NONE,
        })));
        send_move(&mut poll_loop);
        let seq = poll_loop.axes[0].places[0].records().stamp();
        now(poll_loop.timed_pass());
        assert_eq!(
            poll_loop.axes[0].places[0].records().stamp(),
            seq,
            "RETRY posts nothing"
        );
        now(poll_loop.timed_pass());
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq + 1);
        let posted = poll_loop.axes[0].last_status();
        assert!(posted.home, "{posted:?}");
        assert!(posted.comms_error && posted.problem && posted.done);
    }

    fn send_move(poll_loop: &mut MotorPollLoop) {
        let trace = Arc::new(MotorTrace::with_user(TraceUser::Model1 {
            device: "devHeld",
            card: 0,
            signal: 0,
        }));
        now(poll_loop.execute(
            0,
            0,
            ToAxis::from(CommandBatch {
                trace,
                record: None,
                commands: vec![MotorCommand::MoveAbsolute {
                    position: 5.0,
                    min_velocity: 0.0,
                    velocity: 1.0,
                    acceleration: 1.0,
                }],
            }),
        ));
    }

    fn held(poll_loop: &MotorPollLoop) -> Held {
        match poll_loop.axes[0].pacing {
            Pacing::MotorTask { held, .. } => held,
            Pacing::Poller => unreachable!(),
        }
    }

    /// A model-1 axis that sent a move from rest at 0 and then polls
    /// `script` on timed wakes; each pass's post, if any.
    fn passes(script: Vec<Option<MotorStatus>>) -> (Vec<Option<MotorStatus>>, bool) {
        let n = script.len();
        let (poll_loop, _) = model1_task(script);
        passes_of(poll_loop, n)
    }

    fn passes_of(mut poll_loop: MotorPollLoop, n: usize) -> (Vec<Option<MotorStatus>>, bool) {
        send_move(&mut poll_loop);
        let posts = (0..n)
            .map(|_| {
                let seq = poll_loop.axes[0].places[0].records().stamp();
                now(poll_loop.timed_pass());
                (poll_loop.axes[0].places[0].records().stamp() != seq)
                    .then(|| poll_loop.axes[0].last_status())
            })
            .collect();
        (posts, held(&poll_loop).is_some())
    }

    #[test]
    fn a_held_move_calls_back_while_the_position_moves() {
        let (posts, held) = passes(vec![Some(at(1.0)), Some(at(1.0)), Some(at(2.0))]);
        assert_eq!(posts, [Some(at(1.0)), None, Some(at(2.0))]);
        assert!(held);
    }

    /// A driver whose C `set_status` disabled the no-motion test.
    struct Unwatched(Script);

    /// At rest at 0 when its card's `motor_init` read it.
    impl Model1Motor for Unwatched {
        fn take_init_status(&mut self) -> MotorStatus {
            MotorStatus::default()
        }
    }

    impl AsynMotor for Unwatched {
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
            PollFailure::NONE
        }
        fn poll(&mut self, user: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            self.0.poll(user, status)
        }
        fn no_motion_test(&self) -> bool {
            false
        }
    }

    // drvEMC18011.cc:356-358: with the test off every pass is called back.
    #[test]
    fn a_driver_without_the_no_motion_test_calls_back_a_still_pass() {
        let script = [at(1.0), at(1.0), at(2.0)];
        let motor = Unwatched(Script(script.into_iter().map(Some).collect()));
        let (posts, held) = passes_of(model1_task_on(Arc::new(Mutex::new(motor))), 3);
        assert_eq!(posts, [Some(at(1.0)), Some(at(1.0)), Some(at(2.0))]);
        assert!(held);
    }

    #[test]
    fn a_problem_ends_the_held_move_with_done_set() {
        let problem = MotorStatus {
            problem: true,
            comms_error: true,
            ..at(1.0)
        };
        let (posts, held) = passes(vec![
            Some(at(1.0)),
            Some(problem.clone()),
            Some(problem.clone()),
        ]);
        let ended = MotorStatus {
            done: true,
            ..problem
        };
        assert_eq!(posts[..2], [Some(at(1.0)), Some(ended)]);
        assert!(!held);
    }

    #[test]
    fn a_limit_ends_the_held_move_only_in_the_direction_of_travel() {
        let behind = MotorStatus {
            low_limit: true,
            ..at(1.0)
        };
        let ahead = MotorStatus {
            high_limit: true,
            ..at(1.0)
        };
        let (posts, held) = passes(vec![Some(behind.clone()), Some(ahead.clone())]);
        let ended = MotorStatus {
            done: true,
            ..ahead
        };
        assert_eq!(posts, [Some(behind), Some(ended)]);
        assert!(!held);
    }

    #[test]
    fn a_retry_pass_calls_back_nothing_and_keeps_the_move() {
        let (posts, held) = passes(vec![None, Some(at(0.0)), None, Some(at(1.0))]);
        assert_eq!(posts, [None, None, None, Some(at(1.0))]);
        assert!(held);
    }

    // motor_task waits forever with no move held (motordrvCom.cc:216-217),
    // so a dead line on an idle axis raises nothing.
    #[test]
    fn an_idle_model1_axis_is_never_polled() {
        let (mut poll_loop, script) = model1_task(vec![Some(at(1.0))]);
        assert_eq!(now(poll_loop.timed_pass()), None);
        assert_eq!(script.lock().unwrap().0.len(), 1);
    }

    // The move's message is processed after the wake's query, so its first
    // query is a scan period later (motordrvCom.cc:225-240).
    #[test]
    fn a_new_move_is_first_queried_a_scan_period_later() {
        let (mut poll_loop, script) = model1_task(vec![Some(at(1.0))]);
        let tick = Instant::now();
        now(poll_loop.query_task());
        send_move(&mut poll_loop);
        assert_eq!(poll_loop.task_due(tick), Some(tick + SCAN));
        assert_eq!(script.lock().unwrap().0.len(), 1);
        assert_eq!(held(&poll_loop).map(|held| held.count), Some(0));
    }

    // An INFO while the move is held reports it not done, whatever the
    // driver read, and done once no move is held.
    #[test]
    fn an_info_reports_done_as_no_move_held() {
        let (mut poll_loop, _) = model1_task(vec![]);
        send_move(&mut poll_loop);
        poll_loop.axes[0].publish_info(Ok(MotorStatus {
            done: true,
            ..at(1.0)
        }));
        assert!(!poll_loop.axes[0].last_status().done);
        poll_loop.axes[0].pacing = Pacing::MotorTask {
            held: None,
            no_motion_test: true,
        };
        poll_loop.axes[0].publish_info(Ok(at(1.0)));
        assert!(poll_loop.axes[0].last_status().done);
    }

    // motordrvCom queues each axis's callback as its query returns
    // (motordrvCom.cc:280-318), not after the task's last axis.
    #[test]
    fn a_queried_change_reaches_the_record_in_its_query() {
        let moving = MotorStatus {
            moving: true,
            done: false,
            ..at(2.0)
        };
        let (mut poll_loop, _) = model1_task(vec![Some(moving)]);
        let state = new_shared_state();
        state.lock().unwrap().set_motordevcom();
        state.lock().unwrap().accept();
        let (io_intr_tx, mut io_intr) = mpsc::channel(4);
        poll_loop.axes[0].places[0].records().attach(
            state,
            io_intr_tx,
            Arc::new(MotorTrace::new("queried", 0)),
        );
        send_move(&mut poll_loop);
        now(poll_loop.query_task());
        assert!(poll_loop.axes[0].pulses.is_empty());
        assert!(io_intr.try_recv().is_ok());
    }

    #[test]
    fn an_ended_move_stops_the_scan() {
        let done = MotorStatus {
            done: true,
            moving: false,
            ..at(1.0)
        };
        let (mut poll_loop, _) = model1_task(vec![Some(done)]);
        send_move(&mut poll_loop);
        assert_eq!(now(poll_loop.timed_pass()), None);
        assert_eq!(held(&poll_loop), None);
    }

    #[test]
    fn a_driver_thread_post_publishes_only_a_change_without_polling() {
        let (mut poll_loop, script) = model1_task(vec![Some(at(9.0))]);
        let seq = poll_loop.axes[0].places[0].records().stamp();
        poll_loop.axes[0].notify(MotorStatus::default(), false);
        assert_eq!(
            poll_loop.axes[0].places[0].records().stamp(),
            seq,
            "unchanged: no publish"
        );
        poll_loop.axes[0].notify(at(1.0), false);
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq + 1);
        assert_eq!(poll_loop.axes[0].last_status(), at(1.0));
        assert_eq!(
            script.lock().unwrap().0.len(),
            1,
            "the driver was not polled"
        );
    }

    /// motor_task scans while its motion list holds the axis, whatever the
    /// driver's RA_MOVING; 37 of the 40 C model-1 drivers never set it.
    #[test]
    fn a_held_move_scans_on_without_ra_moving() {
        let unflagged = MotorStatus {
            msta_moving: false,
            ..at(1.0)
        };
        let (mut poll_loop, _) = model1_task(vec![Some(unflagged)]);
        send_move(&mut poll_loop);
        assert!(now(poll_loop.timed_pass()).is_some());
        assert_eq!(held(&poll_loop).map(|held| held.count), Some(0));
    }

    /// A poller at rest whose polls answer from `script`, with distinct
    /// moving and idle periods.
    fn poller(script: Vec<MotorStatus>) -> MotorPollLoop {
        let script = std::iter::once(MotorStatus::default()).chain(script);
        poller_on(Arc::new(Mutex::new(Script(script.map(Some).collect()))))
    }

    fn poller_on(motor: Arc<Mutex<dyn AsynMotor>>) -> MotorPollLoop {
        let (_poll_tx, poll) = mpsc::channel(1);
        let (_tx, commands) = mpsc::unbounded_channel();
        let mut poll_loop = MotorPollLoop::new(
            PollerInbox { poll, commands },
            motor,
            AxisSubscribers::new(),
            MOVING_PERIOD,
            IDLE_PERIOD,
            0,
        );
        poll_loop.run_startup_pass();
        poll_loop
    }

    const MOVING_PERIOD: Duration = Duration::from_millis(100);
    const IDLE_PERIOD: Duration = Duration::from_secs(60);

    /// The period of one poller pass over `status`, and the MSTA it posts.
    fn pace_and_msta(status: MotorStatus) -> (Duration, MstaFlags) {
        let mut poll_loop = poller(vec![status]);
        let due = now(poll_loop.poller_pass(false)).unwrap();
        let period = due.saturating_duration_since(Instant::now());
        let msta = MstaFlags::from_status(&poll_loop.axes[0].last_status());
        (period, msta)
    }

    /// C asynMotorPoller paces on the driver's own `*moving` (:660); MSTA
    /// RA_MOVING is set only by a driver that sets motorStatusMoving_
    /// (asynMotorAxis.cpp:255-264), which SMC100 and most others never do.
    #[test]
    fn a_moving_poll_runs_fast_without_posting_ra_moving() {
        let (period, msta) = pace_and_msta(MotorStatus {
            moving: true,
            ..MotorStatus::default()
        });
        assert!(period <= MOVING_PERIOD, "moving pace: {period:?}");
        assert!(!msta.contains(MstaFlags::MOVING), "{msta:?}");
    }

    #[test]
    fn ra_moving_posts_without_pacing_the_poller() {
        let (period, msta) = pace_and_msta(MotorStatus {
            msta_moving: true,
            ..MotorStatus::default()
        });
        assert!(period > MOVING_PERIOD, "idle pace: {period:?}");
        assert!(msta.contains(MstaFlags::MOVING), "{msta:?}");
    }

    #[test]
    fn moving_and_ra_moving_both_set_post_and_pace() {
        let (period, msta) = pace_and_msta(MotorStatus {
            moving: true,
            msta_moving: true,
            ..MotorStatus::default()
        });
        assert!(period <= MOVING_PERIOD, "moving pace: {period:?}");
        assert!(msta.contains(MstaFlags::MOVING), "{msta:?}");
    }

    #[test]
    fn a_status_that_sets_neither_idles_without_ra_moving() {
        let (period, msta) = pace_and_msta(MotorStatus::default());
        assert!(period > MOVING_PERIOD, "idle pace: {period:?}");
        assert!(!msta.contains(MstaFlags::MOVING), "{msta:?}");
    }

    #[test]
    fn ra_moving_does_not_hold_a_done_move() {
        let done = MotorStatus {
            done: true,
            msta_moving: true,
            ..at(1.0)
        };
        let (mut poll_loop, _) = model1_task(vec![Some(done)]);
        send_move(&mut poll_loop);
        assert_eq!(now(poll_loop.timed_pass()), None);
        assert_eq!(held(&poll_loop), None);
    }

    // devMotorSim on a motor_interface.h driver: drvMotorSim's motorProcTask
    // (drvMotorSim.c:660-690) calls back what changed every period, moving
    // or idle, and motor_callback copies the raw status and scanOnce's the
    // record (devMotorSim.c:73-126).

    fn interface_user() -> TraceUser {
        TraceUser::MotorInterface {
            device: "Motor Simulation",
            driver: "motorSim",
            card: 0,
            signal: 0,
        }
    }

    /// A devMotorSim axis whose startup poll reads `rest` and whose later
    /// polls answer from `script`, with one record bound past its gate.
    fn interface_task(
        rest: MotorStatus,
        script: Vec<Option<MotorStatus>>,
    ) -> (
        MotorPollLoop,
        Arc<Mutex<Script>>,
        crate::device_support::MotorDeviceSupport,
    ) {
        let script = std::iter::once(Some(rest)).chain(script);
        let script = Arc::new(Mutex::new(Script(script.collect())));
        let motor: Arc<Mutex<dyn AsynMotor>> = script.clone();
        let mut setup = crate::builder::MotorBuilder::new(motor)
            .trace_user(interface_user())
            .poll_interval(SCAN)
            .build_axis();
        setup.poll_loop.run_startup_pass();
        let support = setup.axis.attach();
        support.device_state().lock().unwrap().accept();
        (setup.poll_loop, script, support)
    }

    /// Record 0's build_trans.
    fn interface_batch(commands: Vec<MotorCommand>) -> CommandBatch {
        CommandBatch {
            trace: Arc::new(MotorTrace::with_user(interface_user())),
            record: Some(0),
            commands,
        }
    }

    fn msta(status: &MotorStatus) -> u32 {
        crate::flags::MstaFlags::from_status(status).bits()
    }

    /// The sim reflecting off its high limit still moves: each raw status
    /// is posted as the driver set it, DONE clear, and the task polls on.
    #[test]
    fn a_limit_is_posted_raw_while_the_driver_moves() {
        // drvMotorSim raises motorAxisMoving with !done (drvMotorSim.c:652).
        let hit = MotorStatus {
            high_limit: true,
            msta_moving: true,
            ..at(5.0)
        };
        let back = MotorStatus {
            direction: false,
            position: 4.9,
            ..hit.clone()
        };
        let (mut poll_loop, _, _record) = interface_task(
            MotorStatus::default(),
            vec![Some(hit.clone()), Some(back.clone())],
        );
        now(poll_loop.execute(
            0,
            0,
            ToAxis::from(interface_batch(vec![MotorCommand::MoveAbsolute {
                position: 9.0,
                min_velocity: 0.0,
                velocity: 1.0,
                acceleration: 1.0,
            }])),
        ));
        for (want, word) in [(hit, 1029), (back, 1028)] {
            assert!(now(poll_loop.timed_pass()).is_some());
            assert_eq!(poll_loop.axes[0].last_status(), want);
            assert_eq!(msta(&want), word);
        }
    }

    /// No move is needed for a pass: an idle axis is polled every period,
    /// and a change posts.
    #[test]
    fn an_idle_axis_is_polled_every_period_and_posts_a_change() {
        let rest = MotorStatus::default();
        let nudged = MotorStatus {
            position: 1.0,
            ..rest.clone()
        };
        let (mut poll_loop, script, mut record) =
            interface_task(rest.clone(), vec![Some(rest), Some(nudged.clone())]);
        let mut io_intr = {
            use epics_base_rs::server::device_support::DeviceSupport;
            record.io_intr_receiver().unwrap()
        };
        let seq = poll_loop.axes[0].places[0].records().stamp();
        let tick = Instant::now();
        assert!(now(poll_loop.timed_pass()).is_some_and(|due| due >= tick + SCAN));
        assert_eq!(
            poll_loop.axes[0].places[0].records().stamp(),
            seq,
            "unchanged: no post"
        );
        assert!(now(poll_loop.timed_pass()).is_some());
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq + 1);
        assert_eq!(poll_loop.axes[0].last_status(), nudged);
        assert!(poll_loop.axes[0].pulses.is_empty());
        assert!(
            io_intr.try_recv().is_ok(),
            "the change reached the record in its pass"
        );
        assert!(script.lock().unwrap().0.is_empty());
    }

    /// HOMR from rest on the high limit: motorAxisHome's done=0 callback
    /// posts at once with the other bits as they were; the finished write
    /// processes nothing more (no asynCallback), and the task keeps its
    /// period.
    #[test]
    fn a_home_off_the_limit_posts_done_clear_at_once() {
        let parked = MotorStatus {
            position: 5.0,
            high_limit: true,
            direction: true,
            done: true,
            moving: false,
            ..MotorStatus::default()
        };
        let (mut poll_loop, _, mut record) = interface_task(parked.clone(), vec![]);
        let mut io_intr = {
            use epics_base_rs::server::device_support::DeviceSupport;
            record.io_intr_receiver().unwrap()
        };
        let seq = poll_loop.axes[0].places[0].records().stamp();
        now(poll_loop.execute(
            0,
            0,
            ToAxis::from(interface_batch(vec![MotorCommand::Home {
                forward: false,
                min_velocity: 0.0,
                velocity: 1.0,
                acceleration: 1.0,
            }])),
        ));
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq + 1);
        assert_eq!(
            poll_loop.axes[0].last_status(),
            MotorStatus {
                done: false,
                ..parked
            }
        );
        assert!(io_intr.try_recv().is_ok());
        assert!(io_intr.try_recv().is_err());
        assert!(!poll_loop.axes[0].woken);
    }

    /// A motor recording the order of its GET_INFO calls.
    struct ForceCallbackOrder(Vec<&'static str>);

    impl AsynMotor for ForceCallbackOrder {
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
            PollFailure::NONE
        }
        fn force_callback(&mut self, _: &AsynUser) {
            self.0.push("force_callback");
        }
        fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
            self.0.push("poll");
            *status = MotorStatus::default();
            Ok(())
        }
    }

    /// drvMotorAsyn's GET_INFO is its port's writeInt32, which calls the
    /// driver's forceCallback (drvMotorAsyn.c:617-619); the poll is the
    /// poller's, after it.
    #[test]
    fn a_model2_get_info_calls_force_callback_through_its_port() {
        let motor = Arc::new(Mutex::new(ForceCallbackOrder(Vec::new())));
        let (mut poll_loop, axis) = model2_axis("PLM2FORCE", motor.clone());
        motor.lock().unwrap().0.clear();
        axis.send(MotorCommand::Poll);
        poll_loop.run_queued_commands();
        assert_eq!(motor.lock().unwrap().0, ["force_callback"]);
        assert!(matches!(
            poll_loop.axes[0].places[0].queue().inbox.poll.try_recv(),
            Ok(PollCommand::ForcePoll)
        ));
    }

    /// asynMotorController's GET_INFO only polls (asynMotorController.cpp:
    /// 217-221).
    #[test]
    fn a_controller_get_info_polls_without_force_callback() {
        let mut motor = ForceCallbackOrder(Vec::new());
        let trace = MotorTrace::with_user(TraceUser::Asyn {
            port: "PLM3FORCE".into(),
            addr: 0,
        });
        execute_commands(&mut motor, &trace, &[MotorCommand::Poll], &mut |_| {});
        assert_eq!(motor.0, ["poll"]);
    }

    /// GET_INFO is `needUpdate = 1` on the status last called back: no
    /// driver call and no process.
    #[test]
    fn a_get_info_marks_need_update_and_neither_polls_nor_processes() {
        let rest = MotorStatus::default();
        let (mut poll_loop, script, record) = interface_task(rest.clone(), vec![Some(at(9.0))]);
        let state = record.device_state().clone();
        state.lock().unwrap().init_status();
        let seq = poll_loop.axes[0].places[0].records().stamp();
        now(poll_loop.execute(
            0,
            0,
            ToAxis::from(interface_batch(vec![MotorCommand::Poll])),
        ));
        assert_eq!(
            script.lock().unwrap().0.len(),
            1,
            "the driver was not polled"
        );
        assert_eq!(poll_loop.axes[0].places[0].records().stamp(), seq);
        assert!(poll_loop.axes[0].pulses.is_empty());
        assert_eq!(state.lock().unwrap().update_values(), Some(rest));
    }

    /// A controller port address for an axis added with none mirrors its
    /// status there; a different second one is refused, as one
    /// asynMotorAxis has one address.
    #[test]
    fn a_later_asyn_address_of_an_added_axis_is_mirrored() {
        let motor: Arc<Mutex<dyn AsynMotor>> =
            Arc::new(Mutex::new(crate::sim_motor::SimMotor::default()));
        let mut controller = crate::builder::ControllerBuilder::new();
        let axes = controller.axes();
        let model2 = TraceUser::Model2 {
            port: "PLM2".into(),
            addr: 0,
        };
        let _first = controller.axis(motor.clone(), model2);
        let at = |addr| TraceUser::Asyn {
            port: "PLMIRROR".into(),
            addr,
        };
        let _second = axes.add(motor.clone(), at(3)).unwrap();
        assert!(axes.add(motor.clone(), at(3)).is_ok());
        assert!(axes.add(motor, at(4)).is_err());
        let idle = Duration::from_secs(60);
        let poll_loop = controller.build(idle, idle, 0);
        assert_eq!(poll_loop.axes.len(), 1);
        assert_eq!(
            poll_loop.axes[0].controller,
            Some(("PLMIRROR".to_owned(), 3))
        );
    }
}
