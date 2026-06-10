//! Boundary tests for the `sseq` per-step async state machine
//! (C `calcApp/src/sseqRecord.c`: `process`/`processNextLink`/
//! `processCallback`/`putCallbackCB`/`asyncFinish`/`special`).
//!
//! The sequence is driven one step at a time through the framework PACT
//! primitive — never an all-at-once dispatch. These tests pin the
//! invariant boundaries the machine must hold:
//!
//!   * a `WAITn` step blocks the sequence until the downstream
//!     put-with-completion finishes (vs. a hung callback escaped by a
//!     second `ABORT`),
//!   * each selected `LNKn` is dispatched exactly once per cycle (the
//!     regression that retiring the old `MultiOut::Sseq` all-at-once
//!     dispatch did not drop or duplicate a write),
//!   * `ABORT` mid-sequence cancels the pending step cleanly (the
//!     superseded re-entry is a no-op via the `AsyncToken` generation
//!     gate — no stale double-fire), and
//!   * the status fields (`BUSY`/`WTGn`/`ABORTING`) reflect live state
//!     across the cycle and are cleared at the final step.
#![allow(clippy::all)]

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

/// A target that never finishes its own `process()` — it goes
/// async-pending and stays there until the test drives
/// `complete_async_record`. A `WAITn` step writing into it (PP) joins
/// the sseq's put-notify wait-set and keeps it open, so the sequence
/// blocks. Models a slow downstream `dbCaPutLinkCallback` target.
struct AsyncHold {
    val: f64,
}

impl Record for AsyncHold {
    fn record_type(&self) -> &'static str {
        "async_hold"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::async_pending())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = value.to_f64().unwrap_or(self.val);
                Ok(())
            }
            _ => Err(epics_base_rs::error::CaError::FieldNotFound(name.into())),
        }
    }
    fn field_list(&self) -> &'static [FieldDesc] {
        &[]
    }
}

/// A target whose `process()` only counts how many times it is driven —
/// proves a link DID / DID NOT (and how often) dispatch to it.
struct CountingTarget {
    process_count: Arc<AtomicU32>,
}

impl Record for CountingTarget {
    fn record_type(&self) -> &'static str {
        "counting_target"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.process_count.fetch_add(1, Ordering::SeqCst);
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, _name: &str) -> Option<EpicsValue> {
        None
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn field_list(&self) -> &'static [FieldDesc] {
        &[]
    }
}

