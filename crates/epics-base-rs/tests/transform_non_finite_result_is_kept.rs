//! R16-4 — a non-finite calc result is STORED in the channel and alarmed
//! beside it; it is not discarded.
//!
//! C `sCalcPerform` has two different `-1`s:
//!
//! ```c
//! /* the epilogue, sCalcPerform.c:2034-2056 — runs to completion first */
//! if (presult) *presult = ps->d;
//! ...
//! return(((isnan(*presult)||isinf(*presult)) ? -1 : 0));
//! ```
//!
//! so an expression whose operators all succeeded but whose RESULT is `inf` /
//! `NaN` writes the cell and THEN reports -1. The other `-1` — an operator
//! refusing outright (`1/0`, `SQRT(-1)`) — returns from inside the loop, before
//! the epilogue, and never writes.
//!
//! `transformRecord.c:593-597` reads only the status:
//!
//! ```c
//! if (sCalcPerform(&ptran->a, 16, NULL, 0, pval, NULL, 0, prpcbuf, ptran->prec)) {
//!     recGblSetSevr(ptran, CALC_ALARM, INVALID_ALARM);
//!     ptran->udf = TRUE;
//! }
//! ```
//!
//! — it never rolls the value back, so on the first `-1` the channel KEEPS the
//! `inf` and the record fans it out through `OUTx`. (scalcout differs: it
//! overwrites VAL with its own -1 / "***ERROR***" sentinel, so the written
//! value is invisible there. transform is where C's contract shows.)
//!
//! C harness (`sCalcPostfix.c` + `sCalcPerform.c` built standalone):
//! `1e308*10` → `st=-1 d=inf`; `ACOS(2)` → `st=-1 d=nan`; `1/0` → `st=-1 d=0`
//! (nothing written).

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// A transform in COPT=Always, so every channel's CLCx is evaluated whether or
/// not the channel was just written (C `transformRecord.c:575`).
async fn transform_db() -> PvDatabase {
    let db = PvDatabase::new();
    let mut t = TransformRecord::default();
    t.copt = 1; // Always
    db.add_record("T", Box::new(t)).await.unwrap();
    db
}

async fn put(db: &PvDatabase, field: &str, v: EpicsValue) {
    db.put_record_field_from_ca("T", field, v).await.unwrap();
}

async fn channel(db: &PvDatabase, field: &str) -> f64 {
    let rec = db.get_record("T").unwrap();
    let g = rec.read();
    match g.record.get_field(field).unwrap() {
        EpicsValue::Double(d) => d,
        other => panic!("{field}: expected a double, got {other:?}"),
    }
}

/// The boundary: overflow to `+inf`. C writes it into the channel and returns
/// -1 — the record alarms AND keeps the infinity.
#[epics_macros_rs::epics_test]
async fn an_overflowing_result_is_stored_in_the_channel_and_alarms() {
    let db = transform_db().await;
    put(&db, "CLCA", EpicsValue::String("1e308*10".into())).await;
    process(&db, "T").await;

    assert_eq!(
        channel(&db, "A").await,
        f64::INFINITY,
        "C `*presult` is written BEFORE the -1 (sCalcPerform.c:2034-2056), and \
         transformRecord.c:593-597 never rolls it back"
    );
    let rec = db.get_record("T").unwrap();
    let g = rec.read();
    assert_eq!(
        g.common.sevr,
        AlarmSeverity::Invalid,
        "the -1 still raises CALC_ALARM/INVALID (transformRecord.c:594)"
    );
    assert_eq!(g.common.stat, alarm_status::CALC_ALARM);
    assert!(g.common.udf != 0, "transformRecord.c:595 sets udf = TRUE");
}

/// The same rule with a NaN — C's tail tests `isnan(*presult)` too, and the
/// written cell is the NaN.
#[epics_macros_rs::epics_test]
async fn a_nan_result_is_stored_in_the_channel_and_alarms() {
    let db = transform_db().await;
    put(&db, "CLCB", EpicsValue::String("ACOS(2)".into())).await;
    process(&db, "T").await;

    assert!(
        channel(&db, "B").await.is_nan(),
        "C: ACOS(2) → st=-1 d=nan; the nan IS written"
    );
    let rec = db.get_record("T").unwrap();
    let g = rec.read();
    assert_eq!(g.common.sevr, AlarmSeverity::Invalid);
    assert!(g.common.udf != 0);
}

/// The value C wrote is a value like any other: the OUTx loop fans it out.
/// This is what made the discard observable off-record — a downstream ao that
/// C drives to `inf` saw nothing at all.
#[epics_macros_rs::epics_test]
async fn the_non_finite_value_is_fanned_out_through_outa() {
    let db = transform_db().await;
    db.add_record("T_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    put(&db, "CLCA", EpicsValue::String("1e308*10".into())).await;
    put(&db, "OUTA", EpicsValue::String("T_TGT.VAL PP".into())).await;
    process(&db, "T").await;

    match db.get_pv("T_TGT").unwrap() {
        EpicsValue::Double(d) => assert_eq!(
            d,
            f64::INFINITY,
            "transform writes every non-constant OUTx each process — including \
             the channel the failing perform just filled with inf"
        ),
        other => panic!("expected a double, got {other:?}"),
    }
}

/// The DISTINCT half, and the one the port already had right: an operator that
/// REFUSES returns -1 from inside the loop, so C never reaches the epilogue and
/// `*pval` keeps its old value. `1/0` (`sCalcPerform.c:1022-1030`) must NOT
/// write the channel — the alarm is raised all the same.
#[epics_macros_rs::epics_test]
async fn an_operator_refusal_leaves_the_channel_untouched() {
    let db = transform_db().await;
    // Seed the channel so "untouched" is distinguishable from "written 0".
    put(&db, "C", EpicsValue::Double(7.0)).await;
    put(&db, "CLCC", EpicsValue::String("1/0".into())).await;
    process(&db, "T").await;

    assert_eq!(
        channel(&db, "C").await,
        7.0,
        "C returns -1 BEFORE the epilogue for a zero divisor — *pval is never \
         assigned, so the channel keeps its previous value"
    );
    let rec = db.get_record("T").unwrap();
    let g = rec.read();
    assert_eq!(
        g.common.sevr,
        AlarmSeverity::Invalid,
        "both of C's -1s raise the same CALC_ALARM/INVALID"
    );
    assert!(g.common.udf != 0);
}

/// Control — a finite result is status 0: no alarm, no UDF, value stored.
#[epics_macros_rs::epics_test]
async fn a_finite_result_stores_the_value_and_does_not_alarm() {
    let db = transform_db().await;
    put(&db, "CLCA", EpicsValue::String("1e308/10".into())).await;
    process(&db, "T").await;

    assert_eq!(channel(&db, "A").await, 1e307);
    let rec = db.get_record("T").unwrap();
    let g = rec.read();
    assert_eq!(g.common.sevr, AlarmSeverity::NoAlarm);
    assert!(g.common.udf == 0);
}
