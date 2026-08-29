//! R10-64 — C's scaler writes COUTP from `special()`, i.e. inside `dbPut`, so
//! the link's target is processed BEFORE the count is armed.
//!
//! ```c
//! case scalerRecordCNT:
//!     if (pscal->cnt && (pscal->us != USER_STATE_IDLE)) return(0);
//!     status = dbPutLink(&pscal->coutp, DBR_SHORT, &pscal->cnt, 1);   /* :625 */
//!     ...
//!     pscal->us = USER_STATE_REQSTART;                                /* :639 */
//! ```
//!
//! `dbPutField` runs `dbPut` (→ `special(after=1)` → this `dbPutLink`, target
//! processing included) to completion and only then reaches `dbProcess`. So a
//! record wired to `.COUTP` sees `SS == IDLE`, `US == IDLE` — the scaler has not
//! started counting — which is the whole point of a link that "triggers anything
//! that should coincide with scaler integration".
//!
//! The port had no action channel in `special()`, so the put was deferred to the
//! CNT-triggered process cycle and executed with the rest of that cycle's
//! actions: the target ran against an already-armed, already-COUNTING scaler.
//!
//! `Record::take_special_actions` is that channel; the put owner drains it and
//! executes it before the `pp(TRUE)` process.
//!
//! The `COUT` link (`:457`) is the negative control: C writes THAT one from
//! `process()`, after arming, so its target must still see `SS == COUNTING`.

// RTEMS-EXEC-MODEL-ALLOW(5): checked, not waived — all 5 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p scaler-rs
// --all-features`, 112/112). scaler-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use scaler_rs::records::scaler::ScalerRecord;

const SCALER_STATE_IDLE: f64 = 0.0;
const SCALER_STATE_COUNTING: f64 = 2.0;

/// A passive calc that latches `SCAL.SS` (the scaler's state machine) every time
/// it is processed. Wired as a link target, its VAL is the scaler's state as seen
/// at the moment the link fired.
fn state_latch() -> CalcRecord {
    let mut rec = CalcRecord::new("A");
    rec.put_field("INPA", EpicsValue::String("SCAL.SS NPP".into()))
        .unwrap();
    rec
}

/// SCAL with a 16-channel, 1 s preset; COUTP → `special()`'s target, COUT →
/// `process()`'s target. Both are `PP`, so the link write processes them.
async fn db_with_link_targets() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("TRIGP", Box::new(state_latch()))
        .await
        .unwrap();
    db.add_record("TRIGC", Box::new(state_latch()))
        .await
        .unwrap();

    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.nch = 16;
    rec.init_record(1).unwrap();
    rec.put_field("COUTP", EpicsValue::String("TRIGP PP".into()))
        .unwrap();
    rec.put_field("COUT", EpicsValue::String("TRIGC PP".into()))
        .unwrap();
    db.add_record("SCAL", Box::new(rec)).await.unwrap();
    db
}

async fn caput(db: &PvDatabase, rec: &str, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca(rec, field, value)
        .await
        .unwrap();
}

async fn val(db: &PvDatabase, rec: &str) -> f64 {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field("VAL").unwrap().to_f64().unwrap()
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> f64 {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f).unwrap().to_f64().unwrap()
}

async fn watch(db: &PvDatabase, rec: &str, field: &str) -> EventReader {
    let inst = db.get_record(rec).unwrap();
    let mut g = inst.write();
    g.add_subscriber(field, 1, DbFieldType::Double, EventMask::VALUE.bits())
        .expect("subscription must be accepted")
}

fn drain(rx: &mut EventReader) -> Vec<f64> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Some(v) = ev.snapshot.value.to_f64() {
            out.push(v);
        }
    }
    out
}

/// The cited divergence: on a count start, COUTP's target must run against an
/// IDLE scaler (C fires it in `special()`, before `:638` REQSTART and before the
/// process cycle arms), while COUT's target — written from `process()` at `:457`
/// — runs against the COUNTING scaler. One put, two link channels, two states.
#[tokio::test]
async fn r10_64_coutp_target_sees_the_scaler_before_it_is_armed() {
    let db = db_with_link_targets().await;

    caput(&db, "SCAL", "CNT", EpicsValue::Short(1)).await;

    assert_eq!(
        field(&db, "SCAL", "SS").await,
        SCALER_STATE_COUNTING,
        "the put did arm the count"
    );
    assert_eq!(
        val(&db, "TRIGP").await,
        SCALER_STATE_IDLE,
        "C's :624 dbPutLink runs inside dbPut — its target is processed before \
         the CNT-driven dbProcess arms the scaler"
    );
    assert_eq!(
        val(&db, "TRIGC").await,
        SCALER_STATE_COUNTING,
        "negative control: C's :457 COUT put is made by process(), AFTER arming"
    );
}

