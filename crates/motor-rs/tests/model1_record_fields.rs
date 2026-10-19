//! A model-1 device reads record fields in `build_trans`: a faulhaber HOME
//! runs at VELO, not at the HVEL the record sent, in revolutions of SREV
//! (devMCDC2805.cc:224-227).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure, RecordFields};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::DevSup;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::devices::{
    device_support_factory, devices, model1_device, register_axis, register_model1_device,
};
use motor_rs::trace::TraceUser;

static MODEL1: DevSup = model1_device("devTestFields", "TestFields");

#[derive(Debug, PartialEq)]
enum Call {
    Fields(RecordFields),
    Home(f64),
    SetPosition,
}

/// Done at every poll; logs the calls the record's commands make.
#[derive(Default)]
struct Card {
    calls: Arc<Mutex<Vec<Call>>>,
}

impl AsynMotor for Card {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, velocity: f64, _: f64, _: bool) -> AsynResult<()> {
        self.calls.lock().unwrap().push(Call::Home(velocity));
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        self.calls.lock().unwrap().push(Call::SetPosition);
        Ok(())
    }
    fn set_record_fields(&mut self, _: &AsynUser, fields: &RecordFields) -> AsynResult<()> {
        self.calls.lock().unwrap().push(Call::Fields(*fields));
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        Ok(MotorStatus {
            done: true,
            ..MotorStatus::default()
        })
    }
}

async fn boot(card: i32) -> (Arc<PvDatabase>, Arc<Mutex<Vec<Call>>>) {
    register_model1_device(&MODEL1);
    let motor = Card::default();
    let calls = motor.calls.clone();
    let AxisSetup {
        axis, poll_loop, ..
    } = MotorBuilder::new(Arc::new(Mutex::new(motor)))
        .trace_user(TraceUser::Model1 {
            device: MODEL1.choice,
            card,
            signal: 0,
        })
        .poll_interval(Duration::from_millis(10))
        .build_axis();
    register_axis(axis).unwrap();
    tokio::spawn(poll_loop.start());
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(
            &format!(
                r##"
record(motor, "V:m{card}") {{ field(DTYP, "TestFields") field(OUT, "#C{card} S0 @") field(MRES, "0.5") field(VELO, "3") field(HVEL, "1") field(SREV, "400") field(RTRY, "0") }}
"##
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    (db, calls)
}

async fn wait_for(calls: &Mutex<Vec<Call>>, done: impl Fn(&[Call]) -> bool) {
    for _ in 0..200 {
        if done(&calls.lock().unwrap()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_home_is_given_the_records_fields_ahead_of_it() {
    let (db, calls) = boot(0).await;
    db.put_record_field_from_ca("V:m0", "HOMF", EpicsValue::Short(1))
        .await
        .unwrap();
    wait_for(&calls, |c| c.iter().any(|c| matches!(c, Call::Home(_)))).await;
    // VELO 3 / MRES 0.5; the home itself runs at HVEL 1 / 0.5.
    let fields = RecordFields {
        velocity: 6.0,
        srev: 400,
    };
    assert_eq!(
        *calls.lock().unwrap(),
        [Call::Fields(fields), Call::Home(2.0)]
    );
}

#[tokio::test]
async fn a_transaction_that_does_not_move_carries_no_fields() {
    let (db, calls) = boot(1).await;
    db.put_record_field_from_ca("V:m1", "SET", EpicsValue::Short(1))
        .await
        .unwrap();
    db.put_record_field_from_ca("V:m1", "DVAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    wait_for(&calls, |c| !c.is_empty()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(*calls.lock().unwrap(), [Call::SetPosition]);
}
