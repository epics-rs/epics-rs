use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::{CallbackPriority, sleep_background, spawn_background};
use epics_base_rs::server::device_support::{DeviceInitOutcome, DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::PvString;
use tokio::sync::mpsc;

use std::sync::Mutex;

use asyn_rs::interfaces::motor::AsynMotor;

use crate::device_state::*;
use crate::flags::MotorCommand;
use crate::poll_loop::{
    CommandBatch, MissingAxis, MotorCommandSender, PollCommand, SharedControllerPoll, ToAxis,
    Written,
};
use crate::trace::{MotorTrace, TraceUser};

/// An axis subclass's C `report(fp, level)`, which its controller's
/// `asynMotorController::report` calls for each axis; an axis with none
/// is `asynMotorAxis::report`, which prints nothing.
///
/// It reads what it prints without the driver's lock, as C reads its
/// members unlocked: the controller report runs on the port thread, which
/// a poller holding that lock may be waiting on.
pub trait AxisReport: Send + Sync {
    fn report(&self, out: &mut dyn std::fmt::Write, level: i32);
}

/// A model-1 device's C `build_trans` return, which it decides from its
/// own state while the record builds the transaction: no controller
/// reply, and without the driver's lock, which a poller may hold.
pub trait BuildTrans: Send + Sync {
    /// SET_HIGH_LIMIT (`high`) or SET_LOW_LIMIT at `raw` (dial / MRES).
    fn limit(&self, _high: bool, _raw: f64, _mres: f64) -> LimitBuild {
        LimitBuild::Built
    }

    /// OK with nothing built (devEMC18011.cc:224-231, another motor
    /// selected): the record does not count it against RTRY.
    fn defers(&self, _command: &MotorCommand) -> bool {
        false
    }
}

/// C `build_trans`'s return on a soft-limit command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LimitBuild {
    /// OK: the record sends it.
    Built,
    /// ERROR: nothing is sent, and the device may have reset one of the
    /// record's dial limits to the controller's (devMM4000.cc:316-336).
    Refused(Option<DialLimitReset>),
}

/// The record dial limit a refusing device wrote, C `mr->dhlm` / `mr->dllm`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DialLimitReset {
    Dhlm(f64),
    Dllm(f64),
}

/// One axis as its create command registers it: the poller's queues and the
/// records bound to it. Each record binding gets its own
/// [`MotorDeviceSupport`] from [`Self::attach`], as each C devMotorAsyn
/// record has its own asynUser and status callback on the axis.
#[derive(Clone)]
pub struct MotorAxis {
    user: TraceUser,
    /// The driver, which a controller-port write calls on the port thread.
    motor: Arc<Mutex<dyn AsynMotor>>,
    controller_poll: SharedControllerPoll,
    timeout: Duration,
    poll_cmd_tx: mpsc::Sender<PollCommand>,
    motor_cmd_tx: MotorCommandSender,
    subscribers: AxisSubscribers,
    /// The model-2 parameter library its poller keeps.
    params: Model2Params,
    report: Option<Arc<dyn AxisReport>>,
    build_trans: Option<Arc<dyn BuildTrans>>,
    /// The driver's [`asyn_rs::interfaces::motor::AsynMotor::disables_ntm`].
    disables_ntm: bool,
    /// C `asynMotorAxis::referencingModeMove_`, which
    /// `asynMotorEnableMoveToHome` sets.
    referencing_mode_move: Arc<std::sync::atomic::AtomicI32>,
}

impl MotorAxis {
    /// `subscribers` is the list the axis' poller posts to.
    pub fn new(
        user: TraceUser,
        motor: Arc<Mutex<dyn AsynMotor>>,
        timeout: Duration,
        poll_cmd_tx: mpsc::Sender<PollCommand>,
        motor_cmd_tx: MotorCommandSender,
        subscribers: AxisSubscribers,
    ) -> Self {
        Self {
            user,
            motor,
            controller_poll: SharedControllerPoll::default(),
            timeout,
            poll_cmd_tx,
            motor_cmd_tx,
            subscribers,
            params: Model2Params::default(),
            report: None,
            build_trans: None,
            disables_ntm: false,
            referencing_mode_move: Arc::default(),
        }
    }

