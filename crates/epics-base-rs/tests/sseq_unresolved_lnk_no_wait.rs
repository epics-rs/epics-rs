//! R17-3: "no put" and "no wait" are ONE decision.
//!
//! C `processCallback` switches on the `LNKn` target's DBF class
//! (sseqRecord.c:714-792) and raises `waiting` only INSIDE a class arm, in the
//! branch where `dbCaPutLinkCallback` actually succeeded:
//!
//! ```c
//! case DBF_SHORT: ... case DBF_DOUBLE:
//!     if (plinkGroup->usePutCallback && (plinkGroup->lnk.type == CA_LINK)) {
//!         status = dbCaPutLinkCallback(...);
//!         if (status) { pR->abort = 1; ... }
//!         else { plinkGroup->waiting = 1;                 /* :748-750 */
//!                db_post_events(pR, &plinkGroup->waiting, DBE_VALUE); }
//!     } else { status = dbPutLink(...); }
//!     break;
//! default:
//!     break;                                              /* :790 — nothing */
//! ```
//!
//! A `WAITn` step whose `LNKn` resolves to no class at all — a disconnected CA
//! link, `dbCaGetLinkDBFtype` → -1 — takes the `default:` arm: no put, and
//! therefore no `waiting`, no `WTGn` event.
//!
//! The port raised `WTGn` and pushed the step into `in_flight` BEFORE the
//! framework's typed put path discovered there was no buffer to put, so the
//! two halves came apart: a `WTGn = 1` event C never emits, an `in_flight`
//! entry that only an immediately-fired fake completion took back out — and, on
//! any path where that completion is not delivered, a step waiting forever on a
//! put that was never made.

use std::collections::HashSet;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn kick(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

async fn poll_short(db: &PvDatabase, pv: &str, want: i16, label: &str) {
    for _ in 0..400 {
        if let Ok(EpicsValue::Short(v)) = db.get_pv(pv)
            && v == want
        {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "{label}: {pv} did not reach Short({want}) (last {:?})",
        db.get_pv(pv)
    );
}

/// Wait for a step's TARGET to hold the value the step put there.
///
/// `BUSY == 0` is not that fact and does not imply it. A no-wait step returns
/// its write as a [`ProcessAction::WriteDbLink`] in the outcome's action list
/// (`sseq.rs:772-780`) while `finish` clears `busy` inside the same `process()`
/// call (`sseq.rs:937-940`), so `BUSY` reads 0 the instant `process()` returns
/// and the framework executes the queued write only afterwards. Polling `BUSY`
/// and then asserting on the target reads across that window.
async fn poll_double(db: &PvDatabase, pv: &str, want: f64, label: &str) {
    for _ in 0..400 {
        if let Ok(EpicsValue::Double(v)) = db.get_pv(pv)
            && (v - want).abs() < 1e-10
        {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "{label}: {pv} did not reach Double({want}) (last {:?})",
        db.get_pv(pv)
    );
}

/// Step 1 is a `WAITn` step into a CA link that resolves to nothing (no link
/// set is registered, so the target has no DBF class — C's disconnected
/// `CA_LINK`). C's `default:` arm makes no put and raises no `waiting`, so no
/// `WTG1` event is ever emitted and the sequence runs straight through step 2.
#[epics_macros_rs::epics_test]
async fn r17_3_a_wait_step_with_an_unresolvable_lnk_never_raises_wtg() {
    let db = PvDatabase::new();
    db.add_record("SS_NR_TGT2", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap(); // All steps
    // Step 1: WAIT into a CA link nothing can resolve.
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("ca://SS_NR_NOWHERE".into()))
        .unwrap();
    sseq.put_field("WAIT1", EpicsValue::Short(1)).unwrap();
    // Step 2: an ordinary local write, so the sequence has an observable tail.
    sseq.put_field("DO2", EpicsValue::Double(22.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String("SS_NR_TGT2 PP".into()))
        .unwrap();
    db.add_record("SS_NR", Box::new(sseq)).await.unwrap();

    let mut wtg_rx = db
        .get_record("SS_NR")
        .unwrap()
        .write()
        .add_subscriber(
            "WTG1",
            1,
            DbFieldType::Short,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .expect("a WTG1 subscription must be accepted");

    kick(&db, "SS_NR").await;

    // The sequence finishes: step 2 lands and BUSY clears. Two facts, so two
    // waits — the write is the one the assertion below needs.
    poll_short(&db, "SS_NR.BUSY", 0, "the sequence must complete").await;
    poll_double(&db, "SS_NR_TGT2.VAL", 22.0, "step 2's write must land").await;
    assert_eq!(
        db.get_pv("SS_NR_TGT2.VAL").unwrap(),
        EpicsValue::Double(22.0),
        "step 2 must still run — step 1 made no put, so it blocks nothing"
    );

    // Settle before the negative: an erroneous WTG1 post rides the same tail
    // the write does (`processing.rs:3474-3477` runs the link writes, THEN
    // `notify_from_snapshot`), so "no event yet" is not "no event".
    epics_base_rs::runtime::task::sleep(Duration::from_millis(40)).await;
    assert!(
        wtg_rx.try_recv().is_err(),
        "C's `default:` arm makes no put and never sets `waiting`: a WAIT step \
         whose LNK resolves to no class must emit NO WTG1 event at all"
    );
    assert_eq!(
        db.get_pv("SS_NR.WTG1").unwrap(),
        EpicsValue::Short(0),
        "WTG1 must never have been raised"
    );
}

/// The counter-boundary, so the fix cannot be "never wait": the same step with
/// a `WAITn` on a link that DOES resolve still parks the sequence. (The full
/// wait/complete cycle is `sseq_async_machine.rs`; this pins that the put-class
/// gate did not swallow the waiting path.)
#[epics_macros_rs::epics_test]
async fn r17_3_a_wait_step_with_a_resolvable_local_lnk_still_puts() {
    let db = PvDatabase::new();
    db.add_record("SS_NR2_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap();
    // A local DB link: C cannot attach a put-callback to it, so `WAIT1` does
    // NOT wait — but the class resolves, so the put still goes out.
    sseq.put_field("DO1", EpicsValue::Double(33.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SS_NR2_TGT PP".into()))
        .unwrap();
    sseq.put_field("WAIT1", EpicsValue::Short(1)).unwrap();
    db.add_record("SS_NR2", Box::new(sseq)).await.unwrap();

    kick(&db, "SS_NR2").await;
    poll_short(&db, "SS_NR2.BUSY", 0, "the sequence must complete").await;
    poll_double(&db, "SS_NR2_TGT.VAL", 33.0, "the step's write must land").await;

    assert_eq!(
        db.get_pv("SS_NR2_TGT.VAL").unwrap(),
        EpicsValue::Double(33.0),
        "a resolvable target is still written"
    );
    assert_eq!(
        db.get_pv("SS_NR2.WTG1").unwrap(),
        EpicsValue::Short(0),
        "no put-callback on a DB link (C `dbPutLink`), so no wait"
    );
}
