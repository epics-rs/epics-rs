//! A NaN or infinite coordinate loaded before iocInit is one C init_record
//! keeps: a loaded OFF makes VAL, RBV and the user limits non-finite, an
//! RSTM=Always DVAL is written to the driver as its position (motorSim then
//! reads -2147483648), and closed loop keeps VAL and DVAL as loaded. The
//! port takes the readback for VAL and DVAL and the field default for OFF.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use motor_rs::builder::{ControllerAxis, ControllerBuilder};
use motor_rs::devices::{device_support_factory, devices, register_axis, register_controller_port};
use motor_rs::poll_loop::PollCommand;
use motor_rs::trace::TraceUser;

/// A done axis at step 3 that counts its position loads.
struct AtThree(Arc<AtomicU32>);

impl AsynMotor for AtThree {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn move_relative(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        *status = MotorStatus {
            done: true,
            ..MotorStatus::default()
        }
        .with_position(3.0);
        Ok(())
    }
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loaded_coordinate_that_is_not_finite_is_replaced_at_init() {
    let port = "NFLOAD";
    let recs = [
        (
            "NF:off",
            r#"field(OFF, "nan") field(DHLM, "10") field(DLLM, "-10")"#,
        ),
        ("NF:offinf", r#"field(OFF, "-inf")"#),
        ("NF:dval", r#"field(DVAL, "nan") field(RSTM, "Always")"#),
        (
            "NF:cl",
            r#"field(OMSL, "closed_loop") field(VAL, "nan") field(DVAL, "inf")"#,
        ),
    ];
    register_controller_port(&open_owner(), port, recs.len() as i32, ()).unwrap();
    let mut controller = ControllerBuilder::new();
    let loads: Vec<Arc<AtomicU32>> = recs.iter().map(|_| Arc::default()).collect();
    let axes: Vec<ControllerAxis> = loads
        .iter()
        .enumerate()
        .map(|(addr, loads)| {
            controller.axis(
                Arc::new(Mutex::new(AtThree(loads.clone()))),
                TraceUser::Asyn {
                    port: port.into(),
                    addr: addr as i32,
                },
            )
        })
        .collect();
    let hour = Duration::from_secs(3600);
    let first = controller.axes();
    let poller = controller.build(hour, hour, 0).start();
    let deadline = Instant::now() + Duration::from_secs(5);
    while first.first_polls_pending() > 0 {
        assert!(Instant::now() < deadline, "the startup pass never ended");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let polls: Vec<_> = axes.iter().map(|a| a.poll_cmd_tx.clone()).collect();
    for ControllerAxis { axis, .. } in axes {
        register_axis(axis).unwrap();
    }
    let db: String = recs
        .iter()
        .enumerate()
        .map(|(addr, (rec, fields))| {
            format!(
                r#"record(motor, "{rec}") {{ field(DTYP, "asynMotor") field(OUT, "@asyn({port},{addr})") field(MRES, "1") {fields} }}
"#
            )
        })
        .collect();
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build_served()
        .await
        .unwrap();
    for ((rec, _), loads) in recs.iter().zip(&loads) {
        assert_eq!(loads.load(Ordering::SeqCst), 0, "{rec} loaded no position");
        for field in ["VAL", "DVAL", "RBV", "LDVL"] {
            assert_eq!(num(&db, &format!("{rec}.{field}")), 3.0, "{rec}.{field}");
        }
        assert_eq!(num(&db, &format!("{rec}.OFF")), 0.0, "{rec}.OFF");
    }
    assert_eq!(num(&db, "NF:off.HLM"), 10.0);
    assert_eq!(num(&db, "NF:off.LLM"), -10.0);
    polls[0].send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}
