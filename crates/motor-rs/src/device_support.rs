use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::{CallbackPriority, sleep_background, spawn_background};
use epics_base_rs::server::device_support::{DeviceInitOutcome, DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::{EpicsValue, PvString};
use tokio::sync::mpsc;

use crate::device_state::*;
use crate::flags::MotorCommand;
use crate::poll_loop::{CommandBatch, MissingAxis, MotorCommandSender, PollCommand};
use crate::trace::{MotorTrace, TraceUser};

/// One axis as its create command registers it: the poller's queues and the
/// records bound to it. Each record binding gets its own
/// [`MotorDeviceSupport`] from [`Self::attach`], as each C devMotorAsyn
/// record has its own asynUser and status callback on the axis.
#[derive(Clone)]
pub struct MotorAxis {
    user: TraceUser,
    timeout: Duration,
    poll_cmd_tx: mpsc::Sender<PollCommand>,
    motor_cmd_tx: MotorCommandSender,
    subscribers: AxisSubscribers,
}

impl MotorAxis {
    /// `subscribers` is the list the axis' poller posts to.
    pub fn new(
        user: TraceUser,
        timeout: Duration,
        poll_cmd_tx: mpsc::Sender<PollCommand>,
        motor_cmd_tx: MotorCommandSender,
        subscribers: AxisSubscribers,
    ) -> Self {
        Self {
            user,
            timeout,
            poll_cmd_tx,
            motor_cmd_tx,
            subscribers,
        }
    }

    /// The axis' asyn `(port, addr)` or model-1 card and signal, the address
    /// a record's OUT names it by.
    pub fn address(&self) -> &TraceUser {
        &self.user
    }

    /// Queue `command` on the poller for a writer that is no record: C's
    /// controller port running a base function on the axis.
    pub(crate) fn send(&self, command: MotorCommand) {
        let _ = self.motor_cmd_tx.send(CommandBatch {
            trace: Arc::new(MotorTrace::with_user(self.user.clone())),
            record: None,
            commands: vec![command],
        });
    }

    /// Poll now and post: C `motorUpdateStatus_`.
    pub(crate) fn force_poll(&self) {
        let _ = self.poll_cmd_tx.try_send(PollCommand::ForcePoll);
    }

    /// Bind one more record: its own mailbox, seeded from the axis' cache,
    /// its own trace, and a place on the subscriber list.
    pub fn attach(&self) -> MotorDeviceSupport {
        let device_state = new_shared_state();
        let (io_intr_tx, io_intr_rx) = mpsc::channel(16);
        let trace = Arc::new(MotorTrace::with_user(self.user.clone()));
        let record =
            self.subscribers
                .attach(device_state.clone(), io_intr_tx.clone(), trace.clone());
        MotorDeviceSupport::with_link(
            trace,
            self.timeout,
            AxisLink::Bound {
                poll_cmd_tx: self.poll_cmd_tx.clone(),
                motor_cmd_tx: self.motor_cmd_tx.clone(),
                record,
            },
            device_state,
            (io_intr_tx, io_intr_rx),
        )
    }
}

/// What a record's commands reach: the axis poller, or a port's answer for
/// an axis it never created.
#[derive(Clone)]
enum AxisLink {
    Bound {
        poll_cmd_tx: mpsc::Sender<PollCommand>,
        motor_cmd_tx: MotorCommandSender,
        record: RecordId,
    },
    Missing(MissingAxis),
}

/// The one way a record's driver commands leave it, C's `build_trans`:
/// the device support's process pass and the record's `special()` puts
/// both send through a copy of it.
#[derive(Clone)]
pub struct CommandPort {
    link: AxisLink,
    trace: Arc<MotorTrace>,
    device_state: SharedDeviceState,
    io_intr_tx: mpsc::Sender<()>,
    /// The record's `PRIO` band, C's `callbackSetPriority(pmr->prio, ...)`
    /// for the settle delay (motorRecord.cc:634); LOW is PRIO's default.
    prio: CallbackPriority,
}

impl std::fmt::Debug for CommandPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandPort")
            .field("user", self.trace.user())
            .finish_non_exhaustive()
    }
}

/// Motor device support — bridges MotorRecord to AsynMotor driver.
pub struct MotorDeviceSupport {
    port: CommandPort,
    _timeout: Duration,
    io_intr_rx: Option<mpsc::Receiver<()>>,
    initialized: bool,
}

