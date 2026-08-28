//! R13-63 — transform serves LA..LP, the "Prev Value of A..P" readback cells.
//!
//! `transformRecord.dbd:505-584` declares sixteen of them:
//!
//! ```
//! field(LA,DBF_DOUBLE) {
//!     prompt("Prev Value of A")
//!     special(SPC_NOMOD)
//!     interest(3)
//! }
//! ```
//!
//! `monitor()` (`transformRecord.c:797-806`) is their only writer — it posts
//! each channel that differs from its `l*` cell and copies the posted value in:
//!
//! ```c
//! for (i = 0, pnew = &ptran->a, pprev = &ptran->la; i < MAX_FIELDS; i++, pnew++, pprev++) {
//!     if ((*pnew != *pprev) || (prpvt->firstCalcPosted == 0)) {
//!         db_post_events(ptran, pnew, monitor_mask);
//!         *pprev = *pnew;
//!     }
//! }
//! ```
//!
//! So after a cycle that ran `monitor()`, `l* == *`; the two diverge only in the
//! window between cycles — which is exactly what a `caget .LA` is for. The port
//! did not serve the fields at all: `caget TR.LA` failed.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// LA tracks A across the monitor commit, and lags it in the window C exposes:
/// a value written to A but not yet published leaves LA at the last posted one.
#[epics_macros_rs::epics_test]
async fn r13_63_la_is_the_previous_posted_value_of_a() {
    let db = PvDatabase::new();
    let mut t = TransformRecord::new();
    // CLCA = "A+1": every process moves A, so LA has something to lag behind.
    t.put_field("CLCA", EpicsValue::String("A+1".into()))
        .unwrap();
    db.add_record("TR", Box::new(t)).await.unwrap();

    // Never processed: both cells are at their DBD initial 0.
    assert_eq!(db.get_pv("TR.LA").unwrap(), EpicsValue::Double(0.0));

    // Cycle 1: A = 0+1 = 1. monitor() posts A and copies it into LA.
    process(&db, "TR").await;
    assert_eq!(db.get_pv("TR.A").unwrap(), EpicsValue::Double(1.0));
    assert_eq!(
        db.get_pv("TR.LA").unwrap(),
        EpicsValue::Double(1.0),
        "after monitor() runs, `*pprev = *pnew` — LA == A"
    );

    // Cycle 2: A = 1+1 = 2, and LA follows it in the same cycle.
    process(&db, "TR").await;
    assert_eq!(db.get_pv("TR.A").unwrap(), EpicsValue::Double(2.0));
    assert_eq!(db.get_pv("TR.LA").unwrap(), EpicsValue::Double(2.0));

    // The channels are independent cells: LP is P's, and P was never driven.
    assert_eq!(db.get_pv("TR.LP").unwrap(), EpicsValue::Double(0.0));
}

/// SPC_NOMOD: a client may not write LA (`transformRecord.dbd:507`).
#[epics_macros_rs::epics_test]
async fn r13_63_la_is_read_only() {
    let db = PvDatabase::new();
    db.add_record("TR2", Box::new(TransformRecord::new()))
        .await
        .unwrap();

    assert!(
        db.put_pv("TR2.LA", EpicsValue::Double(5.0)).await.is_err(),
        "special(SPC_NOMOD) refuses the put (S_db_noMod)"
    );
    assert_eq!(db.get_pv("TR2.LA").unwrap(), EpicsValue::Double(0.0));
}
