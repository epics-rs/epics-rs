//! `SMOO` on a `DTYP="Soft Channel"` `ai` — C applies it in the dset, not in
//! `convert()`.
//!
//! `devAiSoft.c:81-93` (and `devAiSoftCallback.c:180-194`) blend the reading
//! into the previous `VAL` inside `read_ai`, then `return 2` so `convert()` is
//! skipped. `aiRecord.c:440-444`, the copy the record body runs, is therefore
//! the RAW dset's — `devAiSoftRaw::read_ai` returns 0. With the filter written
//! only in the record body, `SMOO` had no effect at any value on the default
//! `ai` DTYP.
//!
//! Boundaries, one case each: the first read (no history), a subsequent read
//! (history), `SMOO = 0`, `SMOO = 1`, a non-finite previous `VAL`, a failed
//! read dropping the history, and the RAW dset still smoothing in `convert()`.

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "SRC") {
    field(VAL, "100")
}
record(ai, "SOFT") {
    field(DTYP, "Soft Channel")
    field(INP, "SRC")
    field(SMOO, "0.9")
}
record(ai, "RAW") {
    field(DTYP, "Raw Soft Channel")
    field(INP, "SRC")
    field(SMOO, "0.9")
}
"#;

async fn build() -> std::sync::Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &epics_base_rs::server::database::PvDatabase, name: &str) {
    let mut visited = std::collections::HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

/// The lead's trigger: `SRC=100`, `SOFT` with `SMOO=0.9`. The first cycle takes
/// the reading whole (C `dpvt` is still NULL); the second blends
/// `0 * 0.1 + 100 * 0.9`.
#[epics_macros_rs::epics_test]
async fn a_soft_channel_ai_smooths_the_reading_against_its_previous_val() {
    let db = build().await;

    process(&db, "SOFT").await;
    assert_eq!(
        db.get_pv("SOFT").unwrap().to_f64(),
        Some(100.0),
        "C `devAiSoft.c:84` `prec->dpvt` is NULL on the first read, so the \
         three-part guard fails and VAL takes the reading whole"
    );

    db.put_pv("SRC", EpicsValue::Double(0.0)).await.unwrap();
    process(&db, "SOFT").await;
    assert_eq!(
        db.get_pv("SOFT").unwrap().to_f64(),
        Some(90.0),
        "C `devAiSoft.c:85` `vt.val * (1 - smoo) + prec->val * smoo`"
    );
}

/// The RAW dset keeps `convert()`'s copy of the filter: `read_ai` returns 0
/// there, so `aiRecord.c:440-444` runs and produces the same two values. The
/// dset-side filter must not double up on this path.
#[epics_macros_rs::epics_test]
async fn a_raw_soft_channel_ai_still_smooths_through_convert() {
    let db = build().await;

    process(&db, "RAW").await;
    assert_eq!(db.get_pv("RAW").unwrap().to_f64(), Some(100.0));

    db.put_pv("SRC", EpicsValue::Double(0.0)).await.unwrap();
    process(&db, "RAW").await;
    assert_eq!(
        db.get_pv("RAW").unwrap().to_f64(),
        Some(90.0),
        "smoothed once, by the record body — not once per path"
    );
}

/// `SMOO = 0` is C's first guard term: the reading lands whole, forever.
#[test]
fn smoo_zero_takes_every_reading_whole() {
    let mut ai = create_record("ai").unwrap();
    ai.soft_input_read(Some(EpicsValue::Double(100.0))).unwrap();
    ai.soft_input_read(Some(EpicsValue::Double(0.0))).unwrap();

    assert_eq!(ai.get_field("VAL").unwrap().to_f64(), Some(0.0));
}

/// `SMOO = 1` is the other end: `new * 0 + old * 1` — the reading is weighed
/// out entirely and `VAL` never moves again.
#[test]
fn smoo_one_holds_the_first_reading() {
    let mut ai = create_record("ai").unwrap();
    ai.put_field("SMOO", EpicsValue::Double(1.0)).unwrap();

    ai.soft_input_read(Some(EpicsValue::Double(100.0))).unwrap();
    assert_eq!(ai.get_field("VAL").unwrap().to_f64(), Some(100.0));

    ai.soft_input_read(Some(EpicsValue::Double(0.0))).unwrap();
    assert_eq!(ai.get_field("VAL").unwrap().to_f64(), Some(100.0));
}

/// C's third guard term is `finite(prec->val)` — the value being blended INTO,
/// not the reading. A `VAL` parked at NaN is replaced, not blended, however
/// much history the dset has.
#[test]
fn a_non_finite_previous_val_is_replaced_not_blended() {
    let mut ai = create_record("ai").unwrap();
    ai.put_field("SMOO", EpicsValue::Double(0.9)).unwrap();

    ai.soft_input_read(Some(EpicsValue::Double(100.0))).unwrap();
    ai.put_field("VAL", EpicsValue::Double(f64::NAN)).unwrap();
    ai.soft_input_read(Some(EpicsValue::Double(10.0))).unwrap();

    assert_eq!(
        ai.get_field("VAL").unwrap().to_f64(),
        Some(10.0),
        "C `devAiSoft.c:84` `finite(prec->val)` fails, so the else arm runs"
    );
}

/// C `devAiSoft.c:92` `prec->dpvt = NULL` on a failed read: the next good
/// reading has nothing to blend against and lands whole.
#[test]
fn a_failed_read_drops_the_history_so_the_next_one_is_unsmoothed() {
    let mut ai = create_record("ai").unwrap();
    ai.put_field("SMOO", EpicsValue::Double(0.9)).unwrap();

    ai.soft_input_read(Some(EpicsValue::Double(100.0))).unwrap();
    ai.soft_input_read(None).unwrap();
    ai.soft_input_read(Some(EpicsValue::Double(0.0))).unwrap();

    assert_eq!(
        ai.get_field("VAL").unwrap().to_f64(),
        Some(0.0),
        "with the history kept this would be 90 — the failed read is exactly \
         what re-arms the filter's initial condition"
    );
}

/// The failure is delivered by the framework, not only by a direct call: an
/// `INP` naming a record that is not in the database is C's `status != 0`.
#[epics_macros_rs::epics_test]
async fn a_broken_inp_link_drops_the_history_through_the_process_cycle() {
    const BROKEN: &str = r#"
record(ai, "SRC") {
    field(VAL, "100")
}
record(ai, "BAD") {
    field(DTYP, "Soft Channel")
    field(INP, "NOSUCHRECORD")
    field(SMOO, "0.9")
}
"#;
    let db = IocBuilder::new()
        .db_string(BROKEN, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    // Prime the history the way a good read would, then let the broken link
    // fail a cycle, then hand it another reading.
    {
        let rec = db.get_record("BAD").unwrap();
        let mut guard = rec.write();
        guard
            .record
            .soft_input_read(Some(EpicsValue::Double(100.0)))
            .unwrap();
    }
    process(&db, "BAD").await;
    {
        let rec = db.get_record("BAD").unwrap();
        let mut guard = rec.write();
        guard
            .record
            .soft_input_read(Some(EpicsValue::Double(0.0)))
            .unwrap();
    }

    assert_eq!(
        db.get_pv("BAD").unwrap().to_f64(),
        Some(0.0),
        "the failed cycle must have cleared the dset's history"
    );
}
