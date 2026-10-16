use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus};
use asyn_rs::user::AsynUser;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

use crate::device_state::*;
use crate::flags::MotorCommand;

/// Queue of driver commands from the device support to the poller thread.
///
/// Unbounded like the asyn request queue C `devMotorAsyn::build_trans` feeds,
/// so a record pass never waits on the driver.
pub type MotorCommandSender = mpsc::UnboundedSender<Vec<MotorCommand>>;

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
    pub commands: mpsc::UnboundedReceiver<Vec<MotorCommand>>,
}

/// Motor poll loop — one per record, stays alive for the record's lifetime.
///
/// It is the only caller of the driver after `init`: polls and the record's
/// commands both run on its own `motorPoller` OS thread (see [`Self::run`]),
/// as C runs `asynMotorPoller` on an epicsThread, so a driver blocked on a
/// dead serial line or TCP peer never stalls the IOC's async executor.
pub struct MotorPollLoop {
    cmd_rx: mpsc::Receiver<PollCommand>,
    motor_cmd_rx: mpsc::UnboundedReceiver<Vec<MotorCommand>>,
    io_intr_tx: mpsc::Sender<()>,
    motor: Arc<Mutex<dyn AsynMotor>>,
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

    /// Run the commands queued so far on the calling thread: for a harness
    /// standing in for the poller thread. The IOC path is [`Self::run`].
    pub fn run_queued_commands(&mut self) {
        while let Ok(commands) = self.motor_cmd_rx.try_recv() {
            self.execute(&commands);
        }
    }

