//! A devAsyn record's interrupt client follows its SCAN. C dbScan's
//! `scanDelete` asks `getIoIntInfo(1)`, which cancels the client, and
//! `scanAdd` asks `getIoIntInfo(0)`, which registers it again
//! (devAsynInt32.c:377-410). A record moved off I/O Intr therefore reads by
//! polling, not from callbacks that arrived after the move, and one moved back
//! processes on the next callback.
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::request::ParamSetValue;
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime_unclaimed};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
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

fn port(db: &PvDatabase) -> PortRuntimeHandle {
    let mut base = PortDriverBase::new(
        "SCANMOVE",
        1,
        PortFlags::default(),
        InterfaceMask::DRV_USER.or(InterfaceMask::INT32),
        InterfaceMask::INT32,
    );
    base.create_param("I32", ParamType::Int32).unwrap();
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Device { base }, RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        "SCANMOVE",
        runtime.port_handle().clone(),
        db.scan_control().interrupt_gate(),
    )
    .unwrap();
    runtime
}

async fn fire(runtime: &PortRuntimeHandle, value: i32) {
    let update = ParamSetValue::Value {
        reason: 0,
        addr: 0,
        value: ParamValue::Int32(value),
    };
    runtime
        .port_handle()
        .set_params_and_notify(0, vec![update])
        .await
        .unwrap();
}

async fn put_scan(db: &PvDatabase, scan: &str) {
    db.put_record_field_from_ca("LI", "SCAN", EpicsValue::String(scan.into()))
        .await
        .unwrap();
}

async fn settled(db: &PvDatabase, want: i32) -> EpicsValue {
    let mut got = db.get_pv("LI").unwrap();
    for _ in 0..200 {
        if got == EpicsValue::Long(want) {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        got = db.get_pv("LI").unwrap();
    }
    got
}

#[epics_base_rs::epics_test]
async fn a_scan_move_cancels_and_reregisters_the_interrupt_client() {
    let db = Arc::new(PvDatabase::new());
    let runtime = port(&db);
    let db_text = "record(longin, \"LI\") { field(DTYP, \"asynInt32\") \
                   field(INP, \"@asyn(SCANMOVE,0,1)I32\") field(SCAN, \"I/O Intr\") }\n";
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.scan_control().run();

    fire(&runtime, 3).await;
    assert_eq!(settled(&db, 3).await, EpicsValue::Long(3), "on I/O Intr");

    put_scan(&db, "Passive").await;
    fire(&runtime, 7).await;
    fire(&runtime, 9).await;
    db.process_record("LI").await.unwrap();
    assert_eq!(
        db.get_pv("LI").unwrap(),
        EpicsValue::Long(9),
        "a Passive record polls; no callback reached it after the move"
    );

    put_scan(&db, "I/O Intr").await;
    fire(&runtime, 11).await;
    assert_eq!(
        settled(&db, 11).await,
        EpicsValue::Long(11),
        "back on I/O Intr"
    );
}
