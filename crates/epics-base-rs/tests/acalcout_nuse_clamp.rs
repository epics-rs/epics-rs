//! R19-5 — acalcout's `NUSE > NELM` repair must POST the corrected NUSE.
//!
//! C clamps at three sites and posts at all three:
//!
//! ```c
//! /* init_record pass 0, aCalcoutRecord.c:188-190 */
//! if (pcalc->nuse > pcalc->nelm) { pcalc->nuse = pcalc->nelm;
//!     db_post_events(pcalc,&pcalc->nuse,DBE_VALUE|DBE_LOG); }
//!
//! /* process, :374-377 — "Make sure.  Autosave is capable of setting NUSE
//!    to an illegal value." */
//! if (pcalc->nuse > pcalc->nelm) { pcalc->nuse = pcalc->nelm;
//!     db_post_events(pcalc,&pcalc->nuse, DBE_VALUE|DBE_LOG); }
//!
//! /* special(NUSE), :495-499 — clamps, posts DBE_VALUE, and FAILS the put */
//! if (pcalc->nuse > pcalc->nelm) { pcalc->nuse = pcalc->nelm;
//!     db_post_events(pcalc,&pcalc->nuse,DBE_VALUE); return(-1); }
//! ```
//!
//! The port clamped silently — no post at any site — and clamped inside the
//! field STORE, which is also the `.db`/autosave load path, so a record whose
//! NUSE was listed before its NELM had NUSE measured against the default NELM
//! of 1. A monitor on NUSE therefore kept reporting the illegal value the
//! client wrote while the record calculated over a different one.
//!
//! Boundaries: NUSE < NELM (nothing happens), NUSE == NELM (the edge: not
//! illegal), NUSE > NELM through each of C's three sites.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use std::collections::HashSet;

async fn acalcout_db(nelm: u32, nuse: u32) -> PvDatabase {
    let db = PvDatabase::new();
    let mut r = AcalcoutRecord::new();
    r.put_field("NELM", EpicsValue::ULong(nelm)).unwrap();
    r.put_field("NUSE", EpicsValue::ULong(nuse)).unwrap();
    r.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    db.add_record("AC", Box::new(r)).await.unwrap();
    db
}

async fn nuse(db: &PvDatabase) -> u32 {
    match db.get_record("AC").unwrap().read().record.get_field("NUSE") {
        Some(EpicsValue::ULong(v)) => v,
        other => panic!("NUSE reads as {other:?}"),
    }
}

async fn process(db: &PvDatabase) {
    let mut visited = HashSet::new();
    db.process_record_with_links("AC", &mut visited, 0)
        .await
        .unwrap();
}

/// The finding's case — C's process clamp, the net under every path that does
/// NOT go through `special()`. C names the path in the comment above it:
/// *"Make sure.  Autosave is capable of setting NUSE to an illegal value."*
/// Here the field store is written directly, exactly as a restore does, and the
/// next cycle must repair AND post it.
#[epics_macros_rs::epics_test]
async fn the_process_time_clamp_posts_the_corrected_nuse() {
    let db = acalcout_db(4, 2).await;
    assert_eq!(nuse(&db).await, 2, "legal at init");

    let inst = db.get_record("AC").unwrap();
    let mut rx = inst
        .write()
        .add_subscriber("NUSE", 4, DbFieldType::ULong, EventMask::VALUE.bits())
        .expect("a NUSE subscription must be accepted");

    // The restore: straight into the field store, no `special()` in the path.
    inst.write()
        .record
        .put_field("NUSE", EpicsValue::ULong(99))
        .unwrap();
    while rx.try_recv().is_ok() {} // ignore whatever that store itself posted

    process(&db).await;

    assert_eq!(nuse(&db).await, 4, "process clamped NUSE to NELM");
    let event = rx.try_recv().expect("the clamp must post NUSE");
    assert_eq!(event.snapshot.value, EpicsValue::ULong(4));
}

/// `special(NUSE)` — C clamps, posts a bare `DBE_VALUE`, and returns -1, so the
/// PUT FAILS. The client is told its value was illegal instead of the record
/// silently rewriting it.
#[epics_macros_rs::epics_test]
async fn a_put_of_an_illegal_nuse_is_refused_and_the_clamped_value_is_posted() {
    let db = acalcout_db(4, 0).await;

    let inst = db.get_record("AC").unwrap();
    let mut rx = inst
        .write()
        .add_subscriber("NUSE", 4, DbFieldType::ULong, EventMask::VALUE.bits())
        .expect("a NUSE subscription must be accepted");

    let status = db
        .put_record_field_from_ca("AC", "NUSE", EpicsValue::ULong(99))
        .await;

    assert!(status.is_err(), "C's special() returns -1: the put fails");
    assert_eq!(nuse(&db).await, 4, "the clamped value stays");
    let event = rx.try_recv().expect("the refused put must still post NUSE");
    assert_eq!(event.snapshot.value, EpicsValue::ULong(4));
}

/// A LEGAL put is unaffected — the refusal is not a blanket rejection of NUSE.
#[epics_macros_rs::epics_test]
async fn a_legal_nuse_put_succeeds() {
    let db = acalcout_db(10, 0).await;

    db.put_record_field_from_ca("AC", "NUSE", EpicsValue::ULong(6))
        .await
        .expect("NUSE <= NELM is accepted");
    assert_eq!(nuse(&db).await, 6);
}

/// The edge: `NUSE == NELM` is NOT illegal — C's test is `>`, not `>=`.
#[epics_macros_rs::epics_test]
async fn nuse_equal_to_nelm_is_legal() {
    let db = acalcout_db(4, 0).await;

    db.put_record_field_from_ca("AC", "NUSE", EpicsValue::ULong(4))
        .await
        .expect("NUSE == NELM is accepted");
    assert_eq!(nuse(&db).await, 4);
    process(&db).await;
    assert_eq!(nuse(&db).await, 4, "and process does not touch it");
}

/// The load-order case: a `.db` listing NUSE before NELM. C's loader stores both
/// verbatim and `init_record` clamps once, against the FINAL nelm. Clamping in
/// the store instead measured NUSE against the default NELM of 1 and silently
/// turned 5 into 1.
#[epics_macros_rs::epics_test]
async fn nuse_listed_before_nelm_survives_the_load() {
    let db = acalcout_db(10, 5).await; // the helper puts NELM first, so do it by hand
    assert_eq!(nuse(&db).await, 5);

    let db2 = PvDatabase::new();
    let mut r = AcalcoutRecord::new();
    r.put_field("NUSE", EpicsValue::ULong(5)).unwrap(); // NUSE first, NELM still 1
    r.put_field("NELM", EpicsValue::ULong(10)).unwrap();
    db2.add_record("AC", Box::new(r)).await.unwrap();

    assert_eq!(nuse(&db2).await, 5, "clamped against the final NELM, not 1");
}
