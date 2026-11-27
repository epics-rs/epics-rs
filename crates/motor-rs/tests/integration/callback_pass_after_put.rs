//! devMotorAsyn's `statusCallback` and `asynCallback` (devMotorAsyn.c:
//! 717-728, :756-764) process the record under `dbScanLock` on the
//! driver's thread, so a callback that has returned has processed, and a
//! put that follows it finds the record past that pass. motordevCom's
//! `motor_callback` (motordevCom.cc:555-557) processes it on the callback
//! band motordrvCom queues it on (motordrvCom.cc:347). A pass the callback asked
//! for must not land after a later put: with retries left it retries the
//! move in flight, with none it ends the move early.

use std::sync::Arc;
use std::time::Duration;

use asyn_rs::interfaces::motor::MotorStatus;
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::EpicsValue;
use motor_rs::MotorRecord;
use motor_rs::device_state::{
    AxisSubscribers, MotorDset, RecordId, SharedDeviceState, StampedStatus,
};
use motor_rs::device_support::MotorDeviceSupport;
use motor_rs::flags::{MotorCommand, MstaFlags};
use motor_rs::poll_loop::ToAxis;
use motor_rs::trace::{MotorTrace, TraceUser};
use tokio::sync::mpsc;

/// The retries a stale pass would spend: with some left it retries the
/// move, with none it ends it.
const RTRY: [i16; 2] = [10, 0];

struct Rig {
    db: Arc<PvDatabase>,
    axis: AxisSubscribers,
    record: RecordId,
    /// The pulses of the passes the record's callbacks asked for.
    io_intr: mpsc::Receiver<()>,
    /// The settle delay's pulse.
    delay_pulse: mpsc::Receiver<()>,
    _poll_cmd_rx: mpsc::Receiver<motor_rs::poll_loop::PollCommand>,
    motor_cmd_rx: mpsc::UnboundedReceiver<ToAxis>,
    state: SharedDeviceState,
}

fn at(position: f64) -> MotorStatus {
    MotorStatus {
        done: true,
        ..MotorStatus::default().with_position(position)
    }
}

impl Rig {
    /// `M1` bound to an axis at rest at 0 with `rtry` retries, past its
    /// startup pass and `finishDevSup`.
    async fn new(user: TraceUser, rtry: i16) -> Self {
        Self::build(user, rtry, 0.0, false).await
    }

    /// [`Self::new`] with a settle delay of `dly`, and with a post between
    /// the startup pass and `finishDevSup` when `post_before_init`.
    async fn build(user: TraceUser, rtry: i16, dly: f64, post_before_init: bool) -> Self {
        Self::assemble(user, rtry, dly, post_before_init, true).await
    }

    /// [`Self::build`], its callbacks bound to the database only if `bound`.
    async fn assemble(
        user: TraceUser,
        rtry: i16,
        dly: f64,
        post_before_init: bool,
        bound: bool,
    ) -> Self {
        let trace = Arc::new(MotorTrace::with_user(user));
        let state = motor_rs::device_state::new_shared_state();
        // As MotorAxis::attach binds a model-1 record.
        if trace.is_model1() {
            state.lock().unwrap().set_dset(MotorDset::MotordevCom);
        } else if trace.is_motor_interface() {
            state.lock().unwrap().set_dset(MotorDset::DevMotorSim);
        }
        let axis = AxisSubscribers::new();
        axis.store(at(0.0));
        let (io_intr_tx, io_intr) = mpsc::channel(16);
        let record = axis.attach(state.clone(), io_intr_tx, trace.clone());

        let mut rec = MotorRecord::new();
        rec.set_device_state(state.clone());
        rec.put_field("MRES", EpicsValue::Double(0.001)).unwrap();
        rec.put_field("DHLM", EpicsValue::Double(100.0)).unwrap();
        rec.put_field("DLLM", EpicsValue::Double(-100.0)).unwrap();
        rec.put_field("HLM", EpicsValue::Double(100.0)).unwrap();
        rec.put_field("LLM", EpicsValue::Double(-100.0)).unwrap();
        rec.put_field("VELO", EpicsValue::Double(100000.0)).unwrap();
        rec.put_field("ACCL", EpicsValue::Double(0.5)).unwrap();
        rec.put_field("MSTA", EpicsValue::ULong(MstaFlags::DONE.bits()))
            .unwrap();
        rec.put_field("RTRY", EpicsValue::Short(rtry)).unwrap();
        rec.put_field("DLY", EpicsValue::Double(dly)).unwrap();
        let (poll_cmd_tx, _poll_cmd_rx) = mpsc::channel(16);
        let (motor_cmd_tx, motor_cmd_rx) = mpsc::unbounded_channel();
        let mut dev = MotorDeviceSupport::new(
            trace,
            Duration::from_secs(1),
            poll_cmd_tx,
            motor_cmd_tx,
            state.clone(),
        );
        let db = Arc::new(PvDatabase::new());
        dev.set_record_info("M1", ScanType::Passive);
        if bound {
            dev.bind_database(db.async_handle());
        }
        dev.bind_dev_sup_gate(db.scan_control().dev_sup_gate());
        dev.init(&mut rec as &mut dyn Record).unwrap();
        let delay_pulse = dev.io_intr_receiver().unwrap();
        db.add_record("M1", Box::new(rec)).await.unwrap();
        {
            let arc = db.get_record("M1").unwrap();
            let mut inst = arc.write();
            inst.common.dtyp = "simMotor".into();
            inst.device = Some(Box::new(dev));
        }
        // The poller's startup post, before the gate.
        assert!(axis.post(at(0.0)).is_empty());
        db.process_record_readback("M1", &mut ProcStack::new())
            .await
            .unwrap();
        if post_before_init {
            assert!(axis.post(at(0.0)).is_empty());
        }
        db.scan_control().finish_dev_sup();
        let mut rig = Self {
            db,
            axis,
            record,
            io_intr,
            delay_pulse,
            _poll_cmd_rx,
            motor_cmd_rx,
            state,
        };
        rig.commands();
        rig
    }

