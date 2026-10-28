//! A model-1 driver receives each SET_VELOCITY / SET_VEL_BASE / SET_ACCEL
//! the record writes, in the record's order: C's `build_trans` appends each
//! WRITE_MSG to the message (motordevCom.cc), so a driver's preamble holds
//! exactly what motorRecord.cc wrote and nothing it did not.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{Model1Motor, MotorStatus, PollFailure};
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

static MODEL1: DevSup = model1_device("devTestSpeedWrites", "TestSpeedWrites");

#[derive(Clone, Copy, Debug, PartialEq)]
enum Call {
    Velocity(f64),
    VelBase(f64),
    Accel(f64),
    Move(f64),
    Jog(f64),
    Home(bool),
    Stop,
}

/// Ends a move at its target and a home at 0 on the next poll; a jog, and
/// a move while `holds`, run a step a poll until stopped.
#[derive(Default)]
struct Card {
    position: f64,
    target: Option<f64>,
    running: bool,
    holds: bool,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl Card {
    fn log(&self, call: Call) -> AsynResult<()> {
        self.calls.lock().unwrap().push(call);
        Ok(())
    }
}

impl Model1Motor for Card {
    fn set_velocity(&mut self, _: &AsynUser, velocity: f64) -> AsynResult<()> {
        self.log(Call::Velocity(velocity))
    }
    fn set_vel_base(&mut self, _: &AsynUser, velocity: f64) -> AsynResult<()> {
        self.log(Call::VelBase(velocity))
    }
    fn set_accel(&mut self, _: &AsynUser, acceleration: f64) -> AsynResult<()> {
        self.log(Call::Accel(acceleration))
    }
    fn move_absolute(&mut self, _: &AsynUser, position: f64) -> AsynResult<()> {
        if self.holds {
            self.running = true;
        } else {
            self.target = Some(position);
        }
        self.log(Call::Move(position))
    }
    fn move_relative(&mut self, user: &AsynUser, distance: f64) -> AsynResult<()> {
        self.move_absolute(user, self.position + distance)
    }
    fn move_velocity(&mut self, _: &AsynUser, velocity: f64) -> AsynResult<()> {
        self.running = true;
        self.log(Call::Jog(velocity))
    }
    fn home(&mut self, _: &AsynUser, forward: bool) -> AsynResult<()> {
        self.target = Some(0.0);
        self.log(Call::Home(forward))
    }
    fn stop(&mut self, _: &AsynUser) -> AsynResult<()> {
        self.running = false;
        self.log(Call::Stop)
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        if self.running {
            self.position += 1.0;
        } else if let Some(target) = self.target.take() {
            self.position = target;
        }
        *status = MotorStatus {
            position: self.position,
            done: !self.running,
            moving: self.running,
            direction: true,
            ..MotorStatus::default()
        };
        Ok(())
    }
    fn update_delay(&self) -> Duration {
        Duration::ZERO
    }
    fn take_init_status(&mut self) -> MotorStatus {
        MotorStatus {
            done: true,
            ..MotorStatus::default()
        }
    }
}

/// Raw units are EGU at MRES 1. A move's ACCL gives (VELO - VBAS) / ACCL
/// = 6, a home's (HVEL - VBAS) / ACCL = 2, the backlash's
/// (BVEL - VBAS) / BACC = 2.
const FIELDS: &str = r#"field(MRES, "1") field(VELO, "4") field(VBAS, "1") field(ACCL, "0.5") field(HVEL, "2") field(JVEL, "3") field(JAR, "5") field(BVEL, "2") field(BACC, "0.5") field(RTRY, "0")"#;

/// Record `W:m{card}` on a [`Card`] holding moves if `holds`, with
/// `fields` beside [`FIELDS`], past iocInit.
async fn boot(card: i32, holds: bool, fields: &str) -> (Arc<PvDatabase>, Arc<Mutex<Vec<Call>>>) {
    register_model1_device(&MODEL1);
    let motor = Card {
        holds,
        ..Card::default()
    };
    let calls = motor.calls.clone();
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
        .db_string(
            &format!(
                r##"
record(motor, "W:m{card}") {{ field(DTYP, "TestSpeedWrites") field(OUT, "#C{card} S0 @") {FIELDS} {fields} }}
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

async fn put(db: &PvDatabase, card: i32, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca(&format!("W:m{card}"), field, value)
        .await
        .unwrap();
}

/// The calls once `until` holds of them and DMOV is back at 1.
async fn settled(
    db: &PvDatabase,
    card: i32,
    calls: &Mutex<Vec<Call>>,
    until: impl Fn(&[Call]) -> bool,
) -> Vec<Call> {
    for _ in 0..300 {
        let dmov = db.get_pv(format!("W:m{card}.DMOV")).unwrap();
        if until(&calls.lock().unwrap()) && dmov == EpicsValue::Short(1) {
            return calls.lock().unwrap().clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never settled: {:?}", calls.lock().unwrap());
}

async fn running(calls: &Mutex<Vec<Call>>, call: Call) {
    for _ in 0..300 {
        if calls.lock().unwrap().contains(&call) {
            // A pass or two, so MOVN is up.
            tokio::time::sleep(Duration::from_millis(50)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{call:?} never reached the card");
}

// motorRecord.cc:2527-2534: SET_VELOCITY, SET_VEL_BASE, SET_ACCEL, MOVE_ABS.
#[tokio::test]
async fn a_move_writes_velocity_base_and_accel_ahead_of_it() {
    let (db, calls) = boot(0, false, "").await;
    put(&db, 0, "VAL", EpicsValue::Double(5.0)).await;
    let calls = settled(&db, 0, &calls, |c| c.contains(&Call::Move(5.0))).await;
    use Call::*;
    assert_eq!(calls, [Velocity(4.0), VelBase(1.0), Accel(6.0), Move(5.0)]);
}

// motorRecord.cc:2140-2142: a jog writes SET_VEL_BASE, SET_ACCEL (JAR),
// JOG, and no SET_VELOCITY.
#[tokio::test]
async fn a_jog_writes_base_and_accel_ahead_of_it() {
    let (db, calls) = boot(1, false, "").await;
    put(&db, 1, "JOGF", EpicsValue::Short(1)).await;
    running(&calls, Call::Jog(3.0)).await;
    use Call::*;
    assert_eq!(*calls.lock().unwrap(), [VelBase(1.0), Accel(5.0), Jog(3.0)]);
}

// The first leg stops BDST short at the slew speeds (motorRecord.cc:
// 2511-2534); the backlash leg writes SET_VELOCITY BVEL and SET_ACCEL BACC
// and no SET_VEL_BASE (:955-968).
#[tokio::test]
async fn a_move_s_backlash_leg_writes_velocity_and_accel_alone() {
    let (db, calls) = boot(2, false, r#"field(BDST, "1")"#).await;
    put(&db, 2, "VAL", EpicsValue::Double(5.0)).await;
    let calls = settled(&db, 2, &calls, |c| c.contains(&Call::Move(5.0))).await;
    use Call::*;
    assert_eq!(
        calls,
        [
            Velocity(4.0),
            VelBase(1.0),
            Accel(6.0),
            Move(4.0),
            Velocity(2.0),
            Accel(2.0),
            Move(5.0),
        ]
    );
}

// A stopped jog takes the backlash out at the slew speeds (motorRecord.cc:
// 937-945), then its last leg writes SET_VELOCITY BVEL and SET_ACCEL BACC
// and no SET_VEL_BASE (:1004-1017).
#[tokio::test]
async fn a_jog_s_backlash_leg_writes_velocity_and_accel_alone() {
    let (db, calls) = boot(3, false, r#"field(BDST, "1")"#).await;
    put(&db, 3, "JOGF", EpicsValue::Short(1)).await;
    running(&calls, Call::Jog(3.0)).await;
    put(&db, 3, "JOGF", EpicsValue::Short(0)).await;
    let calls = settled(&db, 3, &calls, |c| {
        c.iter().filter(|c| matches!(c, Call::Move(_))).count() == 2
    })
    .await;
    let stop = calls.iter().position(|c| *c == Call::Stop).unwrap();
    let [Call::Move(take_out), Call::Move(at)] = [calls[stop + 4], calls[stop + 7]] else {
        panic!("{calls:?}");
    };
    assert_eq!(at - take_out, 1.0, "{calls:?}");
    use Call::*;
    assert_eq!(
        calls[stop..],
        [
            Stop,
            Velocity(4.0),
            VelBase(1.0),
            Accel(6.0),
            Move(take_out),
            Velocity(2.0),
            Accel(2.0),
            Move(at),
        ]
    );
}

// motorRecord.cc:2052-2063: SET_VELOCITY HVEL, SET_VEL_BASE, SET_ACCEL,
// HOME_FOR.
#[tokio::test]
async fn a_home_writes_velocity_base_and_accel_ahead_of_it() {
    let (db, calls) = boot(4, false, "").await;
    put(&db, 4, "HOMF", EpicsValue::Short(1)).await;
    let calls = settled(&db, 4, &calls, |c| c.contains(&Call::Home(true))).await;
    use Call::*;
    assert_eq!(calls, [Velocity(2.0), VelBase(1.0), Accel(2.0), Home(true)]);
}

// A home asked mid-move stops first (motorRecord.cc:2026-2032), then
// writes SET_VELOCITY HVEL, SET_VEL_BASE, SET_ACCEL, HOME_FOR once stopped
// (:872-883).
#[tokio::test]
async fn a_home_after_a_stop_writes_velocity_base_and_accel_ahead_of_it() {
    let (db, calls) = boot(5, true, "").await;
    put(&db, 5, "VAL", EpicsValue::Double(50.0)).await;
    running(&calls, Call::Move(50.0)).await;
    put(&db, 5, "HOMF", EpicsValue::Short(1)).await;
    let calls = settled(&db, 5, &calls, |c| c.contains(&Call::Home(true))).await;
    use Call::*;
    assert_eq!(
        calls,
        [
            Velocity(4.0),
            VelBase(1.0),
            Accel(6.0),
            Move(50.0),
            Stop,
            Velocity(2.0),
            VelBase(1.0),
            Accel(2.0),
            Home(true),
        ]
    );
}

// motorRecord.cc:3058-3071: a JVEL write on a running jog writes SET_ACCEL
// and JOG_VELOCITY. Most model-1 `build_trans` have no JOG_VELOCITY case
// (devMM3000.cc:333 `default:`) and append nothing for it, so the card sees
// the SET_ACCEL and no second JOG.
#[tokio::test]
async fn a_jvel_retune_of_a_running_jog_does_not_jog_again() {
    let (db, calls) = boot(6, false, "").await;
    put(&db, 6, "JOGF", EpicsValue::Short(1)).await;
    running(&calls, Call::Jog(3.0)).await;
    put(&db, 6, "JVEL", EpicsValue::Double(2.0)).await;
    for _ in 0..300 {
        if calls.lock().unwrap().len() > 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    use Call::*;
    assert_eq!(
        *calls.lock().unwrap(),
        [VelBase(1.0), Accel(5.0), Jog(3.0), Accel(5.0)]
    );
}
