//! C motor_task has no startup pass: a model-1 axis's first status is the
//! one its driver's `motor_init` read with `set_status` (drvSoloist.cc:734,
//! drvMVP2001.cc:720), and the record's init reads it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interfaces::motor::{Model1Motor, MotorStatus, PidGainKind, PollFailure};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::DevSup;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::device_support::BuildTrans;
use motor_rs::devices::{
    device_support_factory, devices, model1_device, register_axis, register_model1_device,
};
use motor_rs::flags::{MotorCommand, MstaFlags};
use motor_rs::trace::TraceUser;

static MODEL1: DevSup = model1_device("devTestInitStatus", "TestInitStatus");

/// Counts its polls, each reading `reads` or failing on None, and keeps
/// the positions it was set to and the init calls it saw.
struct Card {
    polls: Arc<AtomicUsize>,
    set_to: Arc<Mutex<Vec<f64>>>,
    calls: Arc<Mutex<Vec<String>>>,
    init: Option<MotorStatus>,
    reads: Option<MotorStatus>,
}

impl Card {
    /// A card whose driver's `motor_init` read `init` and whose polls read
    /// a done axis at step 3.
    fn new(init: MotorStatus) -> Self {
        Self {
            polls: Arc::default(),
            set_to: Arc::default(),
            calls: Arc::default(),
            init: Some(init),
            reads: Some(MotorStatus {
                position: 3.0,
                done: true,
                ..MotorStatus::default()
            }),
        }
    }
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
    fn move_absolute(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn move_relative(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
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
    fn set_position(&mut self, _: &AsynUser, position: f64) -> AsynResult<()> {
        self.set_to.lock().unwrap().push(position);
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn primitive(&mut self, _: &AsynUser, _: &[u8]) -> AsynResult<()> {
        self.calls.lock().unwrap().push("init".into());
        Ok(())
    }
    fn set_encoder_ratio(&mut self, _: &AsynUser, ratio: f64) -> AsynResult<()> {
        self.calls.lock().unwrap().push(format!("ratio {ratio}"));
        Ok(())
    }
    fn set_pid_gain(&mut self, _: &AsynUser, kind: PidGainKind, gain: f64) -> AsynResult<()> {
        let kind = match kind {
            PidGainKind::Proportional => "P",
            PidGainKind::Integral => "I",
            PidGainKind::Derivative => "D",
        };
        self.calls.lock().unwrap().push(format!("{kind} {gain}"));
        Ok(())
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        self.polls.fetch_add(1, Ordering::Relaxed);
        self.calls.lock().unwrap().push("poll".into());
        *status = self.reads.clone().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "read failed".into(),
        })?;
        Ok(())
    }
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
    boot_card(card, Card::new(init), fields, true, None).await
}

/// [`boot`] on `motor`, its motor task started if `polled`, its device's
/// `build_trans` `device`.
async fn boot_card(
    card: i32,
    motor: Card,
    fields: &str,
    polled: bool,
    device: Option<Arc<dyn BuildTrans>>,
) -> Booted {
    register_model1_device(&MODEL1);
    let (polls, set_to) = (motor.polls.clone(), motor.set_to.clone());
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
    let axis = match device {
        Some(device) => axis.with_build_trans(device),
        None => axis,
    };
    register_axis(axis).unwrap();
    let _poller = polled.then(|| tokio::spawn(poll_loop.start()));
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

/// The errlog lines `boot` printed and how long it took.
async fn logged(
    boot: impl std::future::Future<Output = Booted>,
) -> (Booted, Vec<String>, Duration) {
    use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush, errlog_remove_listener};
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    let id = errlog_add_listener(move |m| sink.lock().unwrap().push(m.trim_end().to_owned()));
    let start = Instant::now();
    let booted = boot.await;
    let took = start.elapsed();
    errlog_flush();
    errlog_remove_listener(id);
    let lines = lines.lock().unwrap().clone();
    (booted, lines, took)
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
/// (drvMCDC2805.cc:589-598, :227, :368-369), at motor_init and again at
/// init_record_com's GET_INFO, so init_record's process_motor_info keeps
/// UEIP Yes on an EA_PRESENT axis (motorRecord.cc:3671).
#[tokio::test]
async fn a_failed_motor_init_read_keeps_what_motor_init_prepared() {
    let prepared = MotorStatus {
        has_encoder: true,
        gain_support: true,
        ..MotorStatus::default()
    };
    let card = Card {
        reads: None,
        ..Card::new(prepared.clone())
    };
    let booted = boot_card(3, card, r#"field(UEIP, "Yes")"#, true, None).await;
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

/// The card at step 14 with PID support, which its driver reads without
/// RA_DONE.
fn gains_at_14() -> MotorStatus {
    MotorStatus {
        position: 14.0,
        gain_support: true,
        ..MotorStatus::default()
    }
}

/// GAIN_SUPPORT starts init_record_com's round (motordevCom.cc:313-381):
/// its GET_INFO reads the card again, and motor_task's INFO answer, done
/// as no move is held (motordrvCom.cc:470-481), is the status init_record
/// takes.
#[tokio::test]
async fn a_gain_support_card_is_read_again_by_the_init_round() {
    let card = Card {
        reads: Some(MotorStatus {
            position: 3.0,
            ..gains_at_14()
        }),
        ..Card::new(gains_at_14())
    };
    let booted = boot_card(4, card, "", true, None).await;
    assert_eq!(booted.polls, 1, "one GET_INFO");
    assert_eq!(
        field(&booted.db, 4, "MSTA"),
        EpicsValue::ULong((MstaFlags::GAIN_SUPPORT | MstaFlags::DONE).bits())
    );
    assert_eq!(field(&booted.db, 4, "DRBV"), EpicsValue::Double(1.5));
    assert_eq!(field(&booted.db, 4, "DVAL"), EpicsValue::Double(1.5));
}

/// `motor_init_callback` signals `initSem` (motordevCom.cc:563-585), so an
/// answered round goes on at once, short of MAX_TIMEOUT and its dev_NoInit.
#[tokio::test]
async fn an_answered_init_round_does_not_wait_out_the_timeout() {
    let (booted, lines, took) = logged(boot(10, gains_at_14(), "")).await;
    assert_eq!(booted.polls, 1, "one GET_INFO");
    assert!(took < Duration::from_secs(2), "{took:?}");
    assert_eq!(lines, Vec::<String>::new());
}

/// With no answer in MAX_TIMEOUT init_record_com reports S_dev_NoInit,
/// M_devSup 19 (devSup.h), and the record starts from the card's own
/// readback (motordevCom.cc:379-393).
#[tokio::test]
async fn an_unanswered_init_round_starts_from_the_init_read() {
    let (booted, lines, _) = logged(boot_card(5, Card::new(gains_at_14()), "", false, None)).await;
    assert_eq!(
        lines,
        [
            "recGblRecordError: dev_NoInit (init_record_com: callback2 timeout Error (514,19) PV: V:i5"
        ]
    );
    assert_eq!(booted.polls, 0);
    assert_eq!(
        field(&booted.db, 5, "MSTA"),
        EpicsValue::ULong(MstaFlags::GAIN_SUPPORT.bits())
    );
    assert_eq!(field(&booted.db, 5, "DRBV"), EpicsValue::Double(7.0));
}

/// Only an encoder in use gets the ratio (motordevCom.cc:271-286), and a
/// positive gain its SET_xGAIN (:349-371), each ahead of the GET_INFO.
#[tokio::test]
async fn the_init_round_programs_only_what_the_card_has() {
    let card = Card::new(MotorStatus {
        has_encoder: true,
        ..gains_at_14()
    });
    let calls = card.calls.clone();
    boot_card(
        6,
        card,
        r#"field(PCOF, "0.5") field(DCOF, "0.25")"#,
        true,
        None,
    )
    .await;
    assert_eq!(
        *calls.lock().unwrap(),
        ["P 0.5", "D 0.25", "poll"],
        "no ratio with UEIP No, no I gain at ICOF 0"
    );
}

/// Each init command is its own transaction (motordevCom.cc:325-336), so a
/// device deferring INIT (devEMC18011.cc:233-240) ends that one with its
/// own INFO before the encoder ratio goes out, ahead of the round's
/// GET_INFO.
#[tokio::test]
async fn a_deferred_init_command_ends_its_own_transaction() {
    struct DefersInit;
    impl BuildTrans for DefersInit {
        fn defers(&self, command: &MotorCommand) -> bool {
            matches!(command, MotorCommand::Primitive { .. })
        }
    }
    let card = Card::new(MotorStatus {
        has_encoder: true,
        ..MotorStatus::default()
    });
    let calls = card.calls.clone();
    let fields = r#"field(INIT, "X") field(UEIP, "Yes")"#;
    boot_card(7, card, fields, true, Some(Arc::new(DefersInit))).await;
    assert_eq!(*calls.lock().unwrap(), ["poll", "ratio 1", "poll"]);
}

/// INIT leads the round when the device builds it (motordevCom.cc:325-329).
#[tokio::test]
async fn init_leads_the_init_round() {
    let card = Card::new(MotorStatus {
        has_encoder: true,
        ..MotorStatus::default()
    });
    let calls = card.calls.clone();
    let fields = r#"field(INIT, "X") field(UEIP, "Yes")"#;
    boot_card(8, card, fields, true, None).await;
    assert_eq!(*calls.lock().unwrap(), ["init", "ratio 1", "poll"]);
}

/// An encoder in use with no ERES takes MRES's before the ratio goes out
/// (motordevCom.cc:273-280).
#[tokio::test]
async fn an_encoder_ratio_defends_a_zero_eres() {
    let card = Card::new(MotorStatus {
        has_encoder: true,
        ..MotorStatus::default()
    });
    let calls = card.calls.clone();
    let booted = boot_card(
        9,
        card,
        r#"field(UEIP, "Yes") field(ERES, "1e-12")"#,
        true,
        None,
    )
    .await;
    assert_eq!(*calls.lock().unwrap(), ["ratio 1", "poll"]);
    assert_eq!(field(&booted.db, 9, "ERES"), EpicsValue::Double(0.5));
}