impl MotorDeviceSupport {
    /// A support whose mailbox the caller feeds itself, record 0 to the
    /// poller: for a harness with no subscriber list. An IOC binds records
    /// through [`MotorAxis::attach`].
    pub fn new(
        trace: Arc<MotorTrace>,
        timeout: Duration,
        poll_cmd_tx: mpsc::Sender<PollCommand>,
        motor_cmd_tx: MotorCommandSender,
        device_state: SharedDeviceState,
    ) -> Self {
        Self::with_link(
            trace,
            timeout,
            AxisLink::Bound {
                poll_cmd_tx,
                motor_cmd_tx,
                record: 0,
            },
            device_state,
            mpsc::channel(16),
        )
    }

    /// A record on a live port whose axis is missing. C keeps it: its status
    /// stays the calloc-zeroed one and every command fails at the driver.
    pub fn missing(trace: Arc<MotorTrace>, missing: MissingAxis) -> Self {
        Self::with_link(
            trace,
            Duration::ZERO,
            AxisLink::Missing(missing),
            new_shared_state(),
            mpsc::channel(16),
        )
    }

    fn with_link(
        trace: Arc<MotorTrace>,
        timeout: Duration,
        link: AxisLink,
        device_state: SharedDeviceState,
        (io_intr_tx, io_intr_rx): (mpsc::Sender<()>, mpsc::Receiver<()>),
    ) -> Self {
        Self {
            port: CommandPort {
                link,
                trace,
                device_state,
                io_intr_tx,
                prio: CallbackPriority::Low,
            },
            _timeout: timeout,
            io_intr_rx: Some(io_intr_rx),
            initialized: false,
        }
    }

    /// The record's mailbox.
    pub(crate) fn device_state(&self) -> &SharedDeviceState {
        &self.port.device_state
    }

    /// Hand the driver a model-1 record's VME_IO parm, which C devices read
    /// from `mr->out.value.vmeio.parm` (devMVP2001.cc:262, :361).
    pub(crate) fn send_out_parm(&self, parm: String) {
        if !parm.is_empty() {
            self.port.send(vec![MotorCommand::OutParm {
                parm: PvString::from_bytes(parm),
            }]);
        }
    }

    /// Get the io_intr sender (for poll loop to trigger record re-processing).
    pub fn io_intr_sender(&self) -> mpsc::Sender<()> {
        self.port.io_intr_tx.clone()
    }

    /// Queue the commands on the poller thread and manage the poll loop.
    fn execute_actions(&mut self, mut actions: DeviceActions) {
        let port = &self.port;
        let commands = std::mem::take(&mut actions.commands);
        if !commands.is_empty() {
            port.send(commands);
        }

        if actions.force_poll {
            port.send_poll(PollCommand::ForcePoll);
        }
        if let Some(ref delay) = actions.schedule_delay {
            port.schedule_delay(delay);
        }
    }
}

impl CommandPort {
    /// A port whose commands land on the returned channel as record 0's:
    /// for a harness with no poller.
    pub fn capture() -> (Self, mpsc::UnboundedReceiver<CommandBatch>) {
        let (motor_cmd_tx, motor_cmd_rx) = mpsc::unbounded_channel();
        let port = Self {
            link: AxisLink::Bound {
                poll_cmd_tx: mpsc::channel(1).0,
                motor_cmd_tx,
                record: 0,
            },
            trace: Default::default(),
            device_state: new_shared_state(),
            io_intr_tx: mpsc::channel(1).0,
            prio: CallbackPriority::Low,
        };
        (port, motor_cmd_rx)
    }

    /// Queue `commands` on the poller thread, or refuse them as the port
    /// does for a missing axis. Each move-class command is counted pending
    /// first, as C `build_trans` does; a record in COMM_ALARM sends nothing.
    pub(crate) fn send(&self, commands: Vec<MotorCommand>) {
        let moves = commands.iter().filter(|c| c.is_move()).count();
        if let Ok(mut ds) = self.device_state.lock() {
            if ds.comm_alarm() {
                return;
            }
            for _ in 0..moves {
                ds.move_queued();
            }
        }
        match &self.link {
            // Fails only once the poller has ended, a state C's port queue
            // cannot be in, so there is no C line to print.
            AxisLink::Bound {
                motor_cmd_tx,
                record,
                ..
            } => {
                let _ = motor_cmd_tx.send(CommandBatch {
                    trace: self.trace.clone(),
                    record: Some(*record),
                    commands,
                });
            }
            AxisLink::Missing(missing) => {
                crate::poll_loop::refuse_commands(&self.trace, *missing, &commands);
                // asynCallback runs for a refused write as for any other.
                for command in commands.iter().filter(|c| c.is_move()) {
                    let done = self
                        .device_state
                        .lock()
                        .is_ok_and(|mut ds| ds.move_done(command));
                    if done {
                        self.pulse();
                    }
                }
            }
        }
    }

