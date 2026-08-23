//! An analog-alarm limit is stored and compared in the record's own numeric
//! domain, not in `f64`.
//!
//! C declares HIHI/HIGH/LOW/LOLO, HYST and LALM with the record's VAL type and
//! compares them against VAL with no conversion anywhere
//! (`int64inRecord.c:262-264`):
//!
//! ```c
//!     epicsInt64 val, hyst, lalm;
//!     epicsInt64 alev;
//!     ...
//!     if ((asev = prec->hhsv) &&
//!         (val >= (alev = prec->hihi) ||
//!          ((lalm == alev) && (val >= alev - hyst))))
//! ```
//!
//! `int64in`/`int64out` declare them `DBF_INT64`
//! (`int64inRecord.dbd.pod:152-243`), `longin`/`longout` `DBF_LONG`, everything
//! else `DBF_DOUBLE`. The port decided the stored type from the field's NAME,
//! so every limit became an `f64` and an `epicsInt64` threshold above 2^53 was
//! rounded before it reached storage — one count of slack at exactly the
//! nanosecond-timestamp magnitudes `int64` records exist for.
//!
//! Boundaries: the first `i64` that is not an `f64` (2^53 + 1) on the storage
//! side and on the comparison side; the limit still reachable from above; the
//! hysteresis band at that magnitude, which needs LALM exact too; the
//! `DBF_LONG` half of the family, whose string put must parse through
//! `epicsParseInt32`; and the `DBF_DOUBLE` records, whose fractional limits
//! must be untouched.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

/// 2^53 — the largest `i64` whose successor an `f64` cannot represent.
const TWO_53: i64 = 9_007_199_254_740_992;

const DB: &str = r#"
record(int64in, "I64:HI")   { field(HIHI, "9007199254740993") field(HHSV, "MAJOR") }
record(int64in, "I64:BAND") { field(HIHI, "9007199254740993") field(HHSV, "MAJOR")
                              field(HYST, "2") }
record(longin,  "L:HI")     { field(HIHI, "10") field(HHSV, "MAJOR") }
record(ai,      "A:HI")     { field(HIHI, "0.5") field(HHSV, "MAJOR") }
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

/// `caput <rec> <val>` then one process cycle — the put clears UDF the way C
/// `dbPut` does, and `checkAlarms` runs inside the cycle that follows.
async fn drive(db: &PvDatabase, rec: &str, val: EpicsValue) -> AlarmSeverity {
    db.put_pv(rec, val).await.unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
    db.get_record(rec).unwrap().read().common.sevr
}

/// Storage boundary: the `.db` string is the first `i64` an `f64` cannot hold.
#[epics_macros_rs::epics_test]
async fn an_int64_limit_survives_the_load_exactly() {
    let db = build().await;
    assert_eq!(
        db.get_pv("I64:HI.HIHI").unwrap(),
        EpicsValue::Int64(TWO_53 + 1),
    );
}

/// Comparison boundary: VAL one count BELOW an `epicsInt64` HIHI does not
/// alarm. Through an `f64` both collapse onto 2^53 and `val >= hihi` fires.
#[epics_macros_rs::epics_test]
async fn a_value_one_count_below_an_int64_hihi_does_not_alarm() {
    let db = build().await;
    assert_eq!(
        drive(&db, "I64:HI", EpicsValue::Int64(TWO_53)).await,
        AlarmSeverity::NoAlarm,
    );
}

/// The other side of the same boundary: the limit is still reachable.
#[epics_macros_rs::epics_test]
async fn a_value_at_an_int64_hihi_alarms() {
    let db = build().await;
    assert_eq!(
        drive(&db, "I64:HI", EpicsValue::Int64(TWO_53 + 1)).await,
        AlarmSeverity::Major,
    );
}

/// Hysteresis at that magnitude. `alev - hyst` is 2^53 - 1, so a retreat to
/// 2^53 is inside the band and one to 2^53 - 2 is outside — which only
/// separates once LALM has remembered the exact `epicsInt64` threshold.
#[epics_macros_rs::epics_test]
async fn the_hysteresis_band_separates_at_int64_resolution() {
    let db = build().await;
    assert_eq!(
        drive(&db, "I64:BAND", EpicsValue::Int64(TWO_53 + 1)).await,
        AlarmSeverity::Major,
    );
    assert_eq!(
        db.get_pv("I64:BAND.LALM").unwrap(),
        EpicsValue::Int64(TWO_53 + 1),
        "LALM holds the threshold, in the record's own DBF",
    );
    assert_eq!(
        drive(&db, "I64:BAND", EpicsValue::Int64(TWO_53)).await,
        AlarmSeverity::Major,
        "inside the band: val >= alev - hyst",
    );
    assert_eq!(
        drive(&db, "I64:BAND", EpicsValue::Int64(TWO_53 - 2)).await,
        AlarmSeverity::NoAlarm,
        "outside the band: val < alev - hyst",
    );
}

/// The `DBF_LONG` half of the family: longin/longout's limits are
/// `epicsInt32`, so they are stored and served as `DBF_LONG` — not widened to
/// the `epicsInt64` of the int64 records, and not flattened to the
/// `DBF_DOUBLE` the name table gave every record.
#[epics_macros_rs::epics_test]
async fn a_long_records_limit_keeps_its_own_width() {
    let db = build().await;
    assert_eq!(db.get_pv("L:HI.HIHI").unwrap(), EpicsValue::Long(10));
    assert_eq!(
        drive(&db, "L:HI", EpicsValue::Long(10)).await,
        AlarmSeverity::Major,
    );
}

/// The `DBF_DOUBLE` records keep fractional limits, on both sides of one.
#[epics_macros_rs::epics_test]
async fn a_double_records_fractional_limit_is_untouched() {
    let db = build().await;
    assert_eq!(db.get_pv("A:HI.HIHI").unwrap(), EpicsValue::Double(0.5));
    assert_eq!(
        drive(&db, "A:HI", EpicsValue::Double(0.4)).await,
        AlarmSeverity::NoAlarm,
    );
    assert_eq!(
        drive(&db, "A:HI", EpicsValue::Double(0.5)).await,
        AlarmSeverity::Major,
    );
}
