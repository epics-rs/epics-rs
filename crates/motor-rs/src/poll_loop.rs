use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure, StatusWrite};
use asyn_rs::trace::TraceMask;
use asyn_rs::user::AsynUser;
use epics_base_rs::runtime::task::{
    StackSizeClass, ThreadPriority, block_on_sync, sleep_background, spawn_dedicated_thread,
};
use tokio::sync::{mpsc, oneshot};

use crate::controller_port::{FinishedWrite, WriteValue};
use crate::device_state::*;
use crate::flags::{MotorCommand, PidGainKind, StoredSpeeds};
use crate::trace::MotorTrace;

/// Queue of driver commands from the device support to the poller thread.
///
/// Unbounded like the asyn request queue C `devMotorAsyn::build_trans` feeds,
/// so a record pass never waits on the driver.
pub type MotorCommandSender = mpsc::UnboundedSender<CommandBatch>;

/// One record pass's commands, with the record's trace: C's asynUser that
/// `build_trans` queued them on, which `asynCallback` prints through and
/// whose record a finished move processes. `record` is `None` for a write
/// through the controller port, which processes no record.
#[derive(Debug)]
pub struct CommandBatch {
    pub trace: Arc<MotorTrace>,
    pub record: Option<RecordId>,
    pub commands: Vec<MotorCommand>,
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
    pub commands: mpsc::UnboundedReceiver<CommandBatch>,
}

/// Motor poll loop — one per controller, polling its axes in turn and
/// posting each to every record bound to it.
///
/// It is the only caller of the drivers after `init`: polls and the records'
/// commands all run on its own `motorPoller` OS thread (see [`Self::run`]),
/// as C runs one `asynMotorPoller` per controller on an epicsThread
/// (asynMotorController.cpp:615-695), so a driver blocked on a dead serial
/// line or TCP peer never stalls the IOC's async executor, and a slow axis
/// delays its controller's other axes as in C.
pub struct MotorPollLoop {
    axes: Vec<Axis>,
    /// Axes added to the controller once the poller runs.
    joining: Option<mpsc::UnboundedReceiver<Joining>>,
    schedule: Schedule,
    controller_poll: Option<ControllerPoll>,
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
    /// with none: its status is mirrored there.
    Mirror {
        motor: Arc<Mutex<dyn AsynMotor>>,
        port: String,
        addr: i32,
    },
}

impl Join {
    /// Apply the add to `axes`, kept in address order.
    pub(crate) fn place(self, axes: &mut Vec<Axis>) {
        let axis = match self {
            Self::Axis(axis) => *axis,
            Self::Mirror { motor, port, addr } => {
                let Some(at) = axes
                    .iter()
                    .position(|a| std::ptr::addr_eq(Arc::as_ptr(&a.motor), Arc::as_ptr(&motor)))
                else {
                    return;
                };
                let mut axis = axes.remove(at);
                axis.mirror_to(&port, addr);
                axis
            }
        };
        let at = axes.partition_point(|a| a.order() <= axis.order());
        axes.insert(at, axis);
    }
}

/// C `asynMotorController::poll`, run before the axes on every pass.
pub type ControllerPoll = Box<dyn FnMut() + Send>;

mod polled {
    use super::ControllerPoll;

    /// Proof the controller poll has run: C polls an axis only after
    /// `pC->poll()`, on a poller pass (asynMotorController.cpp:652-656) and
    /// in `motorUpdateStatus_` (:217-221). Only [`after`] makes one.
    pub(super) struct Polled(());

    /// Run the controller poll, for the axis polls that follow it.
    pub(super) fn after(poll: &mut Option<ControllerPoll>) -> Polled {
        if let Some(poll) = poll {
            poll();
        }
        Polled(())
    }
}
use polled::Polled;

/// When the poll loop runs a pass.
enum Schedule {
    /// C `asynMotorPoller` (asynMotorController.cpp:615-695): every axis,
    /// at the moving or idle period, fast for a while after a wakeup.
    Poller {
        moving: Duration,
        idle: Duration,
        forced_fast_polls: u32,
        remaining: u32,
    },
    /// C motordrvCom `motor_task` (motordrvCom.cc:180-262), a scan period
    /// after the last wake while a move is held.
    MotorTask { scan: Duration },
}

/// One axis of the loop and the records bound to it.
pub(crate) struct Axis {
    cmd_rx: mpsc::Receiver<PollCommand>,
    motor_cmd_rx: mpsc::UnboundedReceiver<CommandBatch>,
    motor: Arc<Mutex<dyn AsynMotor>>,
    subscribers: AxisSubscribers,
    pacing: Pacing,
    status_seq: u64,
    /// Last status delivered to the record. An autonomous (timed) idle poll
    /// notifies the record only when the freshly polled status differs from
    /// this — the analogue of C `asynMotorAxis::callParamCallbacks` firing the
    /// status callback only when `statusChanged_` is set (asynMotorAxis.cpp:
    /// 316-322), so a stationary axis does not re-post DIFF/RDIF every idle
    /// period. A forced poll (ForcePoll, or a GET_INFO) bypasses this and
    /// always notifies.
    last_status: Option<MotorStatus>,
    /// C `statusChanged_` left set: `last_status` is a status a failed poll
    /// left that no post has carried yet, so the next post goes out even if
    /// it equals it.
    status_changed: bool,
    /// A model-2 driver's parameter library, shared with its [`MotorAxis`]
    /// for the driver's report.
    ///
    /// [`MotorAxis`]: crate::device_support::MotorAxis
    params: Model2Params,
    /// A model-2 axis's `min_velocity` / `max_velocity` / `accel`, which
    /// the record's speed writes store (drvMotorAsyn.c:722-733). 0 until
    /// one is: drvMotorAsyn's axis is calloc'd.
    speeds: StoredSpeeds,
    /// The io_intr pulses owed for processes the callbacks queued.
    pulses: Pulses,
    /// A move ran: C `wakeupPoller`.
    woken: bool,
    /// The controller port and axis whose parameters mirror the status.
    controller: Option<(String, i32)>,
    /// The status word last set in the controller port's parameters.
    mirrored_word: Option<u32>,
    /// [`AsynMotor::param_owned_msta_bits`]: bit params the driver owns.
    owned_bits: u32,
    /// [`AsynMotor::take_poll_params`] of the last poll or write, which the
    /// next `callParamCallbacks` on the axis posts.
    poll_params: Vec<(usize, asyn_rs::param::ParamValue)>,
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
/// record that sent it with that record's CDIR as the driver last left it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Held {
    count: u32,
    mover: Option<(RecordId, bool)>,
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

/// The speeds an asynMotorController axis moves on: its port's
/// parameters. asyn's paramList::getDouble zeroes an unset one before it
/// fails.
fn controller_speeds(trace: &MotorTrace) -> Option<StoredSpeeds> {
    use crate::controller_port::index;
    let port = crate::devices::controller_port_handle(trace.port())?;
    let read = |function| {
        port.read_float64_blocking(function, trace.addr())
            .unwrap_or(0.0)
    };
    Some(StoredSpeeds {
        min_velocity: read(index::VEL_BASE),
        velocity: read(index::VELOCITY),
        acceleration: read(index::ACCEL),
    })
}

/// [`Axis::speeds_for`]'s verdict on a record command.
enum Speeds {
    /// A speed write the store took; no driver call follows.
    Stored,
    /// A move or stop on the stored speeds.
    Resolved(MotorCommand),
}

/// C `epicsThreadCreate`'s report of a refused `pthread_create`
/// (osdThread.c:675, `checkStatusOnce`); the WIN32 port prints nothing.
fn thread_create_refused(e: &std::io::Error) {
    #[cfg(unix)]
    if let Some(errno) = e.raw_os_error() {
        // SAFETY: `strerror` returns a NUL-terminated string valid until the
        // next `strerror` call; it is copied out at once.
        let text = unsafe { std::ffi::CStr::from_ptr(libc::strerror(errno)) }.to_string_lossy();
        eprintln!("pthread_create error {text}");
    }
    #[cfg(not(unix))]
    let _ = e;
}

impl Axis {
    /// An axis polled on every pass of a [`Schedule::Poller`] loop.
    pub(crate) fn polled(
        inbox: PollerInbox,
        motor: Arc<Mutex<dyn AsynMotor>>,
        subscribers: AxisSubscribers,
    ) -> Self {
        Self::with_pacing(inbox, motor, subscribers, Pacing::Poller)
    }

