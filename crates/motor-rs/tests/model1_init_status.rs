//! C motor_task has no startup pass: a model-1 axis's first status is the
//! one its driver's `motor_init` read with `set_status` (drvSoloist.cc:734,
//! drvMVP2001.cc:720), and the record's init reads it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, Model1Motor, MotorStatus, PollFailure};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::DevSup;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::devices::{
    device_support_factory, devices, model1_device, register_axis, register_model1_device,
};
use motor_rs::flags::MstaFlags;
use motor_rs::trace::TraceUser;

static MODEL1: DevSup = model1_device("devTestInitStatus", "TestInitStatus");

/// Counts its polls, each a done axis at step 3, and keeps the positions
/// it was set to.
struct Card {
    polls: Arc<AtomicUsize>,
    set_to: Arc<Mutex<Vec<f64>>>,
    init: Option<MotorStatus>,
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
    fn set_position(&mut self, _: &AsynUser, position: f64) -> AsynResult<()> {
        self.set_to.lock().unwrap().push(position);
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        self.polls.fetch_add(1, Ordering::Relaxed);
        *status = MotorStatus {
            position: 3.0,
            done: true,
            ..MotorStatus::default()
        };
        Ok(())
    }
}

impl Model1Motor for Card {
    fn take_init_status(&mut self) -> MotorStatus {
        self.init.take().expect("taken once")
    }
}

/// What a booted card's driver saw.
struct Booted {
    db: Arc<PvDatabase>,
    polls: usize,
    set_to: Vec<f64>,
}

/// Card `card`'s record `V:i{card}`, with `fields` beside MRES 0.5, past
/// iocInit, its driver's `motor_init` having read `init`.
async fn boot(card: i32, init: MotorStatus, fields: &str) -> Booted {
    register_model1_device(&MODEL1);
    let polls = Arc::new(AtomicUsize::new(0));
    let set_to = Arc::new(Mutex::new(Vec::new()));
    let motor = Card {
        polls: polls.clone(),
        set_to: set_to.clone(),
        init: Some(init),
    };
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
record(motor, "V:i{card}") {{ field(DTYP, "TestInitStatus") field(OUT, "#C{card} S0 @") field(MRES, "0.5") {fields} }}
"##
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let set_to = set_to.lock().unwrap().clone();
    Booted {
        db,
        polls: polls.load(Ordering::Relaxed),
        set_to,
    }
}

fn field(db: &PvDatabase, card: i32, field: &str) -> EpicsValue {
    db.get_pv(format!("V:i{card}.{field}")).unwrap()
}

/// The card at step 14.
fn at_14() -> MotorStatus {
    MotorStatus {
        position: 14.0,
        encoder_position: 14.0,
        done: true,
        ..MotorStatus::default()
    }
}

#[tokio::test]
async fn the_record_starts_from_the_status_motor_init_read() {
    let booted = boot(0, at_14(), "").await;
    assert_eq!(booted.polls, 0, "motor_task makes no startup read");
    assert_eq!(field(&booted.db, 0, "DRBV"), EpicsValue::Double(7.0));
    assert_eq!(field(&booted.db, 0, "RMP"), EpicsValue::Long(14));
}

/// A failed `set_status` read on COMM_ERR exits with CNTRL_COMM_ERR and
/// RA_PROBLEM set (drvMCDC2805.cc:244-250).
#[tokio::test]
async fn a_failed_motor_init_read_stores_its_comm_err_exit() {
    let comm_err = MotorStatus {
        problem: true,
        comms_error: true,
        ..MotorStatus::default()
    };
    let booted = boot(1, comm_err, "").await;
    assert_eq!(booted.polls, 0);
    let EpicsValue::ULong(msta) = field(&booted.db, 1, "MSTA") else {
        panic!("MSTA");
    };
    let raised = (MstaFlags::PROBLEM | MstaFlags::COMM_ERR).bits();
    assert_eq!(msta & raised, raised, "{msta:#x}");
    assert_eq!(field(&booted.db, 1, "DRBV"), EpicsValue::Double(0.0));
}

/// C set_status's failed read exits storing the status motor_init prepared
/// (drvMCDC2805.cc:589-598, :227, :368-369), so init_record's
/// process_motor_info keeps UEIP Yes on an EA_PRESENT axis
/// (motorRecord.cc:3671).
#[tokio::test]
async fn a_failed_motor_init_read_keeps_what_motor_init_prepared() {
    let prepared = MotorStatus {
        has_encoder: true,
        gain_support: true,
        ..MotorStatus::default()
    };
    let booted = boot(3, prepared, r#"field(UEIP, "Yes")"#).await;
    let EpicsValue::ULong(msta) = field(&booted.db, 3, "MSTA") else {
        panic!("MSTA");
    };
    let prepared = (MstaFlags::ENCODER_PRESENT | MstaFlags::GAIN_SUPPORT).bits();
    assert_eq!(
        (msta & prepared, field(&booted.db, 3, "UEIP")),
        (prepared, EpicsValue::Short(1)),
        "{msta:#x}"
    );
}

/// An autosaved DVAL 10 under RSTM NearZero is restored only onto a
/// controller near zero (devMotorAsyn.c init_controller): the card at step
/// 14 keeps its position, which only the init read tells the record.
#[tokio::test]
async fn an_autosaved_dval_does_not_overwrite_a_card_away_from_zero() {
    let booted = boot(2, at_14(), r#"field(RSTM, "NearZero") field(DVAL, "10")"#).await;
    assert_eq!(booted.set_to, Vec::<f64>::new(), "no LOAD_POS");
    assert_eq!(field(&booted.db, 2, "DRBV"), EpicsValue::Double(7.0));
    assert_eq!(field(&booted.db, 2, "DVAL"), EpicsValue::Double(7.0));
}
