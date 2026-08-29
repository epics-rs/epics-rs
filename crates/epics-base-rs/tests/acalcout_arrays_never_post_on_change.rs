//! W10-A6 — an aCalcout array posts from AMASK/NEWM and from nothing else.
//!
//! C `monitor()` (`aCalcoutRecord.c:1024-1029`) change-detects the SCALAR inputs
//! against their previous copies:
//!
//! ```c
//! for (i=0, pnew=&pcalc->a, pprev=&pcalc->pa; i<MAX_FIELDS; i++, pnew++, pprev++) {
//!     if ((*pnew != *pprev) || (monitor_mask&DBE_ALARM)) {
//!         db_post_events(pcalc,pnew,monitor_mask|DBE_VALUE|DBE_LOG);
//!         *pprev = *pnew;
//!     }
//! }
//!
//! for (i=0, panew=&pcalc->aa; i<ARRAY_MAX_FIELDS; i++, panew++) {
//!     if (*panew && (pcalc->newm & (1<<i))) {
//!         db_post_events(pcalc, *panew, monitor_mask|DBE_VALUE|DBE_LOG);
//!     }
//! }
//! ```
//!
//! Note what the array loop does NOT do: it holds no `pprev`, it compares no
//! value. There is no PAA..PLL — the record keeps no previous copy of an array,
//! so it cannot post one "because it changed". The only array comparison in the
//! whole record lives in `fetch_values` (`:1099-1101`), against the link's own
//! previous delivery, and its result IS the NEWM bit.
//!
//! The port ran AA..LL through the generic change-detection loop, which posts
//! any field whose value differs from what the subscriber last saw. A client
//! `caput` to AA moves the field without advancing `last_posted`, so the next
//! process — storing nothing into AA, fetching nothing into it — emitted a post
//! C never makes.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// CC is written by nobody: no expression stores into it (AMASK bit clear) and
/// no INCC link feeds it (NEWM bit clear). A caput moves it; the next process
/// must stay silent on CC.
#[epics_macros_rs::epics_test]
async fn w10_a6_a_caput_array_is_not_reposted_by_the_next_process() {
    let db = PvDatabase::new();
    let mut a = AcalcoutRecord::new();
    a.put_field("NELM", EpicsValue::ULong(4)).unwrap();
    // Stores into AA only — CC is untouched by the expression.
    a.put_field("CALC", EpicsValue::String("AA:=BB*2;SUM(AA)".into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("BB", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0]))
        .unwrap();
    db.add_record("A6", Box::new(a)).await.unwrap();

    let inst = db.get_record("A6").unwrap();
    let mut cc_rx = inst
        .write()
        .add_subscriber(
            "CC",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .expect("a CC subscription must be accepted");

    // Settle: drain whatever the first process posts.
    process(&db, "A6").await;
    while cc_rx.try_recv().is_ok() {}

    // A client caput to CC. This posts the put's value — the subscriber has
    // already heard it.
    db.put_pv("A6.CC", EpicsValue::DoubleArray(vec![7.0, 7.0, 7.0, 7.0]))
        .await
        .unwrap();
    while cc_rx.try_recv().is_ok() {}

    // The next process stores nothing into CC and fetches nothing into it, so
    // AMASK bit 2 and NEWM bit 2 are both clear.
    process(&db, "A6").await;

    assert_eq!(
        inst.read().record.get_field("AMASK").unwrap(),
        EpicsValue::ULong(1),
        "the expression stored into AA (bit 0) and nothing else"
    );
    assert!(
        cc_rx.try_recv().is_err(),
        "C's array loop is gated on NEWM alone — it holds no previous copy of CC \
         and cannot post it for having changed"
    );
}
