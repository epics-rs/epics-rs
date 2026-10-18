//! C `initCommon` runs `drvUserCreate` once itself and once more in every
//! SyncIO `connect` it makes with the record's userParam
//! (asynInt32SyncIO.c:94-110 and siblings), so a driver sees, and traces,
//! one create per connect. The counts are C's: asyn R4-46 testErrors with
//! the same records, counted from its `drvUserCreate: drvInfo=` FLOW lines.
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interfaces::Capability;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{DrvUserInfo, DrvUserRequest, PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime_unclaimed};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::records::busy::BusyRecord;

type Creates = Arc<Mutex<HashMap<String, usize>>>;

struct Counting {
    base: PortDriverBase,
    creates: Creates,
    enum_iface: bool,
    /// The create, counted per drvInfo, that fails.
    fail_at: Option<usize>,
}

impl PortDriver for Counting {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn drv_user_create(&mut self, req: &DrvUserRequest) -> AsynResult<DrvUserInfo> {
        let mut creates = self.creates.lock().unwrap();
        let n = creates.entry(req.drv_info.clone()).or_default();
        *n += 1;
        if Some(*n) == self.fail_at {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "refused".into(),
            });
        }
        self.base().drv_user_create(req)
    }
    fn capabilities(&self) -> Vec<Capability> {
        let mut caps = InterfaceMask::UNMIGRATED_ALL.capabilities();
        if !self.enum_iface {
            caps.retain(|c| !matches!(c, Capability::EnumRead | Capability::EnumWrite));
        }
        caps
    }
}

const PARAMS: [(&str, ParamType); 8] = [
    ("I32", ParamType::Int32),
    ("I32B", ParamType::Int32),
    ("I64", ParamType::Int64),
    ("U32", ParamType::UInt32Digital),
    ("U32B", ParamType::UInt32Digital),
    ("F64", ParamType::Float64),
    ("MSG", ParamType::Octet),
    ("MSGR", ParamType::Octet),
];

