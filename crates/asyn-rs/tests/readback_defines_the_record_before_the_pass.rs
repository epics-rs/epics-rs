//! An undefined `asyn:READBACK` output that takes a good readback ends defined
//! and at NO_ALARM: `outputCallbackCallback` writes `pr->udf = 0` before
//! `dbProcess` (devAsynInt32.c:631, devAsynOctet.c:579), so `checkAlarms`
//! never sees it undefined. The clear is asyn's own, at arm time; the readback
//! read alone, after `checkAlarms`, would leave UDF/INVALID.
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

struct Drv(PortDriverBase);

impl PortDriver for Drv {
    fn base(&self) -> &PortDriverBase {
        &self.0
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.0
    }
}

const RECORDS: &str = r#"
record(longout, "LO") { field(DTYP, "asynInt32") field(OUT, "@asyn(RBKUDF,0,1)I32") info(asyn:READBACK, "1") }
record(stringout, "SO") { field(DTYP, "asynOctetWrite") field(OUT, "@asyn(RBKUDF,0,1)MSG") info(asyn:READBACK, "1") }
"#;

fn alarm(db: &PvDatabase, name: &str) -> (u8, u16, AlarmSeverity) {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    (inst.common.udf, inst.common.stat, inst.common.sevr)
}

#[epics_base_rs::epics_test]
async fn a_good_readback_defines_an_undefined_output_without_a_udf_alarm() {
    let db = Arc::new(PvDatabase::new());
    let mut base = PortDriverBase::new(
        "RBKUDF",
        1,
        PortFlags::default(),
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_ALL,
    );
    let i32_reason = base.create_param("I32", ParamType::Int32).unwrap();
    let msg_reason = base.create_param("MSG", ParamType::Octet).unwrap();
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Drv(base), RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        "RBKUDF",
        runtime.port_handle().clone(),
        db.scan_control().interrupt_gate(),
    )
    .unwrap();
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(RECORDS, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    // The parameters were never set, so the init read defined nothing.
    for name in ["LO", "SO"] {
        assert_eq!(alarm(&db, name).0, 1, "{name} before the readback");
    }

    runtime
        .port_handle()
        .with_driver::<Drv, _>(move |d| {
            let b = &mut d.0;
            b.set_int32_param(i32_reason, 0, 7).unwrap();
            b.set_string_param(msg_reason, 0, "x").unwrap();
            b.call_param_callbacks(0).unwrap();
        })
        .await
        .unwrap();

    for (name, val) in [
        ("LO", EpicsValue::Long(7)),
        ("SO", EpicsValue::String("x".into())),
    ] {
        for _ in 0..400 {
            if db.get_pv(name).unwrap() == val {
                break;
            }
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(db.get_pv(name).unwrap(), val, "{name}");
        assert_eq!(alarm(&db, name), (0, 0, AlarmSeverity::NoAlarm), "{name}");
    }
}
