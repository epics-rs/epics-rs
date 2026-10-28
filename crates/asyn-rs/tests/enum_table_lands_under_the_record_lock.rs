//! When a driver's new enum table reaches the record, relative to the value
//! callback the driver fires next.
//!
//! C applies the table on the driver's thread, under the record lock, before
//! `doCallbacksEnum` returns: `interruptCallbackEnumMbbi` is `dbScanLock`,
//! `setEnums`, `db_post_events(DBE_PROPERTY)`, `dbScanUnlock`
//! (devAsynInt32.c:712-766, devAsynUInt32Digital.c:547-601, and
//! devBusyAsyn.c's copy). A value callback fired after it, or a put made after
//! it, converts through the new table.
//!
//! Each test fires the table and then the value or put without yielding the
//! test's runtime, and blocks the thread until the record processed. Anything
//! that applies the table later than the callback itself has not run by then.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::asyn_record::register_port;
use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::InterfaceType;
use asyn_rs::interrupt::InterruptValue;
use asyn_rs::param::{EnumEntry, ParamType, ParamValue};
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime_unclaimed;
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const REC: &str = "ENUM:ORDER";

struct EnumPort {
    base: PortDriverBase,
    writes: Arc<Mutex<Vec<i32>>>,
}

impl PortDriver for EnumPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn write_int32(&mut self, _user: &mut AsynUser, value: i32) -> AsynResult<()> {
        self.writes.lock().unwrap().push(value);
        Ok(())
    }
    fn read_enum(&mut self, user: &AsynUser) -> AsynResult<(usize, Arc<[EnumEntry]>)> {
        self.base.get_enum_param(user.reason, user.addr)
    }
}

fn entry(s: &str, value: i32, severity: u16) -> EnumEntry {
    EnumEntry {
        string: s.to_string(),
        value,
        severity,
    }
}

struct Ioc {
    db: Arc<PvDatabase>,
    handle: PortHandle,
    writes: Arc<Mutex<Vec<i32>>>,
}

/// One record binding `dtyp` to an Enum param seeded with OFF=0, ON=1.
async fn enum_ioc(port: &str, record: &str) -> Ioc {
    let mut base = PortDriverBase::new(
        port,
        1,
        PortFlags::default(),
        InterfaceMask::INT32 | InterfaceMask::ENUM | InterfaceMask::DRV_USER,
        InterfaceMask::INT32 | InterfaceMask::UINT32_DIGITAL | InterfaceMask::ENUM,
    );
    base.create_param("MODE", ParamType::Enum).unwrap();
    base.set_enum_choices_param(0, 0, vec![entry("OFF", 0, 0), entry("ON", 1, 0)].into())
        .unwrap();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let driver = EnumPort {
        base,
        writes: writes.clone(),
    };
    let (runtime, _join) = create_port_runtime_unclaimed(driver, RuntimeConfig::default())
        .expect("port runtime starts");
    let handle = runtime.port_handle().clone();
    let owner = Arc::new(PvDatabase::new());
    register_port(port, handle.clone(), owner.scan_control().interrupt_gate())
        .expect("port name is free");
    drop(runtime);

    let db_text = record.replace("PORT", port);
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(
        IocBuilder::new().register_record_type("busy", || Box::new(BusyRecord::default())),
    )
    .with_database(owner)
    .db_string(&db_text, &HashMap::new())
    .unwrap()
    .build()
    .await
    .unwrap();
    db.scan_control().run();
    settle().await;
    Ioc { db, handle, writes }
}

