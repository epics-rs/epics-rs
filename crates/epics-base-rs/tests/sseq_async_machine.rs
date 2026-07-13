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
//!
//! The concurrency boundaries (C `processNextLink` selective barriers,
//! sseqRecord.c:407-441) pin the `After<n>` overlap model: several `WAITn`
//! put-callbacks run in flight at once; a `Wait` is a full barrier, an
//! `After<n>` blocks only steps at absolute index `>= n`, the
//! end-of-sequence drain waits for every outstanding callback, and an abort
//! drains the in-flight set before finishing.
#![allow(clippy::all)]

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, FieldDesc, ProcessOutcome, Record};
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

/// A link target must expose a `VAL` field: sseq chooses each step's buffer
/// from the DESTINATION's DBF type (C `processCallback`'s switch on
/// `dbGetLinkDBFtype(&lnk)`), and a target whose type does not resolve gets
/// no put at all (C's `default: break`). C has no field-less record.
static COUNTING_TARGET_FIELDS: &[FieldDesc] = &[FieldDesc {
    name: "VAL",
    dbf_type: epics_base_rs::types::DbFieldType::Double,
    read_only: false,
}];

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
        COUNTING_TARGET_FIELDS
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

/// Poll an integer-valued status PV (DBF_SHORT or a DBF_MENU served as
/// DBR_ENUM) until its numeric value equals `want`. Coerces through
/// `to_f64` so it handles both `Short` (DTn/LTn/WERRn) and `Enum`
/// (DOLnV/LNKnV) without caring which the field uses.
async fn poll_i16(db: &PvDatabase, pv: &str, want: i16, label: &str) {
    for _ in 0..400 {
        if let Ok(v) = db.get_pv(pv).await {
            if v.to_f64().map(|f| f as i16) == Some(want) {
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

/// Read an integer-valued status PV's current numeric value as `i16`.
async fn read_i16(db: &PvDatabase, pv: &str) -> i16 {
    db.get_pv(pv)
        .await
        .ok()
        .and_then(|v| v.to_f64())
        .map(|f| f as i16)
        .unwrap_or_else(|| panic!("{pv} not readable as a number"))
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

/// Boundary — `SELM=Specified` with an out-of-range `SELN` (> the 10
/// steps) is an invalid selection: C `process` raises
/// `recGblSetSevr(pR,SOFT_ALARM,INVALID_ALARM)` and finishes without
/// running any step (sseqRecord.c:319-323). The async start path must
/// raise the same alarm AND dispatch no `LNKn` — the regression the old
/// `MultiOut::Sseq` dispatch covered via `apply_selm_alarm` and the
/// per-step machine dropped (it finished silently).
#[tokio::test]
async fn sseq_seln_out_of_range_raises_invalid_alarm_and_no_dispatch() {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicU32::new(0));
    db.add_record(
        "SSEQ_SELN_TGT",
        Box::new(CountingTarget {
            process_count: count.clone(),
        }),
    )
    .await
    .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.selm = 1; // Specified
    sseq.seln = 11; // out of range — only steps 1..10 exist
    // A configured step so a stray dispatch would be observable.
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_SELN_TGT PP".into()))
        .unwrap();
    db.add_record("SSEQ_SELN", Box::new(sseq)).await.unwrap();

    // The invalid-selection path finishes synchronously (Complete), so the
    // kick returns with the alarm already evaluated.
    kick(&db, "SSEQ_SELN").await;

    let rec = db.get_record("SSEQ_SELN").await.unwrap();
    let inst = rec.read().await;
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "out-of-range SELN must raise INVALID severity"
    );
    assert_eq!(
        inst.common.stat, 15,
        "out-of-range SELN must raise SOFT_ALARM (status 15)"
    );
    drop(inst);

    // Settle so any erroneous step dispatch would also have landed.
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "an invalid selection must dispatch no LNKn"
    );
    assert_eq!(
        read_short(db.get_pv("SSEQ_SELN.BUSY").await.ok()),
        0,
        "BUSY cleared after the invalid selection finished"
    );
}

/// Boundary — `DOLnV`/`LNKnV` connection status and `DTn`/`LTn` target
/// field type, classified by `checkLinks` (C `sseqRecord.c:862-941` /
/// `init_record` 202-250). A LOCAL DB link → `LOC` (2) + the resolved
/// target field type; an empty (constant) link → `CON` (3) + the unknown
/// (-1) field type. The refresh runs at record init (`set_async_context`).
#[tokio::test]
async fn sseq_link_status_loc_vs_con() {
    let db = PvDatabase::new();
    // A local target (ao VAL is DBF_DOUBLE = 10 in C dbStatic numbering,
    // dbFldTypes.h:35) so a DB link to it resolves to LOC with a known field
    // type. DTn/LTn report the dbStatic DBF_* index, not the CA DBR value (6).
    db.add_record("SSEQ_LS_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    // Step 1: local DOL and local LNK → both LOC, field types resolved.
    sseq.put_field("DOL1", EpicsValue::String("SSEQ_LS_TGT.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_LS_TGT".into()))
        .unwrap();
    // Step 2: empty (constant) DOL and LNK → both CON, field types unknown.
    db.add_record("SSEQ_LS", Box::new(sseq)).await.unwrap();

    // Init refresh classifies the links and posts the diagnostics.
    poll_i16(&db, "SSEQ_LS.DOL1V", 2, "local DOL → LOC").await;
    poll_i16(&db, "SSEQ_LS.LNK1V", 2, "local LNK → LOC").await;
    poll_i16(
        &db,
        "SSEQ_LS.DT1",
        10,
        "local DOL field type resolved (DBF_DOUBLE = 10)",
    )
    .await;
    poll_i16(
        &db,
        "SSEQ_LS.LT1",
        10,
        "local LNK field type resolved (DBF_DOUBLE = 10)",
    )
    .await;

    // Empty links classify as CON with unknown (-1) field type.
    poll_i16(&db, "SSEQ_LS.DOL2V", 3, "empty DOL → CON").await;
    poll_i16(&db, "SSEQ_LS.LNK2V", 3, "empty LNK → CON").await;
    assert_eq!(
        read_i16(&db, "SSEQ_LS.DT2").await,
        -1,
        "empty DOL field type is unknown (-1)"
    );
    assert_eq!(
        read_i16(&db, "SSEQ_LS.LT2").await,
        -1,
        "empty LNK field type is unknown (-1)"
    );
}

/// Boundary — a runtime `DOLn`/`LNKn` re-point (`special()` → `checkLinks`,
/// sseqRecord.c:862-941) must not be clobbered by a *stale* concurrent
/// refresh. `refresh_link_status` classifies a snapshot of the link strings
/// off-thread; the init-time refresh (empty link → `CON`) can finish *after*
/// the runtime re-point refresh (local link → `LOC`). The monotonic
/// generation gate makes the later classification win regardless of which
/// spawned task posts last, so `DOL1V` settles at `LOC` and stays there.
#[tokio::test]
async fn sseq_link_status_reclassifies_on_special() {
    let db = PvDatabase::new();
    db.add_record("SSEQ_SP_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("SSEQ_SP", Box::new(SseqRecord::new()))
        .await
        .unwrap();

    // Empty DOL1 at init → CON (3).
    poll_i16(&db, "SSEQ_SP.DOL1V", 3, "empty DOL1 → CON").await;

    // A client put to the DOL1 link string re-runs checkLinks via special().
    // The init-time CON refresh and this LOC refresh race; the gate must let
    // the newer LOC classification win.
    db.put_pv("SSEQ_SP.DOL1", EpicsValue::String("SSEQ_SP_TGT.VAL".into()))
        .await
        .unwrap();
    poll_i16(&db, "SSEQ_SP.DOL1V", 2, "DOL1 repointed to local → LOC").await;

    // And it must stay LOC — a stale CON post arriving late cannot clobber it.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(
            read_i16(&db, "SSEQ_SP.DOL1V").await,
            2,
            "DOL1V must remain LOC; a stale refresh must not clobber it",
        );
    }
}

/// Boundary — `WERRn` is REDEFINED from C (sseqRecord.c:915-927). C raises
/// it for a local DB link with `WAITn` (it cannot `dbCaPutLinkCallback` a
/// non-CA link); the Rust `WriteDbLinkNotify` seam DOES complete on a local
/// PP link, so that is not an error here. `WERRn` instead flags `WAITn` set
/// on a link that can never deliver a completion — a Constant/unset (`CON`)
/// link. So: WAIT on a local DB link → `WERRn` clears (0); WAIT on an empty
/// (constant) link → `WERRn` raises (1).
#[tokio::test]
async fn sseq_werr_wait_on_constant_raises_wait_on_local_clears() {
    let db = PvDatabase::new();
    db.add_record("SSEQ_WE_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    // Step 1: WAIT on a LOCAL DB link — completable, so no error.
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_WE_TGT".into()))
        .unwrap();
    sseq.put_field("WAIT1", EpicsValue::Short(1)).unwrap(); // Wait
    // Step 2: WAIT on an EMPTY (constant) link — can never complete → error.
    sseq.put_field("WAIT2", EpicsValue::Short(1)).unwrap(); // Wait, LNK2 empty
    // Step 3: control — NoWait on an empty link is not a misconfig.
    db.add_record("SSEQ_WE", Box::new(sseq)).await.unwrap();

    // The raise is the discriminating transition (default WERR2 == 0).
    poll_i16(
        &db,
        "SSEQ_WE.WERR2",
        1,
        "WAIT on a constant link raises WERR",
    )
    .await;
    // The local-DB step's WAIT must NOT be flagged (the C-inversion).
    poll_i16(&db, "SSEQ_WE.LNK1V", 2, "local LNK → LOC").await;
    assert_eq!(
        read_i16(&db, "SSEQ_WE.WERR1").await,
        0,
        "WAIT on a local DB link is completable → WERR clears (vs C)"
    );
    poll_i16(&db, "SSEQ_WE.LNK2V", 3, "empty LNK → CON").await;
    assert_eq!(
        read_i16(&db, "SSEQ_WE.WERR3").await,
        0,
        "NoWait on an empty link is not a misconfig → WERR stays 0"
    );
}

/// Concurrency boundary (1) — a `Wait` (full barrier) keeps exactly ONE
/// put-callback outstanding: the next step does not even dispatch while a
/// `Wait` step is in flight. C `processNextLink` (sseqRecord.c:424-431):
/// `usePutCallback == sseqWAIT_Wait` returns immediately, blocking every
/// later step until that callback completes. Both steps are `Wait` into
/// async holds; only `WTG1` is high until `HOLD1` completes, then `WTG2`.
#[tokio::test]
async fn sseq_wait_full_barrier_serialises_steps() {
    let db = PvDatabase::new();
    db.add_record("SSEQ_FB_HOLD1", Box::new(AsyncHold { val: 0.0 }))
        .await
        .unwrap();
    db.add_record("SSEQ_FB_HOLD2", Box::new(AsyncHold { val: 0.0 }))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.selm = 0; // All steps.
    // Step 1: Wait → async hold (full barrier for every later step).
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_FB_HOLD1 PP".into()))
        .unwrap();
    sseq.put_field("WAIT1", EpicsValue::Short(1)).unwrap(); // Wait
    // Step 2: also Wait → its own async hold.
    sseq.put_field("DO2", EpicsValue::Double(22.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String("SSEQ_FB_HOLD2 PP".into()))
        .unwrap();
    sseq.put_field("WAIT2", EpicsValue::Short(1)).unwrap(); // Wait
    db.add_record("SSEQ_FB", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_FB").await;

    // Step 1 in flight; step 2 blocked by the full barrier → NOT dispatched.
    poll_short(&db, "SSEQ_FB.WTG1", 1, "step 1 Wait parks").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_FB.WTG2").await.ok()),
        0,
        "a Wait full barrier blocks step 2 from even dispatching"
    );
    assert_eq!(
        read_short(db.get_pv("SSEQ_FB.BUSY").await.ok()),
        1,
        "BUSY held while the Wait step is outstanding"
    );

    // Complete step 1 → step 2 now dispatches (and parks on its own hold).
    db.complete_async_record("SSEQ_FB_HOLD1").await.unwrap();
    poll_short(
        &db,
        "SSEQ_FB.WTG2",
        1,
        "step 2 dispatches after barrier clears",
    )
    .await;
    poll_short(&db, "SSEQ_FB.WTG1", 0, "step 1 cleared on completion").await;

    // Complete step 2 → the sequence drains and finishes.
    db.complete_async_record("SSEQ_FB_HOLD2").await.unwrap();
    poll_short(&db, "SSEQ_FB.BUSY", 0, "sequence finishes after both steps").await;
    poll_short(&db, "SSEQ_FB.WTG2", 0, "step 2 cleared at finish").await;
}

/// Concurrency boundary (2) — `After<n>` lets earlier put-callbacks overlap
/// in flight while still barriering a later step. C `processNextLink`
/// (sseqRecord.c:432-439): an earlier `After<n>` step blocks the current one
/// only when `(usePutCallback - 2) < plinkGroupCurrent->index`, i.e. the
/// current step's absolute index `>= n`. Here `LNK1`/`LNK2` are `After2`
/// (menu 3 → blocks absolute index `>= 2`) and `LNK3` is `Wait`:
///   * steps 1 (index 0) and 2 (index 1) overlap — neither is `>= 2`;
///   * step 3 (index 2) waits for BOTH `After2` steps to complete.
///
/// (The task brief's "After3" does not block step 3 under literal C
/// arithmetic — `After3` gates index `>= 3` — so the overlap+barrier shape
/// it describes is pinned with `After2`, the value that actually gates
/// index 2.)
#[tokio::test]
async fn sseq_after_n_overlaps_then_barriers() {
    let db = PvDatabase::new();
    db.add_record("SSEQ_OV_HOLD1", Box::new(AsyncHold { val: 0.0 }))
        .await
        .unwrap();
    db.add_record("SSEQ_OV_HOLD2", Box::new(AsyncHold { val: 0.0 }))
        .await
        .unwrap();
    db.add_record("SSEQ_OV_HOLD3", Box::new(AsyncHold { val: 0.0 }))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.selm = 0; // All steps.
    // After2 = menu index 3 (NoWait=0, Wait=1, After1=2, After2=3).
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_OV_HOLD1 PP".into()))
        .unwrap();
    sseq.put_field("WAIT1", EpicsValue::Short(3)).unwrap(); // After2
    sseq.put_field("DO2", EpicsValue::Double(22.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String("SSEQ_OV_HOLD2 PP".into()))
        .unwrap();
    sseq.put_field("WAIT2", EpicsValue::Short(3)).unwrap(); // After2
    sseq.put_field("DO3", EpicsValue::Double(33.0)).unwrap();
    sseq.put_field("LNK3", EpicsValue::String("SSEQ_OV_HOLD3 PP".into()))
        .unwrap();
    sseq.put_field("WAIT3", EpicsValue::Short(1)).unwrap(); // Wait
    db.add_record("SSEQ_OV", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_OV").await;

    // Steps 1 and 2 are BOTH in flight simultaneously (After2 imposes no
    // barrier on index 0 or 1); step 3 (index 2) is blocked → not dispatched.
    poll_short(&db, "SSEQ_OV.WTG2", 1, "step 2 dispatches concurrently").await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_OV.WTG1").await.ok()),
        1,
        "step 1 is still in flight while step 2 is also in flight (overlap)"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_OV.WTG3").await.ok()),
        0,
        "step 3 (index 2) is barriered by the After2 steps → not dispatched"
    );

    // Complete only step 1 → step 3 STILL blocked (step 2's After2 holds it).
    db.complete_async_record("SSEQ_OV_HOLD1").await.unwrap();
    poll_short(&db, "SSEQ_OV.WTG1", 0, "step 1 cleared").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_OV.WTG3").await.ok()),
        0,
        "step 3 waits for BOTH After2 steps — still blocked after only one"
    );
    assert_eq!(
        read_short(db.get_pv("SSEQ_OV.WTG2").await.ok()),
        1,
        "step 2 remains in flight",
    );

    // Complete step 2 → both barriers cleared → step 3 dispatches.
    db.complete_async_record("SSEQ_OV_HOLD2").await.unwrap();
    poll_short(
        &db,
        "SSEQ_OV.WTG3",
        1,
        "step 3 dispatches after both barriers clear",
    )
    .await;
    poll_short(&db, "SSEQ_OV.WTG2", 0, "step 2 cleared").await;

    // Complete step 3 → finish.
    db.complete_async_record("SSEQ_OV_HOLD3").await.unwrap();
    poll_short(&db, "SSEQ_OV.BUSY", 0, "sequence finishes after step 3").await;
}

/// Concurrency boundary (3) — the end-of-sequence drain finishes only after
/// EVERY outstanding put-callback completes. C `processNextLink`
/// (sseqRecord.c:407-417): with `plinkGroupCurrent == NULL`, return while any
/// earlier link-group is still `waiting`; call `process` (→ `asyncFinish`)
/// only when none remain. Two `After2` steps (no later step, so no barrier
/// between them) both dispatch and the sequence drains: completing one keeps
/// `BUSY` high; completing the second finishes.
#[tokio::test]
async fn sseq_end_drain_waits_for_all_in_flight() {
    let db = PvDatabase::new();
    db.add_record("SSEQ_DR_HOLD1", Box::new(AsyncHold { val: 0.0 }))
        .await
        .unwrap();
    db.add_record("SSEQ_DR_HOLD2", Box::new(AsyncHold { val: 0.0 }))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.selm = 0; // All steps.
    // Two After2 steps; no step at index >= 2 exists, so neither barriers the
    // other → both dispatch and overlap, then the sequence end-drains.
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_DR_HOLD1 PP".into()))
        .unwrap();
    sseq.put_field("WAIT1", EpicsValue::Short(3)).unwrap(); // After2
    sseq.put_field("DO2", EpicsValue::Double(22.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String("SSEQ_DR_HOLD2 PP".into()))
        .unwrap();
    sseq.put_field("WAIT2", EpicsValue::Short(3)).unwrap(); // After2
    db.add_record("SSEQ_DR", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_DR").await;

    // Both in flight, sequence draining.
    poll_short(&db, "SSEQ_DR.WTG1", 1, "step 1 in flight").await;
    poll_short(&db, "SSEQ_DR.WTG2", 1, "step 2 in flight concurrently").await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_DR.BUSY").await.ok()),
        1,
        "BUSY held while draining two in-flight callbacks"
    );

    // Complete ONE → drain is not done; BUSY must stay high.
    db.complete_async_record("SSEQ_DR_HOLD1").await.unwrap();
    poll_short(&db, "SSEQ_DR.WTG1", 0, "step 1 drained").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_DR.BUSY").await.ok()),
        1,
        "the drain finishes only after BOTH complete — one is not enough"
    );
    assert_eq!(
        read_short(db.get_pv("SSEQ_DR.WTG2").await.ok()),
        1,
        "step 2 still outstanding"
    );

    // Complete the second → drain finishes.
    db.complete_async_record("SSEQ_DR_HOLD2").await.unwrap();
    poll_short(&db, "SSEQ_DR.BUSY", 0, "drain finishes after both complete").await;
    poll_short(&db, "SSEQ_DR.WTG2", 0, "step 2 cleared at finish").await;
}

/// Concurrency boundary (4) — `ABORT` while put-callbacks are in flight
/// drains them before finishing; no completion is stranded and `BUSY`
/// clears. C `process`/`processNextLink` under `pR->abort`: the first abort
/// dispatches no new step and lets the outstanding callbacks drain
/// (`asyncFinish` runs once they return). Two overlapping `After2` steps are
/// in flight; the abort holds `ABORTING`/`BUSY` high until BOTH complete,
/// then finishes cleanly.
#[tokio::test]
async fn sseq_abort_drains_in_flight_before_finish() {
    let db = PvDatabase::new();
    db.add_record("SSEQ_ABD_HOLD1", Box::new(AsyncHold { val: 0.0 }))
        .await
        .unwrap();
    db.add_record("SSEQ_ABD_HOLD2", Box::new(AsyncHold { val: 0.0 }))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.selm = 0; // All steps.
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_ABD_HOLD1 PP".into()))
        .unwrap();
    sseq.put_field("WAIT1", EpicsValue::Short(3)).unwrap(); // After2
    sseq.put_field("DO2", EpicsValue::Double(22.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String("SSEQ_ABD_HOLD2 PP".into()))
        .unwrap();
    sseq.put_field("WAIT2", EpicsValue::Short(3)).unwrap(); // After2
    db.add_record("SSEQ_ABD", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_ABD").await;
    poll_short(&db, "SSEQ_ABD.WTG1", 1, "step 1 in flight").await;
    poll_short(&db, "SSEQ_ABD.WTG2", 1, "step 2 in flight concurrently").await;

    // Abort while both are outstanding. C: no new step fires; drain first.
    db.put_pv("SSEQ_ABD.ABORT", EpicsValue::Short(1))
        .await
        .unwrap();
    poll_short(&db, "SSEQ_ABD.ABORTING", 1, "abort accepted, draining").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_ABD.BUSY").await.ok()),
        1,
        "abort does not finish while callbacks are still outstanding"
    );

    // Complete one → still draining the other.
    db.complete_async_record("SSEQ_ABD_HOLD1").await.unwrap();
    poll_short(
        &db,
        "SSEQ_ABD.WTG1",
        0,
        "first in-flight drained under abort",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_ABD.BUSY").await.ok()),
        1,
        "abort drain still waits for the second outstanding callback"
    );

    // Complete the second → drain finishes; no stranded completion.
    db.complete_async_record("SSEQ_ABD_HOLD2").await.unwrap();
    poll_short(
        &db,
        "SSEQ_ABD.BUSY",
        0,
        "abort finishes after the drain completes",
    )
    .await;
    poll_short(&db, "SSEQ_ABD.WTG2", 0, "second in-flight cleared").await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_ABD.ABORTING").await.ok()),
        0,
        "ABORTING cleared by the finish"
    );
    assert_eq!(
        read_short(db.get_pv("SSEQ_ABD.ABORT").await.ok()),
        0,
        "ABORT cleared by the finish"
    );
}

/// Concurrency boundary (5) — `After<n>` does NOT block a step whose absolute
/// index is `< n`: that step fires concurrently while the `After<n>` callback
/// is still outstanding. C `processNextLink` (sseqRecord.c:432): the barrier
/// applies only when `(usePutCallback - 2) < plinkGroupCurrent->index`, which
/// is FALSE for a lower-indexed current step. Step 1 is `After2` (menu 3,
/// gates index `>= 2`) into an async hold that stays in flight; step 2
/// (index 1, `< 2`) is `NoWait` into a sync target and must fire immediately,
/// landing its value while step 1's callback is still outstanding.
#[tokio::test]
async fn sseq_after_n_does_not_block_lower_index_step() {
    let db = PvDatabase::new();
    db.add_record("SSEQ_NB_HOLD1", Box::new(AsyncHold { val: 0.0 }))
        .await
        .unwrap();
    // A sync AO target so step 2's NoWait write lands observably.
    db.add_record("SSEQ_NB_TGT2", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.selm = 0; // All steps.
    // Step 1: After2 (gates index >= 2) into the async hold → in flight.
    sseq.put_field("DO1", EpicsValue::Double(11.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_NB_HOLD1 PP".into()))
        .unwrap();
    sseq.put_field("WAIT1", EpicsValue::Short(3)).unwrap(); // After2
    // Step 2 (index 1 < 2): NoWait → sync target; After2 imposes no barrier.
    sseq.put_field("DO2", EpicsValue::Double(22.0)).unwrap();
    sseq.put_field("LNK2", EpicsValue::String("SSEQ_NB_TGT2 PP".into()))
        .unwrap();
    db.add_record("SSEQ_NB", Box::new(sseq)).await.unwrap();

    kick(&db, "SSEQ_NB").await;

    // Step 1 parks in flight; step 2 fires concurrently (NOT blocked by the
    // After2) — its value lands while step 1's callback is still outstanding.
    poll_short(&db, "SSEQ_NB.WTG1", 1, "step 1 After2 in flight").await;
    poll_double(
        &db,
        "SSEQ_NB_TGT2",
        22.0,
        "step 2 (index 1 < 2) fires concurrently — After2 does not block it",
    )
    .await;
    assert_eq!(
        read_short(db.get_pv("SSEQ_NB.WTG1").await.ok()),
        1,
        "step 1 is STILL in flight while step 2 has already fired (overlap)"
    );
    assert_eq!(
        read_short(db.get_pv("SSEQ_NB.BUSY").await.ok()),
        1,
        "BUSY held: step 1's After2 callback is still draining"
    );

    // Complete step 1 → the sequence drains and finishes.
    db.complete_async_record("SSEQ_NB_HOLD1").await.unwrap();
    poll_short(
        &db,
        "SSEQ_NB.BUSY",
        0,
        "finish after the After2 callback drains",
    )
    .await;
    poll_short(&db, "SSEQ_NB.WTG1", 0, "step 1 cleared at finish").await;
}
