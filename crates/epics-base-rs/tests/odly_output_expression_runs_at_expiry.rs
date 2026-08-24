//! The DOPT output expression is evaluated when the ODLY delay EXPIRES, not
//! when it is scheduled.
//!
//! C `calcoutRecord.c::process` returns at `:282` with the DOPT switch unrun —
//! that switch is the first half of `execOutput` (`:613-627`), which `process`
//! reaches only through the immediate arm at `:283` or the delayed continuation
//! at `:296`. So `calcPerform(&prec->a, &prec->oval, prec->orpc)` runs against
//! the A..U present at EXPIRY. The twin is `sCalcoutRecord.c:429` ->
//! `execOutput:755-777`.
//!
//! `aCalcout` is the asymmetry: its C really does evaluate OVAL early, in
//! `call_aCalcPerform` (`aCalcoutRecord.c:1287-1291`), and its `execOutput`
//! (`:895-935`) carries no DOPT switch at all — so a mid-window input change
//! must NOT reach its output. The last case pins that difference so the three
//! records are not "harmonised" into one shape.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// `CALC="A"`, `OCAL="A*10"`, DOPT=Use OCAL, OOPT=Every Time, ODLY long enough
/// that only the explicit continuation can end it, `OUT` to an ai.
async fn calcout_with_ocal(db: &PvDatabase, name: &str, target: &str) {
    db.add_record(target, Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let mut c = CalcoutRecord::default();
    c.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    c.put_field("OCAL", EpicsValue::String("A*10".into()))
        .unwrap();
    c.special("CALC", true).unwrap();
    c.special("OCAL", true).unwrap();
    c.dopt = 1;
    c.oopt = 0;
    c.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    db.add_record(name, Box::new(c)).await.unwrap();
    wire_out(db, name, target);
}

/// `OUT` is a common field, so it is set on the instance after `add_record`.
fn wire_out(db: &PvDatabase, name: &str, target: &str) {
    let rec = db.get_record(name).expect("just added");
    let mut inst = rec.write();
    inst.put_common_field("OUT", EpicsValue::String(target.into()))
        .unwrap();
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

async fn expire(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_continuation(name, &mut visited, 0)
        .await
        .unwrap();
}

/// The lead's trigger: A moves during the window, and the OCAL pass at expiry
/// must see the new A. C drives 70; scheduling-time evaluation drives 10.
#[epics_macros_rs::epics_test]
async fn calcout_ocal_reads_the_inputs_present_at_expiry() {
    let db = PvDatabase::new();
    calcout_with_ocal(&db, "C", "T").await;

    db.put_pv_no_process("C.A", EpicsValue::Double(1.0))
        .await
        .unwrap();
    process(&db, "C").await;
    assert_eq!(
        db.get_pv("T").unwrap().to_f64(),
        Some(0.0),
        "the delaying cycle writes no output"
    );

    // The mid-window write. NPP, as in C's trigger — it must not process C.
    db.put_pv_no_process("C.A", EpicsValue::Double(7.0))
        .await
        .unwrap();
    expire(&db, "C").await;

    assert_eq!(
        db.get_pv("T").unwrap().to_f64(),
        Some(70.0),
        "OCAL runs at expiry against A=7 (C calcoutRecord.c:296 -> execOutput:621)"
    );
}

/// OVAL itself must not move while the delay is pending: C's `execOutput` has
/// not run yet, so a client reading OVAL inside the window sees the previous
/// output value.
#[epics_macros_rs::epics_test]
async fn calcout_oval_stands_still_inside_the_window() {
    let db = PvDatabase::new();
    calcout_with_ocal(&db, "C", "T").await;

    db.put_pv_no_process("C.A", EpicsValue::Double(1.0))
        .await
        .unwrap();
    process(&db, "C").await;
    assert_eq!(
        db.get_record("C").unwrap().read().record.get_field("OVAL"),
        Some(EpicsValue::Double(0.0)),
        "OVAL holds its pre-delay value until execOutput runs"
    );

    expire(&db, "C").await;
    assert_eq!(
        db.get_record("C").unwrap().read().record.get_field("OVAL"),
        Some(EpicsValue::Double(10.0)),
        "and takes the OCAL result at expiry"
    );
}

/// The DOPT=Use_VAL arm is the same switch and the same timing: C copies
/// `oval = val` inside `execOutput` (`:617-618`), so it too happens at expiry.
/// VAL is frozen for the window (the continuation runs no CALC pass), which is
/// what makes this arm look identical either way — assert the output still
/// lands.
#[epics_macros_rs::epics_test]
async fn calcout_use_val_still_writes_at_expiry() {
    let db = PvDatabase::new();
    db.add_record("TV", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let mut c = CalcoutRecord::default();
    c.put_field("CALC", EpicsValue::String("A+5".into()))
        .unwrap();
    c.special("CALC", true).unwrap();
    c.oopt = 0;
    c.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    db.add_record("CV", Box::new(c)).await.unwrap();
    wire_out(&db, "CV", "TV");

    db.put_pv_no_process("CV.A", EpicsValue::Double(2.0))
        .await
        .unwrap();
    process(&db, "CV").await;
    assert_eq!(db.get_pv("TV").unwrap().to_f64(), Some(0.0));

    expire(&db, "CV").await;
    assert_eq!(
        db.get_pv("TV").unwrap().to_f64(),
        Some(7.0),
        "DOPT=Use_VAL copies VAL to OVAL in execOutput, at expiry"
    );
}

/// scalcout's own trigger, needing no second record: an OCAL that STORES
/// (`A:=A+1`) must not touch A while the delay is pending. C runs
/// `sCalcPerform` from `execOutput` (`sCalcoutRecord.c:768`), which the
/// scheduling cycle returns before reaching (`:407`).
#[epics_macros_rs::epics_test]
async fn scalcout_ocal_stores_land_at_expiry() {
    let db = PvDatabase::new();
    let mut sc = ScalcoutRecord::default();
    sc.put_field("CALC", EpicsValue::String("A".into()))
        .unwrap();
    sc.put_field("OCAL", EpicsValue::String("A:=A+1;A".into()))
        .unwrap();
    sc.special("CALC", true).unwrap();
    sc.special("OCAL", true).unwrap();
    sc.dopt = 1;
    sc.oopt = 0;
    sc.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    db.add_record("SC", Box::new(sc)).await.unwrap();

    db.put_pv_no_process("SC.A", EpicsValue::Double(1.0))
        .await
        .unwrap();
    process(&db, "SC").await;
    assert_eq!(
        db.get_record("SC").unwrap().read().record.get_field("A"),
        Some(EpicsValue::Double(1.0)),
        "the OCAL store must not run on the scheduling cycle"
    );

    expire(&db, "SC").await;
    assert_eq!(
        db.get_record("SC").unwrap().read().record.get_field("A"),
        Some(EpicsValue::Double(2.0)),
        "it runs once, at expiry (sCalcoutRecord.c:429 -> execOutput:768)"
    );
    assert_eq!(
        db.get_record("SC").unwrap().read().record.get_field("OVAL"),
        Some(EpicsValue::Double(2.0)),
        "and OVAL is that pass's result"
    );
}

/// The asymmetry, pinned: aCalcout's OVAL is computed with VAL in
/// `call_aCalcPerform` (`aCalcoutRecord.c:1287-1291`), long before `afterCalc`
/// decides to defer, and its `execOutput` (`:895-935`) has no DOPT switch — so
/// OVAL is already final while the delay runs, and an input that moves inside
/// the window does NOT reach it. Making this record late would be a
/// regression, not a harmonisation.
#[epics_macros_rs::epics_test]
async fn acalcout_ocal_stays_evaluated_at_scheduling_time() {
    let db = PvDatabase::new();
    let mut ac = AcalcoutRecord::default();
    ac.put_field("CALC", EpicsValue::String("A".into()))
        .unwrap();
    ac.put_field("OCAL", EpicsValue::String("A*10".into()))
        .unwrap();
    ac.special("CALC", true).unwrap();
    ac.special("OCAL", true).unwrap();
    ac.put_field("DOPT", EpicsValue::Short(1)).unwrap();
    ac.put_field("OOPT", EpicsValue::Short(0)).unwrap();
    ac.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    db.add_record("AC", Box::new(ac)).await.unwrap();

    db.put_pv_no_process("AC.A", EpicsValue::Double(1.0))
        .await
        .unwrap();
    process(&db, "AC").await;
    assert_eq!(
        db.get_record("AC").unwrap().read().record.get_field("OVAL"),
        Some(EpicsValue::Double(10.0)),
        "aCalcout has OVAL before the delay even starts"
    );

    db.put_pv_no_process("AC.A", EpicsValue::Double(7.0))
        .await
        .unwrap();
    expire(&db, "AC").await;
    assert_eq!(
        db.get_record("AC").unwrap().read().record.get_field("OVAL"),
        Some(EpicsValue::Double(10.0)),
        "and the mid-window change does not reach it — no DOPT switch in its execOutput"
    );
}