    /// A model-1 axis, whose held move a [`Schedule::MotorTask`] queries.
    pub(crate) fn task(
        inbox: PollerInbox,
        motor: Arc<Mutex<dyn AsynMotor>>,
        subscribers: AxisSubscribers,
    ) -> Self {
        let no_motion_test = motor.lock().map_or(true, |m| m.no_motion_test());
        let pacing = Pacing::MotorTask {
            held: None,
            no_motion_test,
        };
        Self::with_pacing(inbox, motor, subscribers, pacing)
    }

    fn with_pacing(
        inbox: PollerInbox,
        motor: Arc<Mutex<dyn AsynMotor>>,
        subscribers: AxisSubscribers,
        pacing: Pacing,
    ) -> Self {
        Self {
            cmd_rx: inbox.poll,
            motor_cmd_rx: inbox.commands,
            motor,
            subscribers,
            pacing,
            status_seq: 1, // starts at 1 (init already wrote seq=1)
            last_status: None,
            status_changed: false,
            params: Default::default(),
            speeds: StoredSpeeds::default(),
            pulses: Pulses::new(),
            woken: false,
            controller: None,
            mirrored_word: None,
            owned_bits: 0,
            poll_params: Vec::new(),
            power: Power::Idle,
        }
    }

    /// Keep the model-2 parameters in `params`, the table its `MotorAxis`
    /// reads.
    pub(crate) fn with_params(mut self, params: Model2Params) -> Self {
        self.params = params;
        self
    }

    /// The axis's `delay` parameter when its MOTOR_POWER_AUTO_ONOFF is 1,
    /// read as C's getIntegerParam / getDoubleParam do: 0 when unset.
    fn auto_power(&self, delay: usize) -> Option<Duration> {
        use crate::controller_port::index;
        let (port, addr) = self.controller.as_ref()?;
        let handle = crate::devices::controller_port_handle(port)?;
        let on = handle.read_int32_blocking(index::POWER_AUTO_ONOFF, *addr);
        (on.unwrap_or(0) == 1).then(|| {
            let secs = handle.read_float64_blocking(delay, *addr).unwrap_or(0.0);
            Duration::try_from_secs_f64(secs).unwrap_or_default()
        })
    }

    /// One axis of C `asynMotorPoller`'s pass (asynMotorController.cpp:
    /// 652-683): the poll, then an ended move under auto power powers the
    /// axis off once MOTOR_POWER_OFF_DELAY has passed.
    fn poller_poll(&mut self, polled: &Polled) -> bool {
        let off_delay = self.auto_power(crate::controller_port::index::POWER_OFF_DELAY);
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
            if let Ok(mut motor) = self.motor.lock() {
                let _ = motor.set_closed_loop(&AsynUser::new(0), false);
            }
            self.power = Power::Idle;
        }
        moving
    }

    /// Mirror each status into axis `addr` of controller port `port`.
    pub(crate) fn mirror_to(&mut self, port: &str, addr: i32) {
        self.controller = Some((port.to_owned(), addr));
        self.mirrored_word = None;
        if let Ok(motor) = self.motor.lock() {
            self.owned_bits = motor.param_owned_msta_bits();
        }
        // The startup poll ran before the port was known.
        if let Some(status) = self.last_status.clone() {
            self.hold(&status);
        }
    }

    fn run_queued_commands(&mut self, controller_poll: &mut Option<ControllerPoll>) {
        while let Ok(batch) = self.motor_cmd_rx.try_recv() {
            self.execute(&batch, controller_poll);
        }
        for io_intr in self.pulses.drain(..) {
            let _ = io_intr.try_send(());
        }
    }

    /// Send the owed pulses, waiting on a full queue as C's callback waits
    /// in `dbProcess`.
    async fn send_pulses(&mut self) {
        for io_intr in std::mem::take(&mut self.pulses) {
            let _ = io_intr.send(()).await;
        }
    }