    fn execute(&self, commands: &[MotorCommand]) {
        match self.motor.lock() {
            Ok(mut motor) => execute_commands(&mut *motor, commands),
            Err(e) => tracing::error!("motor lock poisoned: {e}"),
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

    async fn poll_and_notify(&mut self, force: bool) {
        let user = AsynUser::new(0);
        // A transport failure — or a poisoned driver mutex, which leaves the
        // motor just as unreachable — is the Rust spelling of the C driver
        // poll that bails to its `commsError` label. It must still produce a
        // status post, or MSTA never raises COMM_ERR, the record never
        // processes, and a `STUP=BUSY` refresh latches forever waiting for a
        // sequence that will not come (record/status_update.rs:48-58).
        let polled = match self.motor.lock() {
            Ok(mut motor) => motor.poll(&user).map_err(|e| {
                tracing::warn!("motor poll failed, posting COMM_ERR: {e}");
            }),
            Err(e) => {
                tracing::error!("motor mutex poisoned, posting COMM_ERR: {e}");
                Err(())
            }
        };
        let status = match polled {
            Ok(s) => s,
            Err(()) => self.comms_error_status(),
        };
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
            match self.device_state.lock() {
                Ok(mut ds) => {
                    ds.latest_status = Some(StampedStatus {
                        seq: self.status_seq,
                        status,
                    });
                }
                Err(e) => {
                    tracing::error!("device state lock poisoned in poll_and_notify: {e}");
                    return;
                }
            }
        }
        let _ = self.io_intr_tx.send(()).await;
    }

    /// Spawn a settle-delay timer as an independent task so the `select!`
    /// loop keeps servicing commands (Shutdown/Stop/Start) while the delay
    /// runs. On expiry it records `expired_delay_id` and pulses io_intr.
    ///
    /// The timer runs on the IOC runtime (`ioc`), not on the poller thread, so
    /// a driver call blocked there cannot hold the settle delay past expiry.
    fn spawn_delay(&self, ioc: &Handle, delay_id: u64, dur: Duration) {
        let device_state = self.device_state.clone();
        let io_intr_tx = self.io_intr_tx.clone();
        ioc.spawn(async move {
            tokio::time::sleep(dur).await;
            match device_state.lock() {
                Ok(mut ds) => ds.expired_delay_id = Some(delay_id),
                Err(e) => {
                    tracing::error!("device state lock poisoned in delay expiry: {e}");
                    return;
                }
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
    /// the returned future completes when that thread ends. Call from a task
    /// on the IOC runtime, which keeps the settle-delay timers.
    pub async fn run(self) {
        let ioc = Handle::current();
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("motorPoller".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                {
                    Ok(rt) => rt.block_on(self.serve(ioc)),
                    Err(e) => tracing::error!("motorPoller: runtime build failed: {e}"),
                }
                drop(done_tx);
            });
        match spawned {
            Ok(_) => {
                let _ = done_rx.await;
            }
            Err(e) => tracing::error!("motorPoller: thread create failed: {e}"),
        }
    }

    async fn serve(mut self, ioc: Handle) {
        // Start idle: device support init() sends StartPolling after
        // iocInit, matching C EPICS where the poller starts in init_record.
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
                                self.spawn_delay(&ioc, delay_id, dur);
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
                    _ = tokio::time::sleep(interval), if !interval.is_zero() => {
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
                        self.spawn_delay(&ioc, delay_id, dur);
                    }
                    Some(PollCommand::Shutdown) | None => {
                        return;
                    }
                }
            }
        }
    }
}

/// Run `commands` against the driver in order. Only the poller thread calls
/// this outside tests, so a blocking driver stalls nothing but that thread.
pub fn execute_commands(motor: &mut dyn AsynMotor, commands: &[MotorCommand]) {
    let user = AsynUser::new(0);
    for cmd in commands {
        let result = match cmd {
            MotorCommand::MoveAbsolute {
                position,
                min_velocity,
                velocity,
                acceleration,
            } => {
                tracing::info!("motor command: MoveAbsolute pos={position}, vel={velocity}");
                motor.move_absolute(&user, *position, *min_velocity, *velocity, *acceleration)
            }
            MotorCommand::MoveRelative {
                distance,
                min_velocity,
                velocity,
                acceleration,
            } => {
                tracing::info!("motor command: MoveRelative dist={distance}, vel={velocity}");
                motor.move_relative(&user, *distance, *min_velocity, *velocity, *acceleration)
            }
            MotorCommand::MoveVelocity {
                direction,
                min_velocity,
                velocity,
                acceleration,
            } => {
                let signed_vel = if *direction { *velocity } else { -*velocity };
                tracing::info!("motor command: MoveVelocity dir={direction}, vel={velocity}");
                motor.move_velocity(&user, *min_velocity, signed_vel, *acceleration)
            }
            MotorCommand::Home {
                forward,
                min_velocity,
                velocity,
                acceleration,
            } => {
                tracing::info!("motor command: Home forward={forward}");
                motor.home(&user, *min_velocity, *velocity, *acceleration, *forward)
            }
            MotorCommand::Stop { acceleration } => {
                tracing::info!("motor command: Stop");
                motor.stop(&user, *acceleration)
            }
            MotorCommand::SetPosition { position } => {
                tracing::info!("motor command: SetPosition pos={position}");
                motor.set_position(&user, *position)
            }
            MotorCommand::SetClosedLoop { enable } => {
                tracing::info!("motor command: SetClosedLoop enable={enable}");
                motor.set_closed_loop(&user, *enable)
            }
            MotorCommand::DeferMoves { defer } => {
                tracing::info!("motor command: DeferMoves defer={defer}");
                motor.set_deferred_moves(&user, *defer)
            }
            MotorCommand::ProfileInitialize { max_points } => {
                tracing::info!("motor command: ProfileInitialize max_points={max_points}");
                motor.initialize_profile(&user, *max_points)
            }
            MotorCommand::ProfileBuild => {
                tracing::info!("motor command: ProfileBuild");
                motor.build_profile(&user)
            }
            MotorCommand::ProfileExecute => {
                tracing::info!("motor command: ProfileExecute");
                motor.execute_profile(&user)
            }
            MotorCommand::ProfileAbort => {
                tracing::info!("motor command: ProfileAbort");
                motor.abort_profile(&user)
            }
            MotorCommand::ProfileReadback => {
                tracing::info!("motor command: ProfileReadback");
                motor.readback_profile(&user).map(|_| ())
            }
            MotorCommand::MoveToHome {
                position,
                min_velocity,
                velocity,
                acceleration,
            } => {
                tracing::info!(
                    "motor command: MoveToHome position={position} velocity={velocity} accel={acceleration}"
                );
                motor.move_to_home(&user, *position, *min_velocity, *velocity, *acceleration)
            }
            MotorCommand::SetPidGain { kind, gain } => {
                tracing::info!("motor command: SetPidGain kind={kind:?} gain={gain}");
                motor.set_pid_gain(&user, *kind, *gain)
            }
            MotorCommand::SetHighLimit { position } => {
                tracing::info!("motor command: SetHighLimit pos={position}");
                motor.set_high_limit(&user, *position)
            }
            MotorCommand::SetLowLimit { position } => {
                tracing::info!("motor command: SetLowLimit pos={position}");
                motor.set_low_limit(&user, *position)
            }
            MotorCommand::Poll => Ok(()),
        };

        if let Err(e) = result {
            tracing::error!("motor command error: {e}");
        }
    }
}
