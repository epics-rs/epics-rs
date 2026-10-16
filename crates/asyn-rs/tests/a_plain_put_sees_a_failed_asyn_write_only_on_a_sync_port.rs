//! The async boundary of a plain put's status. devAsyn's `processAo` returns
//! -1 when the write failed and the record returns it (`devAsynFloat64.c:
//! 679-683`, `aoRecord.c:239`), so `caput` and `pvxput` fail; on an
//! `ASYN_CANBLOCK` port the same write only queues, the record goes PACT and
//! returns 0 at put time (`devAsynFloat64.c:662`, `aoRecord.c:223`), and the
//! completing pass's -1 reaches nobody.
#![cfg(feature = "epics")]

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use asyn_rs::adapter::{AsynDeviceSupport, AsynLink};
use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::record::ScanType;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

/// Holds a port's writes until opened, so a test can look at the record
/// while its write is still queued.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
    fn pass(&self) {
        let (open, cv) = &*self.0;
        let _open = cv.wait_while(open.lock().unwrap(), |o| !*o).unwrap();
    }
}

struct Failing(PortDriverBase, Gate);

impl PortDriver for Failing {
    fn base(&self) -> &PortDriverBase {
        &self.0
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.0
    }
    fn write_float64(&mut self, _: &mut AsynUser, _: f64) -> AsynResult<()> {
        self.1.pass();
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "refused".into(),
        })
    }
}

/// An ao on a port whose every write fails once `gate` opens, bound as
/// `asynFloat64`.
async fn ioc_gated(port: &str, can_block: bool, gate: Gate) -> PvDatabase {
    let mut base = PortDriverBase::new(
        port,
        1,
        PortFlags {
            can_block,
            ..PortFlags::default()
        },
    );
    base.create_param("VAL", ParamType::Float64).unwrap();
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Failing(base, gate), RuntimeConfig::default()).unwrap();
    let mut rec = AoRecord::new(0.0);
    let mut ads = AsynDeviceSupport::from_handle(
        runtime.port_handle().clone(),
        AsynLink {
            port_name: port.into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        },
        "asynFloat64",
    );
    ads.set_record_info("A:AO", ScanType::Passive);
    ads.init(&mut rec).unwrap();
    let db = PvDatabase::new();
    db.add_record("A:AO", Box::new(rec)).await.unwrap();
    db.ioc_init().await;
    let cell = db.get_record("A:AO").unwrap();
    let mut inst = cell.write();
    inst.common.dtyp = "asynFloat64".into();
    inst.common.udf = 0;
    inst.device = Some(Box::new(ads));
    drop(inst);
    db
}

async fn ioc(port: &str, can_block: bool) -> PvDatabase {
    let gate = Gate::default();
    gate.open();
    ioc_gated(port, can_block, gate).await
}

#[epics_macros_rs::epics_test]
async fn a_failed_write_on_a_sync_port_fails_the_plain_put() {
    let db = ioc("PSTAT_SYNC", false).await;
    let put = db
        .put_record_field_from_ca_no_notify("A:AO", "VAL", EpicsValue::Double(1.0))
        .await;
    assert!(
        matches!(put, Err(CaError::ProcessStatus(s)) if s.get() == -1),
        "processAo's -1 is the put's status, got {put:?}"
    );
}

#[epics_macros_rs::epics_test]
async fn a_failed_write_on_a_blocking_port_returns_zero_at_put_time() {
    // Held, or the port thread may complete the record before it is read.
    let gate = Gate::default();
    let db = ioc_gated("PSTAT_BLOCK", true, gate.clone()).await;
    let put = db
        .put_record_field_from_ca_no_notify("A:AO", "VAL", EpicsValue::Double(1.0))
        .await;
    assert!(put.is_ok(), "the record went PACT: status 0, got {put:?}");
    let processing = || db.get_record("A:AO").unwrap().read().is_processing();
    assert!(
        processing(),
        "the write is still queued when the put returns"
    );
    gate.open();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while processing() {
        assert!(
            std::time::Instant::now() < deadline,
            "the write never completed"
        );
        epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await;
    }
}

/// `dbNotify` never reads the status, sync port or not.
#[epics_macros_rs::epics_test]
async fn a_put_callback_ignores_a_failed_write() {
    for (port, can_block) in [("PSTAT_CB_SYNC", false), ("PSTAT_CB_BLOCK", true)] {
        let db = ioc(port, can_block).await;
        let put = db
            .put_record_field_from_ca("A:AO", "VAL", EpicsValue::Double(1.0))
            .await;
        assert!(put.is_ok(), "{port}: got {put:?}");
    }
}
