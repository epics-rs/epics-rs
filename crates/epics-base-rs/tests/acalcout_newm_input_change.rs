//! R11-C4 — NEWM: the arrays whose INAA..INLL LINK delivered a changed value.
//!
//! C `fetch_values` (`aCalcoutRecord.c:1088-1106`) saves the field, fetches the
//! link into it, and compares over `acalcGetNumElements` elements:
//!
//! ```c
//! for (j=0; j<numElements; j++) pcalc->paa[j] = (*pavalue)[j];
//! status = dbGetLink(plink, DBR_DOUBLE, *pavalue, 0, &nRequest);
//! for (j=0; j<numElements; j++) {
//!     if (pcalc->paa[j] != (*pavalue)[j]) {pcalc->newm |= 1<<i; break;}
//! }
//! ```
//!
//! `monitor()` (`:1031-1036`) posts exactly those arrays and clears the mask.
//! The port never computed NEWM, so a changed input array was posted only by the
//! framework's change detection — which compares against the LAST POSTED value,
//! not against the value the field held before the fetch. Those two disagree the
//! moment anything else moves the field without advancing `last_posted`, and a
//! client caput is exactly that: it posts its own value and leaves `last_posted`
//! behind. The link then re-delivering the ORIGINAL array is invisible to change
//! detection and a NEWM post in C — so in the port the caput value stayed the
//! last thing the subscriber ever heard, permanently wrong.
//!
//! NEWM is a per-cycle mask like AMASK: `fetch_values` ORs bits in, `monitor()`
//! zeroes it. It is NOT the same mask — AMASK is the arrays the EXPRESSION stored
//! into (`aCalcPerform.c:487`), NEWM the arrays the LINK changed.

// RTEMS-EXEC-MODEL-ALLOW(3): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> EpicsValue {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f).unwrap()
}

/// `WF` (4 elements) -> `A.INAA` -> AA, with a CALC that never stores into AA.
async fn wf_into_aa(db: &PvDatabase, data: Vec<f64>) {
    let mut wf = WaveformRecord::new(data.len() as i32, DbFieldType::Double);
    wf.put_field("VAL", EpicsValue::DoubleArray(data)).unwrap();
    db.add_record("WF", Box::new(wf)).await.unwrap();

    let mut a = AcalcoutRecord::new();
    a.put_field("NELM", EpicsValue::ULong(4)).unwrap();
    a.put_field("CALC", EpicsValue::String("SUM(AA)".into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("INAA", EpicsValue::String("WF".into()))
        .unwrap();
    db.add_record("A", Box::new(a)).await.unwrap();
}

/// The finding. A caput moves AA under the subscriber; the next process re-fetches
/// the link, which restores the array to what it was. C: `paa` (the pre-fetch
/// field = the caput value) differs from the fetched value, so NEWM bit 0 is set
/// and `monitor()` posts AA. Change detection alone sees the same value it last
/// posted and says nothing.
#[tokio::test]
async fn r11_c4_a_link_that_reverts_a_caput_still_posts_aa() {
    let db = PvDatabase::new();
    wf_into_aa(&db, vec![1.0, 2.0, 3.0, 4.0]).await;
    let inst = db.get_record("A").unwrap();

    let mut aa_rx = inst
        .write()
        .add_subscriber(
            "AA",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .expect("an AA subscription must be accepted");

    // Cycle 1: the link delivers [1,2,3,4]. AA posts it (it changed).
    process(&db, "A").await;
    let first = aa_rx.try_recv().expect("AA moved on the first fetch");
    assert_eq!(
        first.snapshot.value,
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0])
    );

    // A client overwrites AA. The put posts its own value and processes the
    // record (AA is process-passive), so the same cycle's `fetch_values` pulls
    // [1,2,3,4] back over the caput's [9,9,9,9]. The put's post does not advance
    // the record's last-posted bookkeeping, so change detection compares the
    // restored value against the [1,2,3,4] it posted in cycle 1 and finds nothing
    // to say. C compares against the PRE-FETCH field — the caput's [9,9,9,9] —
    // sets NEWM bit 0, and posts.
    db.put_record_field_from_ca("A", "AA", EpicsValue::DoubleArray(vec![9.0, 9.0, 9.0, 9.0]))
        .await
        .unwrap();

    assert_eq!(
        field(&db, "A", "AA").await,
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0]),
        "the link must have overwritten the caput value"
    );

    let mut seen = Vec::new();
    while let Ok(e) = aa_rx.try_recv() {
        seen.push(e.snapshot.value);
    }
    assert_eq!(
        seen.last(),
        Some(&EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0])),
        "the LAST thing the subscriber hears about AA must be what the record \
         actually holds. fetch_values set NEWM bit 0 (pre-fetch [9,9,9,9] != \
         fetched [1,2,3,4]) and monitor() posts it; without NEWM the caput's \
         [9,9,9,9] is the subscriber's final word and it is wrong. Saw: {seen:?}"
    );
}

/// NEWM is per-cycle: `monitor()` zeroes it (`:1036`). A cycle whose link delivers
/// nothing new must not re-post — this is the control that the mask is taken, not
/// accumulated.
#[tokio::test]
async fn r11_c4_an_unchanged_link_value_does_not_post_aa() {
    let db = PvDatabase::new();
    wf_into_aa(&db, vec![1.0, 2.0, 3.0, 4.0]).await;
    let inst = db.get_record("A").unwrap();

    let mut aa_rx = inst
        .write()
        .add_subscriber(
            "AA",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .expect("an AA subscription must be accepted");

    process(&db, "A").await;
    while aa_rx.try_recv().is_ok() {}

    // Same source, same value: C's compare finds no difference, NEWM stays 0.
    process(&db, "A").await;
    process(&db, "A").await;

    assert_eq!(
        field(&db, "A", "NEWM").await,
        EpicsValue::ULong(0),
        "monitor() cleared the mask, and no fetch set it again"
    );
    assert!(
        aa_rx.try_recv().is_err(),
        "the link delivered the value the field already held: no NEWM bit, no post"
    );
}

/// NEWM tracks the LINK, not the client. A caput to AA sets no NEWM bit — C only
/// ever writes `newm` in `fetch_values`. (The put posts on its own path; what must
/// not happen is the next process cycle re-posting it from a NEWM bit.)
#[tokio::test]
async fn r11_c4_a_caput_alone_sets_no_newm_bit() {
    let db = PvDatabase::new();

    // No INAA link at all — nothing can fetch into AA.
    let mut a = AcalcoutRecord::new();
    a.put_field("NELM", EpicsValue::ULong(4)).unwrap();
    a.put_field("CALC", EpicsValue::String("SUM(AA)".into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    db.add_record("A", Box::new(a)).await.unwrap();

    db.put_record_field_from_ca("A", "AA", EpicsValue::DoubleArray(vec![5.0, 5.0, 5.0, 5.0]))
        .await
        .unwrap();

    assert_eq!(
        field(&db, "A", "NEWM").await,
        EpicsValue::ULong(0),
        "a client put is not fetch_values: it sets no NEWM bit"
    );
    process(&db, "A").await;
    assert_eq!(field(&db, "A", "VAL").await.to_f64().unwrap(), 20.0);
    assert_eq!(field(&db, "A", "NEWM").await, EpicsValue::ULong(0));
}
