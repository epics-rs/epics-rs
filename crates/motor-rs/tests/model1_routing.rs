//! motordrvCom calls back only the record whose message it answers: an
//! INFO's own node, and the held move's `mrecord` (motordrvCom.cc:300-318,
//! :441-471). A record motordevCom refuses keeps its transactions, which
//! still reach the motor task (motordevCom.cc:237-262, :486-519).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{Model1Motor, MotorStatus, PollFailure};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::DevSup;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::devices::{
    device_support_factory, devices, model1_device, register_axis, register_model1_device,
};
use motor_rs::trace::TraceUser;

static MODEL1: DevSup = model1_device("devTestRouting", "TestRouting");

/// Steps one count per poll toward its last target, done there; the test
/// may move it by hand. Keeps every move it was sent.
#[derive(Default)]
struct Card {
    position: Arc<Mutex<f64>>,
    target: Option<f64>,
    moves: Arc<Mutex<Vec<f64>>>,
}

impl Model1Motor for Card {
    fn set_velocity(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_vel_base(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_accel(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn move_absolute(&mut self, _: &AsynUser, p: f64) -> AsynResult<()> {
        self.moves.lock().unwrap().push(p);
        self.target = Some(p);
        Ok(())
    }
    fn move_relative(&mut self, _: &AsynUser, d: f64) -> AsynResult<()> {
        let p = *self.position.lock().unwrap() + d;
        self.move_absolute(&AsynUser::default(), p)
    }
    fn move_velocity(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        let mut position = self.position.lock().unwrap();
        if let Some(target) = self.target {
            *position += (target - *position).clamp(-1.0, 1.0);
            if *position == target {
                self.target = None;
            }
        }
        *status = MotorStatus {
            done: self.target.is_none(),
            moving: self.target.is_some(),
            ..MotorStatus::default().with_position(*position)
        };
        Ok(())
    }
    fn update_delay(&self) -> Duration {
        Duration::ZERO
    }
    fn take_init_status(&mut self) -> MotorStatus {
        let mut status = MotorStatus::default();
        self.poll(&AsynUser::default(), &mut status)
            .expect("the card answers");
        status
    }
}

/// `motor` as card `card` signal 0, its motor task querying every 10 ms,
/// and `db` past iocInit.
async fn boot(card: i32, motor: Card, db: &str) -> Arc<PvDatabase> {
    register_model1_device(&MODEL1);
    let AxisSetup {
        axis, poll_loop, ..
    } = MotorBuilder::model1(Arc::new(Mutex::new(motor)))
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
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    db
}

fn field(db: &PvDatabase, pv: &str) -> EpicsValue {
    db.get_pv(pv).unwrap()
}

/// Wait up to 2 s for `pv` to read `want`.
async fn until(db: &PvDatabase, pv: &str, want: EpicsValue) {
    for _ in 0..200 {
        if field(db, pv) == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{pv} is {:?}, not {want:?}", field(db, pv));
}

/// Two records on one signal: the first binds it, the second is the one
/// motor_init_record_com refuses as already in use (motordevCom.cc:244-247).
fn two_records(card: i32) -> String {
    format!(
        r##"
record(motor, "R{card}:a") {{ field(DTYP, "TestRouting") field(OUT, "#C{card} S0 @") field(MRES, "1") field(RTRY, "0") }}
record(motor, "R{card}:b") {{ field(DTYP, "TestRouting") field(OUT, "#C{card} S0 @") field(MRES, "1") field(RTRY, "0") }}
"##
    )
}

/// The refused record's GET_INFO reaches the card, and only it reads the
/// reply (motordrvCom.cc:441-471).
#[tokio::test]
async fn an_info_reply_reaches_only_the_record_that_asked() {
    let card = Card::default();
    let position = card.position.clone();
    let db = boot(1, card, &two_records(1)).await;
    *position.lock().unwrap() = 7.0;
    db.put_record_field_from_ca("R1:b", "STUP", EpicsValue::Short(1))
        .await
        .unwrap();
    until(&db, "R1:b.RMP", EpicsValue::Long(7)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(field(&db, "R1:a.RMP"), EpicsValue::Long(0));
}

/// The refused record's move reaches the card, and each query of the held
/// move calls back only it (motordrvCom.cc:300-318).
#[tokio::test]
async fn a_held_move_is_queried_for_its_sender_alone() {
    let card = Card::default();
    let moves = card.moves.clone();
    let db = boot(2, card, &two_records(2)).await;
    db.put_record_field_from_ca("R2:b", "VAL", EpicsValue::Double(3.0))
        .await
        .unwrap();
    until(&db, "R2:b.RMP", EpicsValue::Long(3)).await;
    until(&db, "R2:b.DMOV", EpicsValue::Short(1)).await;
    assert_eq!(*moves.lock().unwrap(), [3.0]);
    assert_eq!(field(&db, "R2:a.RMP"), EpicsValue::Long(0));
}

/// C defect not copied (motordrvCom.cc:403-418): a move from the second
/// record frees the first's held move with no callback, so the first's
/// DMOV stays 0. Here the first is called back done.
#[tokio::test]
async fn a_move_replaced_by_another_records_ends_done() {
    let card = Card::default();
    let position = card.position.clone();
    let db = boot(3, card, &two_records(3)).await;
    db.put_record_field_from_ca("R3:a", "VAL", EpicsValue::Double(1000.0))
        .await
        .unwrap();
    until(&db, "R3:a.DMOV", EpicsValue::Short(0)).await;
    while *position.lock().unwrap() < 2.0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    db.put_record_field_from_ca("R3:b", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    until(&db, "R3:a.DMOV", EpicsValue::Short(1)).await;
    until(&db, "R3:b.DMOV", EpicsValue::Short(1)).await;
}

/// A signal the card lacks: motordevCom refuses the record, and the motor
/// task answers each of its transactions with RA_PROBLEM and zeroed
/// positions, never calling the driver (motordrvCom.cc:491-498).
#[tokio::test]
async fn a_missing_signal_answers_each_transaction_with_a_problem() {
    let card = Card::default();
    let moves = card.moves.clone();
    let db = boot(
        4,
        card,
        r##"
record(motor, "R4:m") { field(DTYP, "TestRouting") field(OUT, "#C4 S4 @") field(MRES, "1") field(RTRY, "0") }
"##,
    )
    .await;
    let mut dmov = db
        .get_record("R4:m")
        .unwrap()
        .write()
        .add_subscriber("DMOV", 1, DbFieldType::Short, EventMask::VALUE.bits())
        .unwrap();
    db.put_record_field_from_ca("R4:m", "VAL", EpicsValue::Double(2.0))
        .await
        .unwrap();
    until(&db, "R4:m.DMOV", EpicsValue::Short(1)).await;
    let posted: Vec<_> = std::iter::from_fn(|| dmov.try_recv().ok())
        .map(|event| event.value().as_value().cloned().unwrap())
        .collect();
    assert_eq!(posted, [EpicsValue::Short(0), EpicsValue::Short(1)]);
    assert_eq!(field(&db, "R4:m.MSTA"), EpicsValue::ULong(0x200));
    assert_eq!(field(&db, "R4:m.RMP"), EpicsValue::Long(0));
    assert!(moves.lock().unwrap().is_empty());
}
