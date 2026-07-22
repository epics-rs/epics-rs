//! R9-73 — a failed input-link fetch gates the calc, in every record whose C
//! `process()` wraps the calc in `if (fetch_values(prec) == 0)`.
//!
//! * calc      — calcRecord.c:120  (fetch reads on, keeps the first bad status)
//! * calcout   — calcoutRecord.c:237 (same)
//! * sCalcout  — sCalcoutRecord.c:356 (fetch returns at the first bad link)
//! * aCalcout  — aCalcoutRecord.c:399 (same; and the gate also covers afterCalc,
//!   so the OOPT decision and the OUT write are skipped too)
//! * swait     — swaitRecord.c:408 (same; and the else arm raises READ_ALARM)
//!
//! On a gated cycle VAL freezes at the previous cycle's value. The port ran the
//! engine regardless, so VAL was recomputed from whatever stale inputs happened
//! to be in the record.
//!
//! An unresolvable link name is the failure driven here: it is what
//! `dbGetLink` reports failure for, and the framework's link read returns no
//! value for it.

// RTEMS-EXEC-MODEL-ALLOW(6): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn val(db: &PvDatabase, rec: &str) -> f64 {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field("VAL").unwrap().to_f64().unwrap()
}

async fn sevr(db: &PvDatabase, rec: &str) -> AlarmSeverity {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.common.sevr
}

/// calc: INPA resolves (A=2), INPB does not. C's fetch reads BOTH (so A still
/// refreshes) but returns INPB's failure, and `process` skips calcPerform.
/// CALC="A+1" therefore holds VAL at its previous value instead of computing 3.
#[tokio::test]
async fn r9_73_calc_freezes_val_when_an_input_link_fails() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(2.0)))
        .await
        .unwrap();

    let mut c = CalcRecord::new("A+1");
    c.put_field("INPA", EpicsValue::String("SRC".into()))
        .unwrap();
    c.put_field("VAL", EpicsValue::Double(77.0)).unwrap();
    // A second input whose link cannot be resolved: C `dbGetLink` fails on it.
    c.put_field("INPB", EpicsValue::String("NOSUCHREC".into()))
        .unwrap();
    db.add_record("C", Box::new(c)).await.unwrap();

    process(&db, "C").await;

    assert_eq!(
        val(&db, "C").await,
        77.0,
        "fetch_values != 0 — C skips calcPerform, VAL keeps the previous value"
    );

    // The inputs that DID resolve still refreshed: C's calc fetch loop does not
    // abort, it only remembers the first failing status.
    let inst = db.get_record("C").unwrap();
    let a = inst.read().record.get_field("A").unwrap();
    assert_eq!(
        a,
        EpicsValue::Double(2.0),
        "calcRecord.c:427-443 reads every link even after one fails"
    );
}

