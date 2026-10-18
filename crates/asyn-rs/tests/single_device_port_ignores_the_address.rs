//! On a port without `ASYN_MULTIDEVICE`, C's `getAddr` is -1 for every user,
//! so `getAddress` reads list 0 and the interrupt callbacks match a client at
//! any address (asynPortDriver.cpp:627-630, :1905-1906). A record at addr 3
//! on such a port follows list 0's callbacks.
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::request::RequestOp;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

struct Plain {
    base: PortDriverBase,
}

impl PortDriver for Plain {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

#[epics_base_rs::epics_test]
async fn an_io_intr_record_at_any_address_follows_list_0() {
    let db = Arc::new(PvDatabase::new());
    let mut base = PortDriverBase::new(
        "SDANYADDR",
        1,
        PortFlags::default(),
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_ALL,
    );
    base.create_param("I32", ParamType::Int32).unwrap();
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Plain { base }, RuntimeConfig::default()).unwrap();
    let handle = runtime.port_handle().clone();
    asyn_rs::registry::register_port(
        "SDANYADDR",
        handle.clone(),
        db.scan_control().interrupt_gate(),
    )
    .unwrap();
    let records = "record(longin, \"SDA\") { field(DTYP, \"asynInt32\") \
                   field(INP, \"@asyn(SDANYADDR,3,1)I32\") field(SCAN, \"I/O Intr\") }\n";
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();

    handle
        .submit_blocking(RequestOp::Int32Write { value: 7 }, AsynUser::new(0))
        .unwrap();
    for _ in 0..500 {
        if db.get_pv("SDA").unwrap() == EpicsValue::Long(7) {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("SDA = {:?}, never 7", db.get_pv("SDA").unwrap());
}