/// The creates each drvInfo saw while `records` initialised, the database,
/// and the port's runtime, which must outlive it.
async fn ioc(
    port: &str,
    enum_iface: bool,
    fail_at: Option<usize>,
    records: &str,
) -> (Creates, Arc<PvDatabase>, PortRuntimeHandle) {
    let db = Arc::new(PvDatabase::new());
    let mut base = PortDriverBase::new(
        port,
        1,
        PortFlags::default(),
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_ALL,
    );
    for (name, kind) in PARAMS {
        base.create_param(name, kind).unwrap();
    }
    let creates = Creates::default();
    let driver = Counting {
        base,
        creates: creates.clone(),
        enum_iface,
        fail_at,
    };
    let (runtime, _actor) =
        create_port_runtime_unclaimed(driver, RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        port,
        runtime.port_handle().clone(),
        db.scan_control().interrupt_gate(),
    )
    .unwrap();
    let records = records.replace("PORT", port);
    let builder =
        IocBuilder::new().register_record_type("busy", || Box::new(BusyRecord::default()));
    let db = asyn_rs::adapter::register_asyn_device_support_for_builder(builder)
        .with_database(db)
        .db_string(&records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    (creates, db, runtime)
}

async fn creates(port: &str, enum_iface: bool, records: &str) -> (Creates, PortRuntimeHandle) {
    let (creates, _db, runtime) = ioc(port, enum_iface, None, records).await;
    (creates, runtime)
}

/// A refused SyncIO create is the dset's `goto bad`: dead, at LINK/INVALID.
async fn dead_after_create(port: &str, n: usize, records: &str, name: &str) {
    let (_creates, db, _rt) = ioc(port, true, Some(n), records).await;
    let rec = db.get_record(name).unwrap();
    assert!(rec.read().is_processing(), "{name} after create {n}");
    assert_eq!(rec.read().common.nsta, alarm_status::LINK_ALARM, "{name}");
}

fn count(creates: &Creates, drv_info: &str) -> usize {
    creates.lock().unwrap().get(drv_info).copied().unwrap_or(0)
}

/// devAsynInt32.c:290 `Int32SyncIO->connect`, and :303 `EnumSyncIO->connect`
/// for a record with enum states on a port with asynEnum.
#[epics_base_rs::epics_test]
async fn int32_connects_its_sync_io_and_the_enum_sync_io() {
    let records = r#"
record(ai, "SIOI32:AI") { field(DTYP, "asynInt32") field(INP, "@asyn(PORT,0,1)I32") }
record(bi, "SIOI32:BI") { field(DTYP, "asynInt32") field(INP, "@asyn(PORT,0,1)I32B") }
"#;
    let (with_enum, _rt) = creates("SIOI32", true, records).await;
    assert_eq!(
        (count(&with_enum, "I32"), count(&with_enum, "I32B")),
        (2, 3)
    );
    let (no_enum, _rt) = creates("SIOI32N", false, records).await;
    assert_eq!((count(&no_enum, "I32"), count(&no_enum, "I32B")), (2, 2));
    dead_after_create("SIOI32F", 2, records, "SIOI32:AI").await;
    dead_after_create("SIOI32E", 3, records, "SIOI32:BI").await;
}

/// devAsynInt64.c:246 `Int64SyncIO->connect`; devAsynInt64 reads no enum table.
#[epics_base_rs::epics_test]
async fn int64_connects_its_sync_io() {
    let records = r#"
record(int64in, "SIOI64:IN") { field(DTYP, "asynInt64") field(INP, "@asyn(PORT,0,1)I64") }
"#;
    let (creates, _rt) = creates("SIOI64", true, records).await;
    assert_eq!(count(&creates, "I64"), 2);
    dead_after_create("SIOI64F", 2, records, "SIOI64:IN").await;
}

/// devAsynUInt32Digital.c:244 `UInt32DigitalSyncIO->connect`, and :260
/// `EnumSyncIO->connect` for a record with enum states on a port with asynEnum.
#[epics_base_rs::epics_test]
async fn uint32_digital_connects_its_sync_io_and_the_enum_sync_io() {
    let records = r#"
record(longin, "SIOU32:LI") { field(DTYP, "asynUInt32Digital") field(INP, "@asynMask(PORT,0,0xff,1)U32") }
record(bi, "SIOU32:BI") { field(DTYP, "asynUInt32Digital") field(INP, "@asynMask(PORT,0,0x1,1)U32B") }
"#;
    let (with_enum, _rt) = creates("SIOU32", true, records).await;
    assert_eq!(
        (count(&with_enum, "U32"), count(&with_enum, "U32B")),
        (2, 3)
    );
    let (no_enum, _rt) = creates("SIOU32N", false, records).await;
    assert_eq!((count(&no_enum, "U32"), count(&no_enum, "U32B")), (2, 2));
    dead_after_create("SIOU32F", 2, records, "SIOU32:LI").await;
    dead_after_create("SIOU32E", 3, records, "SIOU32:BI").await;
}

/// devAsynFloat64.c:203 `Float64SyncIO->connect`.
#[epics_base_rs::epics_test]
async fn float64_connects_its_sync_io() {
    let records = r#"
record(ai, "SIOF64:AI") { field(DTYP, "asynFloat64") field(INP, "@asyn(PORT,0,1)F64") }
"#;
    let (creates, _rt) = creates("SIOF64", true, records).await;
    assert_eq!(count(&creates, "F64"), 2);
    dead_after_create("SIOF64F", 2, records, "SIOF64:AI").await;
}

/// devAsynOctet.c:361 `octetSyncIO->connect`, made only for an output's
/// `asyn:INITIAL_READBACK` read.
#[epics_base_rs::epics_test]
async fn octet_connects_its_sync_io_for_the_initial_readback() {
    let records = r#"
record(stringout, "SIOOCT:SO") { field(DTYP, "asynOctetWrite") field(OUT, "@asyn(PORT,0,1)MSG") }
record(stringout, "SIOOCT:SOR") {
    field(DTYP, "asynOctetWrite") field(OUT, "@asyn(PORT,0,1)MSGR") info(asyn:INITIAL_READBACK, "1")
}
"#;
    let (creates, _rt) = creates("SIOOCT", true, records).await;
    assert_eq!((count(&creates, "MSG"), count(&creates, "MSGR")), (1, 2));
    dead_after_create("SIOOCTF", 2, records, "SIOOCT:SOR").await;
}

/// devBusyAsyn.c:210 `Int32SyncIO->connect`, and :223 `EnumSyncIO->connect`
/// on a port with asynEnum: the busy record has bo's two states.
#[epics_base_rs::epics_test]
async fn busy_connects_its_sync_io_and_the_enum_sync_io() {
    let records = r#"
record(busy, "SIOBUSY:B") { field(DTYP, "asynInt32") field(OUT, "@asyn(PORT,0,1)I32B") }
"#;
    let (with_enum, _rt) = creates("SIOBUSY", true, records).await;
    assert_eq!(count(&with_enum, "I32B"), 3);
    let (no_enum, _rt) = creates("SIOBUSYN", false, records).await;
    assert_eq!(count(&no_enum, "I32B"), 2);
    dead_after_create("SIOBUSYF", 2, records, "SIOBUSY:B").await;
    dead_after_create("SIOBUSYE", 3, records, "SIOBUSY:B").await;
}
