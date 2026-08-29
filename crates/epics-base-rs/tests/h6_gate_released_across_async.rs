//! H6 boundaries: the L1 record gate is NOT held across an asynchronous wait.
//!
//! C `dbProcess` never blocks under `dbScanLock`. On async device support it
//! sets `pact` and RETURNS (`dbAccess.c:536-699`), releasing the lock; the
//! completion runs on the callback task, which takes the lock again for the
//! epilogue (`callback.c:379-388` `ProcessCallback`). The seq record's `DLYn`
//! chain is the same shape spelt out inside one record type: `process` sets
//! `pact = TRUE` and arms `callbackRequestDelayed` (`seqRecord.c:143`, `:196`,
//! `:201-217`), each group runs in `processCallback` under its own
//! `dbScanLock` (`:243-274`), and the exhausted chain re-enters `process` →
//! `asyncFinish` (`:206-209`, `:219-241`).
//!
//! One case per boundary of that contract, not one per narrative:
//!
//! 1. an async-device record's `process` RETURNS while the device is still
//!    working, and the gate is free during that window;
//! 2. a delayed re-process (seq `DLYn`) fires AFTER the gate is released, and
//!    the gate is free while the delay runs;
//! 3. the completion re-entry RE-TAKES the gate, so it serialises against a
//!    concurrent holder rather than racing it.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::seq::SeqRecord;
use epics_base_rs::types::EpicsValue;

/// A record that goes async-pending on every `process()` and never completes
/// on its own — the completion is the test's to drive, exactly as a device
/// callback would. C `aiRecord.c:122`: `prec->pact = TRUE; return 0;`.
struct NeverFinishes {
    val: f64,
}

impl Record for NeverFinishes {
    fn record_type(&self) -> &'static str {
        "h6_async_test"
    }
    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
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
                self.val = value.to_f64().ok_or(CaError::InvalidValue("bad".into()))?;
                Ok(())
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

/// Boundary 1 — the async window. A record that is async-pending holds no
/// gate: a second entry on the SAME record acquires it while the first cycle's
/// device work is outstanding.
///
/// This is the test that fails on the pre-H6 shape. There the whole body ran
/// under a `_record_gate` binding that lived to the end of
/// `process_record_with_links_inner`, and the awaits inside it (link I/O, the
/// nested locks, the delayed re-process) meant the gate could be held across a
/// suspension — a second taker would then wait on it. Here the gate must be
/// free the instant `process_record_with_links` returns.
// RTEMS-EXEC-MODEL-ALLOW(1): multi-thread flavored — the gate contention needs
// real parallel workers; checked to run and pass in the exec-backend suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_pending_record_does_not_hold_the_gate() {
    let db = PvDatabase::new();
    db.add_record("H6:ASYNC", Box::new(NeverFinishes { val: 0.0 }))
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("H6:ASYNC", &mut visited, 0)
        .await
        .unwrap();

    // The record IS async-pending — nothing completed it.
    assert!(
        db.get_record("H6:ASYNC").unwrap().read().is_processing(),
        "the record must still be PACT: this is the async window under test"
    );

    // ... and the gate is free RIGHT NOW.
    probe_gate_is_free(&db, "H6:ASYNC")
        .await
        .expect("the gate must be free while the record is async-pending");
}

/// Take and immediately release `record`'s gate on a blocking-pool thread,
/// bounded by a timeout.
///
/// The gate is a blocking priority-inheritance mutex, so "is it free?" cannot
/// be asked by racing a future against a timer — a held gate parks the
/// *thread*. Doing the acquisition on a `spawn_blocking` worker keeps the
/// timeout meaningful: the probe returns `Err` exactly when the gate was still
/// owned, which is the assertion these tests make.
async fn probe_gate_is_free(db: &PvDatabase, record: &'static str) -> Result<(), &'static str> {
    let db = db.clone();
    epics_base_rs::runtime::task::timeout(
        Duration::from_secs(5),
        epics_base_rs::runtime::task::spawn_blocking(move || drop(db.lock_record(record))),
    )
    .await
    .map_err(|_| "gate still held")?
    .expect("gate probe task");
    Ok(())
}

/// Boundary 1b — the same property through a real put. A CA-route put to an
/// async-pending record must reach its PACT/RPRO decision instead of blocking
/// on a gate the async cycle never released.
// RTEMS-EXEC-MODEL-ALLOW(1): multi-thread flavored — the gate contention needs
// real parallel workers; checked to run and pass in the exec-backend suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_put_during_the_async_window_is_not_gate_blocked() {
    let db = PvDatabase::new();
    db.add_record("H6:ASYNC2", Box::new(NeverFinishes { val: 0.0 }))
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("H6:ASYNC2", &mut visited, 0)
        .await
        .unwrap();
    assert!(db.get_record("H6:ASYNC2").unwrap().read().is_processing());

    // The plain `dbPutField` analogue (no put-notify wait-set — C builds one
    // only in `dbPutNotify`).
    epics_base_rs::runtime::task::timeout(
        Duration::from_secs(5),
        db.put_record_field_from_ca_no_notify("H6:ASYNC2", "VAL", EpicsValue::Double(3.0)),
    )
    .await
    .expect("a put during the async window must not block on the gate")
    .expect("the put itself is accepted (C defers the process via RPRO)");

    // C `dbPutField` on a PACT record sets RPRO and returns success
    // (`dbAccess.c:1260-1276`, `rpro = TRUE` at :1269); the value is written
    // either way — `dbPut` already ran at :1259.
    let rec = db.get_record("H6:ASYNC2").unwrap();
    let inst = rec.read();
    assert_eq!(inst.record.get_field("VAL"), Some(EpicsValue::Double(3.0)));
    assert_eq!(
        inst.common.rpro, 1,
        "a put to a PACT record must request a reprocess, C dbAccess.c:1260-1276"
    );
}

