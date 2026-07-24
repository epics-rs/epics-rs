//! W10-A5 — an aCalcout array is posted once per C `db_post_events` call, with
//! that call site's own mask.
//!
//! There are TWO call sites and they do not agree on the mask.
//!
//! `afterCalc` (`aCalcoutRecord.c:293-297`) posts the AMASK-flagged arrays — the
//! ones the expression stored into — with a LITERAL mask; `monitor_mask` does not
//! even exist in that function:
//!
//! ```c
//! for (j=0, panew=&pcalc->aa; j<ARRAY_MAX_FIELDS; j++, panew++) {
//!     if (*panew && (pcalc->amask & (1<<j))) {
//!         db_post_events(pcalc, *panew, DBE_VALUE|DBE_LOG);
//!     }
//! }
//! ```
//!
//! `monitor()` (`:1031-1036`) posts the NEWM-flagged arrays — the ones an input
//! link changed — with the alarm bits folded in:
//!
//! ```c
//! for (i=0, panew=&pcalc->aa; i<ARRAY_MAX_FIELDS; i++, panew++) {
//!     if (*panew && (pcalc->newm & (1<<i))) {
//!         db_post_events(pcalc, *panew, monitor_mask|DBE_VALUE|DBE_LOG);
//!     }
//! }
//! ```
//!
//! So on an alarm-transition cycle the two masks differ (DBE_ALARM is in one and
//! not the other), and an array in BOTH masks gets BOTH events. The port
//! collapsed the two marks into one post carrying `monitor_mask|DBE_VALUE|
//! DBE_LOG`: one event instead of two, and the AMASK event carrying an alarm bit
//! C never puts on it.

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

/// `WF` -> `A.INAA` -> AA, with `calc` deciding whether the expression also
/// STORES into AA. HIGH=1/HSV=MINOR makes the first process an alarm transition,
/// which is the only cycle on which the two C masks differ.
async fn acalcout_with(db: &PvDatabase, calc: &str) {
    let mut wf = WaveformRecord::new(4, DbFieldType::Double);
    wf.put_field("VAL", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0]))
        .unwrap();
    db.add_record("WF", Box::new(wf)).await.unwrap();

    let mut a = AcalcoutRecord::new();
    a.put_field("NELM", EpicsValue::ULong(4)).unwrap();
    a.put_field("CALC", EpicsValue::String(calc.into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("INAA", EpicsValue::String("WF".into()))
        .unwrap();
    // VAL = SUM(AA) = 10 > HIGH, so the cycle raises MINOR: monitor_mask gains
    // DBE_ALARM (recGblResetAlarms' `if (stat != prev_stat || sevr != prev_sevr)`).
    a.put_field("HIGH", EpicsValue::Double(1.0)).unwrap();
    a.put_field("HSV", EpicsValue::Short(1)).unwrap();
    db.add_record("A", Box::new(a)).await.unwrap();
}

async fn subscribe_aa(db: &PvDatabase) -> epics_base_rs::server::event_queue::EventReader {
    let inst = db.get_record("A").unwrap();
    let mut g = inst.write();
    g.add_subscriber(
        "AA",
        1,
        DbFieldType::Double,
        (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
    )
    .expect("an AA subscription must be accepted")
}

/// AA is in BOTH masks: the link delivered a changed array (NEWM bit 0) and the
/// expression stored into it (AMASK bit 0). C posts it twice — `afterCalc` with
/// a literal DBE_VALUE|DBE_LOG, then `monitor()` with the alarm bit folded in.
#[epics_macros_rs::epics_test]
async fn w10_a5_an_array_in_both_masks_is_posted_twice_with_the_two_c_masks() {
    let db = PvDatabase::new();
    // `AA := AA + 0` stores AA (AMASK bit 0) without altering the fetched value.
    acalcout_with(&db, "AA:=AA+0;SUM(AA)").await;
    let mut aa_rx = subscribe_aa(&db).await;

    process(&db, "A").await;

    let first = aa_rx
        .try_recv()
        .expect("afterCalc posts the AMASK-flagged AA (:293-297)");
    assert_eq!(
        first.mask,
        EventMask::VALUE | EventMask::LOG,
        "afterCalc's post is a LITERAL DBE_VALUE|DBE_LOG — monitor_mask is not in \
         scope there, so the alarm bit of this transition cycle is NOT on it"
    );
    assert_eq!(
        first.snapshot.value,
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0])
    );

    let second = aa_rx
        .try_recv()
        .expect("monitor() posts the NEWM-flagged AA as well (:1031-1036)");
    assert_eq!(
        second.mask,
        EventMask::ALARM | EventMask::VALUE | EventMask::LOG,
        "monitor()'s post is monitor_mask|DBE_VALUE|DBE_LOG, and this cycle's \
         NoAlarm -> Minor transition puts DBE_ALARM in monitor_mask"
    );

    assert!(
        aa_rx.try_recv().is_err(),
        "two C call sites fired, so exactly two events"
    );
}

/// The AMASK half alone: the expression stores into CC, no link feeds it. Even on
/// an alarm-transition cycle the post carries no alarm bit.
#[epics_macros_rs::epics_test]
async fn w10_a5_an_amask_only_array_posts_a_literal_value_log() {
    let db = PvDatabase::new();
    acalcout_with(&db, "CC:=AA+0;SUM(AA)").await;

    let inst = db.get_record("A").unwrap();
    let mut cc_rx = inst
        .write()
        .add_subscriber(
            "CC",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
        )
        .expect("a CC subscription must be accepted");

    process(&db, "A").await;

    let post = cc_rx.try_recv().expect("CC was stored into: AMASK bit 2");
    assert_eq!(
        post.mask,
        EventMask::VALUE | EventMask::LOG,
        "afterCalc posts DBE_VALUE|DBE_LOG literally, alarm transition or not"
    );
    assert!(
        cc_rx.try_recv().is_err(),
        "no link feeds CC, so NEWM bit 2 is clear — one call site, one event"
    );
}
