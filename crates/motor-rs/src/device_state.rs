use asyn_rs::interfaces::motor::MotorStatus;
use asyn_rs::trace::TraceMask;
use epics_base_rs::server::database::AsyncDbHandle;
use epics_base_rs::server::device_support::{IoIntrAction, IoIntrRun};
use epics_libcom_rs::cfmt::CFloat;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::controller_port::WriteValue;
use crate::trace::MotorTrace;

/// Stamped motor status with sequence number for change detection.
#[derive(Debug, Clone)]
pub struct StampedStatus {
    pub seq: u64,
    pub status: MotorStatus,
}

/// The stamps of one address's records list, which its cache and every
/// mailbox bound to it take: no stamp repeats there, whichever axis is at
/// the address.
#[derive(Debug, Clone, Default)]
struct Stamps(Arc<std::sync::atomic::AtomicU64>);

impl Stamps {
    /// The next stamp, past `seen` as well.
    fn after(&self, seen: u64) -> u64 {
        use std::sync::atomic::Ordering::SeqCst;
        let next = |last: u64| last.max(seen) + 1;
        next(self.0.update(SeqCst, SeqCst, next))
    }
}

/// Delay request with unique ID for stale timer prevention.
#[derive(Debug, Clone)]
pub struct DelayRequest {
    pub id: u64,
    pub duration: Duration,
}

/// Atomic bundle of actions from Record → DeviceSupport.
#[derive(Debug, Default)]
pub struct DeviceActions {
    pub commands: Vec<crate::flags::MotorCommand>,
    /// Poll now and post even an unchanged status, for
    /// [`ProcessEffects::request_poll`](crate::flags::ProcessEffects).
    pub force_poll: bool,
    pub schedule_delay: Option<DelayRequest>,
}

impl DeviceActions {
    /// Fold a newer batch into `self` when a previous batch has not yet been
    /// consumed by DeviceSupport.write(). Commands are appended (FIFO order
    /// preserved); a forced poll from either survives; schedule_delay takes
    /// the newer value if present. Prevents a dropped move command when two
    /// process() cycles run without an intervening write().
    pub fn merge_newer(&mut self, newer: DeviceActions) {
        self.commands.extend(newer.commands);
        self.force_poll |= newer.force_poll;
        if newer.schedule_delay.is_some() {
            self.schedule_delay = newer.schedule_delay;
        }
    }
}

/// Shared mailbox between MotorRecord, MotorDeviceSupport, and PollLoop.
///
/// Data flow:
///   PollLoop → [`Callbacks`] → latest_status, expired_delay_id → the
///   record's next pass, whatever processed it, reads
///   (motorRecord.cc:1276)
///   Record.process() → pending_actions → DeviceSupport.write() consumes
#[derive(Debug, Default)]
pub struct MotorDeviceState {
    /// C `pPvt->status` as the record's pass sees it; a new `seq` is
    /// `needUpdate` (devMotorAsyn.c:446).
    pub latest_status: Option<StampedStatus>,
    /// The `seq` [`Self::update_values`] last took: `needUpdate` is
    /// `latest_status` past it.
    consumed_seq: u64,
    /// Where `latest_status` stamps come from: the list the record is bound
    /// to, or its own while bound to none.
    stamps: Stamps,
    pub expired_delay_id: Option<u64>,
    /// The CDIR a model-1 driver left in `mr->cdir` while polling this
    /// record's move ([`AsynMotor::poll_in_motion`]), for the record's next
    /// pass to take.
    ///
    /// [`AsynMotor::poll_in_motion`]: asyn_rs::interfaces::motor::AsynMotor::poll_in_motion
    pub(crate) driver_cdir: Option<bool>,

    // Record → DeviceSupport
    pub pending_actions: Option<DeviceActions>,

    pub callbacks: Callbacks,
}

/// devMotorAsyn's per-record callback state (`motorAsynPvt`): the single
/// owner that turns a driver post, a finished move or the settle delay into
/// a record process.
///
/// devMotorAsyn's `statusCallback` and `asynCallback`, and motordevCom's
/// `motor_callback`, `dbProcess` under `dbScanLock` on the driver's thread
/// (`call_back`). The settle delay and a devMotorSim callback leave
/// their state for the next pass and ask for one on the record's io_intr
/// task (`scanOnce`).
#[derive(Debug, Default)]
pub struct Callbacks {
    /// `pPvt->status`: the last status posted.
    status: MotorStatus,
    /// `pPvt->moveRequestPending`.
    move_request_pending: u32,
    /// devMotorAsyn's `dbScanLockOK`, here its database's `finishDevSup` gate.
    accepting: bool,
    /// `pmr->stat == COMM_ALARM` as the record's last pass committed it.
    comm_alarm: bool,
    dset: MotorDset,
    /// The record's database, which [`call_back`] locks it in.
    db: Option<RecordDb>,
}