    /// The poller's post, with the pulses it owes sent.
    fn post(&self, status: MotorStatus) {
        for io_intr in self.axis.post(status) {
            io_intr.try_send(()).unwrap();
        }
    }

    async fn put_val(&self, val: f64) {
        self.db
            .put_record_field_from_ca_no_notify("M1", "VAL", EpicsValue::Double(val))
            .await
            .unwrap();
    }

    /// One pass that no put triggers.
    async fn pass(&self) {
        self.db
            .process_record_readback("M1", &mut ProcStack::new())
            .await
            .unwrap();
    }

    /// The record's io_intr task: one pass per queued pulse.
    async fn serve_pulses(&mut self) {
        while self.io_intr.try_recv().is_ok() {
            self.pass().await;
        }
    }

    /// The commands the record's passes queued since the last call.
    fn commands(&mut self) -> Vec<MotorCommand> {
        let mut out = Vec::new();
        while let Ok(ToAxis::Batch(batch)) = self.motor_cmd_rx.try_recv() {
            out.extend(batch.commands);
        }
        out
    }

    fn field(&self, name: &str) -> EpicsValue {
        self.db.get_pv(format!("M1.{name}")).unwrap()
    }

    /// The moves the put sent once every queued pass has run, and the
    /// record then.
    async fn after_put(&mut self, val: f64) -> (Vec<MotorCommand>, String) {
        self.put_val(val).await;
        self.serve_pulses().await;
        self.outcome()
    }

    /// The moves queued since the last call, and the record now.
    fn outcome(&mut self) -> (Vec<MotorCommand>, String) {
        let moves = self
            .commands()
            .into_iter()
            .filter(|c| c.is_move())
            .collect();
        let fields = ["DMOV", "MIP", "RCNT", "DVAL", "DRBV"]
            .map(|f| format!("{f}={:?}", self.field(f)))
            .join(" ");
        (moves, fields)
    }
}

fn moved_to(moves: &[MotorCommand], raw: f64) -> bool {
    matches!(moves, [MotorCommand::MoveAbsolute { position, .. }] if *position == raw)
}

fn controller() -> TraceUser {
    TraceUser::Asyn {
        port: "cbAfterPut".into(),
        addr: 0,
    }
}

fn model1() -> TraceUser {
    TraceUser::Model1 {
        device: "devCbAfterPut",
        card: 0,
        signal: 0,
    }
}

fn motor_sim() -> TraceUser {
    TraceUser::MotorInterface {
        device: "devCbAfterPut",
        driver: "cbAfterPut",
        card: 0,
        signal: 0,
    }
}

/// The put's move, the only one sent, in flight: no retry counted.
fn assert_in_flight(rtry: i16, moves: &[MotorCommand], raw: f64, fields: &str) {
    assert!(moved_to(moves, raw), "RTRY {rtry}: {moves:?} / {fields}");
    assert!(
        fields.starts_with("DMOV=Short(0) MIP=UShort(32) RCNT=Short(0)"),
        "RTRY {rtry}: {fields}"
    );
}

/// `statusCallback` for a post before the put.
#[tokio::test]
async fn a_status_pass_does_not_land_after_a_later_put() {
    for rtry in RTRY {
        let mut rig = Rig::new(controller(), rtry).await;
        rig.post(at(0.0));
        let (moves, fields) = rig.after_put(10.0).await;
        assert_in_flight(rtry, &moves, 10000.0, &fields);
    }
}

