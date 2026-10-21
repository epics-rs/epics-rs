//! A motor's `recGblFwdLink` runs on the pass that sets DMOV
//! (motorRecord.cc:1509), so the PUTF a `dbpf` to VAL set stays with the
//! record across the move and goes out with that pass's FLNK
//! (recGbl.c:295-302).
//!
//! oracleMotorIoc (motorSim) on these records, MR:T idle:
//!
//! ```text
//! dbpf MR:M.VAL 0.5; dbpf MR:T.PROC 1
//!   at 1.5 s  MR:M.DMOV 1  MR:T.PACT 1  MR:T.RPRO 1  MR:M.PUTF 0  MR:TC +0
//!   at 2.5 s  MR:TC +1
//!   at 5.0 s  MR:TC +2
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::registry::open_owner;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::devices::{device_support_factory, devices, register_axis, register_controller_port};
use motor_rs::trace::TraceUser;

const DB: &str = r#"
record(motor, "MR:M") { field(DTYP, "asynMotor") field(OUT, "@asyn(FWDPUTF,0)") field(MRES, "0.001") field(VELO, "1") field(DHLM, "100") field(DLLM, "-100") field(FLNK, "MR:T") }
record(calcout, "MR:T") { field(CALC, "1") field(ODLY, "2.0") field(OOPT, "Every Time") field(FLNK, "MR:TC") }
record(calc, "MR:TC") { field(CALC, "A+1") field(INPA, "MR:TC") }
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn sleep_until(t0: Instant, secs: f64) {
    let at = t0 + Duration::from_secs_f64(secs);
    if let Some(left) = at.checked_duration_since(Instant::now()) {
        tokio::time::sleep(left).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_done_pass_forwards_the_putf_of_the_move() {
    register_controller_port(&open_owner(), "FWDPUTF", 1, ()).unwrap();
    let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> =
        Arc::new(Mutex::new(motor_rs::sim_motor::SimMotor::new()));
    let AxisSetup {
        axis,
        poll_loop,
        poll_cmd_tx,
    } = MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: "FWDPUTF".into(),
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
    // The startup status pass's FLNK runs MR:T once; let it end.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let tc = num(&db, "MR:TC");

    let t0 = Instant::now();
    db.put_record_field_from_ca_no_notify("MR:M", "VAL", EpicsValue::Double(0.5))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify("MR:T", "PROC", EpicsValue::Double(1.0))
        .await
        .unwrap();
    sleep_until(t0, 1.5).await;
    let mid = ["MR:M.DMOV", "MR:T.PACT", "MR:T.RPRO", "MR:M.PUTF"].map(|pv| num(&db, pv));
    let mut seen = vec![num(&db, "MR:TC") - tc];
    for at in [2.5, 5.0] {
        sleep_until(t0, at).await;
        seen.push(num(&db, "MR:TC") - tc);
    }
    assert_eq!(
        (mid, seen),
        ([1.0, 1.0, 1.0, 0.0], vec![0.0, 1.0, 2.0]),
        "DMOV, MR:T PACT/RPRO, PUTF at 1.5 s; MR:TC deltas at 1.5, 2.5 and 5 s"
    );
    let _ = poll_cmd_tx
        .send(motor_rs::poll_loop::PollCommand::Shutdown)
        .await;
}
