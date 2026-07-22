//! Unit tests for the PACT async-record primitive
//! (`AsyncToken` + `post_fields` + put-notify wait-set wiring) built in
//! `server/database/processing.rs`.
//!
//! These exercise ONLY the public primitive surface — the SSEQ / ASYN
//! consumers that build on it are Phase 2 and are not tested here. The
//! C references for the primitive are epics-base
//! `modules/database/src/ioc/db/callback.c` (delayed re-entry) and
//! `dbNotify.c` (put-notify wait-set / completion).

// RTEMS-EXEC-MODEL-ALLOW(11): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{AsyncDbHandle, PvDatabase};
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::*;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Synthetic record that records how many times `process()` ran (shared
/// `AtomicU32`) and stores a single `VAL` (DBF_LONG). Re-entry via an
/// `AsyncToken` increments the counter exactly once per `process()`.
struct TestAsyncRecord {
    val: i32,
    process_count: Arc<AtomicU32>,
}

impl TestAsyncRecord {
    fn new(process_count: Arc<AtomicU32>) -> Self {
        Self {
            val: 0,
            process_count,
        }
    }
}

static TEST_FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Long, false)];

impl Record for TestAsyncRecord {
    fn record_type(&self) -> &'static str {
        "testasync"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.process_count.fetch_add(1, Ordering::Relaxed);
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(self.val)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Long(v) => {
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        TEST_FIELDS
    }
}

async fn db_with_record(name: &str) -> (PvDatabase, Arc<AtomicU32>) {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicU32::new(0));
    db.add_record(name, Box::new(TestAsyncRecord::new(count.clone())))
        .await
        .unwrap();
    (db, count)
}

/// (1) A fresh `AsyncToken` drives a process re-entry: firing it runs the
/// record's `process()` once (C `callbackRequestDelayed` → `(*process)`).
#[tokio::test]
async fn async_token_fire_drives_process_reentry() {
    let (db, count) = db_with_record("T1").await;
    let baseline = count.load(Ordering::Relaxed);

    let token = db
        .mint_async_token("T1")
        .expect("record exists, mint must succeed");
    assert!(token.is_current(), "freshly-minted token is current");

    token.fire(&db).await.expect("fire re-enters process");

    assert_eq!(
        count.load(Ordering::Relaxed),
        baseline + 1,
        "firing a current token must drive exactly one process() re-entry"
    );
}

/// (2) Cancel via the generation gate makes a stale re-entry a structural
/// no-op: after `cancel_async_reentry`, the outstanding token is no longer
/// current and `fire` re-enters nothing.
#[tokio::test]
async fn async_token_stale_after_cancel_is_noop() {
    let (db, count) = db_with_record("T2").await;

    let token = db.mint_async_token("T2").unwrap();
    let baseline = count.load(Ordering::Relaxed);

    // Cancel (C `callbackCancelDelayed`): advance the generation so the
    // already-minted token is superseded.
    db.cancel_async_reentry("T2");
    assert!(
        !token.is_current(),
        "token must be stale once the generation has advanced"
    );

    token.fire(&db).await.expect("firing a stale token is Ok");
    assert_eq!(
        count.load(Ordering::Relaxed),
        baseline,
        "a stale (cancelled) token must NOT re-enter process()"
    );

    // A fresh mint after cancel is current again and does re-enter.
    let token2 = db.mint_async_token("T2").unwrap();
    assert!(token2.is_current());
    token2.fire(&db).await.unwrap();
    assert_eq!(
        count.load(Ordering::Relaxed),
        baseline + 1,
        "a token minted after cancel must re-enter normally"
    );
}

