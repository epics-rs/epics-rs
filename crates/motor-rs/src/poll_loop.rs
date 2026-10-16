use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, StatusWrite};
use asyn_rs::trace::TraceMask;
use asyn_rs::user::AsynUser;
use epics_base_rs::runtime::task::{
    StackSizeClass, ThreadPriority, block_on_sync, sleep_background, spawn_dedicated_thread,
};
use tokio::sync::{mpsc, oneshot};

use crate::device_state::*;
use crate::flags::{MotorCommand, PidGainKind};
use crate::trace::MotorTrace;

/// Queue of driver commands from the device support to the poller thread.
///
/// Unbounded like the asyn request queue C `devMotorAsyn::build_trans` feeds,
/// so a record pass never waits on the driver.
pub type MotorCommandSender = mpsc::UnboundedSender<CommandBatch>;

/// One record pass's commands, with the record's trace: C's asynUser that
/// `build_trans` queued them on, which `asynCallback` prints through and
/// whose record a finished move processes.
#[derive(Debug)]
pub struct CommandBatch {
    pub trace: Arc<MotorTrace>,
    pub record: RecordId,
    pub commands: Vec<QueuedCommand>,
}

/// A driver command with the MRES of the record pass that queued it. The
/// driver takes dial EGU; C's asyn layer carries steps, the command over MRES,
/// so the trace lines read that.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedCommand {
    pub command: MotorCommand,
    pub mres: f64,
}

/// Commands sent to the poll loop. The axis polls while any bound record
/// wants it, so one record settling cannot starve another of posts.
#[derive(Debug)]
pub enum PollCommand {
    /// Poll now, posting even when nothing changed: C `motorUpdateStatus_`
    /// (asynMotorController.cpp:217-222). The timed polls never stop: C's
    /// `asynMotorPoller` has no request that halts them (:615-696).
    ForcePoll,
    Shutdown,
}

/// The poller's two queues: poll control, and the driver commands a record
/// pass queued (drained first, so a ForcePoll sees their effect).
pub struct PollerInbox {
    pub poll: mpsc::Receiver<PollCommand>,
    pub commands: mpsc::UnboundedReceiver<CommandBatch>,
}

/// Motor poll loop — one per axis, posting to every record bound to it.
///
/// It is the only caller of the driver after `init`: polls and the record's
/// commands both run on its own `motorPoller` OS thread (see [`Self::run`]),
/// as C runs `asynMotorPoller` on an epicsThread, so a driver blocked on a
/// dead serial line or TCP peer never stalls the IOC's async executor.
pub struct MotorPollLoop {
    cmd_rx: mpsc::Receiver<PollCommand>,
    motor_cmd_rx: mpsc::UnboundedReceiver<CommandBatch>,
    motor: Arc<Mutex<dyn AsynMotor>>,
    subscribers: AxisSubscribers,
    moving_poll_interval: Duration,
    idle_poll_interval: Duration,
    forced_fast_polls_config: u32,
    forced_fast_polls_remaining: u32,
    last_moving: bool,
    status_seq: u64,
    /// Last status delivered to the record. An autonomous (timed) idle poll
    /// notifies the record only when the freshly polled status differs from
    /// this — the analogue of C `asynMotorAxis::callParamCallbacks` firing the
    /// status callback only when `statusChanged_` is set (asynMotorAxis.cpp:
    /// 316-322), so a stationary axis does not re-post DIFF/RDIF every idle
    /// period. A forced poll (ForcePoll, i.e. request_poll/status_refresh)
    /// bypasses this and always notifies.
    last_status: Option<MotorStatus>,
    /// A model-2 driver's parameter library, by drvMotorAsyn reason: what
    /// the axis's `setDouble` / `setInteger` stored last.
    params: std::collections::HashMap<i32, CValue>,
    /// The io_intr pulses owed for processes the callbacks queued.
    pulses: Pulses,
    /// A move ran: C `wakeupPoller`.
    woken: bool,
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

impl MotorPollLoop {
    pub fn new(
        inbox: PollerInbox,
        motor: Arc<Mutex<dyn AsynMotor>>,
        subscribers: AxisSubscribers,
        moving_poll_interval: Duration,
        idle_poll_interval: Duration,
        forced_fast_polls: u32,
    ) -> Self {
        Self {
            cmd_rx: inbox.poll,
            motor_cmd_rx: inbox.commands,
            motor,
            subscribers,
            moving_poll_interval,
            idle_poll_interval,
            forced_fast_polls_config: forced_fast_polls,
            forced_fast_polls_remaining: 0,
            last_moving: false,
            status_seq: 1, // starts at 1 (init already wrote seq=1)
            last_status: None,
            params: Default::default(),
            pulses: Pulses::new(),
            woken: false,
        }
    }

