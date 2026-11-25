//! An input on an `ASYN_CANBLOCK` port processes as C devAsyn's does:
//! `processAi` sets PACT, queues the record's request and returns
//! (devAsynInt32.c:836-841); the port thread reads and calls
//! `callbackRequestProcessCallback(&pPvt->processCallback, pr->prio, pr)`
//! (:500), and the second pass applies the result. The read used to run
//! inline under the record's hold, stalling the scan thread on the port.
#![cfg(feature = "epics")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::adapter::universal_asyn_factory;
use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, PiniMode, Record, ScanType};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

type Threads = Arc<Mutex<Vec<String>>>;

fn this_thread() -> String {
    std::thread::current().name().unwrap_or("").to_string()
}

#[derive(Clone, Default)]
struct Calls {
    reads: Threads,
    writes: Threads,
    time_out: Arc<AtomicBool>,
}

struct Port {
    base: PortDriverBase,
    calls: Calls,
}

impl PortDriver for Port {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn io_read_float64(&mut self, _user: &AsynUser) -> AsynResult<f64> {
        self.calls.reads.lock().unwrap().push(this_thread());
        if self.calls.time_out.load(Ordering::SeqCst) {
            return Err(AsynError::Status {
                status: AsynStatus::Timeout,
                message: "no reply".into(),
            });
        }
        Ok(7.0)
    }
    fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.calls.writes.lock().unwrap().push(this_thread());
        Ok(data.len())
    }
}

/// The FLNK target: records the thread each pass ran on.
struct Sink(Threads);

impl DeviceSupport for Sink {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        self.0.lock().unwrap().push(this_thread());
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Sink"
    }
}

fn port(name: &str, can_block: bool, calls: &Calls) -> PortRuntimeHandle {
    let mut base = PortDriverBase::new(
        name,
        1,
        PortFlags {
            can_block,
            ..PortFlags::default()
        },
        InterfaceMask::FLOAT64 | InterfaceMask::OCTET | InterfaceMask::DRV_USER,
        InterfaceMask::NONE,
    );
    base.create_param("VAL", ParamType::Float64).unwrap();
    base.create_param("MSG", ParamType::Octet).unwrap();
    let (runtime, _actor) = create_port_runtime_unclaimed(
        Port {
            base,
            calls: calls.clone(),
        },
        RuntimeConfig::default(),
    )
    .unwrap();
    asyn_rs::registry::register_port(
        name,
        runtime.port_handle().clone(),
        &asyn_rs::registry::open_owner(),
    )
    .unwrap();
    runtime
}

