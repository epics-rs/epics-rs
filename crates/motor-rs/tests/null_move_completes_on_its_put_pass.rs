//! A put-callback of VAL to where the motor stands completes on the put's
//! own pass: special() blinks DMOV and too_small restores it in the same
//! locked pass (motorRecord.cc:2603-2606, 2333-2343), so a get right after
//! the callback reads DMOV=1 and MOVN never rises.

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

const DB: &str = r#"
record(motor, "NM:M") { field(DTYP, "asynMotor") field(OUT, "@asyn(NULLMOVE,0)") field(MRES, "0.001") field(VELO, "10") field(DHLM, "100") field(DLLM, "-100") }
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_null_move_completes_on_its_put_pass() {
    register_controller_port(&open_owner(), "NULLMOVE", 1, ()).unwrap();
    let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> =
        Arc::new(Mutex::new(motor_rs::sim_motor::SimMotor::new()));
    let AxisSetup {
        axis,
        poll_loop,
        poll_cmd_tx,
    } = MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: "NULLMOVE".into(),
            addr: 0,
        })
        .poll_interval(Duration::from_millis(10))
        .build_axis();
    register_axis(axis).unwrap();
    tokio::spawn(poll_loop.start());
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A real move first, so the null moves follow a completion as in a scan.
    let done = db
        .put_record_field_from_ca("NM:M", "VAL", EpicsValue::Double(0.5))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), done.into_handle().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(num(&db, "NM:M.DMOV"), 1.0);

    for _ in 0..20 {
        let done = db
            .put_record_field_from_ca("NM:M", "VAL", EpicsValue::Double(0.5))
            .await
            .unwrap();
        assert!(!done.is_async(), "the put-notify completes on the put pass");
        assert_eq!(num(&db, "NM:M.DMOV"), 1.0);
        assert_eq!(num(&db, "NM:M.MOVN"), 0.0);
    }
    let _ = poll_cmd_tx
        .send(motor_rs::poll_loop::PollCommand::Shutdown)
        .await;
}