/// Boundary 2 — the delayed re-process. A `seq` with a non-zero `DLY0` must
/// return from `process` immediately (C `pact = TRUE` + `callbackRequestDelayed`)
/// and drive `LNK0` only after the delay, from a task that re-takes the gate.
///
/// Pre-H6 this slept inside the gate window: `process` did not return until
/// every group's `DLYn` had elapsed, and the gate was held throughout.
// RTEMS-EXEC-MODEL-ALLOW(1): multi-thread flavored — the gate contention needs
// real parallel workers; checked to run and pass in the exec-backend suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seq_delay_runs_outside_the_gate_and_fires_after_release() {
    let db = PvDatabase::new();
    db.add_record("H6:SEQ", Box::new(SeqRecord::new()))
        .await
        .unwrap();
    db.add_record("H6:SEQTGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("H6:SEQ").unwrap();
        let mut inst = rec.write();
        inst.record
            .put_field("DLY0", EpicsValue::Double(0.30))
            .unwrap();
        inst.record
            .put_field("DOL0", EpicsValue::String("7.0".into()))
            .unwrap();
        inst.record
            .put_field("DO0", EpicsValue::Double(7.0))
            .unwrap();
        inst.record
            .put_field("LNK0", EpicsValue::String("H6:SEQTGT".into()))
            .unwrap();
    }

    let start = std::time::Instant::now();
    let mut visited = HashSet::new();
    db.process_record_with_links("H6:SEQ", &mut visited, 0)
        .await
        .unwrap();
    let returned_after = start.elapsed();

    // C `process` returns straight through `processNextLink`; it does not wait
    // out `DLY0`.
    assert!(
        returned_after < Duration::from_millis(200),
        "seq process must return before DLY0 elapses, took {returned_after:?}"
    );
    // The group has NOT run yet.
    assert_eq!(
        db.get_record("H6:SEQTGT")
            .unwrap()
            .read()
            .record
            .get_field("VAL"),
        Some(EpicsValue::Double(0.0)),
        "LNK0 must not fire before DLY0 elapses"
    );
    // The record is ACTIVE across the delay, as C's `pact = TRUE` makes it.
    assert!(
        db.get_record("H6:SEQ").unwrap().read().is_processing(),
        "seq must be PACT while its DLYn chain runs"
    );
    // The gate is free while the delay runs — the whole point of the port.
    probe_gate_is_free(&db, "H6:SEQ")
        .await
        .expect("the gate must be free while the seq DLYn delay is pending");

    // After the delay the group runs and the chain completes the cycle.
    for _ in 0..100 {
        if db
            .get_record("H6:SEQTGT")
            .unwrap()
            .read()
            .record
            .get_field("VAL")
            == Some(EpicsValue::Double(7.0))
        {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        db.get_record("H6:SEQTGT")
            .unwrap()
            .read()
            .record
            .get_field("VAL"),
        Some(EpicsValue::Double(7.0)),
        "LNK0 must be driven once DLY0 elapsed"
    );
    // `asyncFinish` cleared PACT (`seqRecord.c:238`).
    for _ in 0..100 {
        if !db.get_record("H6:SEQ").unwrap().read().is_processing() {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !db.get_record("H6:SEQ").unwrap().read().is_processing(),
        "the exhausted DLYn chain must clear PACT (seqRecord.c asyncFinish)"
    );
}

/// A `LNKn` target whose put parks the thread that is driving it. Which
/// thread that is, is the whole question of UI-81 — so the test needs a put it
/// can still be standing inside while it asks.
struct ParkingTarget {
    val: f64,
}

impl Record for ParkingTarget {
    fn record_type(&self) -> &'static str {
        "ui81_parking_target"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
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
                std::thread::sleep(Duration::from_millis(300));
                self.val = value.to_f64().ok_or(CaError::InvalidValue("bad".into()))?;
                Ok(())
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

/// Boundary 2b (UI-81) — a seq whose selected groups are ALL `DLYn == 0` is
/// async too.
///
/// C `processNextLink` is unconditional about this: "Always use the callback
/// task to avoid recursion" (`seqRecord.c:210-215`). `DLY` only chooses
/// between `callbackRequestDelayed` and `callbackRequest`; it never chooses
/// between the callback task and the caller. So `process` returns at once with
/// `pact` set for a zero-delay group exactly as it does for a delayed one, and
/// the group walk, its per-group `dbScanLock` and `asyncFinish` all run on the
/// callback task. The port used to walk a zero-delay group inline, which held
/// the caller for the whole chain and never made the record active.
///
/// The parking target puts the boundary beyond a race: the walk is provably
/// still inside the first group's put while this test asks its questions.
// RTEMS-EXEC-MODEL-ALLOW(1): multi-thread flavored — the gate contention needs
// real parallel workers; checked to run and pass in the exec-backend suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seq_with_every_delay_zero_still_runs_on_the_callback_task() {
    let db = PvDatabase::new();
    db.add_record("UI81:SEQ", Box::new(SeqRecord::new()))
        .await
        .unwrap();
    db.add_record("UI81:TGT", Box::new(ParkingTarget { val: 0.0 }))
        .await
        .unwrap();
    {
        let rec = db.get_record("UI81:SEQ").unwrap();
        let mut inst = rec.write();
        // DLY0 stays 0.0 — the case under test.
        inst.record
            .put_field("DOL0", EpicsValue::String("5.0".into()))
            .unwrap();
        inst.record
            .put_field("DO0", EpicsValue::Double(5.0))
            .unwrap();
        inst.record
            .put_field("LNK0", EpicsValue::String("UI81:TGT".into()))
            .unwrap();
    }

    let start = std::time::Instant::now();
    let mut visited = HashSet::new();
    db.process_record_with_links("UI81:SEQ", &mut visited, 0)
        .await
        .unwrap();
    let returned_after = start.elapsed();

    // C returns through `processNextLink` without running a single group.
    assert!(
        returned_after < Duration::from_millis(150),
        "a zero-delay seq must return before its group's put completes, took \
         {returned_after:?}"
    );
    // `prec->pact = TRUE` (`seqRecord.c:143`) — the record is ACTIVE, which is
    // what makes it visible as busy and what `asyncFinish` later clears.
    assert!(
        db.get_record("UI81:SEQ").unwrap().read().is_processing(),
        "a zero-delay seq must be PACT across its group chain"
    );
    // And the caller is not the walker: the gate the walk re-takes per group
    // (`processCallback`'s `dbScanLock`, `:252`) is not held by this thread.
    probe_gate_is_free(&db, "UI81:SEQ")
        .await
        .expect("the seq gate must be free while the group chain runs");

    // The chain finishes on its own and `asyncFinish` clears PACT (`:238`).
    for _ in 0..100 {
        if !db.get_record("UI81:SEQ").unwrap().read().is_processing() {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !db.get_record("UI81:SEQ").unwrap().read().is_processing(),
        "the exhausted zero-delay chain must clear PACT"
    );
    assert_eq!(
        db.get_record("UI81:TGT")
            .unwrap()
            .read()
            .record
            .get_field("VAL"),
        Some(EpicsValue::Double(5.0)),
        "LNK0 is still driven — going async must not drop the write"
    );
}

/// Boundary 3 — the completion re-entry takes the gate.
///
/// C `ProcessCallback` brackets the completion in `dbScanLock` /
/// `dbScanUnlock` (`callback.c:379-388`), so the epilogue — alarm commit,
/// snapshot, OUT, FLNK — is serialised against any other holder. A
/// `complete_async_record` that ran gate-free would interleave with a
/// concurrent put on the record it is completing.
// RTEMS-EXEC-MODEL-ALLOW(1): multi-thread flavored — the gate contention needs
// real parallel workers; checked to run and pass in the exec-backend suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_async_completion_waits_for_the_gate() {
    let db = PvDatabase::new();
    db.add_record("H6:CMPL", Box::new(NeverFinishes { val: 0.0 }))
        .await
        .unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("H6:CMPL", &mut visited, 0)
        .await
        .unwrap();

    // Someone else owns the record's gate — a QSRV atomic transaction, say.
    // It is held on its own thread, not by this task: the gate blocks the
    // *thread* that owns it, so a holder that also drives the rest of the test
    // would be asserting about its own runtime rather than about the gate.
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
    let db_holder = db.clone();
    let holder = std::thread::spawn(move || {
        let _epoch = db_holder.lock_records(["H6:CMPL"]);
        held_tx.send(()).expect("holder announces the epoch");
        let _ = release_rx.recv();
    });
    held_rx
        .recv()
        .expect("the epoch must be taken before the test proceeds");

    let db2 = db.clone();
    let completed = Arc::new(AtomicU32::new(0));
    let completed2 = completed.clone();
    let h = epics_base_rs::runtime::task::Reactor::current()
        .expect("the test driver enters an executor")
        .spawn(async move {
            db2.complete_async_record("H6:CMPL").await.unwrap();
            completed2.store(1, Ordering::SeqCst);
        });

    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        completed.load(Ordering::SeqCst),
        0,
        "the completion epilogue must block on the gate a transaction holds"
    );

    drop(release_tx);
    holder.join().expect("epoch holder thread");
    epics_base_rs::runtime::task::timeout(Duration::from_secs(5), h)
        .await
        .expect("the completion must run once the gate is released")
        .unwrap();
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert!(
        !db.get_record("H6:CMPL").unwrap().read().is_processing(),
        "the completion clears PACT"
    );
}
