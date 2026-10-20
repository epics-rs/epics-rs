//! Seven C model-1 device supports force NTM to NO in init_record, as they
//! do their own backlash (devEMC18011.cc:127-129, devPIE816.cc:130,
//! devMDT695.cc:129, ...); a record's own NTM stands otherwise.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, Model1Motor, MotorStatus, PollFailure};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::DevSup;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::devices::{
    device_support_factory, devices, model1_device, register_axis, register_model1_device,
};
use motor_rs::trace::TraceUser;

static MODEL1: DevSup = model1_device("devTestNtm", "TestNtm");

struct Card {
    disables_ntm: bool,
}

/// `motor_init` reads the card with the status function the task queries.
impl Model1Motor for Card {
    fn take_init_status(&mut self) -> MotorStatus {
        let mut status = MotorStatus::default();
        self.poll(&AsynUser::default(), &mut status)
            .expect("the card answers");
        status
    }
}

impl AsynMotor for Card {
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
        Ok(())
    }
    fn disables_ntm(&self) -> bool {
        self.disables_ntm
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        *status = MotorStatus {
            done: true,
            ..MotorStatus::default()
        };
        Ok(())
    }
}

async fn ntm_after_init(disables_ntm: bool, card: i32) -> EpicsValue {
    register_model1_device(&MODEL1);
    let AxisSetup { axis, .. } = MotorBuilder::model1(Arc::new(Mutex::new(Card { disables_ntm })))
        .trace_user(TraceUser::Model1 {
            device: MODEL1.choice,
            card,
            signal: 0,
        })
        .poll_interval(Duration::from_millis(10))
        .build_axis();
    register_axis(axis).unwrap();
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(
            &format!(
                r##"
record(motor, "N:m{card}") {{ field(DTYP, "TestNtm") field(OUT, "#C{card} S0 @") field(NTM, "YES") }}
"##
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.get_pv(format!("N:m{card}.NTM")).unwrap()
}

#[tokio::test]
async fn a_device_doing_its_own_backlash_forces_ntm_off_at_init() {
    assert_eq!(ntm_after_init(true, 0).await, EpicsValue::Short(0));
}

#[tokio::test]
async fn ntm_stands_for_any_other_device() {
    assert_eq!(ntm_after_init(false, 1).await, EpicsValue::Short(1));
}
