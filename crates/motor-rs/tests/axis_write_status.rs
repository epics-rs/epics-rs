//! A port write to an axis returns the driver's status, as C's
//! asynMotorController::writeInt32 / writeFloat64 do
//! (asynMotorController.cpp:203-254, :277-460), so a failed call leaves the
//! asyn record at WRITE/INVALID (devAsynInt32.c, devAsynFloat64.c).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::MotorBuilder;
use motor_rs::devices::{register_axis, register_controller_port};
use motor_rs::trace::TraceUser;

/// An axis whose servo and moves fail on the wire.
struct Failing;

fn failed() -> AsynResult<()> {
    Err(AsynError::Status {
        status: AsynStatus::Error,
        message: "refused".into(),
    })
}

impl AsynMotor for Failing {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        failed()
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
        Ok(())
    }
    fn set_closed_loop(&mut self, _: &AsynUser, _: bool) -> AsynResult<()> {
        failed()
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        *status = MotorStatus::default();
        Ok(())
    }
}

fn alarm(db: &PvDatabase, name: &str) -> (EpicsValue, EpicsValue) {
    (
        db.get_pv(format!("{name}.STAT")).unwrap(),
        db.get_pv(format!("{name}.SEVR")).unwrap(),
    )
}

async fn idle(db: &PvDatabase, name: &str) {
    for _ in 0..400 {
        if !db.get_record(name).unwrap().read().is_processing() {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{name} never completed");
}

#[epics_base_rs::epics_test]
async fn a_failed_axis_write_leaves_the_record_at_write_invalid() {
    let db = Arc::new(PvDatabase::new());
    register_controller_port(db.scan_control().interrupt_gate(), "AWSTAT", 1, ()).unwrap();
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Failing));
    let setup = MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: "AWSTAT".into(),
            addr: 0,
        })
        .build_axis();
    register_axis(setup.axis).unwrap();
    let records = r#"
        record(bo, "SERVO") { field(DTYP, "asynInt32") field(OUT, "@asyn(AWSTAT,0,1)MOTOR_CLOSED_LOOP") }
        record(ao, "MOVE") { field(DTYP, "asynFloat64") field(OUT, "@asyn(AWSTAT,0,1)MOTOR_MOVE_ABS") }
    "#;
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    let write_invalid = (
        EpicsValue::Short(alarm_status::WRITE_ALARM as i16),
        EpicsValue::Short(3),
    );
    for (name, value) in [
        ("SERVO", EpicsValue::Short(1)),
        ("MOVE", EpicsValue::Double(5.0)),
    ] {
        db.put_record_field_from_ca(name, "VAL", value)
            .await
            .unwrap();
        idle(&db, name).await;
        assert_eq!(alarm(&db, name), write_invalid, "{name}");
    }
}