/// The C device support a record's callbacks run as.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MotorDset {
    #[default]
    DevMotorAsyn,
    /// Model-1 motordevCom.
    MotordevCom,
    DevMotorSim,
}

impl MotorDset {
    /// motordevCom and devMotorSim: `build_trans` has no COMM_ALARM check
    /// and no pending-move count, and only the driver's callback processes
    /// the record.
    fn is_drvset(self) -> bool {
        self != Self::DevMotorAsyn
    }
}

/// The database a record's callbacks take `dbScanLock(pmr)` in.
#[derive(Clone)]
struct RecordDb(AsyncDbHandle);

impl std::fmt::Debug for RecordDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecordDb")
    }
}

/// Run a driver callback on `record`'s mailbox as C runs it: once
/// `dbScanLockOK`, `locked` under `dbScanLock(pmr)` on the calling thread, a
/// [`IoIntrAction::Process`] answer `dbProcess` inside the hold
/// (devMotorAsyn.c:717-726, :755-762, motordevCom.cc:555-557); before it,
/// or with no record to hold, `before_init` with no lock.
pub(crate) fn call_back(
    state: &SharedDeviceState,
    record: &str,
    locked: impl FnOnce(&mut MotorDeviceState) -> IoIntrAction + Send + 'static,
    before_init: impl FnOnce(&mut MotorDeviceState),
) {
    let db = state.lock().ok().and_then(|ds| ds.callbacks.db.clone());
    let held = state.clone();
    let run = db.map_or(IoIntrRun::Refused, |db| {
        db.0.run_io_intr_callback_with(record, move |_| {
            held.lock()
                .map_or(IoIntrAction::Skip, |mut ds| locked(&mut ds))
        })
    });
    if matches!(run, IoIntrRun::Refused)
        && let Ok(mut ds) = state.lock()
    {
        before_init(&mut ds);
    }
}

/// C `asynCallback`'s tail for a finished move-class command
/// (devMotorAsyn.c:717-729). motordevCom has none: only the driver's
/// `motor_callback` processes its record (motordevCom.cc:534-556).
pub(crate) fn async_callback(
    state: &SharedDeviceState,
    record: &str,
    command: &crate::flags::MotorCommand,
) {
    if state
        .lock()
        .map_or(true, |ds| ds.callbacks.dset.is_drvset())
    {
        return;
    }
    call_back(state, record, MotorDeviceState::move_done, |ds| {
        ds.move_done_before_init(command)
    });
}

impl MotorDeviceState {
    /// C `statusCallback` (devMotorAsyn.c:755-762) under the record's lock
    /// (`call_back`): copy the post, and unless a move is pending process
    /// with `needUpdate`.
    pub fn status_callback(&mut self, status: StampedStatus) -> IoIntrAction {
        self.callbacks.status = status.status.clone();
        if self.callbacks.move_request_pending > 0 {
            return IoIntrAction::Skip;
        }
        self.latest_status = Some(status);
        IoIntrAction::Process
    }

    /// Copy a post with `needUpdate`: `statusCallback` before
    /// `dbScanLockOK` (devMotorAsyn.c:763-766), motordevCom's
    /// `motor_callback` under `ptrans->lock` (motordevCom.cc:541-548).
    pub fn copy_post(&mut self, status: &StampedStatus) {
        self.callbacks.status = status.status.clone();
        self.latest_status = Some(status.clone());
    }

    /// A devMotorSim callback (devMotorSim.c:126-129): copy the post with
    /// `needUpdate`, and past the gate ask for its `scanOnce`. True when the
    /// caller must pulse io_intr.
    fn motor_sim_callback(&mut self, status: &StampedStatus) -> bool {
        self.copy_post(status);
        self.callbacks.accepting
    }

    /// C `build_trans` queuing a move-class command (devMotorAsyn.c:547,
    /// :565, :582). motordevCom's `motor_end_trans_com` counts nothing.
    pub fn move_queued(&mut self) {
        if !self.callbacks.dset.is_drvset() {
            self.callbacks.move_request_pending += 1;
        }
    }

    /// C `asynCallback`'s tail for a move-class command, whatever the write
    /// returned, under the record's lock (devMotorAsyn.c:717-726,
    /// `async_callback`): the last move to finish processes the record
    /// with the last status posted.
    pub fn move_done(&mut self) -> IoIntrAction {
        let callbacks = &mut self.callbacks;
        callbacks.move_request_pending = callbacks.move_request_pending.saturating_sub(1);
        if callbacks.move_request_pending > 0 {
            return IoIntrAction::Skip;
        }
        self.latest_status = Some(self.stamp(self.callbacks.status.clone()));
        IoIntrAction::Process
    }