/// `REC`, bound through the asyn factory as an IOC binds it, with FLNK to
/// `SINK`, whose passes land in `passes`.
async fn database(
    rec: Box<dyn Record>,
    record_type: &str,
    dtyp: &str,
    inp: &str,
    pini: bool,
    passes: &Threads,
) -> PvDatabase {
    let mut rec = rec;
    let ctx = DeviceSupportContext {
        record_type,
        dtyp,
        inp,
        out: "",
    };
    let mut dev = universal_asyn_factory(&ctx).expect("the factory binds the record");
    dev.set_record_info("REC", ScanType::Passive);
    dev.init(&mut *rec).unwrap();

    let db = PvDatabase::new();
    // C `callbackInit`: no build arms the bands for a bare database.
    epics_base_rs::runtime::task::background_init();
    db.add_record("REC", rec).await.unwrap();
    db.add_record("SINK", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    {
        db.put_pv_no_process("REC.FLNK", EpicsValue::String("SINK".into()))
            .await
            .unwrap();
        if pini {
            db.put_pv_no_process("REC.PINI", EpicsValue::String("YES".into()))
                .await
                .unwrap();
        }
    }
    db.ioc_init().await;
    {
        let cell = db.get_record("REC").unwrap();
        let mut inst = cell.write();
        inst.common.dtyp = dtyp.into();
        inst.device = Some(dev);
    }
    {
        let cell = db.get_record("SINK").unwrap();
        let mut inst = cell.write();
        inst.common.udf = 0;
        inst.common.dtyp = "Sink".into();
        inst.device = Some(Box::new(Sink(passes.clone())));
    }
    db
}

async fn cycle_ends(db: &PvDatabase) {
    for _ in 0..500 {
        if !db.get_record("REC").unwrap().read().is_processing() {
            // A second completing pass would land within this window.
            epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("the cycle never ended");
}

fn alarm(db: &PvDatabase) -> (u16, u16) {
    let cell = db.get_record("REC").unwrap();
    let inst = cell.read();
    (inst.common.stat, inst.common.sevr as u16)
}

fn val(db: &PvDatabase) -> Option<EpicsValue> {
    db.get_record("REC").unwrap().read().record.val()
}

async fn ai(port: &str, pini: bool, passes: &Threads) -> PvDatabase {
    let inp = format!("@asyn({port},0,1)VAL");
    database(
        Box::new(AiRecord::new(0.0)),
        "ai",
        "asynFloat64",
        &inp,
        pini,
        passes,
    )
    .await
}

#[epics_macros_rs::epics_test]
async fn a_blocking_read_completes_once_on_the_record_s_band() {
    let calls = Calls::default();
    let _rt = port("BLKREAD", true, &calls);
    let passes = Threads::default();
    let db = ai("BLKREAD", false, &passes).await;
    db.get_record("REC").unwrap().write().set_prio(2);

    db.process_record("REC").await.unwrap();
    cycle_ends(&db).await;

    let reads = calls.reads.lock().unwrap().clone();
    let passes = passes.lock().unwrap().clone();
    assert_eq!(reads.len(), 1, "one driver read: {reads:?}");
    assert!(
        !reads[0].starts_with("cb"),
        "the port thread reads: {reads:?}"
    );
    assert_eq!(passes.len(), 1, "one completing pass: {passes:?}");
    assert!(passes[0].starts_with("cbHigh"), "{passes:?}");
    assert_eq!(val(&db), Some(EpicsValue::Double(7.0)));
    assert_eq!(alarm(&db), (0, 0));
}

#[epics_macros_rs::epics_test]
async fn a_synchronous_port_reads_inside_process() {
    let calls = Calls::default();
    let _rt = port("SYNREAD", false, &calls);
    let passes = Threads::default();
    let db = ai("SYNREAD", false, &passes).await;

    db.process_record("REC").await.unwrap();
    assert!(
        !db.get_record("REC").unwrap().read().is_processing(),
        "a port that cannot block completes the cycle in process"
    );
    assert_eq!(val(&db), Some(EpicsValue::Double(7.0)));
    assert_eq!(calls.reads.lock().unwrap().len(), 1);
    assert_eq!(passes.lock().unwrap().len(), 1);
}

#[epics_macros_rs::epics_test]
async fn a_timed_out_read_completes_in_timeout_alarm() {
    let calls = Calls::default();
    calls.time_out.store(true, Ordering::SeqCst);
    let _rt = port("TMOREAD", true, &calls);
    let passes = Threads::default();
    let db = ai("TMOREAD", false, &passes).await;

    db.process_record("REC").await.unwrap();
    cycle_ends(&db).await;

    assert_eq!(calls.reads.lock().unwrap().len(), 1);
    assert_eq!(passes.lock().unwrap().len(), 1, "the cycle still ends once");
    assert_eq!(
        val(&db),
        Some(EpicsValue::Double(0.0)),
        "the prior value stands"
    );
    assert_eq!(
        alarm(&db),
        (alarm_status::TIMEOUT_ALARM, AlarmSeverity::Invalid as u16)
    );
}

/// C `reportQueueRequestStatus`: a refused queue leaves PACT clear and the
/// same pass raises the alarm (devAsynInt32.c:841-847).
#[epics_macros_rs::epics_test]
async fn a_disabled_port_refuses_the_read_in_the_same_pass() {
    let calls = Calls::default();
    let rt = port("OFFREAD", true, &calls);
    let passes = Threads::default();
    let db = ai("OFFREAD", false, &passes).await;
    rt.port_handle().set_enable(-1, false).unwrap();

    db.process_record("REC")
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
    assert!(!db.get_record("REC").unwrap().read().is_processing());
    assert!(
        calls.reads.lock().unwrap().is_empty(),
        "nothing reached the driver"
    );
    assert_eq!(passes.lock().unwrap().len(), 1);
    assert_eq!(alarm(&db).1, AlarmSeverity::Invalid as u16);
}

/// ophyd-test-ioc's `PINI` inputs on an areaDetector port: iocInit's PINI
/// pass processes them under their hold, as any scan does.
#[epics_macros_rs::epics_test]
async fn a_pini_read_on_a_blocking_port_completes_after_ioc_init() {
    let calls = Calls::default();
    let _rt = port("PINIREAD", true, &calls);
    let passes = Threads::default();
    let db = ai("PINIREAD", true, &passes).await;

    db.pini_process(PiniMode::Yes).await;
    cycle_ends(&db).await;

    let reads = calls.reads.lock().unwrap().clone();
    assert_eq!(reads.len(), 1, "{reads:?}");
    assert!(
        !reads[0].starts_with("cb"),
        "the port thread reads: {reads:?}"
    );
    assert_eq!(val(&db), Some(EpicsValue::Double(7.0)));
    assert_eq!(passes.lock().unwrap().len(), 1);
}

/// `asynOctetWrite` on a waveform — an input record whose `process` writes
/// (C `callbackWfWrite`) — queues its write the same way.
#[epics_macros_rs::epics_test]
async fn a_write_only_waveform_writes_on_the_port_thread() {
    let calls = Calls::default();
    let _rt = port("WFWRITE", true, &calls);
    let passes = Threads::default();
    let mut wf = WaveformRecord::new(16, DbFieldType::Char);
    wf.put_field("VAL", EpicsValue::CharArray(b"abc".to_vec().into()))
        .unwrap();
    let db = database(
        Box::new(wf),
        "waveform",
        "asynOctetWrite",
        "@asyn(WFWRITE,0,1)MSG",
        true,
        &passes,
    )
    .await;

    db.pini_process(PiniMode::Yes).await;
    cycle_ends(&db).await;
    db.process_record("REC").await.unwrap();
    cycle_ends(&db).await;

    let writes = calls.writes.lock().unwrap().clone();
    assert_eq!(writes.len(), 2, "one driver write per process: {writes:?}");
    assert!(
        writes.iter().all(|t| !t.starts_with("cb")),
        "the port thread writes: {writes:?}"
    );
    assert_eq!(passes.lock().unwrap().len(), 2);
    assert_eq!(alarm(&db), (0, 0));
}
