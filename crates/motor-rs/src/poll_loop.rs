use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus};
use asyn_rs::trace::TraceMask;
use asyn_rs::user::AsynUser;
use epics_base_rs::runtime::task::{
    CallbackPriority, StackSizeClass, ThreadPriority, block_on_sync, sleep_background,
    spawn_background, spawn_dedicated_thread,
};
use tokio::sync::{mpsc, oneshot};

use crate::device_state::*;
use crate::flags::{MotorCommand, PidGainKind};
use crate::trace::MotorTrace;

/// Queue of driver commands from the device support to the poller thread.
///
/// Unbounded like the asyn request queue C `devMotorAsyn::build_trans` feeds,
/// so a record pass never waits on the driver.
pub type MotorCommandSender = mpsc::UnboundedSender<Vec<QueuedCommand>>;

/// A driver command with the MRES of the record pass that queued it. The
/// driver takes dial EGU; C's asyn layer carries steps, the command over MRES,
/// so the trace lines read that.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedCommand {
    pub command: MotorCommand,
    pub mres: f64,
}

/// Commands sent to the poll loop.
#[derive(Debug)]
pub enum PollCommand {
    StartPolling,
    StopPolling,
    ScheduleDelay(u64, Duration),
    Shutdown,
}

/// The poller's two queues: poll control, and the driver commands a record
/// pass queued (drained first, so a StartPolling sees their effect).
pub struct PollerInbox {
    pub poll: mpsc::Receiver<PollCommand>,
    pub commands: mpsc::UnboundedReceiver<Vec<QueuedCommand>>,
}

