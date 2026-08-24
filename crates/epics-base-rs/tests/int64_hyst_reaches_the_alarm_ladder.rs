//! `HYST` reaches the alarm ladder on the record types that declare the field.
//!
//! C `int64inRecord.c:262-264` / `int64outRecord.c:303-305` read the record's
//! own `hyst` into the ladder:
//!
//! ```c
//!     val  = prec->val;
//!     hyst = prec->hyst;
//!     lalm = prec->lalm;
//!     ...
//!     if ((asev = prec->hsv) &&
//!         (val >= (alev = prec->high) ||
//!          ((lalm == alev) && (val >= alev - hyst))))
//! ```
//!
//! In the port `Record::put_field` is tried before `put_common_field`, so a
//! record that DECLARES `HYST` absorbs the client's put; the shared ladder then
//! has to read it back from the record, exactly as it already reads LALM and
//! MDEL/ADEL/MLST/ALST. Reading `common.hyst` instead left `int64in`/`int64out`
//! with a permanent 0.0 while `caget .HYST` cheerfully returned the value.
//!
//! Boundaries: retreat INSIDE the band (alarm holds) vs OUTSIDE it (alarm
//! clears), on both record types, plus the read-back that hid the split and a
//! record whose own `check_alarms` owns HYST (`sel`), which must be untouched.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(int64in,  "I64:IN")  { field(HIGH, "10") field(HSV, "MINOR") field(HYST, "2") }
record(int64out, "I64:OUT") { field(HIGH, "10") field(HSV, "MINOR") field(HYST, "2") }
record(longin,   "L:IN")    { field(HIGH, "10") field(HSV, "MINOR") field(HYST, "2") }
record(ai,       "A:IN")    { field(HIGH, "10") field(HSV, "MINOR") field(HYST, "2") }
record(dfanout,  "D:OWN")   { field(HIGH, "10") field(HSV, "MINOR") field(HYST, "2") }
record(sel,      "S:OWN")   { field(SELM, "Specified") field(SELN, "0")
                              field(HIGH, "10") field(HSV, "MINOR") field(HYST, "2") }
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// VAL in the record's own DBF — `int64in`/`int64out` are DBF_INT64,
/// `longin` DBF_LONG, `ai` DBF_DOUBLE.
fn val_of(rec: &str, v: i64) -> EpicsValue {
    match rec {
        "I64:IN" | "I64:OUT" => EpicsValue::Int64(v),
        "L:IN" => EpicsValue::Long(v as i32),
        _ => EpicsValue::Double(v as f64),
    }
}

/// `caput <rec> <val>` then one process cycle — the put clears UDF the way C
/// `dbPut` does, and `checkAlarms` runs inside the cycle that follows.
async fn drive(db: &PvDatabase, rec: &str, val: i64) -> AlarmSeverity {
    db.put_pv(rec, val_of(rec, val)).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
    db.get_record(rec).unwrap().read().common.sevr
}

/// Boundary: cross HIGH, then retreat to a value still inside `HIGH - HYST`.
/// C keeps the alarm because `lalm == alev && val >= alev - hyst`.
#[epics_macros_rs::epics_test]
async fn a_retreat_inside_the_band_holds_the_alarm() {
    for rec in ["I64:IN", "I64:OUT", "L:IN", "A:IN"] {
        let db = build().await;
        assert_eq!(
            drive(&db, rec, 12).await,
            AlarmSeverity::Minor,
            "{rec}: VAL 12 >= HIGH 10 must latch MINOR"
        );
        assert_eq!(
            drive(&db, rec, 9).await,
            AlarmSeverity::Minor,
            "{rec}: VAL 9 >= HIGH 10 - HYST 2, so the alarm must hold"
        );
    }
}

/// Boundary: retreat past the band. `val < alev - hyst` fails the second
/// clause, so the alarm clears.
#[epics_macros_rs::epics_test]
async fn a_retreat_outside_the_band_clears_the_alarm() {
    for rec in ["I64:IN", "I64:OUT", "L:IN", "A:IN"] {
        let db = build().await;
        assert_eq!(drive(&db, rec, 12).await, AlarmSeverity::Minor);
        assert_eq!(
            drive(&db, rec, 7).await,
            AlarmSeverity::NoAlarm,
            "{rec}: VAL 7 < HIGH 10 - HYST 2, so the alarm must clear"
        );
    }
}

/// The read-back that made the split silent: HYST must still answer with the
/// value that was put, whichever side of the split stores it.
#[epics_macros_rs::epics_test]
async fn hyst_reads_back_the_value_that_was_put() {
    let db = build().await;
    for rec in ["I64:IN", "I64:OUT", "L:IN", "A:IN", "S:OWN"] {
        assert_eq!(
            db.get_record(rec)
                .unwrap()
                .read()
                .record
                .get_field("HYST")
                .and_then(|v| v.to_f64())
                .or_else(|| Some(db.get_record(rec).unwrap().read().common.hyst)),
            Some(2.0),
            "{rec}: HYST read-back"
        );
    }
}

/// `sel` and `dfanout` declare HYST for their OWN `check_alarms`
/// (`selRecord.c:250-304`, `dfanoutRecord.c:227-281`) and carry no shared
/// ladder slot, so the fix must not reach them. `dfanout` proves the
/// behaviour, and the slot assertion pins the classification itself.
#[epics_macros_rs::epics_test]
async fn a_record_owning_its_own_ladder_is_untouched() {
    let db = build().await;
    assert_eq!(drive(&db, "D:OWN", 12).await, AlarmSeverity::Minor);
    assert_eq!(
        drive(&db, "D:OWN", 9).await,
        AlarmSeverity::Minor,
        "dfanout's own ladder applies its own HYST"
    );
    assert_eq!(drive(&db, "D:OWN", 7).await, AlarmSeverity::NoAlarm);

    for rec in ["S:OWN", "D:OWN"] {
        assert!(
            db.get_record(rec)
                .unwrap()
                .read()
                .common
                .analog_alarm
                .is_none(),
            "{rec} must carry no shared analog-alarm slot"
        );
    }
    for rec in ["I64:IN", "I64:OUT", "L:IN", "A:IN"] {
        assert!(
            db.get_record(rec)
                .unwrap()
                .read()
                .common
                .analog_alarm
                .is_some(),
            "{rec} must run the shared ladder"
        );
    }
}
