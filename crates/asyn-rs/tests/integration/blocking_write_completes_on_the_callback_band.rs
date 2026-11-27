//! A put on a record bound to an `ASYN_CANBLOCK` port ends its cycle as C
//! devAsyn's does: the port thread runs the write, then
//! `callbackRequestProcessCallback(&pPvt->processCallback, pr->prio, pr)`
//! (devAsynFloat64.c:351) re-enters the record on the callback band its `PRIO`
//! selects, once. The cycle used to end on a spawned task that blocked a pool
//! thread waiting for the write.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::adapter::{AsynDeviceSupport, AsynLink};
use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

type Threads = Arc<Mutex<Vec<String>>>;

fn this_thread() -> String {
    std::thread::current().name().unwrap_or("").to_string()
}

struct BlockingPort {
    base: PortDriverBase,
    writes: Threads,
}

impl PortDriver for BlockingPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        self.writes.lock().unwrap().push(this_thread());
        self.base.set_float64_param(user.reason, user.addr, value)
    }
}

/// The FLNK target: records the thread each completing pass ran on.
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

/// Linux only: `/proc` is the count's source, and std has no portable one.
#[cfg(target_os = "linux")]
fn threads_of_this_process() -> usize {
    std::fs::read_dir("/proc/self/task").unwrap().count()
}

async fn put(db: &PvDatabase, prio: i16, value: f64) {
    db.get_record("BLK:AO").unwrap().write().set_prio(prio);
    db.put_record_field_from_ca_no_notify("BLK:AO", "VAL", EpicsValue::Double(value))
        .await
        .unwrap();
    for _ in 0..500 {
        if !db.get_record("BLK:AO").unwrap().read().is_processing() {
            // A second completing pass would land within this window.
            epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("the cycle never ended");
}

#[epics_macros_rs::epics_test]
async fn a_blocking_write_completes_once_on_the_record_s_band() {
    const PORT: &str = "BLKBAND";
    let writes: Threads = Arc::default();
    let passes: Threads = Arc::default();
    let mut base = PortDriverBase::new(
        PORT,
        1,
        PortFlags {
            can_block: true,
            ..PortFlags::default()
        },
        InterfaceMask::FLOAT64 | InterfaceMask::DRV_USER,
        InterfaceMask::NONE,
    );
    base.create_param("VAL", ParamType::Float64).unwrap();
    let (runtime, _actor) = create_port_runtime_unclaimed(
        BlockingPort {
            base,
            writes: writes.clone(),
        },
        RuntimeConfig::default(),
    )
    .unwrap();

    let mut rec = AoRecord::new(0.0);
    let mut ads = AsynDeviceSupport::from_handle(
        runtime.port_handle().clone(),
        AsynLink {
            port_name: PORT.into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        },
        "asynFloat64",
    );
    ads.set_record_info("BLK:AO", ScanType::Passive);
    ads.init(&mut rec).unwrap();

    let db = PvDatabase::new();
    // C `callbackInit`: no build arms the bands for a bare database.
    epics_base_rs::runtime::task::background_init();
    db.add_record("BLK:AO", Box::new(rec)).await.unwrap();
    db.add_record("BLK:SINK", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv_no_process("BLK:AO.FLNK", EpicsValue::String("BLK:SINK".into()))
        .await
        .unwrap();
    db.ioc_init().await;
    {
        let cell = db.get_record("BLK:AO").unwrap();
        let mut inst = cell.write();
        inst.common.dtyp = "asynFloat64".into();
        inst.common.udf = 0;
        inst.device = Some(Box::new(ads));
    }
    {
        let cell = db.get_record("BLK:SINK").unwrap();
        let mut inst = cell.write();
        inst.common.dtyp = "Sink".into();
        inst.common.udf = 0;
        inst.device = Some(Box::new(Sink(passes.clone())));
    }

    put(&db, 0, 1.0).await;
    // PRIO is read per put: the second completes on the band it now selects.
    #[cfg(target_os = "linux")]
    let threads = threads_of_this_process();
    put(&db, 2, 2.0).await;
    #[cfg(target_os = "linux")]
    assert_eq!(
        threads_of_this_process(),
        threads,
        "the completion runs on threads the IOC already has"
    );

    let writes = writes.lock().unwrap().clone();
    let passes = passes.lock().unwrap().clone();
    assert_eq!(writes.len(), 2, "one driver write per put: {writes:?}");
    assert!(
        writes.iter().all(|t| !t.starts_with("cb")),
        "the port thread writes: {writes:?}"
    );
    assert_eq!(passes.len(), 2, "one completing pass per put: {passes:?}");
    assert!(passes[0].starts_with("cbLow"), "{passes:?}");
    assert!(passes[1].starts_with("cbHigh"), "{passes:?}");
}