    /// `asynCallback` before `dbScanLockOK` (devMotorAsyn.c:728-729): a
    /// SET_POSITION clears the pending count.
    pub fn move_done_before_init(&mut self, command: &crate::flags::MotorCommand) {
        if matches!(command, crate::flags::MotorCommand::SetPosition { .. }) {
            self.callbacks.move_request_pending = 0;
        }
    }

    /// devMotorSim's GET_INFO (devMotorSim.c build_trans): `needUpdate` on
    /// the status last called back, with no process asked for.
    ///
    /// C defect, not copied: on an idle axis nothing processes the record
    /// for this, so C's next put (after a no-op pass armed motorRecord's
    /// implicit GET_INFO, e.g. SPMG Go) reads it as CALLBACK_DATA with STUP
    /// BUSY, skips do_work (motorRecord.cc:1345, 1487) and the move is
    /// lost with DMOV 0. The record runs do_work on that pass.
    pub fn get_info(&mut self) {
        self.latest_status = Some(self.stamp(self.callbacks.status.clone()));
    }

    /// The settle delay expired, for the next pass to read. True when the
    /// caller must pulse io_intr; before the gate the expiry only waits for
    /// the next pass.
    pub fn delay_expired(&mut self, id: u64) -> bool {
        self.expired_delay_id = Some(id);
        self.callbacks.accepting
    }

    /// The record's `stat` as its passes before this batch committed it;
    /// COMM_ALARM or not is all `build_trans` reads of it.
    pub fn set_comm_alarm(&mut self, comm_alarm: bool) {
        self.callbacks.comm_alarm = comm_alarm;
    }

    /// The C device support the record's callbacks run as.
    pub fn set_dset(&mut self, dset: MotorDset) {
        self.callbacks.dset = dset;
    }

    /// Whether `build_trans` refuses a record whose stat is COMM_ALARM
    /// (devMotorAsyn.c:529-532); motordevCom queues regardless.
    pub(crate) fn refuses_in_comm_alarm(&self) -> bool {
        !self.callbacks.dset.is_drvset()
    }

    /// C `build_trans` refusing a record in COMM_ALARM: it queues nothing,
    /// so counts no move pending either.
    pub(crate) fn comm_alarm(&self) -> bool {
        self.callbacks.comm_alarm && self.refuses_in_comm_alarm()
    }

    /// `finishDevSup` ran: callbacks process the record from now on.
    pub fn accept(&mut self) {
        self.callbacks.accepting = true;
    }

    /// The record's database, which its callbacks lock it in.
    pub fn bind_database(&mut self, db: AsyncDbHandle) {
        self.callbacks.db = Some(RecordDb(db));
    }

    /// The axis' cached status, as `readGenericPointer` returns it at bind.
    fn seed(&mut self, status: &StampedStatus) {
        self.latest_status = Some(status.clone());
        self.callbacks.status = status.status.clone();
    }

    /// C `update_values` (devMotorAsyn.c:437-480): the status a pending
    /// `needUpdate` carries, clearing it.
    pub fn update_values(&mut self) -> Option<MotorStatus> {
        let stamped = self
            .latest_status
            .as_ref()
            .filter(|s| s.seq != self.consumed_seq)?;
        self.consumed_seq = stamped.seq;
        Some(stamped.status.clone())
    }

    /// The status init_record reads, then takes with motorRecord's
    /// init-time `update_values` (motorRecord.cc:690): the axis' cache, or
    /// C's calloc-zeroed status for a record bound to no axis.
    pub fn init_status(&mut self) -> MotorStatus {
        let stamps = &self.stamps;
        let stamped = self.latest_status.get_or_insert_with(|| StampedStatus {
            seq: stamps.after(0),
            status: MotorStatus::default(),
        });
        self.consumed_seq = stamped.seq;
        stamped.status.clone()
    }

    fn need_update(&self) -> bool {
        self.latest_status
            .as_ref()
            .is_some_and(|s| s.seq != self.consumed_seq)
    }

    /// `status` under a new stamp from the record's list.
    fn stamp(&self, status: MotorStatus) -> StampedStatus {
        let seen = self.latest_status.as_ref().map_or(0, |s| s.seq);
        let seq = self.stamps.after(seen);
        StampedStatus { seq, status }
    }
}

pub type SharedDeviceState = Arc<Mutex<MotorDeviceState>>;

pub fn new_shared_state() -> SharedDeviceState {
    Arc::new(Mutex::new(MotorDeviceState::default()))
}