    /// Pulse io_intr for a process the record's callbacks queued, waiting
    /// for room rather than dropping it.
    fn pulse(&self) {
        let io_intr_tx = self.io_intr_tx.clone();
        if let Err(mpsc::error::TrySendError::Full(())) = io_intr_tx.try_send(()) {
            spawn_background(self.prio, async move {
                let _ = io_intr_tx.send(()).await;
            });
        }
    }

    /// Send this record's poll request; a missing axis has no poller.
    fn send_poll(&self, command: PollCommand) {
        if let AxisLink::Bound { poll_cmd_tx, .. } = &self.link {
            let _ = poll_cmd_tx.try_send(command);
        }
    }

    /// The settle delay, C `callbackRequestDelayed` (motorRecord.cc:1437):
    /// the record's own callback, run on the callback pool whatever the axis
    /// is doing, on the band of the record's `PRIO`. On expiry it hands the
    /// record its turn and pulses io_intr.
    fn schedule_delay(&self, delay: &DelayRequest) {
        let (id, duration) = (delay.id, delay.duration);
        let device_state = self.device_state.clone();
        let io_intr_tx = self.io_intr_tx.clone();
        spawn_background(self.prio, async move {
            sleep_background(duration).await;
            let pulse = device_state.lock().is_ok_and(|mut ds| ds.delay_expired(id));
            if pulse {
                let _ = io_intr_tx.send(()).await;
            }
        });
    }
}