/// (2b) Minting a newer token supersedes the prior one (C
/// `callbackRequestDelayed` replacing an outstanding delayed callback):
/// the older token's `fire` is a no-op, the newer one's fires.
#[tokio::test]
async fn newer_mint_supersedes_prior_token() {
    let (db, count) = db_with_record("T2B").await;

    let older = db.mint_async_token("T2B").unwrap();
    let newer = db.mint_async_token("T2B").unwrap();
    assert!(!older.is_current(), "older token superseded by newer mint");
    assert!(newer.is_current(), "newer token is current");

    let baseline = count.load(Ordering::Relaxed);
    older.fire(&db).await.unwrap();
    assert_eq!(
        count.load(Ordering::Relaxed),
        baseline,
        "superseded token must not re-enter"
    );
    newer.fire(&db).await.unwrap();
    assert_eq!(
        count.load(Ordering::Relaxed),
        baseline + 1,
        "current token re-enters once"
    );
}

/// (3) `post_fields` applies an async-side field update and posts a monitor
/// event for it (C `db_post_events(precord, &prec->field, DBE_VALUE|DBE_LOG)`),
/// without running a process cycle.
#[tokio::test]
async fn post_fields_applies_and_posts_async_update() {
    let (db, count) = db_with_record("T3").await;
    let baseline = count.load(Ordering::Relaxed);

    // Subscribe to VAL (DBE_VALUE-class) before the async post.
    let mut rx = {
        let rec = db.get_record("T3").unwrap();
        let mut inst = rec.write();
        inst.add_subscriber("VAL", 1, DbFieldType::Long, EventMask::VALUE.bits())
            .expect("VAL subscription accepted")
    };

    let posted = db
        .post_fields("T3", vec![("VAL".to_string(), EpicsValue::Long(7))])
        .expect("post_fields on an existing record");
    assert_eq!(posted, vec!["VAL".to_string()], "VAL reported as posted");

    // Field value applied (read back through the record).
    {
        let rec = db.get_record("T3").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Long(7)),
            "post_fields must write the field value"
        );
    }

    // Monitor event delivered with the new value.
    let event = rx
        .try_recv()
        .expect("post_fields must post a monitor event for the field");
    assert!(
        matches!(event.snapshot.value, EpicsValue::Long(7)),
        "monitor event payload should carry the posted value, got {:?}",
        event.snapshot.value
    );

    assert_eq!(
        count.load(Ordering::Relaxed),
        baseline,
        "post_fields must NOT run a process() cycle"
    );
}

/// (4a) A put-notify wait-set resolves its completion oneshot when the
/// downstream operation completes (C `dbNotifyCompletion` draining the
/// waitList to zero).
#[tokio::test]
async fn put_notify_oneshot_resolves_on_downstream_completion() {
    let (notify, rx) = PvDatabase::new_put_notify();
    assert!(
        !notify.completed(),
        "wait-set armed (pending=1) before the downstream op completes"
    );

    // Downstream operation completes: leave the single armed slot.
    notify.leave();
    assert!(notify.completed(), "wait-set drained to zero");

    rx.await
        .expect("completion oneshot must resolve when the wait-set drains");
}

/// (4b) `reprocess_on_notify` wires a put-notify completion to an
/// `AsyncToken`: when the downstream completes, the waiting record's
/// `process()` is re-entered (SSEQ `WAITn` shape).
#[tokio::test]
async fn reprocess_on_notify_reenters_waiter_on_completion() {
    let (db, count) = db_with_record("T4").await;
    let baseline = count.load(Ordering::Relaxed);

    let token = db.mint_async_token("T4").unwrap();
    let (notify, rx) = PvDatabase::new_put_notify();

    // The waiting record arms the bridge: complete downstream -> re-enter.
    let handle = db.reprocess_on_notify(token, rx);

    // Downstream put-notify completes.
    notify.leave();

    handle.await.expect("wiring task joins cleanly");
    assert_eq!(
        count.load(Ordering::Relaxed),
        baseline + 1,
        "downstream completion must re-enter the waiting record once"
    );
}