/// Motor poll loop — one per record, stays alive for the record's lifetime.
///
/// It is the only caller of the driver after `init`: polls and the record's
/// commands both run on its own `motorPoller` OS thread (see [`Self::run`]),
/// as C runs `asynMotorPoller` on an epicsThread, so a driver blocked on a
/// dead serial line or TCP peer never stalls the IOC's async executor.
pub struct MotorPollLoop {
    cmd_rx: mpsc::Receiver<PollCommand>,
    motor_cmd_rx: mpsc::UnboundedReceiver<Vec<QueuedCommand>>,
    io_intr_tx: mpsc::Sender<()>,
    motor: Arc<Mutex<dyn AsynMotor>>,
    trace: Arc<MotorTrace>,
    device_state: SharedDeviceState,
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
    /// period. A forced poll (StartPolling, i.e. request_poll/status_refresh)
    /// bypasses this and always notifies.
    last_status: Option<MotorStatus>,
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
        io_intr_tx: mpsc::Sender<()>,
        motor: Arc<Mutex<dyn AsynMotor>>,
        device_state: SharedDeviceState,
        moving_poll_interval: Duration,
        idle_poll_interval: Duration,
        forced_fast_polls: u32,
    ) -> Self {
        Self {
            cmd_rx: inbox.poll,
            motor_cmd_rx: inbox.commands,
            io_intr_tx,
            motor,
            trace: Default::default(),
            device_state,
            moving_poll_interval,
            idle_poll_interval,
            forced_fast_polls_config: forced_fast_polls,
            forced_fast_polls_remaining: 0,
            last_moving: false,
            status_seq: 1, // starts at 1 (init already wrote seq=1)
            last_status: None,
        }
    }

    /// Share the device support's trace user, so command failures print as
    /// the record's.
    pub fn with_trace(mut self, trace: Arc<MotorTrace>) -> Self {
        self.trace = trace;
        self
    }

    /// Run the commands queued so far on the calling thread: for a harness
    /// standing in for the poller thread. The IOC path is [`Self::run`].
    pub fn run_queued_commands(&mut self) {
        while let Ok(commands) = self.motor_cmd_rx.try_recv() {
            self.execute(&commands);
        }
    }

    fn execute(&self, commands: &[QueuedCommand]) {
        if let Ok(mut motor) = self.motor.lock() {
            execute_commands(&mut *motor, &self.trace, commands);
        }
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
    /// parameter library. No record is listening yet, so nothing is posted.
    /// [`Self::run`] does this first; a harness calls it before init.
    pub fn prime_cache(&mut self) {
        let status = self.poll_status();
        self.last_status = Some(status.clone());
        if let Ok(mut ds) = self.device_state.lock() {
            ds.latest_status = Some(StampedStatus { seq: 1, status });
        }
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

    async fn poll_and_notify(&mut self, force: bool) {
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
        self.last_status = Some(status.clone());
        self.status_seq += 1;
        {
            let Ok(mut ds) = self.device_state.lock() else {
                return;
            };
            ds.latest_status = Some(StampedStatus {
                seq: self.status_seq,
                status,
            });
        }
        let _ = self.io_intr_tx.send(()).await;
    }

    /// Spawn a settle-delay timer as an independent task so the `select!`
    /// loop keeps servicing commands (Shutdown/Stop/Start) while the delay
    /// runs. On expiry it records `expired_delay_id` and pulses io_intr.
    ///
    /// C `callbackRequestDelayed` (motorRecord.cc:1437): the timer runs on the
    /// callback pool, not on the poller thread, so a driver call blocked there
    /// cannot hold the settle delay past expiry. LOW is the record's default
    /// `PRIO`; the poll loop does not carry the record's own.
    fn spawn_delay(&self, delay_id: u64, dur: Duration) {
        let device_state = self.device_state.clone();
        let io_intr_tx = self.io_intr_tx.clone();
        spawn_background(CallbackPriority::Low, async move {
            sleep_background(dur).await;
            match device_state.lock() {
                Ok(mut ds) => ds.expired_delay_id = Some(delay_id),
                Err(_) => return,
            }
            let _ = io_intr_tx.send(()).await;
        });
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
        // Then idle until the record asks for polling.
        let mut active = false;

        loop {
            if active {
                // Poll mode: check for commands or poll on interval
                let interval = self.effective_poll_interval();
                tokio::select! {
                    biased;
                    Some(commands) = self.motor_cmd_rx.recv() => self.execute(&commands),
                    cmd = self.cmd_rx.recv() => {
                        match cmd {
                            Some(PollCommand::StartPolling) => {
                                active = true;
                                self.forced_fast_polls_remaining = self.forced_fast_polls_config;
                                // Command-triggered (request_poll/status_refresh
                                // /keep-alive resume): force the post.
                                self.poll_and_notify(true).await;
                            }
                            Some(PollCommand::StopPolling) => {
                                active = false;
                            }
                            Some(PollCommand::ScheduleDelay(delay_id, dur)) => {
                                active = false;
                                self.spawn_delay(delay_id, dur);
                            }
                            Some(PollCommand::Shutdown) => {
                                return;
                            }
                            None => {
                                return;
                            }
                        }
                    }
                    // C asynMotorController.cpp:633-634: idlePollPeriod_ == 0
                    // blocks on the event (no timed poll). A zero effective
                    // interval disables this timed arm so the loop is
                    // event-driven only, never busy-spinning on sleep(0).
                    _ = sleep_background(interval), if !interval.is_zero() => {
                        // Autonomous timed poll: notify only on a real change
                        // (C statusChanged_ gate).
                        self.poll_and_notify(false).await;
                    }
                }
            } else {
                // Idle mode: wait for commands only
                let cmd = tokio::select! {
                    biased;
                    Some(commands) = self.motor_cmd_rx.recv() => {
                        self.execute(&commands);
                        continue;
                    }
                    cmd = self.cmd_rx.recv() => cmd,
                };
                match cmd {
                    Some(PollCommand::StartPolling) => {
                        active = true;
                        self.forced_fast_polls_remaining = self.forced_fast_polls_config;
                        // Command-triggered: force the post (see active arm).
                        self.poll_and_notify(true).await;
                    }
                    Some(PollCommand::StopPolling) => {
                        active = false;
                    }
                    Some(PollCommand::ScheduleDelay(delay_id, dur)) => {
                        self.spawn_delay(delay_id, dur);
                    }
                    Some(PollCommand::Shutdown) | None => {
                        return;
                    }
                }
            }
        }
    }
}

/// The value C `asynMotorController::writeFloat64` / `writeInt32` receives.
#[derive(Clone, Copy)]
enum CValue {
    Float64(f64),
    Int32(i32),
}

/// C asynMotorController's `motorStatus_` index: the reason devMotorAsyn's
/// init-time user carries (devMotorAsyn.c:405).
pub(crate) const MOTOR_STATUS_REASON: i32 = 26;

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
/// Each command prints what C's `asynMotorController::writeFloat64` /
/// `writeInt32` (asynMotorController.cpp:181-427) and then devMotorAsyn's
/// `asynCallback` print for it, on the record's asyn user.
pub fn execute_commands(motor: &mut dyn AsynMotor, trace: &MotorTrace, commands: &[QueuedCommand]) {
    let user = AsynUser::new(0);
    let port = trace.port();
    let axis = trace.addr();
    for QueuedCommand { command, mres } in commands {
        let (cmd, mres) = (command, *mres);
        // C's wire units: positions in signed steps, speeds in steps/s.
        let steps = |egu: f64| egu / mres;
        let rate = |egu: f64| egu / mres.abs();
        let c = c_write(cmd, mres);
        let function = c.map_or(0, |(function, _)| function);
        let flow = |args: std::fmt::Arguments<'_>| trace.print(TraceMask::FLOW, function, args);
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

        if let Some((_, value)) = c {
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

        if let MotorCommand::InitEncoderRatio { ratio } = *cmd {
            let record = trace.record();
            match result {
                Ok(()) => trace.print(
                    TraceMask::FLOW,
                    MOTOR_STATUS_REASON,
                    format_args!(
                        "devMotorAsyn::init_controller, {record} set encoder ratio to {ratio:.6}"
                    ),
                ),
                Err(_) => trace.print(
                    TraceMask::ERROR,
                    MOTOR_STATUS_REASON,
                    format_args!(
                        "devMotorAsyn::init_controller, {record} failed to set encoder ratio to {ratio:.6}"
                    ),
                ),
            }
            continue;
        }
        if let MotorCommand::InitPosition { position } = *cmd {
            let steps = steps(position);
            let record = trace.record();
            match result {
                Ok(()) => trace.print(
                    TraceMask::FLOW,
                    MOTOR_STATUS_REASON,
                    format_args!(
                        "devMotorAsyn::init_controller, {record} set position to {steps:.6}"
                    ),
                ),
                Err(_) => trace.print(
                    TraceMask::ERROR,
                    MOTOR_STATUS_REASON,
                    format_args!(
                        "devMotorAsyn::init_controller, {record} failed to set position to {steps:.6}"
                    ),
                ),
            }
            continue;
        }
        let Err(e) = result else { continue };
        if matches!(cmd, MotorCommand::MoveToHome { .. }) {
            trace.print_self(
                TraceMask::ERROR,
                format_args!(
                    "asynMotorController:asynMotorMoveToHome: move to home failed in asynMotorController::asynMotorMoveToHome. Axis number={axis}"
                ),
            );
        } else if sent_by_dev_motor_asyn(cmd) {
            trace.print(
                TraceMask::ERROR,
                function,
                format_args!(
                    "devMotorAsyn::asynCallback: {} pasyn{{Float64,Int32}}->write returned {}",
                    trace.record(),
                    e.message_display()
                ),
            );
        }
    }
}