impl DeviceSupport for MotorDeviceSupport {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        // Inject device_state into MotorRecord (for template-created records)
        let motor_rec = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>());

        if let Some(motor_rec) = motor_rec {
            motor_rec.set_device_state(self.port.device_state.clone());
            motor_rec.set_trace(self.port.trace.clone());
            motor_rec.set_command_port(self.port.clone());
        }

        if let AxisLink::Missing(missing) = self.port.link {
            // C prints the driver's errorMessage with no newline.
            self.port.trace.print(
                asyn_rs::trace::TraceMask::ERROR,
                self.port.trace.status_reason(),
                format_args!(
                    "devMotorAsyn::init_record: {} pasynGenericPointer->read returned {}",
                    self.port.trace.record(),
                    missing.message("readGenericPointer", self.port.trace.addr())
                ),
            );
        }

        // The RSTM/loadpos(#231)/MRES(#196) restore decision is owned
        // entirely by `initial_readback` (status_update.rs), whose
        // `InitPosition` is queued below.
        //
        // C `devMotorAsyn.c::init_controller` (166-239) reseeds only when
        // `initPos == 1`, decided by the RSTM switch testing the controller's
        // *actual current position* (`pPvt->status.position`, fetched before
        // init_controller). RSTM=Never never reseeds; NearZero reseeds only
        // when the controller currently sits near zero (`dval_non_zero_pos_
        // near_zero`) while DVAL is meaningful. An earlier Rust version
        // reseeded *unconditionally* on any pass0 restore, before the first
        // poll — so a controller that kept its true absolute position across
        // an IOC restart (default RSTM=NearZero, or RSTM=Never, or an
        // absolute encoder with LOADPOS_BLOCK) was clobbered with the stale
        // autosaved DVAL every boot — the exact failure RSTM/#231 prevent.
        //
        // C init_record reads the status through readGenericPointer, which
        // only copies the parameter cache the poller's startup poll filled
        // (asynMotorController.cpp:505-525): no hardware call, and no
        // failure for a bound axis. An unfilled cache reads as C's
        // calloc-zeroed status.
        let status = {
            let mut ds = self.port.device_state.lock().map_err(|e| {
                epics_base_rs::error::CaError::InvalidValue(format!("device state lock: {e}"))
            })?;
            ds.init_status()
        };
        self.port.trace.print(
            asyn_rs::trace::TraceMask::IO_DEVICE,
            self.port.trace.update_status_reason(),
            format_args!(
                "{} devMotorAsyn::update_values, needUpdate=1",
                self.port.trace.record()
            ),
        );

        // C init_record: the dset's init_controller, then
        // process_motor_info(initcall) and the drive sync, all before any
        // process.
        let anchor = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>())
            .map(|rec| rec.initial_readback(&status));
        if let Some(effects) = anchor {
            self.port.send(effects.commands);
            if effects.request_poll {
                self.port.send_poll(PollCommand::ForcePoll);
            }
        }

        // C init_record 717-718 ("Reset limits in case database values
        // are invalid"): both dial limits are forwarded to the driver
        // unconditionally after the initial readback, high first.
        let dial_limits = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>())
            .map(|rec| {
                let read =
                    |rec: &mut crate::record::MotorRecord, name: &str| match rec.get_field(name) {
                        Some(EpicsValue::Double(d)) => d,
                        _ => 0.0,
                    };
                let (dhlm, dllm) = (read(rec, "DHLM"), read(rec, "DLLM"));
                vec![
                    rec.dial_limit_command(true, dhlm),
                    rec.dial_limit_command(false, dllm),
                ]
            });
        if let Some(commands) = dial_limits {
            // Queued like C's SEND_MSG; non-fatal, as C ignores the
            // build_trans return, and a refusal prints asynCallback's line
            // on the poller.
            self.port.send(commands);
        }

        self.initialized = true;
        Ok(DeviceInitOutcome::ok())
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::ok())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        // Extract actions atomically from shared state
        let actions = {
            let mut ds = self.port.device_state.lock().map_err(|e| {
                epics_base_rs::error::CaError::InvalidValue(format!("device state lock: {e}"))
            })?;
            ds.pending_actions.take()
        };
        let Some(actions) = actions else {
            return Ok(());
        };

        self.execute_actions(actions);
        Ok(())
    }

    /// The DTYP that reaches this axis: `asynMotor` for an asyn axis (model 2
    /// or 3), the
    /// driver's own choice for a model-1 one.
    fn dtyp(&self) -> &str {
        match self.port.trace.user() {
            TraceUser::Asyn { .. } | TraceUser::Model2 { .. } => {
                crate::devices::DEV_MOTOR_ASYN.choice
            }
            TraceUser::Model1 { device, .. } => device,
        }
    }

    fn set_record_info(&mut self, name: &str, _scan: ScanType) {
        self.port.trace.set_record(name);
    }

    fn set_callback_priority(&mut self, prio: CallbackPriority) {
        self.port.prio = prio;
    }

    /// devMotorAsyn's `dbScanLockOK`, set by its dset `init(after)` at
    /// `finishDevSup` (devMotorAsyn.c:159-162): from then on, PINI and any
    /// put before `scanRun` included, the record's callbacks process it.
    fn bind_dev_sup_gate(
        &mut self,
        gate: &Arc<epics_base_rs::runtime::interrupt_accept::InterruptGate>,
    ) {
        let device_state = self.port.device_state.clone();
        let open = move || {
            if let Ok(mut ds) = device_state.lock() {
                ds.accept();
            }
        };
        if gate.is_open() {
            open();
        } else {
            gate.on_open(open);
        }
    }

    /// The pass one io_intr pulse runs reads the process its callback
    /// queued.
    fn arm_readback_callback(&mut self) {
        if let Ok(mut ds) = self.port.device_state.lock() {
            ds.begin_callback_pass();
        }
    }

    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        self.io_intr_rx.take()
    }

    /// The motor's poll `statusCallback` drives `dbProcess` on every readback
    /// regardless of `SCAN` (C `motorRecord` parity), and the record stays
    /// `SCAN="Passive"` so a `dbPutField` to a `pp(TRUE)` motion field
    /// (VAL/DVAL/RVAL/RLV/JOG/HOME/...) still re-processes it. Decouple the
    /// poll-feedback wiring from the `SCAN` menu.
    fn io_intr_scan_independent(&self) -> bool {
        true
    }

    /// `statusCallback` calls `dbProcess` itself, so an `iocPause` does not
    /// hold it.
    fn io_intr_processes_directly(&self) -> bool {
        true
    }
}