/// C `MOTOR_AXIS_NUM_PARAMS` (motor_interface.h:335): the parameters a
/// model-2 driver's paramLib holds, `motorAxisPosition` through
/// `motorAxisMoveToHome`.
const MOTOR_AXIS_NUM_PARAMS: i32 = 28;

/// A model-2 axis's parameter library (C paramLib), by drvMotorAsyn reason:
/// what the driver's `setDouble` / `setInteger` stored last. The axis's
/// poller writes it; the driver's report reads it.
#[derive(Clone, Default)]
pub struct Model2Params(Arc<Mutex<std::collections::HashMap<i32, WriteValue>>>);

impl Model2Params {
    /// C `paramSet`: whether it changed the parameter's type or value
    /// (paramLib.c:112-118, 141-147).
    pub(crate) fn set(&self, reason: i32, value: WriteValue) -> bool {
        let mut params = self.0.lock().unwrap_or_else(|e| e.into_inner());
        params.insert(reason, value) != Some(value)
    }

    /// C `getDouble` / `getInteger`; `None` while undefined.
    pub fn get(&self, reason: i32) -> Option<WriteValue> {
        let params = self.0.lock().unwrap_or_else(|e| e.into_inner());
        params.get(&reason).copied()
    }

    /// C `paramDump` (paramLib.c:299-317).
    pub fn dump(&self, out: &mut dyn std::fmt::Write) {
        use epics_libcom_rs::cfmt::CFloat;
        let params = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(out, "Number of parameters is: {MOTOR_AXIS_NUM_PARAMS}");
        for i in 0..MOTOR_AXIS_NUM_PARAMS {
            let _ = match params.get(&i) {
                Some(WriteValue::Float64(v)) => {
                    writeln!(out, "Parameter {i} is a double, value {}", CFloat(*v))
                }
                Some(WriteValue::Int32(v)) => {
                    writeln!(out, "Parameter {i} is an integer, value {v}")
                }
                None => writeln!(out, "Parameter {i} is undefined"),
            };
        }
    }
}

/// A record bound to an axis, by the order it attached.
pub type RecordId = usize;

/// The records bound to one axis, C's `registerInterruptUser` list on its
/// `motorStatus` parameter: every status the poller posts reaches each of
/// them (devMotorAsyn.c:376-387, asynMotorAxis.cpp:316-322).
///
/// C defect, not copied: a `motor_interface.h` axis has one paramLib
/// callback slot (paramLib.c:229), which each devMotorSim init_record
/// retakes (devMotorSim.c:173), drvAsynMotorConfigure's included
/// (drvMotorAsyn.c:412); the records bound before the last never process
/// from the driver again, and a move leaves their DMOV 0.
#[derive(Clone, Default)]
pub struct AxisSubscribers(Arc<Mutex<Subscribed>>);

#[derive(Default)]
struct Subscribed {
    stamps: Stamps,
    /// The axis' parameter cache: what `readGenericPointer` returns to a
    /// record that binds after the poller primed it.
    latest: Option<StampedStatus>,
    /// The axis's status parameters as its poller last set them, posted or
    /// not: what every poll of the axis writes into, on the poller or a
    /// controller port's thread.
    status: MotorStatus,
    records: Vec<Bound>,
}

/// One bound record: its mailbox, its io_intr pulse and the user its
/// `statusCallback` prints through.
struct Bound {
    device_state: SharedDeviceState,
    io_intr: tokio::sync::mpsc::Sender<()>,
    trace: Arc<MotorTrace>,
}

/// The io_intr pulses a devMotorSim post owes, one per
/// process.
pub type Pulses = Vec<tokio::sync::mpsc::Sender<()>>;

impl AxisSubscribers {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Subscribed> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Bind a record: its mailbox starts from the axis' cache, and every
    /// later post reaches its [`Callbacks`].
    pub fn attach(
        &self,
        device_state: SharedDeviceState,
        io_intr: tokio::sync::mpsc::Sender<()>,
        trace: Arc<MotorTrace>,
    ) -> RecordId {
        let mut subscribed = self.lock();
        if let Ok(mut ds) = device_state.lock() {
            ds.stamps = subscribed.stamps.clone();
            if let Some(latest) = &subscribed.latest {
                ds.seed(latest);
            }
        }
        subscribed.records.push(Bound {
            device_state,
            io_intr,
            trace,
        });
        subscribed.records.len() - 1
    }

    /// [`Self::set_status`]'s last.
    pub(crate) fn status(&self) -> MotorStatus {
        self.lock().status.clone()
    }

