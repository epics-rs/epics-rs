//! A model-1 driver polling a move reads and may set the moving record's
//! CDIR through `nodeptr->mrecord`: SC800 and SmartMotor take RA_DIRECTION
//! from it (drvSC800.cc:279-282), OMS forces it to the controller's
//! direction at a limit (drvOms58.cc:642-649).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::DevSup;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::devices::{
    device_support_factory, devices, model1_device, register_axis, register_model1_device,
};
use motor_rs::trace::TraceUser;

static MODEL1: DevSup = model1_device("devTestCdir", "TestCdir");

/// Moves in one poll; reports the controller's direction as reverse.
#[derive(Default)]
struct Card {
    position: f64,
    target: Option<f64>,
    seen: Arc<Mutex<Vec<bool>>>,
}

impl AsynMotor for Card {
    fn move_absolute(&mut self, _: &AsynUser, p: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        self.target = Some(p);
        Ok(())
    }
    fn move_relative(&mut self, _: &AsynUser, d: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        self.target = Some(self.position + d);
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
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        if let Some(target) = self.target.take() {
            self.position = target;
        }
        Ok(MotorStatus {
            position: self.position,
            ..MotorStatus::default()
        })
    }
    fn poll_in_motion(&mut self, user: &AsynUser, cdir: &mut bool) -> AsynResult<MotorStatus> {
        self.seen.lock().unwrap().push(*cdir);
        *cdir = false;
        self.poll(user)
    }
}

#[tokio::test]
async fn the_driver_reads_and_sets_the_moving_records_cdir() {
    register_model1_device(&MODEL1);
    let card = Card::default();
    let seen = card.seen.clone();
    let AxisSetup {
        axis, poll_loop, ..
    } = MotorBuilder::new(Arc::new(Mutex::new(card)))
        .trace_user(TraceUser::Model1 {
            device: MODEL1.choice,
            card: 0,
            signal: 0,
        })
        .poll_interval(Duration::from_millis(10))
        .build_axis();
    register_axis(axis).unwrap();
    tokio::spawn(poll_loop.run());
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(
            r##"
record(motor, "D:m") { field(DTYP, "TestCdir") field(OUT, "#C0 S0 @") field(MRES, "1") field(VELO, "1") field(RTRY, "0") }
"##,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    db.put_record_field_from_ca("D:m", "VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    for _ in 0..200 {
        if db.get_pv("D:m.DMOV").unwrap() == EpicsValue::Short(1)
            && !seen.lock().unwrap().is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(seen.lock().unwrap().first(), Some(&true), "forward move");
    assert_eq!(db.get_pv("D:m.CDIR").unwrap(), EpicsValue::Short(0));
}
