//! R11-C14 — transform evaluates with sCalc, and its `monitor()` posts no VAL.
//!
//! Two halves of one finding, both rooted in what `transformRecord.c` actually
//! calls.
//!
//! **The engine.** `:593` is `sCalcPerform(&ptran->a, 16, NULL, 0, pval, NULL,
//! 0, prpcbuf, ptran->prec)` — the sCalc engine (its CLCx buffers are sized
//! `SCALC_INFIX_TO_POSTFIX_SIZE`, `:208`), NOT base's `calcPerform`. The two
//! differ in the rule that decides this record's alarm: `sCalcPerform` ends
//!
//! ```c
//! return (((isnan(*presult)||isinf(*presult)) ? -1 : 0));   /* sCalcPerform.c:2056 */
//! ```
//!
//! so a non-finite result is a FAILURE, and `:593-596` turns it into
//! `recGblSetSevr(ptran, CALC_ALARM, INVALID_ALARM)` + `udf = TRUE`. Base's
//! `calcPerform` has no such check: it returns 0 with the infinity in hand. The
//! port evaluated CLCx with the numeric engine, so `CLCx = "1/0"` produced
//! `inf` and NO_ALARM.
//!
//! **The VAL post.** `monitor()` (`:786-808`) is transform's only
//! `db_post_events` caller and it walks A..P — it posts no VAL, ever. VAL is an
//! inert dummy (`:422`, *"Gotta have a .val field"*). The framework's deadband
//! post fires on any class, and the alarm bits alone are a class, so a
//! transform that went INVALID was firing a `.VAL` monitor C never sends.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::{EventMask, alarm_status};
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn transform_db() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("T", Box::new(TransformRecord::default()))
        .await
        .unwrap();
    db
}

/// `CLCA = "1/0"` — base's engine divides in bare IEEE and hands back `+inf`
/// with status 0; sCalc's `DIV` refuses the zero divisor and returns -1
/// (`sCalcPerform.c:1022-1030`), which is what makes this a calc FAILURE.
///
/// That -1 comes from inside the evaluator loop, BEFORE the epilogue, so C
/// never assigns `*pval` — the channel keeps its previous value. (The other -1,
/// the non-finite tail at `:2056`, writes the cell first; R16-4 and
/// `transform_non_finite_result_is_kept.rs` cover that half.)
#[epics_macros_rs::epics_test]
async fn r11_c14_divide_by_zero_is_a_calc_failure() {
    let db = transform_db().await;

    db.put_record_field_from_ca("T", "CLCA", EpicsValue::String("1/0".into()))
        .await
        .unwrap();
    process(&db, "T").await;

    let rec = db.get_record("T").unwrap();
    let g = rec.read();
    assert_eq!(
        g.common.sevr,
        AlarmSeverity::Invalid,
        "sCalc's DIV returns -1 on a zero divisor (sCalcPerform.c:1022-1030) → \
         recGblSetSevr(CALC_ALARM, INVALID_ALARM) (transformRecord.c:594)"
    );
    assert_eq!(
        g.common.stat,
        alarm_status::CALC_ALARM,
        "the status is CALC_ALARM"
    );
    assert!(
        g.common.udf != 0,
        "transformRecord.c:595 also sets udf = TRUE on the failure"
    );
    // C leaves `*pval` untouched when sCalcPerform fails — the channel keeps its
    // previous value rather than taking the infinity.
    assert_eq!(
        g.record.get_field("A").unwrap(),
        EpicsValue::Double(0.0),
        "a failed calc does not write the channel (C never assigns *pval on the \
         failure path)"
    );
}

/// The control: a finite result is not a failure. Same engine, no alarm.
#[epics_macros_rs::epics_test]
async fn r11_c14_a_finite_result_raises_no_alarm() {
    let db = transform_db().await;

    db.put_record_field_from_ca("T", "CLCA", EpicsValue::String("1/2".into()))
        .await
        .unwrap();
    process(&db, "T").await;

    let rec = db.get_record("T").unwrap();
    let g = rec.read();
    assert_eq!(g.common.sevr, AlarmSeverity::NoAlarm);
    assert_eq!(g.record.get_field("A").unwrap(), EpicsValue::Double(0.5));
}

/// Second half: no process cycle of a transform posts `.VAL` — not the first
/// one (which the deadband's never-posted sentinel would otherwise fire), and
/// not the alarm cycle (whose alarm bits alone would otherwise fire it).
#[epics_macros_rs::epics_test]
async fn r11_c14_no_process_cycle_posts_val() {
    let db = transform_db().await;
    let rec = db.get_record("T").unwrap();

    let full = (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits();
    let (mut val_rx, mut a_rx) = {
        let mut g = rec.write();
        let v = g
            .add_subscriber("VAL", 1, DbFieldType::Double, full)
            .expect("a VAL subscription must be accepted");
        let a = g
            .add_subscriber("A", 2, DbFieldType::Double, full)
            .expect("an A subscription must be accepted");
        (v, a)
    };

    // A clean cycle that MOVES a channel: A must post, VAL must not.
    db.put_record_field_from_ca("T", "CLCA", EpicsValue::String("3+4".into()))
        .await
        .unwrap();
    process(&db, "T").await;
    a_rx.try_recv()
        .expect("A moved 0 -> 7, so C's monitor() posts it");
    assert!(
        val_rx.try_recv().is_err(),
        "transform monitor() posts A..P only — never VAL (transformRecord.c:786-808)"
    );

    // The alarm cycle: the calc fails, the record goes INVALID. C still posts no
    // VAL — `monitor()` does not name it, whatever the alarm did.
    db.put_record_field_from_ca("T", "CLCA", EpicsValue::String("1/0".into()))
        .await
        .unwrap();
    process(&db, "T").await;
    {
        let g = rec.read();
        assert_eq!(
            g.common.sevr,
            AlarmSeverity::Invalid,
            "the cycle really did go into alarm — the alarm bits are live"
        );
    }
    assert!(
        val_rx.try_recv().is_err(),
        "an alarm cycle posts no VAL either: the deadband post must honour the \
         record's closed post set"
    );
}

/// VAL is still a plain writable dummy: a client put stores it and posts it
/// (C `dbPut`, dbAccess.c:1414). The closed set gates PROCESS posts, not puts.
#[epics_macros_rs::epics_test]
async fn r11_c14_a_put_to_the_val_dummy_still_posts() {
    let db = transform_db().await;
    let rec = db.get_record("T").unwrap();

    let mut val_rx = {
        let mut g = rec.write();
        g.add_subscriber("VAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
            .expect("a VAL subscription must be accepted")
    };

    db.put_record_field_from_ca("T", "VAL", EpicsValue::Double(42.0))
        .await
        .unwrap();

    let e = val_rx.try_recv().expect("dbPut posts the field it wrote");
    assert_eq!(e.snapshot.value, EpicsValue::Double(42.0));
    assert!(
        val_rx.try_recv().is_err(),
        "and posts it exactly once (R11-C10)"
    );
}