/// A cancelled token wired through `reprocess_on_notify` re-enters nothing
/// even when the downstream completes — the wiring inherits the structural
/// no-op from `AsyncToken::fire`.
#[tokio::test]
async fn reprocess_on_notify_with_cancelled_token_is_noop() {
    let (db, count) = db_with_record("T5").await;
    let baseline = count.load(Ordering::Relaxed);

    let token = db.mint_async_token("T5").unwrap();
    let (notify, rx) = PvDatabase::new_put_notify();
    let handle = db.reprocess_on_notify(token, rx);

    // Cancel the outstanding token BEFORE the downstream completes.
    db.cancel_async_reentry("T5");
    notify.leave();

    handle.await.expect("wiring task joins cleanly");
    assert_eq!(
        count.load(Ordering::Relaxed),
        baseline,
        "a cancelled token must not re-enter even when downstream completes"
    );
}

// ---------------------------------------------------------------------------
// Seam tests: the `AsyncDbHandle` delivered by `set_async_context`, and the
// in-band `WriteDbLinkNotify` / `CancelReprocess` ProcessAction arms that the
// `sseq` async machine (and the ASYN out-of-band panel) build on. C
// references: sseqRecord.c (WAITn put-callback wait, ABORT cancel) and
// dbNotify.c / callback.c (put-notify wait-set, delayed re-entry cancel).
// ---------------------------------------------------------------------------

/// Spin-wait (bounded) until `pred` holds — the `WriteDbLinkNotify` re-entry
/// is driven by a detached `reprocess_on_notify` task whose handle the arm
/// drops, so the test cannot join it directly.
async fn poll_until<F: Fn() -> bool>(label: &str, pred: F) {
    for _ in 0..2000 {
        if pred() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for: {label}");
}

/// Captures the `AsyncDbHandle` the framework delivers at `add_record` into a
/// shared sink, so the test can drive out-of-band posts through it.
struct HandleCaptureRecord {
    val: i32,
    sink: Arc<Mutex<Option<AsyncDbHandle>>>,
}

impl Record for HandleCaptureRecord {
    fn record_type(&self) -> &'static str {
        "handlecap"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(self.val)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Long(v) => {
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        TEST_FIELDS
    }
    fn set_async_context(&mut self, _name: String, db: AsyncDbHandle) {
        *self.sink.lock().unwrap() = Some(db);
    }
}

/// Source record for the `WriteDbLinkNotify` arm: on its first `process()`
/// it returns `AsyncPending` with a `WriteDbLinkNotify` to its `LNK` target
/// (the `sseq` `WAITn` shape — wait for the downstream put to complete), and
/// `Complete` on the completion re-entry. Counts every `process()` entry.
struct NotifySourceRecord {
    lnk: &'static str,
    process_count: Arc<AtomicU32>,
}

static NOTIFY_SRC_FIELDS: &[FieldDesc] = &[
    FieldDesc::new("VAL", DbFieldType::Long, false),
    FieldDesc::new("LNK", DbFieldType::String, false),
];

impl Record for NotifySourceRecord {
    fn record_type(&self) -> &'static str {
        "notifysrc"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let pass = self.process_count.fetch_add(1, Ordering::Relaxed);
        if pass == 0 {
            Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPending,
                actions: vec![ProcessAction::WriteDbLinkNotify {
                    link_field: "LNK",
                    value: EpicsValue::Long(42),
                }],
                device_did_compute: false,
            })
        } else {
            Ok(ProcessOutcome::complete())
        }
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(0)),
            "LNK" => Some(EpicsValue::String(self.lnk.into())),
            _ => None,
        }
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        NOTIFY_SRC_FIELDS
    }
}

/// Source record for the `CancelReprocess` arm: every `process()` returns
/// `Complete` carrying a single `CancelReprocess` action (the `sseq` `ABORT`
/// shape — drop the pending DLYn/WAITn re-entry). Counts every entry.
struct AbortSourceRecord {
    process_count: Arc<AtomicU32>,
}

impl Record for AbortSourceRecord {
    fn record_type(&self) -> &'static str {
        "abortsrc"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.process_count.fetch_add(1, Ordering::Relaxed);
        Ok(ProcessOutcome {
            result: RecordProcessResult::Complete,
            actions: vec![ProcessAction::CancelReprocess],
            device_did_compute: false,
        })
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(0)),
            _ => None,
        }
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        TEST_FIELDS
    }
}

