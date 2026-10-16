//! orient reads motPut_Auto and motGet_Auto as C's monitored variables,
//! written as each monitor arrives (orient_st.st:126, :136, no efflag).

use epics_base_rs::runtime::task::{self, BlockingBridge};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;
use optics_rs::snl::orient::{self, OrientConfig};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const ORIENT_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/orient.db");
const MOTORS: &str = r#"
record(motor, "X:m:tth") { field(HLM, "180") field(LLM, "-180") }
record(motor, "X:m:th") { field(HLM, "180") field(LLM, "-180") }
record(motor, "X:m:chi") { field(HLM, "180") field(LLM, "-180") }
record(motor, "X:m:phi") { field(HLM, "180") field(LLM, "-180") }
"#;
const P: &str = "X:orient1:";

async fn f64_of(ioc: &PvDatabase, pv: &str) -> f64 {
    ioc.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn string_of(ioc: &PvDatabase, pv: &str) -> String {
    match ioc.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

async fn boot() -> (Arc<PvDatabase>, PvDatabase) {
    let macros: HashMap<String, String> = [("P", "X:"), ("O", "1"), ("PREC", "4")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let ioc = IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
        .db_string(MOTORS, &macros)
        .unwrap()
        .db_file(ORIENT_DB, &macros)
        .unwrap()
        .build_served()
        .await
        .unwrap()
        .0;
    let db = PvDatabase::clone(&ioc);
    BlockingBridge::capture().spawn(orient::run(
        OrientConfig::new(P, "X:m:", "tth", "th", "chi", "phi"),
        db.clone(),
    ));
    for _ in 0..200 {
        if string_of(&ioc, &format!("{P}Msg")).await == "Orient initialized" {
            break;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    (ioc, db)
}

/// Post each value as a monitor, with no await between the posts: on the
/// one-thread runtime the sequencer finds them all queued.
fn post(db: &epics_base_rs::server::database::PvDatabase, posts: &[(String, EpicsValue)]) {
    for (pv, v) in posts {
        let rec = db.get_record(pv).unwrap();
        let mut inst = rec.write();
        inst.record.put_field("VAL", v.clone()).unwrap();
        inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
    }
}

/// A CA put to `{P}field`, processing the record as a client's would.
async fn put(db: &epics_base_rs::server::database::PvDatabase, field: &str, v: f64) {
    db.put_record_field_from_ca(format!("{P}{field}"), "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

async fn wait_for(ioc: &PvDatabase, pv: &str, want: f64) {
    for _ in 0..100 {
        if f64_of(ioc, pv).await == want {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("{pv} = {}, want {want}", f64_of(ioc, pv).await);
}

/// The silicon (400)/(040) pair of the unit tests, posted as monitors.
fn reflections() -> Vec<(String, EpicsValue)> {
    [
        ("H1", 4.0),
        ("TTH1", 24.3414),
        ("TH1", 12.1707),
        ("K2", 4.0),
        ("TTH2", 24.3414),
        ("TH2", 12.1707),
        ("PHI2", 90.0),
    ]
    .into_iter()
    .map(|(f, v)| (format!("{P}{f}"), EpicsValue::Double(v)))
    .collect()
}

/// A motor post queued behind motGet_Auto=1 already sees auto mode and
/// copies the motor into the trial angle, though the motor is taken first.
#[epics_base_rs::epics_test]
async fn a_motor_post_queued_after_mot_get_auto_copies_the_angle() {
    let (ioc, db) = boot().await;
    post(
        &db,
        &[
            (format!("{P}motGet_Auto"), EpicsValue::Enum(1)),
            ("X:m:tth".to_string(), EpicsValue::Double(12.5)),
        ],
    );
    for _ in 0..100 {
        if f64_of(&ioc, &format!("{P}TTH")).await == 12.5 {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("TTH {} not copied", f64_of(&ioc, &format!("{P}TTH")).await);
}

async fn wait_for_msg(ioc: &PvDatabase, want: &str) {
    for _ in 0..100 {
        if string_of(ioc, &format!("{P}Msg")).await == want {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "Msg = {:?}, want {want:?}",
        string_of(ioc, &format!("{P}Msg")).await
    );
}

/// OMTX_Calc wakes the sequencer, the calculation reads the reflections
/// as monitored variables, and the command resets to 0
/// (orient_st.st:169, :222-240, :626-654).
#[epics_base_rs::epics_test]
async fn omtx_calc_takes_the_posted_reflections() {
    let (ioc, db) = boot().await;
    post(&db, &reflections());
    put(&db, "energy", 21.657).await;
    wait_for_msg(&ioc, "Recalculated A0 and OMTX for new energy").await;
    post(&db, &[(format!("{P}OMTX_Calc"), EpicsValue::Enum(1))]);
    wait_for(&ioc, &format!("{P}OMTX_Calc"), 0.0).await;
    // C kohzuIoc oracle: 0.999992 from this pair, 1 (identity) without it.
    let omtx_11 = f64_of(&ioc, &format!("{P}OMTX_11")).await;
    assert!((omtx_11 - 0.999992).abs() < 1e-6, "OMTX_11 = {omtx_11}");
    assert_eq!(
        string_of(&ioc, &format!("{P}Msg")).await,
        "Recalculated A0 and OMTX for new energy"
    );
}

/// A bad OMTX calculation writes motPut_Auto to manual
/// (orient_st.st:649-653, :957-959).
#[epics_base_rs::epics_test]
async fn a_bad_omtx_calc_sets_mot_put_auto_to_manual() {
    let (ioc, db) = boot().await;
    post(&db, &[(format!("{P}motPut_Auto"), EpicsValue::Enum(1))]);
    put(&db, "energy", 21.657).await;
    wait_for_msg(&ioc, "Recalculated A0 and OMTX for new energy").await;
    post(&db, &[(format!("{P}OMTX_Calc"), EpicsValue::Enum(1))]);
    // A stringout holds 39 characters.
    wait_for_msg(&ioc, "Bad OMTX calc; motPut_Auto set to Manua").await;
    wait_for(&ioc, &format!("{P}motPut_Auto"), 0.0).await;
}

async fn wait_near(ioc: &PvDatabase, pv: &str, want: f64) {
    for _ in 0..100 {
        if (f64_of(ioc, pv).await - want).abs() < 1e-3 {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("{pv} = {}, want {want}", f64_of(ioc, pv).await);
}

/// orient wakes on K_event, which posts on every process where K (MDEL 0)
/// posts only a change, so rewriting the standing K drives a hand-moved
/// motor back (orient_st.st:184, :736-740; C kohzuIoc oracle: tth 10 ->
/// 34.6923).
#[epics_base_rs::epics_test]
async fn rewriting_the_standing_k_drives_the_motors_again() {
    let (ioc, db) = boot().await;
    post(&db, &reflections());
    put(&db, "energy", 21.657).await;
    wait_for_msg(&ioc, "Recalculated A0 and OMTX for new energy").await;
    post(&db, &[(format!("{P}motPut_Auto"), EpicsValue::Enum(1))]);
    put(&db, "H", 4.0).await;
    put(&db, "K", 4.0).await;
    wait_near(&ioc, "X:m:tth", 34.6923).await;

    post(&db, &[("X:m:tth".to_string(), EpicsValue::Double(10.0))]);
    task::sleep(Duration::from_millis(300)).await;
    assert_eq!(f64_of(&ioc, "X:m:tth").await, 10.0);

    put(&db, "K", 4.0).await;
    wait_near(&ioc, "X:m:tth", 34.6923).await;
}

/// The harness motors have no driver: finish each started move as one
/// would, DMOV back to 1 and posted.
fn finish_moves(db: &epics_base_rs::server::database::PvDatabase) {
    for name in ["X:m:tth", "X:m:th", "X:m:chi", "X:m:phi"] {
        let rec = db.get_record(name).unwrap();
        let mut inst = rec.write();
        let motor = inst
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
            .unwrap();
        if !motor.stat.dmov {
            motor.stat.dmov = true;
            inst.notify_field("DMOV", EventMask::VALUE | EventMask::LOG);
        }
    }
}

/// orient's own motor drive is not taken for an outside move, and the
/// move's completion clears Busy and waiting4Mot, so motGet_Auto copies a
/// later outside move (orient_st.st:801-875, :1053).
#[epics_base_rs::epics_test]
async fn a_finished_drive_clears_busy_and_lets_mot_get_auto_copy_a_move() {
    let (ioc, db) = boot().await;
    post(&db, &reflections());
    put(&db, "energy", 21.657).await;
    wait_for_msg(&ioc, "Recalculated A0 and OMTX for new energy").await;
    post(
        &db,
        &[
            (format!("{P}motPut_Auto"), EpicsValue::Enum(1)),
            (format!("{P}motGet_Auto"), EpicsValue::Enum(1)),
        ],
    );
    put(&db, "H", 4.0).await;
    put(&db, "K", 4.0).await;
    wait_near(&ioc, "X:m:tth", 34.6923).await;
    // Busy holds until the drive completes.
    task::sleep(Duration::from_millis(300)).await;
    assert_eq!(f64_of(&ioc, &format!("{P}Busy")).await, 1.0);
    finish_moves(&db);
    wait_for(&ioc, &format!("{P}waiting4Mot"), 0.0).await;
    wait_for(&ioc, &format!("{P}Busy"), 0.0).await;
    wait_near(&ioc, &format!("{P}K"), 4.0).await;

    post(&db, &[("X:m:tth".to_string(), EpicsValue::Double(10.0))]);
    wait_near(&ioc, &format!("{P}TTH"), 10.0).await;
}

/// A K write sets K_busy through K_event; the drive's completion clears
/// it, and a rewrite that moves no motor clears it in putAll
/// (orient_st.st:863-875, :1132-1141).
#[epics_base_rs::epics_test]
async fn put_all_and_the_drive_completion_clear_the_busy_records() {
    let (ioc, db) = boot().await;
    post(&db, &reflections());
    put(&db, "energy", 21.657).await;
    wait_for_msg(&ioc, "Recalculated A0 and OMTX for new energy").await;
    post(&db, &[(format!("{P}motPut_Auto"), EpicsValue::Enum(1))]);
    put(&db, "H", 4.0).await;
    put(&db, "K", 4.0).await;
    wait_near(&ioc, "X:m:tth", 34.6923).await;
    assert_eq!(f64_of(&ioc, &format!("{P}K_busy")).await, 1.0);
    finish_moves(&db);
    wait_for(&ioc, &format!("{P}Busy"), 0.0).await;
    wait_for(&ioc, &format!("{P}K_busy"), 0.0).await;
    wait_for(&ioc, &format!("{P}H_busy"), 0.0).await;

    put(&db, "K", 4.0).await;
    task::sleep(Duration::from_millis(300)).await;
    assert_eq!(f64_of(&ioc, &format!("{P}waiting4Mot")).await, 0.0);
    wait_for(&ioc, &format!("{P}K_busy"), 0.0).await;
    wait_for(&ioc, &format!("{P}Busy"), 0.0).await;
}
