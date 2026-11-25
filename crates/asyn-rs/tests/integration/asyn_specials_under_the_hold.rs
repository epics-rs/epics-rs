//! asynRecord's specials run under `dbScanLock`. ENBL and AUCT call
//! `pasynManager->enable` / `autoConnect` (asynRecord.c:481-486), which C
//! stores on that thread (asynManager.c:2224-2250, :2312-2329). On a blocking
//! port a round trip to the port's actor there would hold the record for
//! whatever driver call the actor is in; a debug build asserts it never
//! happens. DRVINFO's `pasynDrvUser->create` (asynRecord.c:1249) is the one
//! named exemption.

use std::collections::HashMap;
use std::time::Duration;

use asyn_rs::asyn_record::AsynRecord;
use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::registry::{get_port, register_port};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_base_rs::epics_test]
async fn enbl_and_auct_puts_store_without_a_port_round_trip() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let _held = listener.accept();
        std::thread::sleep(Duration::from_secs(30));
    });
    let driver = DrvAsynIPPort::new("ENAUHOLD", &addr.to_string()).unwrap();
    let (rt, _jh) = create_port_runtime_unclaimed(driver, RuntimeConfig::default()).unwrap();
    assert!(
        rt.port_handle().can_block(),
        "precondition: a blocking port"
    );
    register_port(
        "ENAUHOLD",
        rt.port_handle().clone(),
        &asyn_rs::registry::open_owner(),
    )
    .unwrap();

    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .db_string(
            "record(asyn, \"A\") {\n  field(PORT, \"ENAUHOLD\")\n}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let port = get_port("ENAUHOLD").unwrap().handle.connect_device(-1);

    db.put_record_field_from_ca_no_notify("A", "ENBL", EpicsValue::Enum(0))
        .await
        .unwrap();
    assert!(!port.enabled(), "ENBL=0 disabled the port in the put");
    db.put_record_field_from_ca_no_notify("A", "AUCT", EpicsValue::Enum(0))
        .await
        .unwrap();
    assert!(
        !port.auto_connect(),
        "AUCT=0 cleared auto-connect in the put"
    );
    db.put_record_field_from_ca_no_notify("A", "ENBL", EpicsValue::Enum(1))
        .await
        .unwrap();
    assert!(port.enabled());
}

struct Blocking {
    base: PortDriverBase,
}

impl PortDriver for Blocking {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

#[epics_base_rs::epics_test]
async fn drvinfo_put_resolves_on_a_blocking_port_under_the_exemption() {
    let mut base = PortDriverBase::new(
        "DRVHOLD",
        1,
        PortFlags {
            can_block: true,
            ..PortFlags::default()
        },
        InterfaceMask::OPTION
            | InterfaceMask::INT32
            | InterfaceMask::UINT32_DIGITAL
            | InterfaceMask::FLOAT64
            | InterfaceMask::OCTET
            | InterfaceMask::DRV_USER,
        InterfaceMask::NONE,
    );
    base.create_param("P0", ParamType::Int32).unwrap();
    base.create_param("P1", ParamType::Int32).unwrap();
    let (rt, _jh) =
        create_port_runtime_unclaimed(Blocking { base }, RuntimeConfig::default()).unwrap();
    assert!(
        rt.port_handle().can_block(),
        "precondition: a blocking port"
    );
    register_port(
        "DRVHOLD",
        rt.port_handle().clone(),
        &asyn_rs::registry::open_owner(),
    )
    .unwrap();

    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .db_string(
            "record(asyn, \"B\") {\n  field(PORT, \"DRVHOLD\")\n}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.put_record_field_from_ca_no_notify("B", "DRVINFO", EpicsValue::String("P1".into()))
        .await
        .unwrap();
    let reason = db
        .get_record("B")
        .unwrap()
        .read()
        .record
        .get_field("REASON");
    assert_eq!(reason, Some(EpicsValue::Long(1)));
}
