//! A STOP of a model-1 move whose driver reads no position until the move
//! is done (drvEMC18011.cc:356-358): every pass is called back, so MOVN is
//! set, the STOP waits for the move to end (motorRecord.cc:1887-1891) and
//! postProcess makes VAL the stopped readback (:826-838), with no retry.

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

static MODEL1: DevSup = model1_device("devTestStopSync", "TestStopSync");

/// Busy until stopped, then at 508 steps; the position reads 0 until done.
#[derive(Default)]
struct Card {
    position: f64,
    busy: bool,
    stopped: bool,
    moves: Arc<Mutex<Vec<f64>>>,
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
    fn move_absolute(&mut self, _: &AsynUser, p: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        self.moves.lock().unwrap().push(p);
        self.busy = true;
        Ok(())
    }
    fn move_relative(
        &mut self,
        _: &AsynUser,
        _distance: f64,
        _: f64,
        _: f64,
        _: f64,
    ) -> AsynResult<()> {
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        self.stopped = true;
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn no_motion_test(&self) -> bool {
        false
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        if self.busy && self.stopped {
            self.busy = false;
            self.position = 508.0;
        }
        *status = MotorStatus {
            position: self.position,
            done: !self.busy,
            direction: true,
            ..MotorStatus::default()
        };
        Ok(())
    }
}

#[tokio::test]
async fn a_stop_of_a_move_with_a_still_readback_ends_at_the_stopped_position() {
    register_model1_device(&MODEL1);
    let card = Card::default();
    let moves = card.moves.clone();
    let AxisSetup {
        axis, poll_loop, ..
    } = MotorBuilder::model1(Arc::new(Mutex::new(card)))
        .trace_user(TraceUser::Model1 {
            device: MODEL1.choice,
            card: 0,
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
            r##"
record(motor, "T:m") { field(DTYP, "TestStopSync") field(OUT, "#C0 S0 @") field(MRES, "0.01") field(VELO, "2") field(HLM, "20") field(LLM, "-20") }
"##,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    let get = |field: &str| db.get_pv(format!("T:m.{field}")).unwrap();
    db.put_record_field_from_ca("T:m", "VAL", EpicsValue::Double(8.0))
        .await
        .unwrap();
    for _ in 0..100 {
        if get("MOVN") == EpicsValue::Short(1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    db.put_record_field_from_ca("T:m", "STOP", EpicsValue::Short(1))
        .await
        .unwrap();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if get("DMOV") == EpicsValue::Short(1) {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(get("RBV"), EpicsValue::Double(5.08));
    assert_eq!(get("VAL"), EpicsValue::Double(5.08));
    assert_eq!(*moves.lock().unwrap(), [800.0], "no retry");
}
