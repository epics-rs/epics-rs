//! An axis a separate CreateAxis adds once the poller runs is polled before
//! iocInit reads it, as C's poller waiting on the `pC->lock()`
//! smarActMCSCreateAxis holds (smarActMCSMotorDriver.cpp:816-819) polls it
//! (asynMotorController.cpp:644-658). Reading an unpolled position 0,
//! init_controller writes the autosaved DVAL (devMotorAsyn.c:175-176,
//! :219-224).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{ControllerAxis, ControllerBuilder};
use motor_rs::devices::{device_support_factory, devices, register_axis, register_controller_port};
use motor_rs::poll_loop::PollCommand;
use motor_rs::trace::TraceUser;

/// A stage parked at step 5000, recording every position set on it.
struct Parked {
    set: Arc<Mutex<Vec<f64>>>,
}

impl AsynMotor for Parked {
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
    fn set_position(&mut self, _: &AsynUser, position: f64) -> AsynResult<()> {
        self.set.lock().unwrap().push(position);
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        *status = MotorStatus {
            position: 5000.0,
            done: true,
            ..MotorStatus::default()
        };
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_autosaved_dval_is_not_written_to_a_joined_axis() {
    register_controller_port(&open_owner(), "JOINDV", 1, ()).unwrap();
    let controller = ControllerBuilder::new();
    let axes = controller.axes();
    // No timed pass: the startup pass, over no axis, is the only one due.
    let hour = Duration::from_secs(3600);
    let poller = controller.build(hour, hour, 0).start();
    let set = Arc::new(Mutex::new(Vec::new()));
    let motor = Parked { set: set.clone() };
    let user = TraceUser::Asyn {
        port: "JOINDV".into(),
        addr: 0,
    };
    let ControllerAxis { axis, poll_cmd_tx } =
        tokio::task::spawn_blocking(move || axes.add(Arc::new(Mutex::new(motor)), user))
            .await
            .unwrap()
            .unwrap();
    register_axis(axis).unwrap();
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(
            r#"record(motor, "JD:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(JOINDV,0)") field(MRES, "1e-3") field(DVAL, "3") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    // Time for a set the init queued to reach the driver.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(*set.lock().unwrap(), Vec::<f64>::new());
    assert_eq!(db.get_pv("JD:m.DRBV").unwrap(), EpicsValue::Double(5.0));
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}