/// calcout: same gate (calcoutRecord.c:237). The OOPT switch is OUTSIDE the
/// gate in C, so OOPT=Every_Time still drives OUT — with the FROZEN VAL.
#[tokio::test]
async fn r9_73_calcout_freezes_val_and_outputs_the_frozen_value() {
    let db = PvDatabase::new();
    db.add_record("SINK", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut c = CalcoutRecord::default();
    c.put_field("CALC", EpicsValue::String("A+1".into()))
        .unwrap();
    c.special("CALC", true).unwrap();
    c.put_field("A", EpicsValue::Double(5.0)).unwrap();
    c.put_field("VAL", EpicsValue::Double(77.0)).unwrap();
    c.put_field("INPB", EpicsValue::String("NOSUCHREC".into()))
        .unwrap();
    c.put_field("OOPT", EpicsValue::Short(0)).unwrap(); // Every Time
    db.add_record("CO", Box::new(c)).await.unwrap();
    db.get_record("CO")
        .unwrap()
        .write()
        .put_common_field("OUT", EpicsValue::String("SINK".into()))
        .unwrap();

    process(&db, "CO").await;

    assert_eq!(
        val(&db, "CO").await,
        77.0,
        "fetch_values != 0 — calcPerform skipped, VAL frozen (would be 6.0)"
    );
    assert_eq!(
        val(&db, "SINK").await,
        77.0,
        "C leaves the OOPT switch outside the gate: Every_Time still writes the frozen VAL"
    );
}

/// sCalcout: fetch returns at the first failing numeric link
/// (sCalcoutRecord.c:885-887), and `process` (356) skips sCalcPerform.
#[tokio::test]
async fn r9_73_scalcout_freezes_val_when_an_input_link_fails() {
    let db = PvDatabase::new();

    let mut c = ScalcoutRecord::new();
    c.put_field("CALC", EpicsValue::String("A+1".into()))
        .unwrap();
    c.special("CALC", true).unwrap();
    c.put_field("A", EpicsValue::Double(5.0)).unwrap();
    c.put_field("VAL", EpicsValue::Double(77.0)).unwrap();
    c.put_field("INPA", EpicsValue::String("NOSUCHREC".into()))
        .unwrap();
    db.add_record("SC", Box::new(c)).await.unwrap();

    process(&db, "SC").await;

    assert_eq!(
        val(&db, "SC").await,
        77.0,
        "fetch_values != 0 — sCalcPerform skipped, VAL frozen (would be 6.0)"
    );
}

/// aCalcout: the gate covers doCalc AND afterCalc (aCalcoutRecord.c:399-414),
/// so on a failed fetch the OOPT decision never happens and OUT is not written.
#[tokio::test]
async fn r9_73_acalcout_freezes_val_and_writes_no_output() {
    let db = PvDatabase::new();
    db.add_record("SINK", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut c = AcalcoutRecord::new();
    c.put_field("CALC", EpicsValue::String("A+1".into()))
        .unwrap();
    c.special("CALC", true).unwrap();
    c.put_field("A", EpicsValue::Double(5.0)).unwrap();
    c.put_field("VAL", EpicsValue::Double(77.0)).unwrap();
    c.put_field("INPB", EpicsValue::String("NOSUCHREC".into()))
        .unwrap();
    c.put_field("OOPT", EpicsValue::Short(0)).unwrap(); // Every Time
    db.add_record("AC", Box::new(c)).await.unwrap();
    db.get_record("AC")
        .unwrap()
        .write()
        .put_common_field("OUT", EpicsValue::String("SINK".into()))
        .unwrap();

    process(&db, "AC").await;

    assert_eq!(
        val(&db, "AC").await,
        77.0,
        "fetch_values != 0 — doCalc skipped, VAL frozen (would be 6.0)"
    );
    assert_eq!(
        val(&db, "SINK").await,
        0.0,
        "afterCalc is INSIDE aCalcout's gate, so OOPT never fires and OUT is untouched"
    );
}

/// swait: same gate, plus the `else` arm — READ_ALARM at INVALID severity
/// (swaitRecord.c:413). The calc family raises nothing on the same failure.
#[tokio::test]
async fn r9_73_swait_freezes_val_and_raises_read_alarm() {
    let db = PvDatabase::new();

    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("A+1".into()))
        .unwrap();
    w.put_field("A", EpicsValue::Double(5.0)).unwrap();
    w.put_field("VAL", EpicsValue::Double(77.0)).unwrap();
    w.put_field("INAN", EpicsValue::String("NOSUCHREC".into()))
        .unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();

    process(&db, "W").await;

    assert_eq!(
        val(&db, "W").await,
        77.0,
        "fetch_values != 0 — calcPerform skipped, VAL frozen (would be 6.0)"
    );
    assert_eq!(
        sevr(&db, "W").await,
        AlarmSeverity::Invalid,
        "swaitRecord.c:413 raises READ_ALARM at INVALID on a failed input fetch"
    );
}

/// The other side of every gate: with all links resolvable the calc runs and
/// VAL updates. Without this, a fix that simply never calculates would pass.
#[tokio::test]
async fn r9_73_calc_still_computes_when_every_link_resolves() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(2.0)))
        .await
        .unwrap();

    let mut c = CalcRecord::new("A+1");
    c.put_field("INPA", EpicsValue::String("SRC".into()))
        .unwrap();
    c.put_field("VAL", EpicsValue::Double(77.0)).unwrap();
    db.add_record("C", Box::new(c)).await.unwrap();

    process(&db, "C").await;

    assert_eq!(
        val(&db, "C").await,
        3.0,
        "fetch_values == 0 — the calc runs: A(2) + 1"
    );
    assert_eq!(
        sevr(&db, "C").await,
        AlarmSeverity::NoAlarm,
        "a successful fetch raises no alarm"
    );
}