/// (S1) `set_async_context` delivers a *working* and *cycle-free* handle: the
/// record receives an `AsyncDbHandle` at registration that drives an
/// out-of-band `post_fields`, and because the handle is a `Weak` reference,
/// dropping the only strong `PvDatabase` reports the handle dead (no
/// ownership-cycle leak) and makes further posts a no-op.
#[tokio::test]
async fn set_async_context_delivers_working_cycle_free_handle() {
    let sink: Arc<Mutex<Option<AsyncDbHandle>>> = Arc::new(Mutex::new(None));
    let db = PvDatabase::new();
    db.add_record(
        "H1",
        Box::new(HandleCaptureRecord {
            val: 0,
            sink: sink.clone(),
        }),
    )
    .await
    .unwrap();

    // The framework called set_async_context at add_record.
    let handle = sink
        .lock()
        .unwrap()
        .clone()
        .expect("set_async_context delivered the handle to the record");
    assert!(
        handle.is_alive(),
        "handle is live while the database is alive"
    );

    // Out-of-band field post through the delivered handle.
    let posted = handle
        .post_fields("H1", vec![("VAL".to_string(), EpicsValue::Long(13))])
        .expect("post through a live handle");
    assert_eq!(posted, vec!["VAL".to_string()], "VAL reported posted");
    {
        let rec = db.get_record("H1").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Long(13)),
            "out-of-band post applied the field value"
        );
    }

    // Cycle-free: the handle holds only a Weak, so dropping the sole strong
    // PvDatabase drops the inner — proving a record stashing the handle does
    // NOT keep the whole database alive.
    drop(db);
    assert!(
        !handle.is_alive(),
        "stashed handle is a Weak ref: dropping the database leaves no strong owner (no cycle)"
    );
    let after = handle
        .post_fields("H1", vec![("VAL".to_string(), EpicsValue::Long(99))])
        .expect("post through a dead handle is Ok");
    assert!(
        after.is_empty(),
        "a post through a handle whose database has dropped is a no-op"
    );
}

/// (S2) The `WriteDbLinkNotify` ProcessAction arm writes the downstream OUT
/// link (DST.VAL) through a put-notify wait-set and re-enters the *source*
/// when the downstream completes — the in-band `sseq` `WAITn` step.
#[tokio::test]
async fn write_db_link_notify_action_drives_downstream_and_reenters_source() {
    let db = PvDatabase::new();
    let src_count = Arc::new(AtomicU32::new(0));
    db.add_record(
        "SRC",
        Box::new(NotifySourceRecord {
            lnk: "DST",
            process_count: src_count.clone(),
        }),
    )
    .await
    .unwrap();
    let dst_count = Arc::new(AtomicU32::new(0));
    db.add_record("DST", Box::new(TestAsyncRecord::new(dst_count)))
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();

    // Pass 1 returned AsyncPending; the arm wrote DST and wired the
    // completion to a re-entry. Wait for pass 2 (completion).
    poll_until("SRC completion re-entry (pass 2)", || {
        src_count.load(Ordering::Relaxed) >= 2
    })
    .await;
    assert_eq!(
        src_count.load(Ordering::Relaxed),
        2,
        "SRC processed exactly twice: initial AsyncPending + completion re-entry"
    );

    let rec = db.get_record("DST").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::Long(42)),
        "WriteDbLinkNotify drove the downstream put (DST.VAL = 42)"
    );
}

/// Source record whose `process()` returns `AsyncPendingNotify` (the
/// intermediate move-start / sub-step notification shape) carrying a single
/// `WriteDbLink` to its `LNK` target. C record support fires its link writes
/// synchronously inside `process()` BEFORE returning with `pact=1`
/// (motorRecord.cc:1495 fires the RLNK readback `dbPutLink` on the move-start
/// pass, before `monitor()` at motorRecord.cc:1507 and the `recGblFwdLink`
/// gate at motorRecord.cc:1509), so the framework must run the requested link
/// writes on the async-pending cycle too.
struct PendingNotifyWriteSource {
    lnk: &'static str,
}