/// `asynCallback` for the last pending move, before the next put.
#[tokio::test]
async fn a_move_done_pass_does_not_land_after_a_later_put() {
    for rtry in RTRY {
        let mut rig = Rig::new(controller(), rtry).await;
        rig.put_val(10.0).await;
        let mv = rig.commands().into_iter().find(|c| c.is_move()).unwrap();
        // The write's own post lands while the move is pending, then the
        // write returns.
        rig.post(at(10000.0));
        rig.axis.move_done(rig.record, &mv);
        assert_eq!(rig.field("DMOV"), EpicsValue::Short(1), "RTRY {rtry}");
        let (moves, fields) = rig.after_put(20.0).await;
        assert_in_flight(rtry, &moves, 20000.0, &fields);
    }
}

/// motordevCom's `motor_callback` for a post before the put.
#[tokio::test]
async fn a_model1_callback_pass_does_not_land_after_a_later_put() {
    for rtry in RTRY {
        let mut rig = Rig::new(model1(), rtry).await;
        rig.post(at(0.0));
        let (moves, fields) = rig.after_put(10.0).await;
        assert_in_flight(rtry, &moves, 10000.0, &fields);
    }
}

/// RRBV after a model-1 post at raw 5: motordrvCom hands the post to
/// `motor_callback` with `callbackRequest` (motordrvCom.cc:312, :347), and
/// none requested after `iocShutdown`'s `callbackStop` runs (iocInit.c:
/// 733-735).
async fn rrbv_after_a_model1_post(stop: bool) -> EpicsValue {
    let rig = Rig::new(model1(), 10).await;
    if stop {
        rig.db.scan_control().stop();
        rig.post(at(5.0));
        // absence-window: a post refused after the stop processes nothing.
        tokio::time::sleep(Duration::from_millis(600)).await;
    } else {
        rig.post(at(5.0));
        let deadline = epics_libcom_rs::runtime::task::deadline_from_now(fact_budget::FACT_BUDGET);
        while rig.field("RRBV") == EpicsValue::Long(0)
            && epics_libcom_rs::runtime::task::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    rig.field("RRBV")
}

#[tokio::test]
async fn a_model1_post_processes_the_record_while_the_ioc_runs() {
    assert_eq!(rrbv_after_a_model1_post(false).await, EpicsValue::Long(5));
}

#[tokio::test]
async fn a_model1_post_after_shutdown_processes_nothing() {
    assert_eq!(rrbv_after_a_model1_post(true).await, EpicsValue::Long(0));
}

/// devMotorSim's `motor_callback` for a post before the put: its
/// `scanOnce` pass follows the put, which already read the post
/// (motorRecord.cc:1276), so that pass reads nothing new
/// (devMotorSim.c:126-129, :215-223).
#[tokio::test]
async fn a_motor_sim_callback_pass_does_not_land_after_a_later_put() {
    for rtry in RTRY {
        let mut rig = Rig::new(motor_sim(), rtry).await;
        rig.post(at(0.0));
        let (moves, fields) = rig.after_put(10.0).await;
        assert_in_flight(rtry, &moves, 10000.0, &fields);
    }
}

/// `statusCallback` before `dbScanLockOK` copies with `needUpdate` and asks
/// for no pass (devMotorAsyn.c:763-766): the first put's pass reads it
/// (motorRecord.cc:1276), so a later pass reads nothing new.
#[tokio::test]
async fn a_post_before_init_is_read_by_the_first_put() {
    for rtry in RTRY {
        let mut rig = Rig::build(controller(), rtry, 0.0, true).await;
        rig.put_val(10.0).await;
        rig.pass().await;
        let (moves, fields) = rig.outcome();
        assert_in_flight(rtry, &moves, 10000.0, &fields);
    }
}

/// motordevCom's `motor_callback` copies under `ptrans->lock` before it
/// takes `dbScanLock` (motordevCom.cc:543-557): a put in between reads the
/// copy (`motor_update_values`, :420-438), and the callback's own pass then
/// reads nothing new.
#[tokio::test]
async fn a_put_between_a_model1_copy_and_its_pass_reads_the_copy() {
    for rtry in RTRY {
        let mut rig = Rig::new(model1(), rtry).await;
        let copy = StampedStatus {
            seq: 1 << 32,
            status: at(0.0),
        };
        rig.state.lock().unwrap().copy_post(&copy);
        rig.put_val(10.0).await;
        rig.pass().await;
        let (moves, fields) = rig.outcome();
        assert_in_flight(rtry, &moves, 10000.0, &fields);
    }
}

/// The settle delay's `callbackFunc` sets MIP_DELAY_ACK before its
/// `scanOnce` (motorRecord.cc:470-477): a put in between sees it and asks
/// for the status (1416-1426), and the expiry's own pass ends the wait and
/// moves to the put's target (1427-1430, 1487-1491).
#[tokio::test]
async fn a_put_after_the_settle_delay_expires_moves_on_the_expiry_pass() {
    let mut rig = Rig::build(controller(), 10, 0.01, false).await;
    rig.put_val(10.0).await;
    let mv = rig.commands().into_iter().find(|c| c.is_move()).unwrap();
    rig.post(at(10000.0));
    rig.axis.move_done(rig.record, &mv);
    assert_eq!(rig.field("MIP"), EpicsValue::UShort(0x420));
    tokio::time::timeout(Duration::from_secs(5), rig.delay_pulse.recv())
        .await
        .unwrap()
        .unwrap();
    rig.put_val(20.0).await;
    rig.pass().await;
    let (moves, fields) = rig.outcome();
    assert_in_flight(10, &moves, 20000.0, &fields);
}

/// Whether the settle delay's expiry asked for a pass, and MIP after one: C's
/// DLY is `callbackRequestDelayed` (motorRecord.cc:1437), requested when
/// its timer fires, and its `callbackFunc` turns DELAY_REQ into DELAY_ACK
/// (:460-480). One requested after `iocShutdown`'s `callbackStop`
/// (iocInit.c:733-735) never runs.
async fn the_settle_delay_expiry(stop: bool) -> (bool, EpicsValue) {
    let mut rig = Rig::build(controller(), 10, 0.3, false).await;
    rig.put_val(10.0).await;
    let mv = rig.commands().into_iter().find(|c| c.is_move()).unwrap();
    rig.post(at(10000.0));
    rig.axis.move_done(rig.record, &mv);
    assert_eq!(rig.field("MIP"), EpicsValue::UShort(0x420));
    let pulsed = if stop {
        rig.db.scan_control().stop();
        // absence-window: an expiry refused after the stop sends no pulse.
        tokio::time::timeout(Duration::from_millis(600), rig.delay_pulse.recv())
            .await
            .is_ok()
    } else {
        tokio::time::timeout(fact_budget::FACT_BUDGET, rig.delay_pulse.recv())
            .await
            .is_ok()
    };
    rig.pass().await;
    (pulsed, rig.field("MIP"))
}

#[tokio::test]
async fn the_settle_delay_expires_while_the_ioc_runs() {
    assert_eq!(
        the_settle_delay_expiry(false).await,
        (true, EpicsValue::UShort(0x820))
    );
}

#[tokio::test]
async fn a_settle_delay_expiring_after_shutdown_never_runs() {
    assert_eq!(
        the_settle_delay_expiry(true).await,
        (false, EpicsValue::UShort(0x420))
    );
}

/// [`the_settle_delay_expiry`] for a delay requested before the record's
/// callbacks are bound to its database, then bound: the expiry's
/// `callbackRequest` is judged against that database as any other's.
async fn the_settle_delay_requested_before_binding(stop: bool) -> (bool, EpicsValue) {
    let mut rig = Rig::assemble(controller(), 10, 0.3, false, false).await;
    rig.put_val(10.0).await;
    let mv = rig.commands().into_iter().find(|c| c.is_move()).unwrap();
    rig.post(at(10000.0));
    rig.axis.move_done(rig.record, &mv);
    rig.pass().await;
    assert_eq!(rig.field("MIP"), EpicsValue::UShort(0x420));
    rig.state
        .lock()
        .unwrap()
        .bind_database(rig.db.async_handle());
    let pulsed = if stop {
        rig.db.scan_control().stop();
        // absence-window: an expiry refused after the stop sends no pulse.
        tokio::time::timeout(Duration::from_millis(600), rig.delay_pulse.recv())
            .await
            .is_ok()
    } else {
        tokio::time::timeout(fact_budget::FACT_BUDGET, rig.delay_pulse.recv())
            .await
            .is_ok()
    };
    rig.pass().await;
    (pulsed, rig.field("MIP"))
}

#[tokio::test]
async fn a_settle_delay_requested_before_binding_expires_while_the_ioc_runs() {
    assert_eq!(
        the_settle_delay_requested_before_binding(false).await,
        (true, EpicsValue::UShort(0x820))
    );
}

#[tokio::test]
async fn a_settle_delay_requested_before_binding_never_runs_after_shutdown() {
    assert_eq!(
        the_settle_delay_requested_before_binding(true).await,
        (false, EpicsValue::UShort(0x420))
    );
}