    /// The controller poll its controller's UPDATE_STATUS write runs.
    pub(crate) fn with_controller_poll(mut self, poll: SharedControllerPoll) -> Self {
        self.controller_poll = poll;
        self
    }

    pub(crate) fn disabling_ntm(mut self, disables_ntm: bool) -> Self {
        self.disables_ntm = disables_ntm;
        self
    }

    /// This axis reached at `user` as well, on the same poller: a second
    /// open of one driver axis returns the same handle (drvMotorSim.c:205-214).
    pub(crate) fn at(&self, user: TraceUser) -> Self {
        Self {
            user,
            motor: self.motor.clone(),
            controller_poll: self.controller_poll.clone(),
            timeout: self.timeout,
            poll_cmd_tx: self.poll_cmd_tx.clone(),
            motor_cmd_tx: self.motor_cmd_tx.clone(),
            subscribers: self.subscribers.clone(),
            params: self.params.clone(),
            report: self.report.clone(),
            build_trans: self.build_trans.clone(),
            disables_ntm: self.disables_ntm,
            referencing_mode_move: self.referencing_mode_move.clone(),
        }
    }

    /// C `getReferencingModeMove`.
    pub(crate) fn referencing_mode_move(&self) -> i32 {
        self.referencing_mode_move
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// C `setReferencingModeMove`.
    pub(crate) fn set_referencing_mode_move(&self, distance: i32) {
        self.referencing_mode_move
            .store(distance, std::sync::atomic::Ordering::Relaxed);
    }

    /// The axis's model-2 parameter library.
    pub(crate) fn params(&self) -> &Model2Params {
        &self.params
    }

    /// The driver's [`AxisReport`], C's `asynMotorAxis::report` override.
    pub fn with_report(mut self, report: Arc<dyn AxisReport>) -> Self {
        self.report = Some(report);
        self
    }

    /// The device's [`BuildTrans`]; an axis with none builds everything.
    pub fn with_build_trans(mut self, build_trans: Arc<dyn BuildTrans>) -> Self {
        self.build_trans = Some(build_trans);
        self
    }

    /// C `pAxis->report(fp, level)`.
    pub(crate) fn report(&self, out: &mut dyn std::fmt::Write, level: i32) {
        if let Some(report) = &self.report {
            report.report(out, level);
        }
    }

    /// The axis' asyn `(port, addr)` or model-1 card and signal, the address
    /// a record's OUT names it by.
    pub fn address(&self) -> &TraceUser {
        &self.user
    }

    /// The driver, for a controller-port write on the port thread.
    pub(crate) fn motor(&self) -> &Arc<Mutex<dyn AsynMotor>> {
        &self.motor
    }

    /// C `pC->poll()`, which UPDATE_STATUS runs before the axis's.
    pub(crate) fn controller_poll(&self) -> &SharedControllerPoll {
        &self.controller_poll
    }

    /// Queue `command` on the poller as a harness's batch, which no record
    /// sent.
    #[cfg(test)]
    pub(crate) fn send(&self, command: MotorCommand) {
        let _ = self.motor_cmd_tx.send(
            CommandBatch {
                trace: Arc::new(MotorTrace::with_user(self.user.clone())),
                record: None,
                commands: vec![command],
            }
            .into(),
        );
    }

    /// Hand the poller what a controller-port write set on the axis.
    pub(crate) fn post_written(&self, written: Written) {
        let _ = self.motor_cmd_tx.send(written.into());
    }

    /// Poll now and post: the drvMotorAsyn stand-in's GET_INFO.
    #[cfg(test)]
    pub(crate) fn force_poll(&self) {
        let _ = self.poll_cmd_tx.try_send(PollCommand::ForcePoll);
    }

    /// C `wakeupPoller`.
    pub(crate) fn wakeup(&self) {
        let _ = self.poll_cmd_tx.try_send(PollCommand::Wakeup);
    }

    /// Bind one more record: its own mailbox, seeded from the axis' cache,
    /// its own trace, and a place on the subscriber list.
    pub fn attach(&self) -> MotorDeviceSupport {
        let device_state = new_shared_state();
        let (io_intr_tx, io_intr_rx) = mpsc::channel(16);
        let trace = Arc::new(MotorTrace::with_user(self.user.clone()));
        if (trace.is_model1() || trace.is_motor_interface())
            && let Ok(mut ds) = device_state.lock()
        {
            ds.set_motordevcom();
        }
        let record =
            self.subscribers
                .attach(device_state.clone(), io_intr_tx.clone(), trace.clone());
        let mut support = MotorDeviceSupport::with_link(
            trace,
            self.timeout,
            AxisLink::Bound {
                poll_cmd_tx: self.poll_cmd_tx.clone(),
                motor_cmd_tx: self.motor_cmd_tx.clone(),
                record,
            },
            device_state,
            (io_intr_tx, io_intr_rx),
        );
        support.disables_ntm = self.disables_ntm;
        support.port.build_trans = self.build_trans.clone();
        support
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
    build_trans: Option<Arc<dyn BuildTrans>>,
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
    disables_ntm: bool,
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
                build_trans: None,
            },
            _timeout: timeout,
            io_intr_rx: Some(io_intr_rx),
            initialized: false,
            disables_ntm: false,
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
            self.port.send(
                vec![MotorCommand::OutParm {
                    parm: PvString::from_bytes(parm),
                }],
                false,
            );
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
            port.send(commands, true);
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
    pub fn capture() -> (Self, mpsc::UnboundedReceiver<ToAxis>) {
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
            build_trans: None,
        };
        (port, motor_cmd_rx)
    }