impl Record for PendingNotifyWriteSource {
    fn record_type(&self) -> &'static str {
        "pendnotifysrc"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome {
            result: RecordProcessResult::AsyncPendingNotify(vec![(
                "VAL".to_string(),
                EpicsValue::Long(1),
            )]),
            actions: vec![ProcessAction::WriteDbLink {
                link_field: "LNK",
                value: EpicsValue::Long(42),
            }],
            device_did_compute: false,
        })
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(0)),
            "LNK" => Some(EpicsValue::String(self.lnk.into())),
            _ => None,
        }
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        NOTIFY_SRC_FIELDS
    }
}

/// (S4) A record that returns `AsyncPendingNotify` and emits a `WriteDbLink`
/// must have that link write executed on the pending cycle: the PP target is
/// written AND processed before the record's async work completes. C fires
/// `dbPutLink` inside `process()` on every pass including an async (pact=1)
/// pass (motorRecord.cc:1495), so dropping the action on the pending cycle
/// would lose one of the target's process cycles. Pre-fix the
/// `AsyncPendingNotify` branch dropped `outcome.actions` entirely.
#[tokio::test]
async fn async_pending_notify_runs_write_db_link_on_pending_cycle() {
    let db = PvDatabase::new();

    // PP target: stores VAL (Long) and counts every process() entry.
    let dst_count = Arc::new(AtomicU32::new(0));
    db.add_record(
        "PEND_DST",
        Box::new(TestAsyncRecord::new(dst_count.clone())),
    )
    .await
    .unwrap();

    // Source returns AsyncPendingNotify carrying a WriteDbLink to PEND_DST PP.
    db.add_record(
        "PEND_SRC",
        Box::new(PendingNotifyWriteSource { lnk: "PEND_DST PP" }),
    )
    .await
    .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("PEND_SRC", &mut visited, 0)
        .await
        .unwrap();

    // The link write ran on the async-pending cycle.
    let rec = db.get_record("PEND_DST").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::Long(42)),
        "WriteDbLink on the AsyncPendingNotify cycle must write the PP target's VAL"
    );
    assert_eq!(
        dst_count.load(Ordering::Relaxed),
        1,
        "WriteDbLink on the AsyncPendingNotify cycle must process the PP target once"
    );
}

/// (S3) The `CancelReprocess` ProcessAction arm advances the record's
/// re-entry generation, so an outstanding token (a pending DLYn/WAITn
/// re-entry) becomes a structural no-op — the `sseq` `ABORT` path, with no
/// runtime is-aborted check on the re-entry.
#[tokio::test]
async fn cancel_reprocess_action_supersedes_outstanding_token() {
    let db = PvDatabase::new();
    let count = Arc::new(AtomicU32::new(0));
    db.add_record(
        "CR",
        Box::new(AbortSourceRecord {
            process_count: count.clone(),
        }),
    )
    .await
    .unwrap();

    // An outstanding re-entry token, as a pending DLYn delay / WAITn wait
    // would hold.
    let token = db.mint_async_token("CR").unwrap();
    assert!(token.is_current(), "freshly-minted token is current");

    // Drive a process() that emits CancelReprocess.
    let mut visited = HashSet::new();
    db.process_record_with_links("CR", &mut visited, 0)
        .await
        .unwrap();

    assert!(
        !token.is_current(),
        "CancelReprocess action advanced the generation, superseding the outstanding token"
    );
    let before = count.load(Ordering::Relaxed);
    token.fire(&db).await.expect("firing a stale token is Ok");
    assert_eq!(
        count.load(Ordering::Relaxed),
        before,
        "a token cancelled by CancelReprocess re-enters nothing"
    );
}