    /// Run the commands queued so far on the calling thread: for a harness
    /// standing in for the poller thread. The IOC path is [`Self::run`].
    pub fn run_queued_commands(&mut self) {
        while let Ok(batch) = self.motor_cmd_rx.try_recv() {
            self.execute(&batch);
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
    fn execute(&mut self, batch: &CommandBatch) {
        let motor = self.motor.clone();
        let Ok(mut motor) = motor.lock() else {
            return;
        };
        for queued in &batch.commands {
            let mut write = AxisWrite::default();
            execute_commands(
                &mut *motor,
                &batch.trace,
                std::slice::from_ref(queued),
                &mut |w| write = w,
            );
            self.post_writes(write);
            let command = &queued.command;
            if !command.is_move() {
                continue;
            }
            self.pulses
                .extend(self.subscribers.move_done(batch.record, command));
            if !matches!(command, MotorCommand::SetPosition { .. }) {
                self.woken = true;
            }
        }
    }

    /// C `asynMotorAxis::callParamCallbacks` after a write (asynMotorAxis.cpp:
    /// 316-322): `write.fields` land on the last posted status, which is
    /// posted only if they changed it (`statusChanged_`). A model-2 parameter
    /// posts the status when it changed the parameter: paramLib flags only a
    /// new type or value (paramLib.c:112-118, 141-147), and `callCallback`
    /// then posts to every record on the axis (drvMotorAsyn.c `intCallback`).
    fn post_writes(&mut self, write: AxisWrite) {
        let param_changed = write
            .param
            .is_some_and(|(reason, value)| self.params.insert(reason, value) != Some(value));
        if write.fields.is_empty() && !param_changed {
            return;
        }
        let mut status = self.last_status.clone().unwrap_or_default();
        for field in write.fields {
            field.apply(&mut status);
        }
        if param_changed || self.last_status.as_ref() != Some(&status) {
            self.publish(status);
        }
    }

    /// Post `status` to every bound record's `statusCallback` under a new
    /// sequence number.
    fn publish(&mut self, status: MotorStatus) {
        self.last_status = Some(status.clone());
        self.status_seq += 1;
        let pulses = self.subscribers.post(StampedStatus {
            seq: self.status_seq,
            status,
        });
        self.pulses.extend(pulses);
    }

    /// Poll the motor and write stamped status to shared state.
    ///
    /// `force` distinguishes the two C poll paths. A forced poll (the analogue
    /// of C `motorUpdateStatus_` forcing `statusChanged_=1`, asynMotorController
    /// .cpp:217-222) always bumps the sequence and pulses io_intr, so the
    /// record re-processes — this is how request_poll / status_refresh (STUP,
    /// implicit GET_INFO, settle-resume, startup) clear STUP=BUSY and refresh
    /// readbacks even on a stationary axis. An autonomous (timed) poll passes
    /// `force=false` and notifies only when the polled status differs from the
    /// last one delivered — the analogue of C's `statusChanged_` gate, so a
    /// settled, unchanging axis does not re-post DIFF/RDIF every idle period.
    /// The status a *failed* poll posts.
    ///
    /// C parity: a driver's `poll()` failure path sets `motorStatusProblem_`
    /// and `motorStatusCommsError_` and still calls `callParamCallbacks()`
    /// before returning the error — e.g. `smarActMCSMotorDriver.cpp:503-507`,
    /// `XPSAxis.cpp:756`. The asyn parameter library keeps the previous value
    /// of every field the failed poll never wrote, so the posted status is the
    /// last known one with exactly those two flags forced on. That is what
    /// this reproduces: `MotorStatus::default()` only stands in for the very
    /// first poll, before any status has been seen.
    ///
    /// `asynMotorController` never acts on the returned status itself — the
    /// forced path captures it and discards it while forcing
    /// `statusChanged_ = 1` (asynMotorController.cpp:219-221), and the
    /// background poller ignores it outright (:658). Failure reaches the
    /// record purely as MSTA bit 12 (`COMM_ERR`), which `alarm_sub` turns
    /// into a COMM/INVALID alarm (motorRecord.cc:3392-3398).
    fn comms_error_status(&self) -> MotorStatus {
        let mut status = self.last_status.clone().unwrap_or_default();
        status.comms_error = true;
        status.problem = true;
        status
    }

    /// C asynMotorPoller's "Force on poll at startup": fills the cache
    /// `MotorDeviceSupport::init` reads, as readGenericPointer reads the
    /// parameter library. No record is woken.
    /// [`Self::run`] does this first; a harness calls it before init.
    pub fn prime_cache(&mut self) {
        let status = self.poll_status();
        self.last_status = Some(status.clone());
        self.subscribers.store(StampedStatus { seq: 1, status });
    }

    fn poll_status(&mut self) -> MotorStatus {
        let user = AsynUser::new(0);
        // A transport failure — or a poisoned driver mutex, which leaves the
        // motor just as unreachable — is the Rust spelling of the C driver
        // poll that bails to its `commsError` label. It must still produce a
        // status post, or MSTA never raises COMM_ERR, the record never
        // processes, and a `STUP=BUSY` refresh latches forever waiting for a
        // sequence that will not come (record/status_update.rs:48-58).
        // C's poller ignores the poll status (asynMotorController.cpp:658),
        // so a failure prints nothing.
        let polled = match self.motor.lock() {
            Ok(mut motor) => motor.poll(&user).ok(),
            Err(_) => None,
        };
        match polled {
            Some(s) => s,
            None => self.comms_error_status(),
        }
    }

    fn poll_and_notify(&mut self, force: bool) {
        let status = self.poll_status();
        // The poll rate decision (moving vs idle) always tracks the latest
        // poll, independent of whether the record is notified. On a failed
        // poll this carries the last known `moving` forward: C's
        // `pAxis->poll(&moving)` leaves `*moving` untouched when the driver
        // bails early, so the poller keeps the rate it was already using —
        // reading the stale value is a C quirk, but "keep the last known
        // rate" is its deterministic meaning.
        self.last_moving = status.moving;
        if !force && self.last_status.as_ref() == Some(&status) {
            // Autonomous idle poll, status unchanged → no record pass (C posts
            // nothing when statusChanged_ stays 0). A *repeated* comms failure
            // lands here too, and correctly so: the COMM_ERR flag is already
            // set, no parameter changed, so C's `statusChanged_` stays 0 and
            // `asynMotorAxis::callParamCallbacks` posts nothing
            // (asynMotorAxis.cpp:316-322). The first failure — the one that
            // flips the flag — always differs from the last status and posts.
            return;
        }
        self.publish(status);
    }

    /// A record's poll request (request_poll / status_refresh): fast polls
    /// restart and the post is forced.
    fn start_polling(&mut self) {
        self.forced_fast_polls_remaining = self.forced_fast_polls_config;
        self.poll_and_notify(true);
    }

    fn effective_poll_interval(&mut self) -> Duration {
        if self.forced_fast_polls_remaining > 0 {
            self.forced_fast_polls_remaining -= 1;
            self.moving_poll_interval
        } else if self.last_moving {
            self.moving_poll_interval
        } else {
            self.idle_poll_interval
        }
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

    async fn serve(mut self) {
        self.prime_cache();
        loop {
            self.send_pulses().await;
            // C asynMotorController.cpp:633-634: idlePollPeriod_ == 0 blocks
            // on the event (no timed poll), so a zero interval disables the
            // timed arm rather than spinning on sleep(0).
            let interval = self.effective_poll_interval();
            tokio::select! {
                biased;
                Some(batch) = self.motor_cmd_rx.recv() => {
                    self.execute(&batch);
                    if std::mem::take(&mut self.woken) {
                        // C's poller wakes from its event: fast polls
                        // restart and it polls at once, posting on change.
                        self.forced_fast_polls_remaining = self.forced_fast_polls_config;
                        self.poll_and_notify(false);
                    }
                }
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(PollCommand::ForcePoll) => self.start_polling(),
                    Some(PollCommand::Shutdown) | None => return,
                },
                _ = sleep_background(interval), if !interval.is_zero() => {
                    // Autonomous timed poll: notify only on a real change
                    // (C statusChanged_ gate).
                    self.poll_and_notify(false);
                }
            }
        }
    }
}

/// The value C `asynMotorController::writeFloat64` / `writeInt32` receives.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CValue {
    Float64(f64),
    Int32(i32),
}

/// C `asynMotorController`'s parameter index for `cmd` (createParam order,
/// asynMotorController.cpp:57-129) and the value written to it.
/// `ProfileInitialize` is a direct driver call in C, no parameter.
fn c_write(cmd: &MotorCommand, mres: f64) -> Option<(i32, CValue)> {
    use CValue::{Float64, Int32};
    Some(match *cmd {
        MotorCommand::MoveRelative { distance, .. } => (0, Float64(distance / mres)),
        MotorCommand::MoveAbsolute { position, .. } => (1, Float64(position / mres)),
        MotorCommand::MoveVelocity {
            direction,
            velocity,
            ..
        } => (
            2,
            Float64(if direction { velocity } else { -velocity } / mres),
        ),
        MotorCommand::Home { forward, .. } => (3, Float64(f64::from(u8::from(forward)))),
        // devMotorAsyn's STOP_AXIS leaves `ivalue` at 0.
        MotorCommand::Stop { .. } => (4, Int32(0)),
        MotorCommand::SetPosition { position } | MotorCommand::InitPosition { position } => {
            (9, Float64(position / mres))
        }
        MotorCommand::DeferMoves { defer } => (11, Int32(defer.into())),
        MotorCommand::SetEncoderRatio { ratio } | MotorCommand::InitEncoderRatio { ratio } => {
            (14, Float64(ratio))
        }
        MotorCommand::MoveToHome { .. } => (12, Int32(1)),
        MotorCommand::SetPidGain { kind, gain } => match kind {
            PidGainKind::Proportional => (15, Float64(gain)),
            PidGainKind::Integral => (16, Float64(gain)),
            PidGainKind::Derivative => (17, Float64(gain)),
        },
        // set_dial_highlimit / set_dial_lowlimit swap the pair for a
        // negative MRES (motorRecord.cc:4245-4249, 4296-4300).
        MotorCommand::SetHighLimit { position } => {
            (if mres < 0.0 { 19 } else { 18 }, Float64(position / mres))
        }
        MotorCommand::SetLowLimit { position } => {
            (if mres < 0.0 { 18 } else { 19 }, Float64(position / mres))
        }
        MotorCommand::SetClosedLoop { enable } => (20, Int32(enable.into())),
        MotorCommand::Poll => (27, Int32(0)),
        MotorCommand::ProfileBuild => (59, Int32(1)),
        MotorCommand::ProfileExecute => (63, Int32(1)),
        MotorCommand::ProfileAbort => (67, Int32(1)),
        MotorCommand::ProfileReadback => (68, Int32(1)),
        MotorCommand::ProfileInitialize { .. } => return None,
    })
}

/// drvMotorAsyn's `motorCommand` for `cmd` (drvMotorAsyn.c:76-104), the
/// reason a model-2 write carries. Profiles have no drvMotorAsyn command.
fn drv_motor_asyn_reason(cmd: &MotorCommand, mres: f64) -> Option<i32> {
    Some(match *cmd {
        MotorCommand::SetPosition { .. } | MotorCommand::InitPosition { .. } => 0,
        MotorCommand::SetEncoderRatio { .. } | MotorCommand::InitEncoderRatio { .. } => 2,
        // set_dial_highlimit / set_dial_lowlimit swap the pair for a
        // negative MRES (motorRecord.cc:4245-4249, 4296-4300).
        MotorCommand::SetLowLimit { .. } => {
            if mres < 0.0 {
                4
            } else {
                3
            }
        }
        MotorCommand::SetHighLimit { .. } => {
            if mres < 0.0 {
                3
            } else {
                4
            }
        }
        MotorCommand::SetPidGain { kind, .. } => match kind {
            PidGainKind::Proportional => 5,
            PidGainKind::Integral => 6,
            PidGainKind::Derivative => 7,
        },
        MotorCommand::SetClosedLoop { .. } => 8,
        MotorCommand::DeferMoves { .. } => 26,
        MotorCommand::MoveToHome { .. } => 27,
        MotorCommand::MoveRelative { .. } => 103,
        MotorCommand::MoveAbsolute { .. } => 104,
        MotorCommand::MoveVelocity { .. } => 105,
        MotorCommand::Home { .. } => 106,
        MotorCommand::Stop { .. } => 107,
        MotorCommand::Poll => 109,
        MotorCommand::ProfileInitialize { .. }
        | MotorCommand::ProfileBuild
        | MotorCommand::ProfileExecute
        | MotorCommand::ProfileAbort
        | MotorCommand::ProfileReadback => return None,
    })
}

/// Whether drvMotorAsyn hands `reason` to the driver's `setDouble` /
/// `setInteger` (drvMotorAsyn.c:613-623, 735-745), which store it as a
/// parameter. Position is a status field, posted through it.
fn is_driver_param(reason: i32) -> bool {
    (1..100).contains(&reason)
}

/// What one command wrote on the axis.
#[derive(Debug, Default)]
pub struct AxisWrite {
    /// The status fields it set.
    pub fields: Vec<StatusWrite>,
    /// The model-2 parameter it stored, by drvMotorAsyn reason.
    param: Option<(i32, CValue)>,
}

/// Whether a failed `cmd` prints C `devMotorAsyn::asynCallback`'s write line
/// (devMotorAsyn.c:661-713). Deferred moves, move-to-home and profiles reach
/// the C driver through other records; `GET_INFO` has no failure print.
fn sent_by_dev_motor_asyn(cmd: &MotorCommand) -> bool {
    !matches!(
        cmd,
        MotorCommand::Poll
            | MotorCommand::InitPosition { .. }
            | MotorCommand::InitEncoderRatio { .. }
            | MotorCommand::DeferMoves { .. }
            | MotorCommand::MoveToHome { .. }
            | MotorCommand::ProfileInitialize { .. }
            | MotorCommand::ProfileBuild
            | MotorCommand::ProfileExecute
            | MotorCommand::ProfileAbort
            | MotorCommand::ProfileReadback
    )
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
/// the parameter the write stored.
pub fn execute_commands(
    motor: &mut dyn AsynMotor,
    trace: &MotorTrace,
    commands: &[QueuedCommand],
    on_write: &mut dyn FnMut(AxisWrite),
) {
    let user = AsynUser::new(0);
    let port = trace.port();
    let axis = trace.addr();
    let model2 = trace.is_model2();
    let controller = trace.is_controller();
    for QueuedCommand { command, mres } in commands {
        let (cmd, mres) = (command, *mres);
        // C's wire units: positions in signed steps, speeds in steps/s.
        let steps = |egu: f64| egu / mres;
        let rate = |egu: f64| egu / mres.abs();
        let c = if model2 {
            c_write(cmd, mres)
                .and_then(|(_, value)| Some((drv_motor_asyn_reason(cmd, mres)?, value)))
        } else {
            c_write(cmd, mres)
        };
        let function = c.map_or(0, |(function, _)| function);
        // asynMotorController's own flow lines; drvMotorAsyn has none of them.
        let flow = |args: std::fmt::Arguments<'_>| {
            if !model2 {
                trace.print(TraceMask::FLOW, function, args)
            }
        };
        if let (true, Some((_, CValue::Float64(_)))) = (model2, c) {
            trace.print(
                TraceMask::FLOW,
                function,
                format_args!(
                    "drvMotorAsyn::writeFloat64, reason={function}, pasynUser={:p} pAxis={:p}",
                    &user, motor as *const dyn AsynMotor as *const (),
                ),
            );
        }
        let mut write_status = None;
        let result = match *cmd {
            MotorCommand::MoveAbsolute {
                position,
                min_velocity,
                velocity,
                acceleration,
            } => {
                let r = motor.move_absolute(&user, position, min_velocity, velocity, acceleration);
                flow(format_args!(
                    "asynMotorController:writeFloat64: Set driver {port}, axis {axis} move absolute to {:.6}, base velocity={:.6}, velocity={:.6}, acceleration={:.6}",
                    steps(position),
                    rate(min_velocity),
                    rate(velocity),
                    rate(acceleration),
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
                    "asynMotorController:writeFloat64: Set driver {port}, axis {axis} move relative by {:.6}, base velocity={:.6}, velocity={:.6}, acceleration={:.6}",
                    steps(distance),
                    rate(min_velocity),
                    rate(velocity),
                    rate(acceleration),
                ));
                r
            }
            MotorCommand::MoveVelocity {
                direction,
                min_velocity,
                velocity,
                acceleration,
            } => {
                let signed_vel = if direction { velocity } else { -velocity };
                let r = motor.move_velocity(&user, min_velocity, signed_vel, acceleration);
                flow(format_args!(
                    "asynMotorController:writeFloat64: Set port {port}, axis {axis} move with velocity of {:.6}, acceleration={:.6}",
                    steps(signed_vel),
                    rate(acceleration),
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
                    "asynMotorController:writeFloat64: Set driver {port}, axis {axis} to home {way}, base velocity={:.6}, velocity={:.6}, acceleration={:.6}",
                    rate(min_velocity),
                    rate(velocity),
                    rate(acceleration),
                ));
                r
            }
            MotorCommand::Stop { acceleration } => motor.stop(&user, acceleration),
            MotorCommand::SetPosition { position } | MotorCommand::InitPosition { position } => {
                let r = motor.set_position(&user, position);
                flow(format_args!(
                    "asynMotorController:writeFloat64: Set driver {port}, axis {axis} to position={:.6}",
                    steps(position)
                ));
                r
            }
            MotorCommand::SetEncoderRatio { ratio } | MotorCommand::InitEncoderRatio { ratio } => {
                let r = motor.set_encoder_ratio(&user, ratio);
                flow(format_args!(
                    "asynMotorController:writeFloat64: Set driver {port}, axis {axis} encoder ratio={ratio:.6}"
                ));
                r
            }
            MotorCommand::SetClosedLoop { enable } => motor.set_closed_loop(&user, enable),
            MotorCommand::DeferMoves { defer } => motor.set_deferred_moves(&user, defer),
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
                    "asynMotorController:writeInt32:: Starting a move to home for axis {axis}"
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
                    "asynMotorController:writeFloat64: Set driver {port}, axis {axis} {what} gain={gain:.6}"
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
                // The parameter C writes: the pair swaps for a negative MRES.
                let which = if high != (mres < 0.0) { "high" } else { "low" };
                flow(format_args!(
                    "asynMotorController:writeFloat64: Set driver {port}, axis {axis} {which} limit={:.6}",
                    steps(position)
                ));
                r
            }
            MotorCommand::ProfileInitialize { max_points } => {
                motor.initialize_profile(&user, max_points)
            }
            MotorCommand::Poll => Ok(()),
        };
        let position = match *cmd {
            MotorCommand::SetPosition { position } | MotorCommand::InitPosition { position } => {
                Some(StatusWrite::Position(position))
            }
            _ => None,
        };
        let moved = matches!(
            cmd,
            MotorCommand::MoveAbsolute { .. }
                | MotorCommand::MoveRelative { .. }
                | MotorCommand::MoveVelocity { .. }
                | MotorCommand::Home { .. }
        )
        .then_some(StatusWrite::Done(false));
        let mut writes = Vec::new();
        if controller {
            writes.extend(position);
            writes.extend(motor.take_status_writes());
            writes.extend(moved);
        } else {
            writes.extend(motor.take_status_writes());
            if model2 && result.is_ok() {
                writes.extend(position.or(moved));
            }
        }
        // A model-2 driver stores a parameter only once the write succeeded
        // (drvMotorSim.c:364-368, 424-428).
        let param = c.filter(|&(reason, _)| model2 && result.is_ok() && is_driver_param(reason));
        on_write(AxisWrite {
            fields: writes,
            param,
        });

        if let (true, Some((_, value))) = (model2, c) {
            // drvMotorAsyn prints the write whatever its status.
            match value {
                CValue::Float64(v) => trace.print(
                    TraceMask::IO_DRIVER,
                    function,
                    format_args!("drvMotorAsyn::writeFloat64, reason={function}, value={v:.6}"),
                ),
                CValue::Int32(v) => trace.print(
                    TraceMask::IO_DRIVER,
                    function,
                    format_args!("drvMotorAsyn::writeInt32, reason={function}, value={v}"),
                ),
            }
        } else if let Some((_, value)) = c {
            let status = write_status.unwrap_or_else(|| match &result {
                Ok(()) => 0,
                Err(e) => e.status() as i32,
            });
            let print = |iface: &str, value: std::fmt::Arguments<'_>| {
                if status != 0 {
                    trace.print(
                        TraceMask::ERROR,
                        function,
                        format_args!(
                            "asynMotorController:{iface} error, status={status} axis={axis}, function={function}, value={value}"
                        ),
                    );
                } else {
                    trace.print(
                        TraceMask::IO_DRIVER,
                        function,
                        format_args!(
                            "asynMotorController:{iface}:: axis={axis}, function={function}, value={value}"
                        ),
                    );
                }
            };
            match value {
                CValue::Float64(v) => print("writeFloat64", format_args!("{v:.6}")),
                CValue::Int32(v) => print("writeInt32", format_args!("{v}")),
            }
        }