    /// This port with `build_trans` as its device's.
    pub fn with_build_trans(mut self, build_trans: Arc<dyn BuildTrans>) -> Self {
        self.build_trans = Some(build_trans);
        self
    }

    /// C `build_trans` on a soft-limit `command`; a port with no device
    /// verdict builds it.
    pub(crate) fn build_limit(&self, command: &MotorCommand, mres: f64) -> LimitBuild {
        let (high, raw) = match *command {
            MotorCommand::SetHighLimit { position } => (true, position),
            MotorCommand::SetLowLimit { position } => (false, position),
            _ => return LimitBuild::Built,
        };
        match &self.build_trans {
            Some(device) => device.limit(high, raw, mres),
            None => LimitBuild::Built,
        }
    }

    /// Whether the device defers `command`; a port with no device builds it.
    pub(crate) fn defers(&self, command: &MotorCommand) -> bool {
        self.build_trans.as_ref().is_some_and(|d| d.defers(command))
    }

    /// Queue `commands` on the poller thread, or refuse them as the port
    /// does for a missing axis. Each move-class command is counted pending
    /// first, as C `build_trans` does; a record in COMM_ALARM sends nothing.
    /// `pact` is the record's while it sends: set in process, clear in
    /// init_record and special.
    pub(crate) fn send(&self, commands: Vec<MotorCommand>, pact: bool) {
        let moves = commands.iter().filter(|c| c.is_move()).count();
        if let Ok(mut ds) = self.device_state.lock() {
            let comm_alarm = ds.comm_alarm();
            crate::poll_loop::build_trans_flow(&self.trace, &commands, pact, comm_alarm);
            if comm_alarm {
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
                let _ = motor_cmd_tx.send(
                    CommandBatch {
                        trace: self.trace.clone(),
                        record: Some(*record),
                        commands,
                    }
                    .into(),
                );
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
            if self.disables_ntm {
                motor_rec.timing.ntm = false;
            }
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
                "{} devMotorAsyn::update_values, needUpdate=1\n",
                self.port.trace.record()
            ),
        );

        // C init_record: the dset's init_controller, then
        // process_motor_info(initcall) and the drive sync, all before any
        // process. Each of its commands is a transaction of its own
        // (motordevCom.cc:327-343).
        if let Some(rec) = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>())
        {
            for command in rec.initial_readback(&status).commands {
                rec.send_special(vec![command]);
            }
        }

        // C init_record 717-718 ("Reset limits in case database values
        // are invalid"): set_dial_highlimit then set_dial_lowlimit.
        if let Some(rec) = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>())
        {
            rec.set_dial_limit(true);
            rec.set_dial_limit(false);
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
            TraceUser::Model1 { device, .. } | TraceUser::MotorInterface { device, .. } => device,
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
            gate.on_open(move || {
                open();
                epics_base_rs::runtime::interrupt_accept::EdgeDone::Inline
            });
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
