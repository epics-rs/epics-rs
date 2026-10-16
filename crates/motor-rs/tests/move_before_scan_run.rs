//! devMotorAsyn's `dbScanLockOK` is latched by its dset `init(after)` at
//! `finishDevSup` (devMotorAsyn.c:159-162), before PINI and `scanRun`: a
//! move started by PINI finishes through its callbacks on a built IOC that
//! never ran.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::registry::open_owner;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::devices::{device_support_factory, devices, register_axis, register_controller_port};
use motor_rs::trace::TraceUser;

async fn wait_done(db: &PvDatabase, rec: &str) -> bool {
    for _ in 0..300 {
        if db.get_pv(format!("{rec}.DMOV")).unwrap() == EpicsValue::Short(1) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pini_move_completes_before_scan_run() {
    register_controller_port(&open_owner(), "PINIM", 1, ()).unwrap();
    let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> =
        Arc::new(Mutex::new(motor_rs::sim_motor::SimMotor::new()));
    let AxisSetup {
        axis, poll_loop, ..
    } = MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: "PINIM".into(),
            addr: 0,
        })
        .poll_interval(Duration::from_millis(10))
        .build_axis();
    register_axis(axis).unwrap();
    tokio::spawn(poll_loop.run());
    let (name, factory) = motor_rs::motor_record_factory();
    let ioc = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(
            r##"
record(motor, "I:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(PINIM,0)") field(MRES, "0.001") field(VELO, "1") field(FLNK, "I:n") }
record(calc, "I:n") { field(CALC, "A+1") field(INPA, "I:n NPP") }
record(ao, "I:go") { field(PINI, "YES") field(VAL, "0.2") field(OUT, "I:m.VAL PP") }
"##,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = Arc::clone(ioc.database());
    assert!(
        wait_done(&db, "I:m").await,
        "the PINI move finished with no scanRun"
    );
    assert_eq!(db.get_pv("I:m.DRBV").unwrap(), EpicsValue::Double(0.2));
    assert_eq!(db.get_pv("I:n").unwrap(), EpicsValue::Double(1.0));

    assert!(ioc.run().await);
    db.put_record_field_from_ca("I:m", "VAL", EpicsValue::Double(0.3))
        .await
        .unwrap();
    assert!(wait_done(&db, "I:m").await, "later moves still finish");
    assert_eq!(db.get_pv("I:m.DRBV").unwrap(), EpicsValue::Double(0.3));
}