/// Kick a record's sequence once. The async machine returns to the caller
/// after the first step is *scheduled* (PACT set); the per-step work runs
/// in spawned re-entries.
async fn kick(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

/// Poll a `DBF_SHORT` status field until it equals `want`.
async fn poll_short(db: &PvDatabase, pv: &str, want: i16, label: &str) {
    for _ in 0..400 {
        if let Ok(EpicsValue::Short(v)) = db.get_pv(pv).await {
            if v == want {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "{label}: {pv} did not reach Short({want}) before timeout (last {:?})",
        db.get_pv(pv).await
    );
}

/// Poll a `DBF_DOUBLE` PV until it equals `want`.
async fn poll_double(db: &PvDatabase, pv: &str, want: f64, label: &str) {
    for _ in 0..400 {
        if let Ok(EpicsValue::Double(v)) = db.get_pv(pv).await {
            if (v - want).abs() < 1e-10 {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "{label}: {pv} did not reach {want} before timeout (last {:?})",
        db.get_pv(pv).await
    );
}

fn read_short(db_value: Option<EpicsValue>) -> i16 {
    match db_value {
        Some(EpicsValue::Short(v)) => v,
        other => panic!("expected Short, got {other:?}"),
    }
}

/// Boundary — a `WAITn` step blocks the whole sequence until the
/// downstream put-with-completion finishes (C `processCallback` issues
/// `dbCaPutLinkCallback`, `putCallbackCB` advances only on completion).
/// While blocked: `BUSY == 1`, `WTG1 == 1`, and the next step has not
/// fired. When the downstream completes, the sequence advances, runs the
/// last step, and clears `BUSY` (C `asyncFinish`).
#[tokio::test]
async fn sseq_waitn_blocks_until_downstream_completes() {
    let db = PvDatabase::new();
    // Step-1 target: async — joins the WAIT put-notify set and holds it
    // open until completed. Step-2 target: a plain sync AO.
    db.add_record("SSEQ_W_HOLD", Box::new(AsyncHold { val: 0.0 }))
        .await
        .unwrap();
    db.add_record("SSEQ_W_TGT2", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.selm = 0; // All steps.
    // Step 1: WAIT (put-with-completion) into the async hold target.
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_W_HOLD PP".into()))
        .unwrap();
    sseq.put_field("WAIT1", EpicsValue::Short(1)).unwrap(); // Wait
    // Step 2: no-wait write into a sync target.
    sseq.put_field("DO2", EpicsValue::Double(22.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String("SSEQ_W_TGT2 PP".into()))
        .unwrap();
    db.add_record("SSEQ_W", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_W").await;

    // Step 1 fires after its (zero) delay and parks on the WAIT callback.
    poll_short(&db, "SSEQ_W.WTG1", 1, "WAIT step parks").await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_W.BUSY").await.ok()),
        1,
        "BUSY must be held high while the WAIT step is outstanding"
    );
    // The downstream is still pending → step 2 must NOT have fired.
    assert_eq!(
        db.get_pv("SSEQ_W_TGT2").await.unwrap(),
        EpicsValue::Double(0.0),
        "step 2 must not fire while the WAIT step's put-callback is pending"
    );

    // Settle: confirm the block is stable, not just not-yet-arrived.
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        db.get_pv("SSEQ_W_TGT2").await.unwrap(),
        EpicsValue::Double(0.0),
        "step 2 stayed gated while the WAIT step is outstanding"
    );
    assert_eq!(
        read_short(db.get_pv("SSEQ_W.WTG1").await.ok()),
        1,
        "WTG1 stays high until the put-callback completes"
    );

    // Complete the downstream — the WAIT put-callback fires, the sequence
    // advances to step 2, runs it, and finishes.
    db.complete_async_record("SSEQ_W_HOLD").await.unwrap();

    poll_double(
        &db,
        "SSEQ_W_TGT2",
        22.0,
        "step 2 fires after WAIT completes",
    )
    .await;
    poll_short(&db, "SSEQ_W.WTG1", 0, "WTG1 clears on completion").await;
    poll_short(&db, "SSEQ_W.BUSY", 0, "last step clears BUSY").await;
}

/// Regression — each selected `LNKn` must be dispatched EXACTLY once per
/// cycle. The old all-at-once `MultiOut::Sseq` dispatch was retired in
/// favour of the per-step machine; this proves the retirement neither
/// dropped a step nor left a duplicate dispatch. Also pins last-step
/// `BUSY` clearing.
#[tokio::test]
async fn sseq_each_lnkn_dispatched_exactly_once_and_clears_busy() {
    let db = PvDatabase::new();
    let c1 = Arc::new(AtomicU32::new(0));
    let c2 = Arc::new(AtomicU32::new(0));
    let c3 = Arc::new(AtomicU32::new(0));
    db.add_record(
        "SSEQ_ONCE_T1",
        Box::new(CountingTarget {
            process_count: c1.clone(),
        }),
    )
    .await
    .unwrap();
    db.add_record(
        "SSEQ_ONCE_T2",
        Box::new(CountingTarget {
            process_count: c2.clone(),
        }),
    )
    .await
    .unwrap();
    db.add_record(
        "SSEQ_ONCE_T3",
        Box::new(CountingTarget {
            process_count: c3.clone(),
        }),
    )
    .await
    .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.selm = 0; // All steps.
    // Three PP no-wait steps; each must process its target once.
    sseq.put_field("DO1", EpicsValue::Double(1.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_ONCE_T1 PP".into()))
        .unwrap();
    sseq.put_field("DO2", EpicsValue::Double(2.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String("SSEQ_ONCE_T2 PP".into()))
        .unwrap();
    sseq.put_field("DO3", EpicsValue::Double(3.0)).unwrap();
    sseq.put_field("LNK3", EpicsValue::String("SSEQ_ONCE_T3 PP".into()))
        .unwrap();
    db.add_record("SSEQ_ONCE", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_ONCE").await;

    // Wait for the last step's target, then settle so any erroneous extra
    // dispatch (a leftover all-at-once path) would also have landed.
    poll_short(&db, "SSEQ_ONCE.BUSY", 0, "sequence finishes").await;
    tokio::time::sleep(Duration::from_millis(40)).await;

    assert_eq!(
        c1.load(Ordering::SeqCst),
        1,
        "step 1 LNKn dispatched exactly once"
    );
    assert_eq!(
        c2.load(Ordering::SeqCst),
        1,
        "step 2 LNKn dispatched exactly once"
    );
    assert_eq!(
        c3.load(Ordering::SeqCst),
        1,
        "step 3 LNKn dispatched exactly once"
    );
    assert_eq!(
        read_short(db.get_pv("SSEQ_ONCE.BUSY").await.ok()),
        0,
        "BUSY cleared after the final step"
    );
}

/// Boundary — `ABORT` during a step's `DLYn` delay (phase `Fire`) cancels
/// the pending step cleanly. C `special` cancels the delay timer and
/// completes the abort immediately (`asyncFinish`): the step's `LNKn`
/// never fires, `BUSY`/`ABORT`/`ABORTING` reset, and the superseded delay
/// re-entry is a structural no-op (the `AsyncToken` generation gate) — no
/// stale double-dispatch.
#[tokio::test]
async fn sseq_abort_during_delay_finishes_without_dispatch() {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicU32::new(0));
    db.add_record(
        "SSEQ_AB_TGT",
        Box::new(CountingTarget {
            process_count: count.clone(),
        }),
    )
    .await
    .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.selm = 0;
    // A single step with a long delay so the abort lands while the
    // sequence is parked in phase `Fire`, before the step writes.
    sseq.put_field("DLY1", EpicsValue::Double(5.0)).unwrap();
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_AB_TGT PP".into()))
        .unwrap();
    db.add_record("SSEQ_AB", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_AB").await;
    // BUSY is raised synchronously at the start; confirm the sequence is
    // running and parked on the 5 s delay.
    poll_short(&db, "SSEQ_AB.BUSY", 1, "sequence started").await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "step must not have fired yet (still in its delay)"
    );

    // Abort. C `special(ABORT)` cancels the delay and finishes now.
    db.put_pv("SSEQ_AB.ABORT", EpicsValue::Short(1))
        .await
        .unwrap();

    poll_short(&db, "SSEQ_AB.BUSY", 0, "abort finishes the sequence").await;
    // finish() resets abort/aborting.
    assert_eq!(
        read_short(db.get_pv("SSEQ_AB.ABORT").await.ok()),
        0,
        "ABORT cleared by asyncFinish"
    );
    assert_eq!(
        read_short(db.get_pv("SSEQ_AB.ABORTING").await.ok()),
        0,
        "ABORTING cleared by asyncFinish"
    );
    // Settle: the aborted step's write must never land.
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "an aborted step must not dispatch its LNKn"
    );
}

/// Boundary — a hung `WAITn` put-callback (downstream that never
/// completes) is escaped by a second `ABORT`. The first abort sets
/// `ABORTING` and waits for the outstanding callback (C does NOT force a
/// re-entry from phase `Wait`); the second abort clears the `waiting`
/// flags, drops the remaining steps, and forces the finish
/// (C `sseqRecord.c` second-abort branch).
#[tokio::test]
async fn sseq_second_abort_escapes_hung_wait_callback() {
    let db = PvDatabase::new();
    // Async target that never completes — the WAIT step parks forever.
    db.add_record("SSEQ_HUNG_HOLD", Box::new(AsyncHold { val: 0.0 }))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.selm = 0;
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_HUNG_HOLD PP".into()))
        .unwrap();
    sseq.put_field("WAIT1", EpicsValue::Short(1)).unwrap(); // Wait
    db.add_record("SSEQ_HUNG", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_HUNG").await;
    poll_short(&db, "SSEQ_HUNG.WTG1", 1, "WAIT step parks on hung callback").await;

    // First abort: ABORTING goes high, but the machine waits for the
    // (never-arriving) callback — BUSY/WTG1 stay high.
    db.put_pv("SSEQ_HUNG.ABORT", EpicsValue::Short(1))
        .await
        .unwrap();
    poll_short(&db, "SSEQ_HUNG.ABORTING", 1, "first abort sets ABORTING").await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_HUNG.BUSY").await.ok()),
        1,
        "first abort on a hung WAIT callback does NOT finish — it waits"
    );
    assert_eq!(
        read_short(db.get_pv("SSEQ_HUNG.WTG1").await.ok()),
        1,
        "WTG1 stays high after the first abort (callback still outstanding)"
    );

    // Second abort: escape the hung callback — clear waiting, finish now.
    db.put_pv("SSEQ_HUNG.ABORT", EpicsValue::Short(1))
        .await
        .unwrap();
    poll_short(&db, "SSEQ_HUNG.BUSY", 0, "second abort forces the finish").await;
    poll_short(&db, "SSEQ_HUNG.WTG1", 0, "second abort clears WTG1").await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_HUNG.ABORTING").await.ok()),
        0,
        "ABORTING cleared by the forced finish"
    );
    assert_eq!(
        read_short(db.get_pv("SSEQ_HUNG.ABORT").await.ok()),
        0,
        "ABORT cleared by the forced finish"
    );
}
