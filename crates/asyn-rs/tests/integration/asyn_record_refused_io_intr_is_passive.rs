//! An asyn record whose `registerInterrupts` fails ends up Passive. C
//! `getIoIntInfo` returns -1 (asynRecord.c:590-591) and `scanAdd` sets
//! `precord->scan = menuScanPassive` (dbScan.c:286-288), whether the record
//! loaded with `SCAN="I/O Intr"` (iocInit's `buildScanLists`) or an operator
//! puts it there later. The refusal is either the port's: no interrupt source
//! on the interface (`getInterruptPvt`), or the record's: "No asynInt32
//! interface".
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::Arc;

use asyn_rs::asyn_record::AsynRecord;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime_unclaimed};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ScanType;
use epics_base_rs::types::EpicsValue;

struct Device {
    base: PortDriverBase,
}

impl PortDriver for Device {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

fn port(
    name: &str,
    interfaces: InterfaceMask,
    interrupts: InterfaceMask,
    db: &PvDatabase,
) -> PortRuntimeHandle {
    let mut base = PortDriverBase::new(name, 1, PortFlags::default(), interfaces, interrupts);
    base.create_param("I32", ParamType::Int32).unwrap();
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Device { base }, RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        name,
        runtime.port_handle().clone(),
        db.scan_control().interrupt_gate(),
    )
    .unwrap();
    runtime
}

fn record(name: &str, port: &str, scan: &str) -> String {
    format!(
        "record(asyn, \"{name}\") {{ field(DTYP, \"asynRecordDevice\") field(PORT, \"{port}\") \
         field(DRVINFO, \"I32\") field(IFACE, \"asynInt32\") field(SCAN, \"{scan}\") }}\n"
    )
}

fn scan(db: &PvDatabase, name: &str) -> ScanType {
    db.get_record(name).unwrap().read().common.scan
}

#[epics_base_rs::epics_test]
async fn a_refused_interrupt_registration_leaves_the_record_passive() {
    let db = Arc::new(PvDatabase::new());
    let int32 = InterfaceMask::DRV_USER.or(InterfaceMask::INT32);
    let _nosrc = port("ARNOSRC", int32, InterfaceMask::NONE, &db);
    let _noif = port(
        "ARNOIF",
        InterfaceMask::DRV_USER.or(InterfaceMask::OCTET),
        InterfaceMask::OCTET,
        &db,
    );
    let _live = port("ARSRC", int32, InterfaceMask::INT32, &db);
    let db_text = [
        record("NSRC", "ARNOSRC", "I/O Intr"),
        record("NIF", "ARNOIF", "I/O Intr"),
        record("Y", "ARSRC", "I/O Intr"),
        record("PNSRC", "ARNOSRC", "Passive"),
        record("PNIF", "ARNOIF", "Passive"),
        record("PY", "ARSRC", "Passive"),
    ]
    .concat();
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(
        IocBuilder::new().register_record_type("asyn", || Box::new(AsynRecord::default())),
    )
    .with_database(db)
    .db_string(&db_text, &HashMap::new())
    .unwrap()
    .build()
    .await
    .unwrap();
    db.ioc_init().await;

    assert_eq!(scan(&db, "Y"), ScanType::IoIntr, "Y at iocInit");
    assert_eq!(scan(&db, "NSRC"), ScanType::Passive, "NSRC at iocInit");
    assert_eq!(scan(&db, "NIF"), ScanType::Passive, "NIF at iocInit");

    for name in ["PY", "PNSRC", "PNIF"] {
        db.put_record_field_from_ca(name, "SCAN", EpicsValue::String("I/O Intr".into()))
            .await
            .unwrap();
    }
    assert_eq!(scan(&db, "PY"), ScanType::IoIntr, "PY after the put");
    assert_eq!(scan(&db, "PNSRC"), ScanType::Passive, "PNSRC after the put");
    assert_eq!(scan(&db, "PNIF"), ScanType::Passive, "PNIF after the put");

    // Demoted at iocInit, so not on the list: a later put asks again.
    for name in ["NSRC", "NIF"] {
        db.put_record_field_from_ca(name, "SCAN", EpicsValue::String("I/O Intr".into()))
            .await
            .unwrap();
        assert_eq!(scan(&db, name), ScanType::Passive, "{name} after the put");
    }
}