    /// Set the axis's status parameters without a callback. Only its
    /// poller's `Axis::stage` and construction set them.
    pub(crate) fn set_status(&self, status: MotorStatus) {
        self.lock().status = status;
    }

    /// How many records are bound; their ids are `0..len`.
    pub fn len(&self) -> usize {
        self.lock().records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fill the cache alone, the axis status `readGenericPointer` returns
    /// to a record that binds later (asynMotorController.cpp:505-525): what
    /// an axis's constructor set, which calls no record back
    /// (asynMotorAxis.cpp:31-68).
    pub fn store(&self, status: MotorStatus) {
        let mut subscribed = self.lock();
        let seq = subscribed.stamps.after(0);
        subscribed.latest = Some(StampedStatus { seq, status });
    }

    /// Fill the cache and run every bound record's `statusCallback` or
    /// `motor_callback`; the pulses of the devMotorSim passes it asked for
    /// are owed.
    pub fn post(&self, status: MotorStatus) -> Pulses {
        let mut subscribed = self.lock();
        let seq = subscribed.stamps.after(0);
        let status = StampedStatus { seq, status };
        let mut pulses = Pulses::new();
        for bound in &subscribed.records {
            let Ok(mut ds) = bound.device_state.lock() else {
                continue;
            };
            print_status_callback(&bound.trace, &ds, &status.status);
            let record = bound.trace.record();
            match ds.callbacks.dset {
                MotorDset::DevMotorAsyn => {
                    drop(ds);
                    let locked = status.clone();
                    call_back(
                        &bound.device_state,
                        record,
                        move |ds| ds.status_callback(locked),
                        |ds| ds.copy_post(&status),
                    );
                }
                MotorDset::MotordevCom => {
                    ds.copy_post(&status);
                    drop(ds);
                    let process = |_: &mut MotorDeviceState| IoIntrAction::Process;
                    call_back(&bound.device_state, record, process, |_| {});
                }
                MotorDset::DevMotorSim => {
                    if ds.motor_sim_callback(&status) {
                        pulses.push(bound.io_intr.clone());
                    }
                }
            }
        }
        subscribed.latest = Some(status);
        pulses
    }

    /// The stamp of the cache's last fill.
    #[cfg(test)]
    pub(crate) fn stamp(&self) -> u64 {
        self.lock().latest.as_ref().map_or(0, |latest| latest.seq)
    }

    /// The CDIR a model-1 driver wrote into `record` while polling its move.
    pub(crate) fn set_driver_cdir(&self, record: RecordId, cdir: bool) {
        let subscribed = self.lock();
        if let Some(mut ds) = subscribed
            .records
            .get(record)
            .and_then(|bound| bound.device_state.lock().ok())
        {
            ds.driver_cdir = Some(cdir);
        }
    }

    /// `record`'s GET_INFO on a devMotorSim axis ([`MotorDeviceState::get_info`]).
    pub(crate) fn get_info(&self, record: RecordId) {
        let subscribed = self.lock();
        if let Some(mut ds) = subscribed
            .records
            .get(record)
            .and_then(|bound| bound.device_state.lock().ok())
        {
            ds.get_info();
        }
    }

    /// `record`'s move-class command finished: its `asynCallback`
    /// (`async_callback`).
    pub fn move_done(&self, record: RecordId, command: &crate::flags::MotorCommand) {
        let Some((state, trace)) = self
            .lock()
            .records
            .get(record)
            .map(|bound| (bound.device_state.clone(), bound.trace.clone()))
        else {
            return;
        };
        async_callback(&state, trace.record(), command);
    }
}

/// C `statusCallback`'s line (devMotorAsyn.c:750-754), printed before the
/// copy: `N` for a `needUpdate` no pass has taken, `P` for a pending move.
fn print_status_callback(trace: &MotorTrace, ds: &MotorDeviceState, status: &MotorStatus) {
    trace.print(
        TraceMask::IO_DEVICE,
        trace.status_reason(),
        format_args!(
            "{} devMotorAsyn::statusCallback new value=[p:{:.6},e:{:.6},s:{:x}] {}{}\n",
            trace.record(),
            CFloat(status.position),
            CFloat(status.encoder_position),
            crate::flags::MstaFlags::from_status(status).bits(),
            if ds.need_update() { 'N' } else { ' ' },
            if ds.callbacks.move_request_pending > 0 {
                'P'
            } else {
                ' '
            },
        ),
    );
}

/// A record in a database past `finishDevSup` for each state a harness
/// binds, to see each `dbProcess` a callback runs and what it read.
#[cfg(test)]
pub(crate) mod passes {
    use super::*;
    use epics_base_rs::error::CaResult;
    use epics_base_rs::server::database::PvDatabase;
    use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
    use epics_base_rs::server::record::{Record, ScanType};
    use epics_base_rs::server::records::ai::AiRecord;