async fn settle() {
    for _ in 0..40 {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

/// The driver's `doCallbacksEnum` for the record's reason.
fn fire_table(handle: &PortHandle, choices: Vec<EnumEntry>) {
    handle.interrupts().notify(InterruptValue {
        reason: 0,
        addr: 0,
        value: ParamValue::Enum {
            index: 0,
            choices: choices.into(),
        },
        iface: Some(InterfaceType::Enum),
        ..Default::default()
    });
}

/// Block this thread, and with it every task on the test's runtime, until
/// `done` answers.
fn block_until<T>(what: &str, mut done: impl FnMut() -> Option<T>) -> T {
    for _ in 0..400 {
        if let Some(v) = done() {
            return v;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("{what} within 2 s");
}

/// A DBE_VALUE|DBE_LOG|DBE_ALARM monitor: a pass posts it, the table's
/// DBE_PROPERTY post does not.
fn subscribe(db: &PvDatabase, field: &str) -> EventReader {
    let rec = db.get_record(REC).unwrap();
    let mut inst = rec.write();
    inst.add_subscriber(field, 1, DbFieldType::String, 0x07)
        .expect("field is subscribable")
}

/// A synchronous dbPutField of VAL, made on this thread.
fn put_val(db: &PvDatabase, value: EpicsValue) {
    let held = db.lock_for_put(REC, "VAL", &value);
    db.put_record_field_from_ca_already_locked(held, value)
        .expect("put accepted");
}

fn field(db: &PvDatabase, name: &str) -> Option<EpicsValue> {
    db.get_record(REC).unwrap().read().record.get_field(name)
}

fn sevr(db: &PvDatabase) -> AlarmSeverity {
    db.get_record(REC).unwrap().read().common.sevr
}

/// devAsynInt32.c:712-724: the RVAL the driver posts next maps through the
/// new ZRVL..., so 7 is state B.
#[epics_base_rs::epics_test]
async fn an_mbbi_maps_the_next_value_through_the_new_table() {
    let ioc = enum_ioc(
        "orderMbbi",
        r#"record(mbbi, "ENUM:ORDER") {
            field(DTYP, "asynInt32") field(INP, "@asyn(PORT,0)MODE") field(SCAN, "I/O Intr")
        }"#,
    )
    .await;
    let mut val = subscribe(&ioc.db, "VAL");
    fire_table(&ioc.handle, vec![entry("A", 5, 0), entry("B", 7, 0)]);
    ioc.handle.interrupts().notify(InterruptValue {
        reason: 0,
        addr: 0,
        value: ParamValue::Int32(7),
        iface: Some(InterfaceType::Int32),
        ..Default::default()
    });
    block_until("the value pass", || val.try_recv().ok());
    assert_eq!(field(&ioc.db, "RVAL"), Some(EpicsValue::ULong(7)));
    assert_eq!(field(&ioc.db, "VAL"), Some(EpicsValue::Enum(1)));
}

/// devAsynInt32.c:726-738: a put of state 1 after the table sends that
/// table's ONVL.
#[epics_base_rs::epics_test]
async fn an_mbbo_put_writes_the_new_tables_raw_value() {
    let ioc = enum_ioc(
        "orderMbbo",
        r#"record(mbbo, "ENUM:ORDER") {
            field(DTYP, "asynInt32") field(OUT, "@asyn(PORT,0)MODE")
        }"#,
    )
    .await;
    ioc.writes.lock().unwrap().clear();
    fire_table(&ioc.handle, vec![entry("A", 5, 0), entry("B", 7, 0)]);
    put_val(&ioc.db, EpicsValue::Enum(1));
    let written = block_until("the write", || ioc.writes.lock().unwrap().last().copied());
    assert_eq!(written, 7);
}

/// devAsynUInt32Digital.c:575-587: the next value takes the new OSV.
#[epics_base_rs::epics_test]
async fn a_uint32_digital_bi_alarms_on_the_new_tables_severity() {
    let ioc = enum_ioc(
        "orderBi",
        r#"record(bi, "ENUM:ORDER") {
            field(DTYP, "asynUInt32Digital") field(INP, "@asynMask(PORT,0,0x1)MODE")
            field(SCAN, "I/O Intr")
        }"#,
    )
    .await;
    let mut val = subscribe(&ioc.db, "VAL");
    fire_table(&ioc.handle, vec![entry("SHUT", 0, 0), entry("OPEN", 1, 2)]);
    ioc.handle.interrupts().notify(InterruptValue {
        reason: 0,
        addr: 0,
        value: ParamValue::UInt32Digital(1),
        uint32_changed_mask: 1,
        iface: Some(InterfaceType::UInt32Digital),
        ..Default::default()
    });
    block_until("the value pass", || val.try_recv().ok());
    assert_eq!(field(&ioc.db, "VAL"), Some(EpicsValue::Enum(1)));
    assert_eq!(sevr(&ioc.db), AlarmSeverity::Major);
}

/// devBusyAsyn.c's enum callback: a put of 1 after the table alarms on its
/// OSV.
#[epics_base_rs::epics_test]
async fn a_busy_put_alarms_on_the_new_tables_severity() {
    let ioc = enum_ioc(
        "orderBusy",
        r#"record(busy, "ENUM:ORDER") {
            field(DTYP, "asynInt32") field(OUT, "@asyn(PORT,0)MODE")
        }"#,
    )
    .await;
    ioc.writes.lock().unwrap().clear();
    fire_table(&ioc.handle, vec![entry("Done", 0, 0), entry("Busy", 1, 1)]);
    put_val(&ioc.db, EpicsValue::Enum(1));
    block_until("the write", || ioc.writes.lock().unwrap().last().copied());
    assert_eq!(sevr(&ioc.db), AlarmSeverity::Minor);
}
