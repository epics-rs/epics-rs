//! `put_pv` carries C `dbPut`'s whole monitor tail (`dbAccess.c:1403-1413`):
//!
//! ```c
//! isValueField = dbIsValueField(pfldDes);
//! if (isValueField) precord->udf = FALSE;
//! if (precord->mlis.count &&
//!     !(isValueField && pfldDes->process_passive))
//!     db_post_events(precord, pfieldsave, DBE_VALUE | DBE_LOG);
//! ```
//!
//! Pre-fix the port's `put_pv` (the `dbPut` analogue under `dbPutLink`, QSRV
//! Inhibit/Force, autosave restore and every internal driver put) posted no
//! field monitor at all and never cleared UDF, so camonitor on anything
//! written through it saw at most the initial snapshot. The C tail is two
//! independent predicates — *value field* gates the UDF clear, *value field
//! AND `pp(TRUE)`* gates the post suppression — so the boundary table below has
//! one case per predicate combination, not one per story:
//!
//! | field            | value field | pp(TRUE) | post | UDF clear |
//! |------------------|-------------|----------|------|-----------|
//! | calc `VAL`       | yes         | no       | yes  | yes       |
//! | ao `VAL`         | yes         | yes      | NO   | yes       |
//! | ai `HIHI`        | no          | yes      | yes  | no        |
//! | ai `EGU`         | no          | no       | yes  | no        |
//!
//! The ao row is C's own suppression, not a leftover gap: a bare C `dbPut`
//! to a `pp(TRUE)` value field posts nothing and relies on the process cycle
//! a `dbPutField` would drive (measured against softIoc in
//! `array_put_posts_nord.rs`'s header — a `caput -a` onto an unprocessed
//! waveform posts NORD and no VAL). `put_pv` deliberately does not process;
//! a caller that needs a monitor there uses `put_record_field_from_ca*` or
//! `put_pv_and_post`.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(calc, "C1")  { field(CALC, "A") }
record(ao,   "AO1") {}
record(ai,   "AI1") {}
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

fn subscribe(db: &PvDatabase, rec: &str, field: &str, dbf: DbFieldType) -> EventReader {
    let r = db.get_record(rec).unwrap();
    let mut inst = r.write();
    inst.add_subscriber(field, 1, dbf, EventMask::VALUE.bits())
        .unwrap_or_else(|| panic!("{rec}.{field} subscription accepted"))
}

fn drain(rx: &mut EventReader) -> Vec<EpicsValue> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev.snapshot.value.clone());
    }
    out
}

fn udf(db: &PvDatabase, rec: &str) -> bool {
    db.get_record(rec).unwrap().read().common.udf != 0
}

/// Row 1 — a value field that is NOT `pp(TRUE)` (calc VAL): the `dbPut` post
/// is the only one there is, and the value-field put clears UDF. This is the
/// case C's comment in `dbPut` exists for — no reprocess will ever re-post it.
#[epics_macros_rs::epics_test]
async fn a_put_to_a_non_pp_value_field_posts_and_clears_udf() {
    let db = build().await;
    assert!(
        udf(&db, "C1"),
        "precondition: an unprocessed calc is undefined"
    );
    let mut rx = subscribe(&db, "C1", "VAL", DbFieldType::Double);

    db.put_pv("C1.VAL", EpicsValue::Double(3.5)).await.unwrap();

    assert_eq!(
        drain(&mut rx),
        vec![EpicsValue::Double(3.5)],
        "calc VAL is not pp(TRUE): C `dbPut` posts DBE_VALUE|DBE_LOG immediately"
    );
    assert!(
        !udf(&db, "C1"),
        "C `dbPut:1406` clears udf on a value-field put"
    );
}

/// Row 2 — the value field IS `pp(TRUE)` (ao VAL): C suppresses the immediate
/// post (the process cycle a `dbPutField` drives is the poster), but the UDF
/// clear is unconditional on a value-field put. Both halves on one write, so
/// the two predicates cannot be merged back into one.
#[epics_macros_rs::epics_test]
async fn a_put_to_a_pp_value_field_is_suppressed_but_clears_udf() {
    let db = build().await;
    assert!(
        udf(&db, "AO1"),
        "precondition: an unwritten ao is undefined"
    );
    let mut rx = subscribe(&db, "AO1", "VAL", DbFieldType::Double);

    db.put_pv("AO1.VAL", EpicsValue::Double(6.25))
        .await
        .unwrap();

    assert_eq!(
        drain(&mut rx),
        Vec::<EpicsValue>::new(),
        "ao VAL is pp(TRUE): C `dbPut` suppresses the immediate post \
         (`!(isValueField && process_passive)`) — posting here would double \
         every processed put route"
    );
    assert!(
        !udf(&db, "AO1"),
        "the UDF clear is not tied to the post: C clears udf even when the \
         post is suppressed"
    );
}

/// Row 3 — `pp(TRUE)` but NOT the value field (ai HIHI): suppression requires
/// BOTH predicates, so this posts. A rule keyed on pp alone would go silent
/// here; a rule keyed on value-field alone would double-post row 2.
#[epics_macros_rs::epics_test]
async fn a_put_to_a_pp_non_value_field_still_posts() {
    let db = build().await;
    let mut rx = subscribe(&db, "AI1", "HIHI", DbFieldType::Double);

    db.put_pv("AI1.HIHI", EpicsValue::Double(90.0))
        .await
        .unwrap();

    assert_eq!(
        drain(&mut rx),
        vec![EpicsValue::Double(90.0)],
        "ai HIHI is pp(TRUE) but not the value field: C posts \
         (`isValueField` is false, the conjunction fails)"
    );
    assert!(
        udf(&db, "AI1"),
        "a non-value-field put must NOT clear udf (C gates the clear on \
         `dbIsValueField` alone)"
    );
}

/// Row 4 — neither predicate (ai EGU): posts, and UDF stands.
#[epics_macros_rs::epics_test]
async fn a_put_to_a_plain_field_posts_and_leaves_udf() {
    let db = build().await;
    let mut rx = subscribe(&db, "AI1", "EGU", DbFieldType::String);

    db.put_pv("AI1.EGU", EpicsValue::String("V".into()))
        .await
        .unwrap();

    assert_eq!(
        drain(&mut rx),
        vec![EpicsValue::String("V".into())],
        "an ordinary field put posts DBE_VALUE|DBE_LOG from `dbPut`'s tail"
    );
    assert!(udf(&db, "AI1"), "an EGU put does not define the record");
}