    type Read = Arc<Mutex<Vec<Option<MotorStatus>>>>;

    /// One record's processes, each with its mailbox's new status, if it
    /// read one. Holds the record's database.
    pub(crate) struct Reads(Read, Arc<PvDatabase>);

    impl Reads {
        pub(crate) fn take(&self) -> Vec<Option<MotorStatus>> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
    }

    /// Takes the mailbox as the motor record's callback pass does.
    struct ReadsMailbox(Option<SharedDeviceState>, Read);

    impl DeviceSupport for ReadsMailbox {
        fn dtyp(&self) -> &str {
            "readsMailbox"
        }
        fn read(&mut self, _: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
            let read = self
                .0
                .as_ref()
                .and_then(|state| state.lock().unwrap().update_values());
            self.1.lock().unwrap().push(read);
            Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
        }
        fn write(&mut self, _: &mut dyn Record) -> CaResult<()> {
            Ok(())
        }
    }

    const NAME: &str = "R";

    /// Record [`NAME`] reading `mailbox`, past the gate if `finished`.
    fn record(mailbox: Option<SharedDeviceState>, finished: bool) -> Reads {
        let db = Arc::new(PvDatabase::new());
        // A thread of its own, which may block whatever runtime the test runs.
        std::thread::scope(|s| {
            s.spawn(|| {
                epics_base_rs::runtime::task::block_on_sync(
                    db.add_record(NAME, Box::new(AiRecord::new(0.0))),
                )
            })
            .join()
        })
        .unwrap()
        .unwrap()
        .unwrap();
        let read = Read::default();
        {
            let rec = db.get_record(NAME).unwrap();
            let mut inst = rec.write();
            inst.common.dtyp = "readsMailbox".into();
            inst.device = Some(Box::new(ReadsMailbox(mailbox, read.clone())));
        }
        if finished {
            db.scan_control().finish_dev_sup();
        }
        Reads(read, db)
    }

    /// `state` past the gate, its callbacks processing a record named into
    /// `trace`.
    pub(crate) fn bind(state: &SharedDeviceState, trace: &MotorTrace) -> Reads {
        let reads = bind_before_init(state, trace);
        reads.1.scan_control().finish_dev_sup();
        state.lock().unwrap().accept();
        reads
    }

    /// [`bind`], before `finishDevSup`.
    pub(crate) fn bind_before_init(state: &SharedDeviceState, trace: &MotorTrace) -> Reads {
        let reads = record(Some(state.clone()), false);
        trace.set_record(NAME);
        assert_eq!(trace.record(), NAME, "one record per trace");
        state.lock().unwrap().bind_database(reads.1.async_handle());
        reads
    }