    /// Run a record's commands, posting each write's status change; a
    /// finished move then processes the record (C `asynCallback`), and a
    /// move wakes the poller (asynMotorController.cpp:303, :320, :336,
    /// :355).
    fn execute(&mut self, batch: &CommandBatch, controller_poll: &mut Option<ControllerPoll>) {
        let motor = self.motor.clone();
        let Ok(mut motor) = motor.lock() else {
            return;
        };
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
                self.subscribers.get_info(record);
                continue;
            }
            if batch.record.is_some() {
                async_callback_flow(&batch.trace, command);
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
            // C writeFloat64 powers the axis on before a move or home
            // (asynMotorController.cpp:291-295, :308-312, :325-329, :342-346).
            if command.is_move() && !matches!(command, MotorCommand::SetPosition { .. }) {
                let on_delay = self.auto_power(crate::controller_port::index::POWER_ON_DELAY);
                if let Some(delay) = on_delay {
                    let _ = motor.set_closed_loop(&AsynUser::new(0), true);
                    self.power = Power::Moving;
                    std::thread::sleep(delay);
                }
            }
            execute_commands_after(
                &mut *motor,
                &batch.trace,
                std::slice::from_ref(command),
                &mut |write| self.post_writes(write),
                &mut || {
                    polled::after(controller_poll);
                },
            );
            if !command.is_move() {
                continue;
            }
            if let Some(record) = batch.record {
                self.pulses
                    .extend(self.subscribers.move_done(record, command));
            }
            if !matches!(command, MotorCommand::SetPosition { .. }) {
                match &mut self.pacing {
                    // drvMotorSim's motorProcTask keeps its period: nothing
                    // wakes it (drvMotorSim.c:660-690).
                    Pacing::Poller => self.woken = !batch.trace.is_motor_interface(),
                    // A MOTION or VELOCITY message replaces the held move
                    // and clears its count (motordrvCom.cc:415-437).
                    Pacing::MotorTask { held, .. } => {
                        *held = Some(Held {
                            count: 0,
                            mover: batch.record.zip(cdir),
                        })
                    }
                }
            }
        }
    }

    /// What the axis's speed store makes of a record's `command`: a speed
    /// write is stored, and a move or stop runs on the stored speeds, as
    /// asynMotorController and drvMotorAsyn read them
    /// (asynMotorController.cpp:208, :297-350; drvMotorAsyn.c:606, :706-733).
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
                Speeds::Resolved(command.on_speeds(StoredSpeeds {
                    acceleration: accel.take().unwrap_or(0.0),
                    ..self.speeds
                }))
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
            return controller_speeds(trace)
                .map(|speeds| Speeds::Resolved(command.on_speeds(speeds)));
        }
        if command.store_speed(&mut self.speeds) {
            // drvMotorAsyn's writeFloat64 prints the write it stores.
            return None;
        }
        command
            .runs_on_speeds()
            .then(|| Speeds::Resolved(command.on_speeds(self.speeds)))
    }

    /// C `asynMotorAxis::callParamCallbacks` after a write (asynMotorAxis.cpp:
    /// 316-322): `write.fields` land on the last posted status, which is
    /// posted only if they changed it (`statusChanged_`). A model-2 parameter
    /// posts the status when it changed the parameter: paramLib flags only a
    /// new type or value (paramLib.c:112-118, 141-147), and `callCallback`
    /// then posts to every record on the axis (drvMotorAsyn.c `intCallback`).
    fn post_writes(&mut self, write: AxisWrite) {
        let param_changed = write
            .params
            .into_iter()
            .fold(false, |changed, (reason, value)| {
                self.params.set(reason, value) | changed
            });
        self.poll_params.extend(write.poll_params);
        if let Some(polled) = write.polled {
            self.publish_info(polled);
            return;
        }
        if !write.fields.is_empty() || param_changed {
            let mut status = self.last_status.clone().unwrap_or_else(status_before_poll);
            for field in write.fields {
                field.apply(&mut status);
            }
            if param_changed || self.last_status.as_ref() != Some(&status) {
                self.publish(status);
                return;
            }
        }
        if !self.poll_params.is_empty() {
            self.call_param_callbacks();
        }
    }

    /// Post `status` to every bound record's `statusCallback` under a new
    /// sequence number.
    fn publish(&mut self, status: MotorStatus) {
        self.hold(&status);
        self.status_changed = false;
        self.status_seq += 1;
        let pulses = self.subscribers.post(StampedStatus {
            seq: self.status_seq,
            status,
        });
        self.pulses.extend(pulses);
    }

    /// Make `status` the axis's: C `asynMotorAxis::setDoubleParam` /
    /// `setIntegerParam` of each of its values on the controller port's axis
    /// address, then `callParamCallbacks` (asynMotorAxis.cpp:250-322).
    /// MOTOR_STATUS is the whole word on every set: C writes it as 0 when a
    /// non-status integer is set (`status` stays 0 at :253, :267), a bug.
    fn hold(&mut self, status: &MotorStatus) {
        self.last_status = Some(status.clone());
        let poll_params = std::mem::take(&mut self.poll_params);
        let Some((port, addr)) = &self.controller else {
            return;
        };
        let Some(handle) = crate::devices::controller_port_handle(port) else {
            return;
        };
        use crate::controller_port::index;
        use asyn_rs::param::ParamValue;
        use asyn_rs::request::ParamSetValue;
        let addr = *addr;
        let word = crate::flags::MstaFlags::from_status(status).bits();
        let mut updates = Vec::from([
            ParamSetValue::new(index::POSITION, addr, ParamValue::Float64(status.position)),
            ParamSetValue::new(
                index::ENCODER_POSITION,
                addr,
                ParamValue::Float64(status.encoder_position),
            ),
            ParamSetValue::new(
                index::ACT_VELOCITY,
                addr,
                ParamValue::Float64(status.velocity),
            ),
            ParamSetValue::new(index::STATUS, addr, ParamValue::Int32(word as i32)),
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
            ParamSetValue::new(
                index::STATUS_DIRECTION + bit,
                addr,
                ParamValue::Int32(value),
            )
        }));
        updates.extend(
            poll_params
                .into_iter()
                .map(|(reason, value)| ParamSetValue::new(reason, addr, value)),
        );
        // C drivers ignore the status of setXxxParam and callParamCallbacks.
        let _ = handle.set_params_and_notify_blocking(addr, updates);
    }

    /// C's `callParamCallbacks` on the axis with no status to set: posts
    /// whatever was set on it since the last one, such as a
    /// [`WriteHook`](crate::controller_port::WriteHook)'s sets.
    fn call_param_callbacks(&mut self) {
        let poll_params = std::mem::take(&mut self.poll_params);
        let Some((port, addr)) = &self.controller else {
            return;
        };
        let addr = *addr;
        let updates = poll_params
            .into_iter()
            .map(|(reason, value)| asyn_rs::request::ParamSetValue::new(reason, addr, value))
            .collect();
        if let Some(handle) = crate::devices::controller_port_handle(port) {
            let _ = handle.set_params_and_notify_blocking(addr, updates);
        }
    }

    /// The status a failed poll leaves in the parameter library: the last
    /// one with the bits `failure` raises set. Whether it posts is
    /// [`Self::poll_and_notify`]'s change gate, so a failure raising no new
    /// bit posts only on a forced poll, as C's `motorUpdateStatus_` path
    /// forces `statusChanged_` (asynMotorController.cpp:217-221) while the
    /// background poller ignores the poll status (:658).
    fn failed_poll_status(&mut self, failed: FailedPoll) -> MotorStatus {
        if let Some(left) = failed.left
            && self.last_status.as_ref() != Some(&left)
        {
            self.hold(&left);
            self.status_changed = true;
        }
        let mut status = self.last_status.clone().unwrap_or_else(status_before_poll);
        status.problem |= failed.bits.problem;
        status.comms_error |= failed.bits.comms_error;
        status
    }

    /// C asynMotorPoller's "Force on poll at startup": fills the cache
    /// `MotorDeviceSupport::init` reads, as readGenericPointer reads the
    /// parameter library. No record is woken. C's poller holds the
    /// controller lock across this poll, so init_record's read waits for it
    /// (asynMotorController.cpp:505-525, :630); taken while the poller is
    /// built, before the creating command returns, init reads it too.
    fn prime_cache(&mut self, polled: &Polled) {
        let status = self.poll_status(polled);
        if status.moving {
            self.power = Power::Moving;
        }
        self.hold(&status);
        self.subscribers.store(StampedStatus { seq: 1, status });
    }

    fn poll_status(&mut self, polled: &Polled) -> MotorStatus {
        self.poll_result(polled)
            .unwrap_or_else(|failure| self.failed_poll_status(failure))
    }

    fn poll_result(&mut self, _: &Polled) -> Result<MotorStatus, FailedPoll> {
        let user = AsynUser::new(0);
        // An `Err` is the C driver poll bailing to its error label, which
        // raises what `poll_failure` names. A poisoned driver mutex has no C
        // counterpart; it raises both bits. A forced poll still posts either
        // way, so a `STUP=BUSY` refresh cannot latch (record/status_update.rs
        // :48-58). C's poller ignores the poll status
        // (asynMotorController.cpp:658), so a failure prints nothing.
        match self.motor.lock() {
            Ok(mut motor) => {
                let polled = poll_driver(&mut *motor, &user);
                self.poll_params.extend(motor.take_poll_params());
                // A model-2 driver's update sets its paramLib
                // (drvMotorSim.c:646-653); the poll's status posts them.
                for write in motor.take_status_writes() {
                    let (reason, value) = model2_param(write);
                    self.params.set(reason, value);
                }
                polled
            }
            Err(_) => Err(FailedPoll::POISONED),
        }
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
            Err(failed) if failed.bits == PollFailure::NONE => {
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
        let still = tested
            && self
                .last_status
                .as_ref()
                .is_some_and(|last| last.position == status.position);
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
        mover: &mut Option<(RecordId, bool)>,
        polled: &Polled,
    ) -> Result<MotorStatus, FailedPoll> {
        let Some((record, cdir)) = mover else {
            return self.poll_result(polled);
        };
        let before = *cdir;
        let polled = match self.motor.lock() {
            Ok(mut motor) => {
                let polled = motor.poll_in_motion(&AsynUser::new(0), cdir);
                polled.map_err(|_| FailedPoll::of(&mut *motor))
            }
            Err(_) => Err(FailedPoll::POISONED),
        };
        if *cdir != before {
            self.subscribers.set_driver_cdir(*record, *cdir);
        }
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
    /// that is the last known `moving`: C's `pAxis->poll(&moving)` leaves
    /// `*moving` untouched when the driver bails early.
    ///
    /// `force` distinguishes the two C poll paths. A forced poll (the analogue
    /// of C `motorUpdateStatus_` forcing `statusChanged_=1`, asynMotorController
    /// .cpp:217-222) always bumps the sequence and pulses io_intr, so the
    /// record re-processes even on a stationary axis. An autonomous (timed) poll passes
    /// `force=false` and notifies only when the polled status differs from the
    /// last one delivered — the analogue of C's `statusChanged_` gate, so a
    /// settled, unchanging axis does not re-post DIFF/RDIF every idle period.
    fn poll_and_notify(&mut self, force: bool, polled: &Polled) -> bool {
        let status = match self.poll_result(polled) {
            Ok(status) => status,
            // A driver returning before callParamCallbacks posts nothing,
            // whatever it left set.
            Err(failed) if failed.bits == PollFailure::NONE && !force => {
                let status = self.failed_poll_status(failed);
                self.call_param_callbacks();
                return status.moving;
            }
            Err(failed) => self.failed_poll_status(failed),
        };
        let moving = status.moving;
        self.notify(status, force);
        moving
    }

    /// C `asynMotorAxis::callParamCallbacks` with `status` set: posted to
    /// the records when `force`d or when it changed the last one.
    fn notify(&mut self, status: MotorStatus, force: bool) {
        if !force && !self.status_changed && self.last_status.as_ref() == Some(&status) {
            // Autonomous idle poll, status unchanged → no record pass (C posts
            // nothing when statusChanged_ stays 0). A *repeated* comms failure
            // lands here too, and correctly so: the COMM_ERR flag is already
            // set, no parameter changed, so C's `statusChanged_` stays 0 and
            // `asynMotorAxis::callParamCallbacks` posts nothing
            // (asynMotorAxis.cpp:316-322). The first failure — the one that
            // flips the flag — always differs from the last status and posts.
            // Its pC_->callParamCallbacks still posts what was set meanwhile.
            self.call_param_callbacks();
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
            vec![Axis::polled(inbox, motor, subscribers)],
            None,
            moving_poll_interval,
            idle_poll_interval,
            forced_fast_polls,
            None,
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
        controller_poll: Option<ControllerPoll>,
    ) -> Self {
        let schedule = Schedule::Poller {
            moving: moving_poll_interval,
            idle: idle_poll_interval,
            forced_fast_polls,
            remaining: 0,
        };
        Self::started(axes, joining, schedule, controller_poll)
    }

    /// A model-1 driver's task over its `axes`, scanning the held moves
    /// every `scan`.
    pub(crate) fn motor_task(axes: Vec<Axis>, scan: Duration) -> Self {
        Self::started(axes, None, Schedule::MotorTask { scan }, None)
    }

    /// The loop after its startup pass: the controller poll, then each
    /// axis's [`Axis::prime_cache`] (asynMotorController.cpp:630, :656-661).
    fn started(
        mut axes: Vec<Axis>,
        joining: Option<mpsc::UnboundedReceiver<Joining>>,
        schedule: Schedule,
        mut controller_poll: Option<ControllerPoll>,
    ) -> Self {
        axes.sort_by_key(Axis::order);
        let polled = polled::after(&mut controller_poll);
        for axis in &mut axes {
            axis.prime_cache(&polled);
        }
        Self {
            axes,
            joining,
            schedule,
            controller_poll,
        }
    }

    /// An add to the running controller. C's poller runs one startup pass;
    /// a later axis is first polled on the next pass, and a record bound
    /// before reads C's zeroed status ([`MotorDeviceState::init_status`]).
    fn join(&mut self, Joining { join, joined }: Joining) {
        join.place(&mut self.axes);
        let _ = joined.send(());
    }

    /// Run the commands queued so far on the calling thread: for a harness
    /// standing in for the poller thread. The IOC path is [`Self::run`].
    pub fn run_queued_commands(&mut self) {
        for axis in &mut self.axes {
            axis.run_queued_commands(&mut self.controller_poll);
        }
    }

    /// Run a batch of axis `index`'s commands and send what they posted, as
    /// C's write ends in `pAxis->callParamCallbacks()` before its
    /// `wakeupPoller` (asynMotorController.cpp:317-320).
    async fn execute(&mut self, index: usize, batch: &CommandBatch) {
        let axis = &mut self.axes[index];
        axis.execute(batch, &mut self.controller_poll);
        axis.send_pulses().await;
    }

    fn is_motor_task(&self) -> bool {
        matches!(self.schedule, Schedule::MotorTask { .. })
    }

    /// One wake of `motor_task`, before it processes the messages: query
    /// each held move (motordrvCom.cc:244-254), its callback queued as its
    /// query returns (:280-318).
    async fn query_task(&mut self) {
        let polled = polled::after(&mut self.controller_poll);
        for axis in &mut self.axes {
            axis.query_task(&polled);
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
    /// the controller poll, then every axis in turn, its change posted to
    /// its records before the next axis polls as `pAxis->poll` ends in
    /// callParamCallbacks, and return when the next timed pass is due.
    /// `woken` is a `wakeupPoller`, which reloads the forced fast polls.
    async fn poller_pass(&mut self, woken: bool) -> Option<Instant> {
        if woken {
            self.reload_forced_fast_polls();
        }
        let polled = polled::after(&mut self.controller_poll);
        let mut moving = false;
        for axis in &mut self.axes {
            moving |= axis.poller_poll(&polled);
            axis.send_pulses().await;
        }
        self.next_pass(moving)
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
        let Schedule::Poller {
            moving: moving_period,
            idle,
            remaining,
            ..
        } = &mut self.schedule
        else {
            return None;
        };
        let timeout = if *remaining > 0 {
            *remaining -= 1;
            *moving_period
        } else if moving {
            *moving_period
        } else {
            *idle
        };
        (!timeout.is_zero()).then(|| Instant::now() + timeout)
    }

    /// Run the poll loop on a dedicated `motorPoller` thread until Shutdown;
    /// the returned future completes when that thread ends. Needs no ambient
    /// runtime: the thread parks between polls and its timers are the
    /// background timer's, so either backend's executor can await it.
    pub async fn run(self) {
        let (done_tx, done_rx) = oneshot::channel::<()>();
        // C startPoller (asynMotorController.cpp:572-575).
        let spawned = spawn_dedicated_thread(
            "motorPoller".into(),
            ThreadPriority::Low,
            StackSizeClass::Medium,
            move || {
                let _ = block_on_sync(self.serve());
                drop(done_tx);
            },
        );
        match spawned {
            Ok(_) => {
                let _ = done_rx.await;
            }
            Err(e) => thread_create_refused(&e),
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
        for (index, axis) in axes.iter_mut().enumerate() {
            if let Poll::Ready(Some(batch)) = axis.motor_cmd_rx.poll_recv(cx) {
                return Poll::Ready(Wake::Batch(index, batch));
            }
        }
        for (index, axis) in axes.iter_mut().enumerate() {
            if let Poll::Ready(cmd) = axis.cmd_rx.poll_recv(cx) {
                return Poll::Ready(Wake::Command(index, cmd));
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

    async fn serve(mut self) {
        // The startup poll is a wakeup (asynMotorController.cpp:630).
        self.reload_forced_fast_polls();
        let moving = self
            .axes
            .iter()
            .any(|axis| axis.last_status.as_ref().is_some_and(|s| s.moving));
        let mut due = self.next_pass(moving);
        loop {
            for axis in &mut self.axes {
                axis.send_pulses().await;
            }
            // Only a poller pass moves `due`: C's motorUpdateStatus_ and
            // non-move writes run on the port thread, beside the poller's
            // running event wait.
            let wait = due.map(|at| at.saturating_duration_since(Instant::now()));
            let wake = tokio::select! {
                biased;
                wake = std::future::poll_fn(|cx| Self::inbox(&mut self.axes, &mut self.joining, cx)) => wake,
                _ = sleep_background(wait.unwrap_or_default()), if wait.is_some() => Wake::Due,
            };
            match wake {
                Wake::Batch(index, batch) if self.is_motor_task() => {
                    // One wake drains the driver's whole queue after the
                    // query (motordrvCom.cc:255, :351).
                    let tick = Instant::now();
                    self.query_task().await;
                    self.execute(index, &batch).await;
                    for axis in &mut self.axes {
                        axis.run_queued_commands(&mut self.controller_poll);
                    }
                    due = self.task_due(tick);
                }
                Wake::Batch(index, batch) => {
                    self.execute(index, &batch).await;
                    if std::mem::take(&mut self.axes[index].woken) {
                        due = self.poller_pass(true).await;
                    }
                }
                Wake::Command(index, cmd) => match cmd {
                    Some(PollCommand::ForcePoll) if self.is_motor_task() => {
                        let tick = Instant::now();
                        self.query_task().await;
                        let polled = polled::after(&mut self.controller_poll);
                        let axis = &mut self.axes[index];
                        let status = axis.poll_result(&polled);
                        axis.publish_info(status);
                        due = self.task_due(tick);
                    }
                    // C motorUpdateStatus_ (asynMotorController.cpp:217-221).
                    Some(PollCommand::ForcePoll) => {
                        let polled = polled::after(&mut self.controller_poll);
                        self.axes[index].poll_and_notify(true, &polled);
                    }
                    Some(PollCommand::Wakeup) if self.is_motor_task() => {
                        let tick = Instant::now();
                        self.query_task().await;
                        due = self.task_due(tick);
                    }
                    Some(PollCommand::Wakeup) => {
                        due = self.poller_pass(true).await;
                    }
                    Some(PollCommand::Post(status)) => self.axes[index].notify(status, false),
                    // C shutdownPoller ends the controller's one poller.
                    Some(PollCommand::Shutdown) | None => return,
                },
                Wake::Join(join) => self.join(*join),
                Wake::Due => {
                    due = self.timed_pass().await;
                }
            }
        }
    }
}

/// What woke the poller thread.
enum Wake {
    Batch(usize, CommandBatch),
    Command(usize, Option<PollCommand>),
    Join(Box<Joining>),
    Due,
}

/// C `asynMotorAxis::status_` before any poll: every bit 0 (asynMotorAxis.cpp
/// `status_.status = 0`), where [`MotorStatus::default`] has `done` set.
fn status_before_poll() -> MotorStatus {
    MotorStatus {
        done: false,
        ..MotorStatus::default()
    }
}

/// C `asynMotorController`'s parameter index for `cmd` (createParam order,
/// asynMotorController.cpp:57-129) and the value written to it.
/// The profile functions are the controller's, whose port prints their
/// write; the axis calls behind them are direct driver calls in C.
fn c_write(cmd: &MotorCommand) -> Option<(i32, WriteValue)> {
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
        MotorCommand::MoveToHome { .. } => (c::MOVE_TO_HOME, Int32(1)),
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
        | MotorCommand::RecordFields(_) => return None,
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
        MotorCommand::MoveToHome { .. } => 27,
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
        | MotorCommand::RecordFields(_) => return None,
    })
}

/// Whether drvMotorAsyn hands `reason` to the driver's `setDouble` /
/// `setInteger` (drvMotorAsyn.c:613-623, 735-745), which store it as a
/// parameter. Position is a status field, posted through it.
fn is_driver_param(reason: i32) -> bool {
    (1..100).contains(&reason)
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
        StatusWrite::PowerOn(v) => (16, bit(v)),
        StatusWrite::Problem(v) => (20, bit(v)),
        StatusWrite::Moving(v) => (21, bit(v)),
        StatusWrite::CommsError(v) => (23, bit(v)),
        StatusWrite::LowHardLimit(v) => (24, bit(v)),
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
    /// The poll's or the write's [`AsynMotor::take_poll_params`].
    poll_params: Vec<(usize, asyn_rs::param::ParamValue)>,
}

/// A failed driver poll: the bits its error label raises and the status it
/// left before bailing.
#[derive(Debug, Clone)]
struct FailedPoll {
    bits: PollFailure,
    left: Option<MotorStatus>,
}

impl FailedPoll {
    /// A poisoned driver mutex has no C counterpart; it raises both bits.
    const POISONED: Self = Self {
        bits: PollFailure::PROBLEM_AND_COMMS_ERROR,
        left: None,
    };

    fn of(motor: &mut dyn AsynMotor) -> Self {
        Self {
            bits: motor.poll_failure(),
            left: motor.take_failed_poll_status(),
        }
    }
}

/// The driver's poll, an `Err` raising what its `poll_failure` names.
fn poll_driver(motor: &mut dyn AsynMotor, user: &AsynUser) -> Result<MotorStatus, FailedPoll> {
    motor.poll(user).map_err(|_| FailedPoll::of(motor))
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
                    "devMotorAsyn::build_trans: {} motor_cmnd={motor_cmnd}, pact={}, value={value:.6}\n",
                    trace.record(),
                    u8::from(pact),
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
                msg.dvalue,
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
            msg.dvalue,
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
    execute_commands_after(motor, trace, commands, on_write, &mut || {});
}

/// [`execute_commands`] on a controller whose `controller_poll` runs ahead
/// of a GET_INFO's axis poll, as C `motorUpdateStatus_` runs `pC->poll()`
/// (asynMotorController.cpp:217-221).
fn execute_commands_after(
    motor: &mut dyn AsynMotor,
    trace: &MotorTrace,
    commands: &[MotorCommand],
    on_write: &mut dyn FnMut(AxisWrite),
    controller_poll: &mut dyn FnMut(),
) {
    let user = AsynUser::new(0);
    let port = trace.port();
    let axis = trace.addr();
    let model2 = trace.is_model2();
    let controller = trace.is_controller();
    // A motorAxisDrvSET_t driver, behind drvMotorAsyn or devMotorSim.
    let drvset = model2 || trace.is_motor_interface();
    // The C subclass write around asynMotorController's.
    let hook = controller
        .then(|| crate::devices::write_hook(port))
        .flatten();
    let params = controller
        .then(|| crate::devices::controller_port_handle(port))
        .flatten();
    let store = controller
        .then(|| crate::devices::param_store(port))
        .flatten();
    for cmd in commands {
        let c = if drvset {
            c_write(cmd).and_then(|(_, value)| Some((drv_motor_asyn_reason(cmd)?, value)))
        } else {
            c_write(cmd)
        };
        let function = c.map_or(0, |(function, _)| function);
        // asynMotorController's own flow lines; drvMotorAsyn has none of them.
        let flow = |args: std::fmt::Arguments<'_>| {
            if !model2 {
                trace.print(TraceMask::FLOW, function, args)
            }
        };
        if let (true, Some((_, WriteValue::Float64(_)))) = (model2, c) {
            trace.print(
                TraceMask::FLOW,
                function,
                format_args!(
                    "drvMotorAsyn::writeFloat64, reason={function}, pasynUser={:p} pAxis={:p}\n",
                    &user, motor as *const dyn AsynMotor as *const (),
                ),
            );
        }
        let refusal = match (&hook, c) {
            (Some(hook), Some((function, _))) => hook.check(axis, function as usize).err(),
            _ => None,
        };
        if let (Some(e), Some(hook), Some((function, value))) = (refusal, &hook, c) {
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
            if let Some(store) = &store {
                store(axis, outcome.delta);
            }
            let status = outcome.status.unwrap_or(status);
            let failure = status.as_ref().err().map(|e| e.message_display());
            dev_motor_asyn_reply(
                trace,
                cmd,
                function,
                failure.as_ref().map(|e| e as &dyn std::fmt::Display),
            );
            continue;
        }
        let mut write_status = None;
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
                    position, min_velocity, velocity, acceleration,
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
                    distance, min_velocity, velocity, acceleration,
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
                    velocity, acceleration,
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
                    min_velocity, velocity, acceleration,
                ));
                r
            }
            MotorCommand::Stop { acceleration } => motor.stop(&user, acceleration),
            MotorCommand::SetPosition { position } | MotorCommand::InitPosition { position } => {
                let r = motor.set_position(&user, position);
                flow(format_args!(
                    "asynMotorController:writeFloat64: Set driver {port}, axis {axis} to position={:.6}\n",
                    position
                ));
                r
            }
            MotorCommand::SetEncoderPosition { position } => {
                let r = motor.set_encoder_position(&user, position);
                flow(format_args!(
                    "asynMotorController:writeFloat64: Set driver {port}, axis {axis} to encoder position={:.6}\n",
                    position
                ));
                r
            }
            MotorCommand::SetEncoderRatio { .. } | MotorCommand::InitEncoderRatio { .. } => {
                let ratio = cmd.encoder_ratio().unwrap_or_default();
                let r = motor.set_encoder_ratio(&user, ratio);
                flow(format_args!(
                    "asynMotorController:writeFloat64: Set driver {port}, axis {axis} encoder ratio={ratio:.6}\n"
                ));
                r
            }
            MotorCommand::SetClosedLoop { enable } => motor.set_closed_loop(&user, enable),
            MotorCommand::DeferMoves { defer } => motor.set_deferred_moves(&user, defer),
            MotorCommand::ProfileDefine { ref positions } => motor.define_profile(&user, positions),
            MotorCommand::ProfileBuild => motor.build_profile(&user),
            MotorCommand::ProfileExecute => motor.execute_profile(&user),
            MotorCommand::ProfileAbort => motor.abort_profile(&user),
            MotorCommand::ProfileReadback => motor.readback_profile(&user).map(|_| ()),
            MotorCommand::MoveToHome {
                position,
                min_velocity,
                velocity,
                acceleration,
            } => {
                flow(format_args!(
                    "asynMotorController:writeInt32:: Starting a move to home for axis {axis}\n"
                ));
                // C hands the move to the moveToHome thread and writeInt32
                // returns success; the thread reports a failure on its own.
                write_status = Some(0);
                motor.move_to_home(&user, position, min_velocity, velocity, acceleration)
            }
            MotorCommand::SetPidGain { kind, gain } => {
                let r = motor.set_pid_gain(&user, kind, gain);
                let what = match kind {
                    PidGainKind::Proportional => "proportional",
                    PidGainKind::Integral => "integral",
                    PidGainKind::Derivative => "derivative",
                };
                flow(format_args!(
                    "asynMotorController:writeFloat64: Set driver {port}, axis {axis} {what} gain={gain:.6}\n"
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
                    position
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
                controller_poll();
                let r = motor.poll(&user);
                polled = Some(match &r {
                    Ok(status) => Ok(status.clone()),
                    Err(_) => Err(FailedPoll::of(&mut *motor)),
                });
                // drvMotorAsyn returns forceCallback's status, not the poll's.
                r.map(|_| ())
                    .or_else(|e| if controller { Err(e) } else { Ok(()) })
            }
            // The base only stores them; a model-2 axis's store is the
            // poller's.
            MotorCommand::SetVelocity { .. }
            | MotorCommand::SetVelBase { .. }
            | MotorCommand::SetAccel { .. } => Ok(()),
        };
        if let (Some(params), Some((function, value))) = (&params, c) {
            // The base sets the parameter whatever the driver returns
            // (asynMotorController.cpp:204, :289); its callParamCallbacks
            // closes the write.
            let value = match value {
                WriteValue::Float64(v) => asyn_rs::param::ParamValue::Float64(v),
                WriteValue::Int32(v) => asyn_rs::param::ParamValue::Int32(v),
            };
            let set = asyn_rs::request::ParamSetValue::new(function as usize, axis, value);
            let _ = params.set_params_and_notify_blocking(axis, vec![set]);
        }
        let poll_params = motor.take_poll_params();
        let position = match *cmd {
            MotorCommand::SetPosition { position } | MotorCommand::InitPosition { position } => {
                Some(StatusWrite::Position(position))
            }
            MotorCommand::SetEncoderPosition { position } => {
                Some(StatusWrite::EncoderPosition(position))
            }
            _ => None,
        };
        let moved = matches!(
            cmd,
            MotorCommand::MoveAbsolute { .. }
                | MotorCommand::MoveRelative { .. }
                | MotorCommand::MoveVelocity { .. }
                | MotorCommand::JogVelocity { .. }
                | MotorCommand::Home { .. }
        )
        .then_some(StatusWrite::Done(false));
        let mut writes = Vec::new();
        if controller {
            writes.extend(position);
            writes.extend(motor.take_status_writes());
            writes.extend(moved);
        } else if drvset {
            writes.extend(motor.take_status_writes());
            if result.is_ok() {
                writes.extend(position.or(moved));
            }
        } else {
            // Model 1: motordrvCom `process_messages` sends a MOTION or
            // VELOCITY message with no callback (motordrvCom.cc:396-438);
            // only `query_axis` on the held move calls the record back.
            let _ = motor.take_status_writes();
        }
        // A model-2 driver stores a parameter only once the write succeeded
        // (drvMotorSim.c:364-368, 424-428).
        let param = c.filter(|&(reason, _)| drvset && result.is_ok() && is_driver_param(reason));
        let params = match drvset {
            true => writes
                .iter()
                .map(|&w| model2_param(w))
                .chain(param)
                .collect(),
            false => Vec::new(),
        };
        on_write(AxisWrite {
            fields: writes,
            params,
            polled,
            poll_params,
        });

        if let (true, Some((_, value))) = (model2, c) {
            // drvMotorAsyn prints the write whatever its status.
            match value {
                WriteValue::Float64(v) => trace.print(
                    TraceMask::IO_DRIVER,
                    function,
                    format_args!("drvMotorAsyn::writeFloat64, reason={function}, value={v:.6}\n"),
                ),
                WriteValue::Int32(v) => trace.print(
                    TraceMask::IO_DRIVER,
                    function,
                    format_args!("drvMotorAsyn::writeInt32, reason={function}, value={v}\n"),
                ),
            }
        } else if let Some((_, value)) = c {
            let status = write_status.unwrap_or_else(|| match &result {
                Ok(()) => 0,
                Err(e) => e.c_status_code(),
            });
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
                WriteValue::Float64(v) => print("writeFloat64", format_args!("{v:.6}")),
                WriteValue::Int32(v) => print("writeInt32", format_args!("{v}")),
            }
        }

        // What the subclass's write returns instead of the base's status.
        let mut returned = None;
        if let (Some(hook), Some((function, value))) = (&hook, c) {
            // MoveToHome's write returned success; its thread reports.
            let ok = Ok(());
            let status = if write_status == Some(0) {
                &ok
            } else {
                &result
            };
            let outcome = hook.after_write(&FinishedWrite::new(
                axis,
                function as usize,
                value,
                status,
                &|mask, args| trace.print(mask, function, args),
            ));
            if let Some(store) = &store {
                store(axis, outcome.delta);
            }
            returned = outcome.status;
        }

        if let (Err(_), MotorCommand::MoveToHome { .. }) = (&result, cmd) {
            if !model2 {
                trace.print_self(
                    TraceMask::ERROR,
                    format_args!(
                        "asynMotorController:asynMotorMoveToHome: move to home failed in asynMotorController::asynMotorMoveToHome. Axis number={axis}\n"
                    ),
                );
            }
            continue;
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
            format_args!("devMotorAsyn::init_controller, {record} set {what} to {value:.6}\n"),
        ),
        Some(_) => trace.print(
            TraceMask::ERROR,
            reason,
            format_args!(
                "devMotorAsyn::init_controller, {record} failed to set {what} to {value:.6}\n"
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use asyn_rs::error::{AsynError, AsynResult, AsynStatus};

    use super::*;

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
            self.writes.extend(self.updated.iter().copied());
            Ok(MotorStatus {
                done: true,
                ..MotorStatus::default()
            })
        }
        fn take_status_writes(&mut self) -> Vec<StatusWrite> {
            std::mem::take(&mut self.writes)
        }
    }

    /// A started poller on axis 0 of model-2 port `port`, the axis's
    /// record half and its parameter library.
    fn model2_sim(
        port: &str,
        created: Vec<StatusWrite>,
        updated: Vec<StatusWrite>,
    ) -> (
        MotorPollLoop,
        crate::device_support::MotorAxis,
        Model2Params,
    ) {
        crate::devices::register_drv_motor_asyn_port(port, 1).unwrap();
        let mut controller = crate::builder::ControllerBuilder::new();
        let sim = Sim {
            writes: created,
            updated,
        };
        let user = TraceUser::Model2 {
            port: port.into(),
            addr: 0,
        };
        let axis = controller.axis(Arc::new(Mutex::new(sim)), user).axis;
        let record_half = axis.at(axis.address().clone());
        crate::devices::register_axis(axis).unwrap();
        let poll_loop = controller.build(SCAN, SCAN, 0);
        let params = crate::devices::model2_params(port, 0).unwrap();
        (poll_loop, record_half, params)
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

    /// Answers each poll from the script in turn; `None` is `set_status`
    /// returning early (RETRY).
    struct Script(std::collections::VecDeque<Option<MotorStatus>>);

    type Held = Option<super::Held>;

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
        fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
            self.0.pop_front().flatten().ok_or(AsynError::Status {
                status: AsynStatus::Error,
                message: "retry".into(),
            })
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
            let moving = self.moving.load(std::sync::atomic::Ordering::SeqCst);
            Ok(MotorStatus {
                moving,
                done: !moving,
                ..MotorStatus::default()
            })
        }
        fn take_poll_params(&mut self) -> Vec<(usize, asyn_rs::param::ParamValue)> {
            let limit = *self.limit.lock().unwrap();
            vec![(
                crate::controller_port::index::HIGH_LIMIT,
                asyn_rs::param::ParamValue::Float64(limit),
            )]
        }
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
        let batch = CommandBatch {
            trace,
            record: None,
            commands: vec![MotorCommand::Poll],
        };
        now(poll_loop.execute(0, &batch));
        assert_eq!(read(), 5.0, "GET_INFO");
        let write = |command| CommandBatch {
            trace: Arc::clone(&batch.trace),
            record: None,
            commands: vec![command],
        };
        *limit.lock().unwrap() = 6.0;
        now(poll_loop.execute(0, &write(MotorCommand::Stop { acceleration: 1.0 })));
        assert_eq!(read(), 6.0, "write leaving the status");
        *limit.lock().unwrap() = 7.0;
        let move_to = MotorCommand::MoveAbsolute {
            position: 1.0,
            min_velocity: 0.0,
            velocity: 1.0,
            acceleration: 1.0,
        };
        now(poll_loop.execute(0, &write(move_to)));
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
        fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
            let moving = self.moving.load(std::sync::atomic::Ordering::SeqCst);
            Ok(MotorStatus {
                moving,
                done: !moving,
                ..MotorStatus::default()
            })
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
            Self {
                poll_loop: controller.build(SCAN, SCAN, 0),
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
                &CommandBatch {
                    trace,
                    record: None,
                    commands: vec![MotorCommand::MoveAbsolute {
                        position: 5.0,
                        min_velocity: 0.0,
                        velocity: 1.0,
                        acceleration: 1.0,
                    }],
                },
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
        poll_loop.axes[0].mirror_to(port, 0);
        (poll_loop, handle, gate)
    }

    fn write_stop(poll_loop: &mut MotorPollLoop, port: &str) {
        now(poll_loop.execute(
            0,
            &CommandBatch {
                trace: Arc::new(MotorTrace::new(port, 0)),
                record: None,
                commands: vec![MotorCommand::Stop { acceleration: 1.0 }],
            },
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
        fn poll(&mut self, u: &AsynUser) -> AsynResult<MotorStatus> {
            self.0.poll(u)
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
        poll_loop.axes[0].mirror_to(port, 0);
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

    /// Axis 0's COMMS_ERROR callbacks on controller port `port` from the
    /// moment `gate` opened and the port's one-shot flush ran. That flush is
    /// on its own thread, as C's `callbackThread::run` (asynPortDriver.cpp:
    /// 923-937), so it would post whatever was set by the time it got there.
    fn comms_posts(
        port: &str,
        handle: &asyn_rs::port_handle::PortHandle,
        gate: &Gate,
    ) -> (
        asyn_rs::interrupt::InterruptSubscription,
        asyn_rs::interrupt::InterruptReceiver,
    ) {
        use crate::controller_port::index::STATUS_COMMS_ERROR;
        // A marker only that flush can post.
        let mut marker = crate::controller_port::ParamDelta::default();
        marker.set_int32(STATUS_COMMS_ERROR, 0);
        crate::devices::param_store(port).unwrap()(0, marker);
        let (sub, mut rx) = handle.interrupts().register_interrupt_user(
            asyn_rs::interrupt::InterruptFilter {
                reason: Some(STATUS_COMMS_ERROR),
                addr: Some(0),
                ..Default::default()
            },
            0,
        );
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
        let (_sub, mut rx) = comms_posts("PLHOOKPASS", &handle, &gate);
        write_stop(&mut poll_loop, "PLHOOKPASS");
        assert_eq!(posted(&handle, &mut rx), None, "set, not yet called back");
        assert_eq!(comms(&handle), Some(1), "set in the parameter library");
        now(poll_loop.poller_pass(false));
        assert_eq!(posted(&handle, &mut rx), Some(1));
    }

    #[test]
    fn a_hook_set_rides_the_next_write_callback() {
        let (mut poll_loop, handle, gate) =
            hooked_poller("PLHOOKWRITE", vec![MotorStatus::default()]);
        let (_sub, mut rx) = comms_posts("PLHOOKWRITE", &handle, &gate);
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
            SCAN,
            poll_tx,
            motor_tx,
            AxisSubscribers::new(),
        ))
        .unwrap();
        let (_sub, mut rx) = comms_posts("PLHOOKPORT", &handle, &gate);
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
        // The constructor's startup poll answers first.
        let script = std::iter::once(Some(MotorStatus::default())).chain(script);
        let script = Arc::new(Mutex::new(Script(script.collect())));
        (model1_task_on(script.clone()), script)
    }

    fn model1_task_on(motor: Arc<Mutex<dyn AsynMotor>>) -> MotorPollLoop {
        let (_poll_tx, poll) = mpsc::channel(1);
        let (_tx, commands) = mpsc::unbounded_channel();
        let axis = Axis::task(
            PollerInbox { poll, commands },
            motor,
            AxisSubscribers::new(),
        );
        let mut poll_loop = MotorPollLoop::motor_task(vec![axis], SCAN);
        poll_loop.axes[0].last_status = Some(MotorStatus::default());
        poll_loop
    }

    /// Answers each poll from its list; an `Err` fails the poll raising
    /// those bits, having left that status.
    struct Failing {
        polls: std::collections::VecDeque<Result<MotorStatus, (PollFailure, Option<MotorStatus>)>>,
        failure: PollFailure,
        left: Option<MotorStatus>,
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
        fn take_failed_poll_status(&mut self) -> Option<MotorStatus> {
            self.left.take()
        }
        fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
            match self.polls.pop_front().unwrap() {
                Ok(status) => Ok(status),
                Err((failure, left)) => {
                    self.failure = failure;
                    self.left = left;
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
            left: None,
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
        let seq = poll_loop.axes[0].status_seq;
        now(poll_loop.poller_pass(false));
        assert_eq!(
            poll_loop.axes[0].status_seq, seq,
            "the early return posts nothing"
        );
        now(poll_loop.poller_pass(false));
        assert_eq!(poll_loop.axes[0].status_seq, seq + 1);
        assert_eq!(poll_loop.axes[0].last_status, Some(fresh));
    }

    #[test]
    fn a_left_status_equal_to_the_last_sets_nothing_to_post() {
        let mut poll_loop = failing_poller(vec![
            Err((PollFailure::NONE, Some(MotorStatus::default()))),
            Ok(MotorStatus::default()),
        ]);
        let seq = poll_loop.axes[0].status_seq;
        now(poll_loop.poller_pass(false));
        now(poll_loop.poller_pass(false));
        assert_eq!(poll_loop.axes[0].status_seq, seq);
    }

    #[test]
    fn a_failure_raising_bits_posts_them_over_what_it_left() {
        let fresh = MotorStatus {
            home: true,
            ..MotorStatus::default()
        };
        let mut poll_loop =
            failing_poller(vec![Err((PollFailure::COMMS_ERROR, Some(fresh.clone())))]);
        let seq = poll_loop.axes[0].status_seq;
        now(poll_loop.poller_pass(false));
        assert_eq!(poll_loop.axes[0].status_seq, seq + 1);
        let posted = poll_loop.axes[0].last_status.clone().unwrap();
        assert!(posted.home && posted.comms_error && !posted.problem);
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
            Ok(MotorStatus::default()),
            Err((PollFailure::NONE, Some(fresh.clone()))),
            Err((PollFailure::PROBLEM_AND_COMMS_ERROR, None)),
        ];
        let mut poll_loop = model1_task_on(Arc::new(Mutex::new(Failing {
            polls: polls.into(),
            failure: PollFailure::NONE,
            left: None,
        })));
        send_move(&mut poll_loop);
        let seq = poll_loop.axes[0].status_seq;
        now(poll_loop.timed_pass());
        assert_eq!(poll_loop.axes[0].status_seq, seq, "RETRY posts nothing");
        now(poll_loop.timed_pass());
        assert_eq!(poll_loop.axes[0].status_seq, seq + 1);
        let posted = poll_loop.axes[0].last_status.clone().unwrap();
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
            &CommandBatch {
                trace,
                record: None,
                commands: vec![MotorCommand::MoveAbsolute {
                    position: 5.0,
                    min_velocity: 0.0,
                    velocity: 1.0,
                    acceleration: 1.0,
                }],
            },
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
                let seq = poll_loop.axes[0].status_seq;
                now(poll_loop.timed_pass());
                (poll_loop.axes[0].status_seq != seq)
                    .then(|| poll_loop.axes[0].last_status.clone().unwrap())
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
        fn poll(&mut self, user: &AsynUser) -> AsynResult<MotorStatus> {
            self.0.poll(user)
        }
        fn no_motion_test(&self) -> bool {
            false
        }
    }

    // drvEMC18011.cc:356-358: with the test off every pass is called back.
    #[test]
    fn a_driver_without_the_no_motion_test_calls_back_a_still_pass() {
        let script = [MotorStatus::default(), at(1.0), at(1.0), at(2.0)];
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
        assert!(!poll_loop.axes[0].last_status.as_ref().unwrap().done);
        poll_loop.axes[0].pacing = Pacing::MotorTask {
            held: None,
            no_motion_test: true,
        };
        poll_loop.axes[0].publish_info(Ok(at(1.0)));
        assert!(poll_loop.axes[0].last_status.as_ref().unwrap().done);
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
        poll_loop.axes[0].subscribers.attach(
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
        let seq = poll_loop.axes[0].status_seq;
        poll_loop.axes[0].notify(MotorStatus::default(), false);
        assert_eq!(poll_loop.axes[0].status_seq, seq, "unchanged: no publish");
        poll_loop.axes[0].notify(at(1.0), false);
        assert_eq!(poll_loop.axes[0].status_seq, seq + 1);
        assert_eq!(poll_loop.axes[0].last_status, Some(at(1.0)));
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
        MotorPollLoop::new(
            PollerInbox { poll, commands },
            motor,
            AxisSubscribers::new(),
            MOVING_PERIOD,
            IDLE_PERIOD,
            0,
        )
    }

    const MOVING_PERIOD: Duration = Duration::from_millis(100);
    const IDLE_PERIOD: Duration = Duration::from_secs(60);

    /// The period of one poller pass over `status`, and the MSTA it posts.
    fn pace_and_msta(status: MotorStatus) -> (Duration, MstaFlags) {
        let mut poll_loop = poller(vec![status]);
        let due = now(poll_loop.poller_pass(false)).unwrap();
        let period = due.saturating_duration_since(Instant::now());
        let msta = MstaFlags::from_status(poll_loop.axes[0].last_status.as_ref().unwrap());
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
        let setup = crate::builder::MotorBuilder::new(motor)
            .trace_user(interface_user())
            .poll_interval(SCAN)
            .build_axis();
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
            &interface_batch(vec![MotorCommand::MoveAbsolute {
                position: 9.0,
                min_velocity: 0.0,
                velocity: 1.0,
                acceleration: 1.0,
            }]),
        ));
        for (want, word) in [(hit, 1029), (back, 1028)] {
            assert!(now(poll_loop.timed_pass()).is_some());
            assert_eq!(poll_loop.axes[0].last_status.as_ref(), Some(&want));
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
        let seq = poll_loop.axes[0].status_seq;
        let tick = Instant::now();
        assert!(now(poll_loop.timed_pass()).is_some_and(|due| due >= tick + SCAN));
        assert_eq!(poll_loop.axes[0].status_seq, seq, "unchanged: no post");
        assert!(now(poll_loop.timed_pass()).is_some());
        assert_eq!(poll_loop.axes[0].status_seq, seq + 1);
        assert_eq!(poll_loop.axes[0].last_status, Some(nudged));
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
        let seq = poll_loop.axes[0].status_seq;
        now(poll_loop.execute(
            0,
            &interface_batch(vec![MotorCommand::Home {
                forward: false,
                min_velocity: 0.0,
                velocity: 1.0,
                acceleration: 1.0,
            }]),
        ));
        assert_eq!(poll_loop.axes[0].status_seq, seq + 1);
        assert_eq!(
            poll_loop.axes[0].last_status,
            Some(MotorStatus {
                done: false,
                ..parked
            })
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
        fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
            self.0.push("poll");
            Ok(MotorStatus::default())
        }
    }

    /// drvMotorAsyn's GET_INFO calls the driver's forceCallback
    /// (drvMotorAsyn.c:617-619); asynMotorController's only polls
    /// (asynMotorController.cpp:217-221).
    #[test]
    fn a_model2_get_info_calls_force_callback_before_the_poll() {
        for (user, want) in [
            (
                TraceUser::Model2 {
                    port: "PLM2FORCE".into(),
                    addr: 0,
                },
                &["force_callback", "poll"][..],
            ),
            (
                TraceUser::Asyn {
                    port: "PLM3FORCE".into(),
                    addr: 0,
                },
                &["poll"][..],
            ),
        ] {
            let mut motor = ForceCallbackOrder(Vec::new());
            let trace = MotorTrace::with_user(user);
            execute_commands(&mut motor, &trace, &[MotorCommand::Poll], &mut |_| {});
            assert_eq!(motor.0, want);
        }
    }

    /// GET_INFO is `needUpdate = 1` on the status last called back: no
    /// driver call and no process.
    #[test]
    fn a_get_info_marks_need_update_and_neither_polls_nor_processes() {
        let rest = MotorStatus::default();
        let (mut poll_loop, script, record) = interface_task(rest.clone(), vec![Some(at(9.0))]);
        let state = record.device_state().clone();
        state.lock().unwrap().init_status();
        let seq = poll_loop.axes[0].status_seq;
        now(poll_loop.execute(0, &interface_batch(vec![MotorCommand::Poll])));
        assert_eq!(
            script.lock().unwrap().0.len(),
            1,
            "the driver was not polled"
        );
        assert_eq!(poll_loop.axes[0].status_seq, seq);
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