/// The stop edge exercises both COUTP puts in one cycle, in C's order:
/// `special()`'s (`:624`, while the scaler is still COUNTING) then `process()`'s
/// (`:463`, after the stop). The target therefore latches 2 and then 0 — the
/// deferred port collapsed both writes into the post-process point and the
/// target only ever saw 0.
///
/// Each COUTP put is a `dbPutLink` → `dbPut` landing CNT in the target's VAL,
/// and a calc's VAL is NOT `pp(TRUE)` — so C's `dbPut` tail
/// (`dbAccess.c:1411-1413`) posts the RAW link write (the CNT value, 0)
/// unconditionally before the `PP` process re-latches SS and the deadband
/// posts that. Four events per stop, interleaved put/process/put/process.
/// (An earlier revision expected only the two process posts — that encoded
/// the port's missing `dbPut` post, not C.)
#[tokio::test]
async fn r10_64_a_user_stop_runs_the_two_coutp_puts_in_c_order() {
    let db = db_with_link_targets().await;
    caput(&db, "SCAL", "CNT", EpicsValue::Short(1)).await;

    let mut trigp = watch(&db, "TRIGP", "VAL").await;
    caput(&db, "SCAL", "CNT", EpicsValue::Short(0)).await;

    assert_eq!(
        drain(&mut trigp),
        vec![
            // :625 put of CNT=0 into TRIGP.VAL — dbPut's own post…
            0.0,
            // …then the PP process latches SS, still COUNTING.
            SCALER_STATE_COUNTING,
            // :463 put of CNT=0 again after the stop…
            0.0,
            // …and its process latches the now-IDLE scaler.
            SCALER_STATE_IDLE,
        ],
        "special()'s put fires first, with the count still running (:625); \
         process()'s second put fires after the stop (:463); each put also \
         posts its raw write per C dbPut:1411-1413 (calc VAL is not pp)"
    );
}

/// The COUTP put carries CNT itself (`dbPutLink(&pscal->coutp, DBR_SHORT,
/// &pscal->cnt, 1)`), and C's `dbPut` has already stored the new CNT when
/// `special()` runs — so the target receives the NEW value, not the old one.
#[tokio::test]
async fn r10_64_the_special_put_carries_the_new_cnt() {
    let db = PvDatabase::new();
    // A calc with no input link: the link write lands in VAL and the calc's
    // CALC="VAL" leaves it there, so VAL is exactly what COUTP wrote.
    db.add_record("SINK", Box::new(CalcRecord::new("VAL")))
        .await
        .unwrap();

    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.nch = 16;
    rec.init_record(1).unwrap();
    rec.put_field("COUTP", EpicsValue::String("SINK PP".into()))
        .unwrap();
    db.add_record("SCAL", Box::new(rec)).await.unwrap();

    caput(&db, "SCAL", "CNT", EpicsValue::Short(1)).await;

    assert_eq!(
        val(&db, "SINK").await,
        1.0,
        "the put wrote CNT=1 before special() ran, so the link carries 1"
    );
}

/// Negative control for the drain contract: a put that queues no `special()`
/// action must write no link. A redundant start (CNT=1 while counting) is
/// rejected by C's `:622` guard before the `dbPutLink`, so COUTP's target is not
/// processed again — its latched value stays the one from the accepted start.
#[tokio::test]
async fn r10_64_a_redundant_start_queues_nothing() {
    let db = db_with_link_targets().await;
    caput(&db, "SCAL", "CNT", EpicsValue::Short(1)).await;
    assert_eq!(val(&db, "TRIGP").await, SCALER_STATE_IDLE);

    // Second CNT=1 while US != IDLE. If the framework fired a stale or
    // unconditional COUTP write here, TRIGP would re-latch SS == COUNTING.
    caput(&db, "SCAL", "CNT", EpicsValue::Short(1)).await;

    assert_eq!(
        val(&db, "TRIGP").await,
        SCALER_STATE_IDLE,
        "C's redundant-command guard (:622) returns before the put — no link write"
    );
}

/// Non-put processing must not fire the `special()` channel either: an ordinary
/// process cycle (no CNT write) drains nothing, so a preset completion writes
/// COUTP exactly once, from `process()` (`:463`).
#[tokio::test]
async fn r10_64_a_plain_process_cycle_fires_no_special_link() {
    let db = db_with_link_targets().await;

    let mut visited = HashSet::new();
    db.process_record_with_links("SCAL", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        val(&db, "TRIGP").await,
        0.0,
        "an idle process cycle makes no COUTP put at all — TRIGP was never \
         processed, so its VAL is still its initial 0"
    );
}