    /// `support`'s callbacks processing a record that reads `mailbox`.
    pub(crate) fn bind_support(
        support: &mut dyn DeviceSupport,
        mailbox: Option<SharedDeviceState>,
    ) -> Reads {
        let reads = record(mailbox, true);
        support.set_record_info(NAME, ScanType::Passive);
        support.bind_database(reads.1.async_handle());
        reads
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags::MotorCommand;

    fn seq_of(state: &SharedDeviceState) -> Option<u64> {
        state.lock().unwrap().latest_status.as_ref().map(|s| s.seq)
    }

    fn accepting() -> SharedDeviceState {
        let state = new_shared_state();
        state.lock().unwrap().accept();
        state
    }

    fn at(position: f64) -> MotorStatus {
        MotorStatus {
            position,
            ..Default::default()
        }
    }

    /// A post of `position` from the list `ds` draws its stamps from.
    fn posted(ds: &MotorDeviceState, position: f64) -> StampedStatus {
        StampedStatus {
            seq: ds.stamps.after(0),
            status: at(position),
        }
    }

    /// devMotorAsyn's `statusCallback` with a post of `position`.
    fn post_to(ds: &mut MotorDeviceState, position: f64) -> IoIntrAction {
        let posted = posted(ds, position);
        ds.status_callback(posted)
    }

    /// devMotorSim's `motor_callback` with a post of `position`.
    fn sim_post(ds: &mut MotorDeviceState, position: f64) -> bool {
        let posted = posted(ds, position);
        ds.motor_sim_callback(&posted)
    }

    /// The position the next pass reads, taking it.
    fn read(ds: &mut MotorDeviceState) -> Option<f64> {
        ds.update_values().map(|status| status.position)
    }

    fn move_to() -> MotorCommand {
        MotorCommand::MoveAbsolute {
            position: 1.0,
            min_velocity: 0.0,
            velocity: 1.0,
            acceleration: 1.0,
        }
    }

    #[test]
    fn an_axis_with_no_record_keeps_its_post_for_the_first_to_bind() {
        let axis = AxisSubscribers::new();
        assert!(axis.post(at(1.5)).is_empty());
        let state = accepting();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        assert_eq!(axis.attach(state.clone(), tx, Arc::default()), 0);
        assert_eq!(seq_of(&state), Some(1));
        assert!(
            rx.try_recv().is_err(),
            "binding does not process the record"
        );
    }

    #[test]
    fn store_fills_the_cache_alone() {
        let axis = AxisSubscribers::new();
        let bound = accepting();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        axis.attach(bound.clone(), tx, Arc::default());
        axis.store(at(2.0));
        assert_eq!(seq_of(&bound), None);
        assert!(rx.try_recv().is_err());
        let later = accepting();
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        axis.attach(later.clone(), tx, Arc::default());
        let latest = later.lock().unwrap().latest_status.clone().unwrap();
        assert_eq!(latest.status.position, 2.0);
    }

    #[test]
    fn every_queued_record_on_the_axis_gets_one_pass_per_post() {
        let axis = AxisSubscribers::new();
        let (a, b) = (accepting(), accepting());
        a.lock().unwrap().set_dset(MotorDset::DevMotorSim);
        b.lock().unwrap().set_dset(MotorDset::DevMotorSim);
        let (tx_a, _rx_a) = tokio::sync::mpsc::channel(1);
        let (tx_b, _rx_b) = tokio::sync::mpsc::channel(1);
        assert_eq!(axis.attach(a.clone(), tx_a, Arc::default()), 0);
        assert_eq!(axis.attach(b.clone(), tx_b, Arc::default()), 1);
        assert_eq!(axis.post(at(1.0)).len(), 2);
        assert_eq!(axis.post(at(2.0)).len(), 2);
        // devMotorSim.c:126-129 updates the status in place: the first
        // pass reads the last post, the second nothing.
        for state in [&a, &b] {
            let mut ds = state.lock().unwrap();
            assert_eq!(read(&mut ds), Some(2.0));
            assert_eq!(read(&mut ds), None);
        }
    }

    /// A finished move's needUpdate draws from the record's list, so the
    /// address's next post is still new to the record.
    #[test]
    fn a_post_after_a_finished_move_is_new_to_the_record() {
        let axis = AxisSubscribers::new();
        let state = accepting();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        axis.attach(state.clone(), tx, Arc::default());
        let read = |state: &SharedDeviceState| read(&mut state.lock().unwrap());
        axis.post(at(1.0));
        assert_eq!(read(&state), Some(1.0));
        {
            let mut ds = state.lock().unwrap();
            ds.move_queued();
            assert_eq!(ds.move_done(), IoIntrAction::Process);
        }
        assert_eq!(read(&state), Some(1.0));
        axis.post(at(3.0));
        assert_eq!(read(&state), Some(3.0));
    }

    #[test]
    fn a_post_before_the_gate_copies_with_need_update_and_asks_no_process() {
        let axis = AxisSubscribers::new();
        let state = new_shared_state();
        let trace = Arc::<MotorTrace>::default();
        let reads = passes::bind_before_init(&state, &trace);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        axis.attach(state.clone(), tx, trace);
        let mut ds = state.lock().unwrap();
        ds.move_queued();
        drop(ds);
        assert!(axis.post(at(3.0)).is_empty());
        assert!(reads.take().is_empty(), "a post before the gate processed");
        assert_eq!(read(&mut state.lock().unwrap()), Some(3.0));
    }

    #[test]
    fn a_post_past_the_gate_processes_with_it_and_queues_nothing() {
        let state = accepting();
        let mut ds = state.lock().unwrap();
        assert_eq!(post_to(&mut ds, 1.0), IoIntrAction::Process);
        assert_eq!(read(&mut ds), Some(1.0));
    }

    /// devMotorSim.c:126-129, :215-223: the first pass reads the last post,
    /// whatever processed the record.
    #[test]
    fn a_motor_sim_pass_reads_the_last_post() {
        let state = accepting();
        let mut ds = state.lock().unwrap();
        assert!(sim_post(&mut ds, 1.0));
        assert!(sim_post(&mut ds, 2.0));
        assert_eq!(read(&mut ds), Some(2.0));
        assert_eq!(read(&mut ds), None);
    }

    #[test]
    fn a_post_while_one_move_is_pending_waits_for_its_completion() {
        let state = accepting();
        let mut ds = state.lock().unwrap();
        ds.move_queued();
        assert_eq!(post_to(&mut ds, 1.0), IoIntrAction::Skip);
        assert_eq!(post_to(&mut ds, 2.0), IoIntrAction::Skip);
        assert_eq!(read(&mut ds), None, "a copy alone is no needUpdate");
        assert_eq!(ds.move_done(), IoIntrAction::Process);
        assert_eq!(read(&mut ds), Some(2.0));
    }

    #[test]
    fn only_the_last_of_two_pending_moves_processes() {
        let state = accepting();
        let mut ds = state.lock().unwrap();
        ds.move_queued();
        ds.move_queued();
        assert_eq!(ds.move_done(), IoIntrAction::Skip);
        assert_eq!(post_to(&mut ds, 5.0), IoIntrAction::Skip);
        assert_eq!(ds.move_done(), IoIntrAction::Process);
        assert_eq!(read(&mut ds), Some(5.0));
        assert_eq!(
            post_to(&mut ds, 6.0),
            IoIntrAction::Process,
            "nothing pending"
        );
        assert_eq!(read(&mut ds), Some(6.0));
    }

    // motordevCom: a move processes nothing itself, and every driver
    // callback processes the record (motordevCom.cc:534-556).
    #[test]
    fn a_motordevcom_move_waits_for_the_driver_callback() {
        let axis = AxisSubscribers::new();
        let state = new_shared_state();
        let trace = Arc::<MotorTrace>::default();
        let reads = passes::bind(&state, &trace);
        let mut ds = state.lock().unwrap();
        ds.set_dset(MotorDset::MotordevCom);
        ds.move_queued();
        drop(ds);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let record = axis.attach(state.clone(), tx, trace);
        axis.move_done(record, &move_to());
        assert!(reads.take().is_empty(), "a motordevCom move processed");
        assert!(axis.post(at(1.0)).is_empty());
        let read: Vec<_> = reads
            .take()
            .into_iter()
            .map(|s| s.map(|s| s.position))
            .collect();
        assert_eq!(read, [Some(1.0)]);
        assert!(rx.try_recv().is_err(), "a motordevCom callback queued");
    }

    #[test]
    fn before_the_gate_only_a_set_position_clears_the_pending_count() {
        let state = new_shared_state();
        let trace = MotorTrace::default();
        let reads = passes::bind_before_init(&state, &trace);
        let mut ds = state.lock().unwrap();
        ds.move_queued();
        ds.move_queued();
        drop(ds);
        let pending = || state.lock().unwrap().callbacks.move_request_pending;
        async_callback(&state, trace.record(), &move_to());
        assert_eq!(pending(), 2);
        async_callback(
            &state,
            trace.record(),
            &MotorCommand::SetPosition { position: 0.0 },
        );
        assert_eq!(pending(), 0);
        assert!(reads.take().is_empty());
    }

    /// C sets MIP_DELAY_ACK beside a pending `needUpdate`
    /// (motorRecord.cc:470-477): the next pass reads both (1276, 1280).
    #[test]
    fn a_delay_expiry_and_a_post_before_it_reach_the_next_pass() {
        let state = accepting();
        let mut ds = state.lock().unwrap();
        assert!(sim_post(&mut ds, 1.0));
        assert!(ds.delay_expired(7));
        assert_eq!(read(&mut ds), Some(1.0));
        assert_eq!(ds.expired_delay_id, Some(7));
    }

    #[test]
    fn merge_newer_appends_commands_fifo() {
        let mut a = DeviceActions {
            commands: vec![MotorCommand::Stop { acceleration: 1.0 }],
            ..Default::default()
        };
        let b = DeviceActions {
            commands: vec![MotorCommand::Poll],
            ..Default::default()
        };
        a.merge_newer(b);
        assert_eq!(a.commands.len(), 2);
        // earlier batch's command stays first
        assert!(matches!(a.commands[0], MotorCommand::Stop { .. }));
        assert!(matches!(a.commands[1], MotorCommand::Poll));
    }

    #[test]
    fn merge_newer_keeps_a_forced_poll_from_either_batch() {
        for (older, newer) in [(true, false), (false, true)] {
            let mut a = DeviceActions {
                force_poll: older,
                ..Default::default()
            };
            a.merge_newer(DeviceActions {
                force_poll: newer,
                ..Default::default()
            });
            assert!(a.force_poll);
        }
    }

    #[test]
    fn merge_newer_takes_newer_delay() {
        let mut a = DeviceActions::default();
        let b = DeviceActions {
            schedule_delay: Some(DelayRequest {
                id: 7,
                duration: Duration::from_millis(5),
            }),
            ..Default::default()
        };
        a.merge_newer(b);
        assert_eq!(a.schedule_delay.as_ref().unwrap().id, 7);
    }
}
