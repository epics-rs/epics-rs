//! C init_record clears UDF only under `if (pmr->dol.type == CONSTANT)`
//! (motorRecord.cc:677-681). An unset DOL and a literal one are CONSTANT; a
//! `{const:5}` DOL is a JSON_LINK (`dbStaticLib.c:2280-2287`), so the record
//! stays undefined and its first process raises UDF/INVALID in alarm_sub
//! (:3372-3376), as the C oracle (oracleMotorIoc on motorSim) reads.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{ControllerAxis, ControllerBuilder};
use motor_rs::devices::{device_support_factory, devices, register_axis, register_controller_port};
use motor_rs::poll_loop::PollCommand;
use motor_rs::trace::TraceUser;

/// A done axis that reads step 0 on its first poll and step 3 after, so a
/// later poll posts a changed status and processes the record.
struct Arrives {
    polls: u32,
}

impl AsynMotor for Arrives {
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
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        self.polls += 1;
        *status = MotorStatus {
            position: if self.polls > 1 { 3.0 } else { 0.0 },
            done: true,
            ..MotorStatus::default()
        };
        Ok(())
    }
}

/// Boot one motor per `(name, fields)` on axes 0.. of `port`, then poll each
/// axis once so its changed status processes the record.
async fn boot(port: &str, motors: &[(&str, &str)], check: impl AsyncFnOnce(&PvDatabase)) {
    register_controller_port(&open_owner(), port, motors.len() as i32, ()).unwrap();
    let mut controller = ControllerBuilder::new();
    let axes: Vec<ControllerAxis> = (0..motors.len())
        .map(|addr| {
            controller.axis(
                Arc::new(Mutex::new(Arrives { polls: 0 })),
                TraceUser::Asyn {
                    port: port.into(),
                    addr: addr as i32,
                },
            )
        })
        .collect();
    let hour = Duration::from_secs(3600);
    let first = controller.axes();
    let poller = controller.build(hour, hour, 0).start();
    let deadline = Instant::now() + Duration::from_secs(5);
    while first.first_polls_pending() > 0 {
        assert!(Instant::now() < deadline, "the startup pass never ended");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let polls: Vec<_> = axes.iter().map(|a| a.poll_cmd_tx.clone()).collect();
    for ControllerAxis { axis, .. } in axes {
        register_axis(axis).unwrap();
    }
    let db: String = motors
        .iter()
        .enumerate()
        .map(|(addr, (name, fields))| {
            format!(
                r#"record(motor, "{name}") {{ field(DTYP, "asynMotor") field(OUT, "@asyn({port},{addr})") field(MRES, "1") {fields} }}"#
            )
        })
        .collect();
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build_served()
        .await
        .unwrap();
    for tx in &polls {
        tx.send(PollCommand::ForcePoll).await.unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    for rec in motors.iter().map(|(name, _)| name) {
        while num(&db, &format!("{rec}.RBV")) != 3.0 {
            assert!(Instant::now() < deadline, "{rec} never processed");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    check(&db).await;
    polls[0].send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn udf(db: &PvDatabase, rec: &str) -> u8 {
    db.get_record(rec).unwrap().read().common.udf
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_constant_type_dol_clears_udf_at_init() {
    boot(
        "UDFDOL",
        &[
            ("UD:json", r#"field(DOL, "{const:5}")"#),
            (
                "UD:jsoncl",
                r#"field(DOL, "{const:5}") field(OMSL, "closed_loop")"#,
            ),
            ("UD:unset", ""),
            ("UD:lit", r#"field(DOL, "5")"#),
        ],
        async |db| {
            for rec in ["UD:json", "UD:jsoncl"] {
                assert_eq!(udf(db, rec), 1, "{rec}");
                let stat = num(db, &format!("{rec}.STAT")) as u16;
                assert_eq!(stat, alarm_status::UDF_ALARM, "{rec}");
                assert_eq!(num(db, &format!("{rec}.SEVR")), 3.0, "{rec} INVALID");
                assert_eq!(num(db, &format!("{rec}.VAL")), 0.0, "{rec}");
            }
            for rec in ["UD:unset", "UD:lit"] {
                assert_eq!(udf(db, rec), 0, "{rec}");
                assert_eq!(num(db, &format!("{rec}.SEVR")), 0.0, "{rec}");
            }
        },
    )
    .await;
}

/// C loads a CONSTANT DOL into VAL (motorRecord.cc:677-681) ahead of the
/// readback adoption that overwrites it unless closed_loop (:706-714), and
/// anchors LVAL on it (:730), so the first process moves nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_loop_constant_dol_seeds_val() {
    boot(
        "VALDOL",
        &[
            ("VD:litcl", r#"field(DOL, "5") field(OMSL, "closed_loop")"#),
            ("VD:lit", r#"field(DOL, "5")"#),
        ],
        async |db| {
            assert_eq!(num(db, "VD:litcl.VAL"), 5.0);
            assert_eq!(num(db, "VD:litcl.DVAL"), 0.0, "no move");
            assert_eq!(num(db, "VD:lit.VAL"), 0.0, "val = rbv at init");
            for rec in ["VD:litcl", "VD:lit"] {
                assert_eq!(udf(db, rec), 0, "{rec}");
                assert_eq!(num(db, &format!("{rec}.SEVR")), 0.0, "{rec}");
            }
        },
    )
    .await;
}

/// Under closed_loop C adopts no drive value at init (:706-714), restore
/// or not: init_controller's restore only sets the controller position.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_loop_restore_keeps_the_drive_values() {
    let restored =
        r#"field(OMSL, "closed_loop") field(RSTM, "Always") field(VAL, "2") field(DVAL, "7")"#;
    let with_dol = format!(r#"{restored} field(DOL, "5")"#);
    boot(
        "RSTDOL",
        &[("RD:cl", restored), ("RD:cldol", &with_dol)],
        async |db| {
            for (rec, val) in [("RD:cl", 2.0), ("RD:cldol", 5.0)] {
                assert_eq!(num(db, &format!("{rec}.VAL")), val, "{rec}");
                assert_eq!(num(db, &format!("{rec}.DVAL")), 7.0, "{rec}");
                assert_eq!(num(db, &format!("{rec}.RVAL")), 0.0, "{rec}");
                assert_eq!(num(db, &format!("{rec}.LVAL")), val, "{rec}");
                assert_eq!(num(db, &format!("{rec}.LRVL")), 0.0, "{rec}");
            }
        },
    )
    .await;
}

/// C takes the DOL read only for `dol.type == DB_LINK` (motorRecord.cc:1994).
/// A name no record here serves is a CA link once iocInit has looked it up, so
/// the record stays undefined until a VAL put moves it, as the C oracle reads;
/// a local name is a DB link, read into VAL on every pass.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_loop_dol_is_read_only_as_a_db_link() {
    boot(
        "CADOL",
        &[
            ("DL:ca", r#"field(OMSL, "closed_loop") field(DOL, "abc")"#),
            (
                "DL:db",
                r#"field(OMSL, "closed_loop") field(DOL, "DL:ca.MRES")"#,
            ),
        ],
        async |db| {
            assert_eq!(udf(db, "DL:ca"), 1);
            let stat = num(db, "DL:ca.STAT") as u16;
            assert_eq!(stat, alarm_status::UDF_ALARM);
            assert_eq!(num(db, "DL:db.VAL"), 1.0, "DOL read");
            assert_eq!(udf(db, "DL:db"), 0);
            db.put_record_field_from_ca("DL:ca", "VAL", EpicsValue::Double(1.0))
                .await
                .unwrap();
            assert_eq!(num(db, "DL:ca.VAL"), 1.0);
            assert_eq!(udf(db, "DL:ca"), 0);
            assert_eq!(num(db, "DL:ca.STAT"), 0.0);
            assert_eq!(num(db, "DL:ca.SEVR"), 0.0);
        },
    )
    .await;
}