        if let (Err(_), MotorCommand::MoveToHome { .. }) = (&result, cmd) {
            if !model2 {
                trace.print_self(
                    TraceMask::ERROR,
                    format_args!(
                        "asynMotorController:asynMotorMoveToHome: move to home failed in asynMotorController::asynMotorMoveToHome. Axis number={axis}"
                    ),
                );
            }
            continue;
        }
        let failure = result.as_ref().err().map(|e| e.message_display());
        dev_motor_asyn_reply(
            trace,
            cmd,
            mres,
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
pub fn refuse_commands(trace: &MotorTrace, missing: MissingAxis, commands: &[QueuedCommand]) {
    let model2 = trace.is_model2();
    for QueuedCommand { command, mres } in commands {
        let c = c_write(command, *mres);
        let function = if model2 {
            drv_motor_asyn_reason(command, *mres)
        } else {
            c.map(|(function, _)| function)
        };
        let method = match c {
            Some((_, CValue::Int32(_))) => "writeInt32",
            _ => "writeFloat64",
        };
        let message = missing.message(method, trace.addr());
        dev_motor_asyn_reply(trace, command, *mres, function.unwrap_or(0), Some(&message));
    }
}

/// devMotorAsyn's own line for a finished `cmd`: `init_controller`'s report
/// of its two synchronous writes (devMotorAsyn.c:187-235), or `asynCallback`'s
/// write failure (:700-707) carrying the driver's `errorMessage`.
fn dev_motor_asyn_reply(
    trace: &MotorTrace,
    cmd: &MotorCommand,
    mres: f64,
    function: i32,
    failure: Option<&dyn std::fmt::Display>,
) {
    let record = trace.record();
    let (what, value) = match *cmd {
        MotorCommand::InitEncoderRatio { ratio } => ("encoder ratio", ratio),
        MotorCommand::InitPosition { position } => ("position", position / mres),
        _ => {
            if let Some(e) = failure.filter(|_| sent_by_dev_motor_asyn(cmd)) {
                trace.print(
                    TraceMask::ERROR,
                    function,
                    format_args!(
                        "devMotorAsyn::asynCallback: {record} pasyn{{Float64,Int32}}->write returned {e}"
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
            format_args!("devMotorAsyn::init_controller, {record} set {what} to {value:.6}"),
        ),
        Some(_) => trace.print(
            TraceMask::ERROR,
            reason,
            format_args!(
                "devMotorAsyn::init_controller, {record} failed to set {what} to {value:.6}"
            ),
        ),
    }
}
