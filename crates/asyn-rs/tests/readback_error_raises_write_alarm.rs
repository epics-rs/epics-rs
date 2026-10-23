//! An `asyn:READBACK` output whose driver posts a failed status ends at
//! WRITE/INVALID: `outputCallbackCallback` clears UDF before `dbProcess`
//! (devAsynInt32.c:631 and its twins), so `checkAlarms` raises no UDF_ALARM,
//! and every output processXxx maps the ring element's status with
//! WRITE_ALARM (devAsynInt32.c:1009, devAsynInt64.c:723,
//! devAsynUInt32Digital.c:748, devAsynFloat64.c:668, devAsynOctet.c:806).
//! The record used to stay at UDF/INVALID with UDF set.
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::error::AsynStatus;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
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

const PARAMS: [(&str, ParamType); 5] = [
    ("I32", ParamType::Int32),
    ("I64", ParamType::Int64),
    ("F64", ParamType::Float64),
    ("U32", ParamType::UInt32Digital),
    ("MSG", ParamType::Octet),
];

const RECORDS: &str = r#"
record(ao, "AO") { field(DTYP, "asynInt32") field(OUT, "@asyn(RBKERR,0,1)I32") info(asyn:READBACK, "1") }
record(bo, "BO") { field(DTYP, "asynInt32") field(OUT, "@asyn(RBKERR,0,1)I32") info(asyn:READBACK, "1") }
record(longout, "LO") { field(DTYP, "asynInt32") field(OUT, "@asyn(RBKERR,0,1)I32") info(asyn:READBACK, "1") }
record(mbbo, "MBBO") { field(DTYP, "asynInt32") field(OUT, "@asyn(RBKERR,0,1)I32") info(asyn:READBACK, "1") }
record(int64out, "I64") { field(DTYP, "asynInt64") field(OUT, "@asyn(RBKERR,0,1)I64") info(asyn:READBACK, "1") }
record(ao, "AOF") { field(DTYP, "asynFloat64") field(OUT, "@asyn(RBKERR,0,1)F64") info(asyn:READBACK, "1") }
record(bo, "BOD") { field(DTYP, "asynUInt32Digital") field(OUT, "@asynMask(RBKERR,0,0x1,1)U32") info(asyn:READBACK, "1") }
record(mbbo, "MBBOD") { field(DTYP, "asynUInt32Digital") field(OUT, "@asynMask(RBKERR,0,0x3,1)U32") info(asyn:READBACK, "1") }
record(stringout, "SO") { field(DTYP, "asynOctetWrite") field(OUT, "@asyn(RBKERR,0,1)MSG") info(asyn:READBACK, "1") }
"#;

async fn stat(db: &PvDatabase, name: &str) -> (EpicsValue, EpicsValue) {
    let want = EpicsValue::Short(alarm_status::WRITE_ALARM as i16);
    let mut got = (EpicsValue::Short(-1), EpicsValue::Short(-1));
    for _ in 0..400 {
        got = (
            db.get_pv(format!("{name}.STAT")).unwrap(),
            db.get_pv(format!("{name}.SEVR")).unwrap(),
        );
        if got.0 == want {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    got
}

#[epics_base_rs::epics_test]
async fn a_failed_readback_on_a_scalar_output_is_write_alarm() {
    let db = Arc::new(PvDatabase::new());
    let mut base = PortDriverBase::new(
        "RBKERR",
        1,
        PortFlags::default(),
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_ALL,
    );
    let reasons: Vec<usize> = PARAMS
        .iter()
        .map(|(name, ty)| base.create_param(name, *ty).unwrap())
        .collect();
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Drv(base), RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        "RBKERR",
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

    // A changed value with asynError and no alarm of its own.
    runtime
        .port_handle()
        .with_driver::<Drv, _>(move |d| {
            let b = &mut d.0;
            b.set_int32_param(reasons[0], 0, 1).unwrap();
            b.set_int64_param(reasons[1], 0, 1).unwrap();
            b.set_float64_param(reasons[2], 0, 1.0).unwrap();
            b.set_uint32_param(reasons[3], 0, 1, 0x3, 0).unwrap();
            b.set_string_param(reasons[4], 0, "x").unwrap();
            for &r in &reasons {
                b.set_param_status(r, 0, AsynStatus::Error).unwrap();
            }
            b.call_param_callbacks(0).unwrap();
        })
        .await
        .unwrap();

    for name in ["AO", "BO", "LO", "MBBO", "I64", "AOF", "BOD", "MBBOD", "SO"] {
        assert_eq!(
            stat(&db, name).await,
            (
                EpicsValue::Short(alarm_status::WRITE_ALARM as i16),
                EpicsValue::Short(3)
            ),
            "{name}"
        );
    }
}
