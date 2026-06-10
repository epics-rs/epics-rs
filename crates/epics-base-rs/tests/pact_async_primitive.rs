//! Unit tests for the PACT async-record primitive
//! (`AsyncToken` + `post_fields` + put-notify wait-set wiring) built in
//! `server/database/processing.rs`.
//!
//! These exercise ONLY the public primitive surface — the SSEQ / ASYN
//! consumers that build on it are Phase 2 and are not tested here. The
//! C references for the primitive are epics-base
//! `modules/database/src/ioc/db/callback.c` (delayed re-entry) and
//! `dbNotify.c` (put-notify wait-set / completion).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
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

static TEST_FIELDS: &[FieldDesc] = &[FieldDesc {
    name: "VAL",
    dbf_type: DbFieldType::Long,
    read_only: false,
}];

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

    fn field_list(&self) -> &'static [FieldDesc] {
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
        .await
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

    let token = db.mint_async_token("T2").await.unwrap();
    let baseline = count.load(Ordering::Relaxed);

    // Cancel (C `callbackCancelDelayed`): advance the generation so the
    // already-minted token is superseded.
    db.cancel_async_reentry("T2").await;
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
    let token2 = db.mint_async_token("T2").await.unwrap();
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

    let older = db.mint_async_token("T2B").await.unwrap();
    let newer = db.mint_async_token("T2B").await.unwrap();
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
        let rec = db.get_record("T3").await.unwrap();
        let mut inst = rec.write().await;
        inst.add_subscriber("VAL", 1, DbFieldType::Long, EventMask::VALUE.bits())
            .expect("VAL subscription accepted")
    };

    let posted = db
        .post_fields("T3", vec![("VAL".to_string(), EpicsValue::Long(7))])
        .await
        .expect("post_fields on an existing record");
    assert_eq!(posted, vec!["VAL".to_string()], "VAL reported as posted");

    // Field value applied (read back through the record).
    {
        let rec = db.get_record("T3").await.unwrap();
        let inst = rec.read().await;
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

    let token = db.mint_async_token("T4").await.unwrap();
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

    let token = db.mint_async_token("T5").await.unwrap();
    let (notify, rx) = PvDatabase::new_put_notify();
    let handle = db.reprocess_on_notify(token, rx);

    // Cancel the outstanding token BEFORE the downstream completes.
    db.cancel_async_reentry("T5").await;
    notify.leave();

    handle.await.expect("wiring task joins cleanly");
    assert_eq!(
        count.load(Ordering::Relaxed),
        baseline,
        "a cancelled token must not re-enter even when downstream completes"
    );
}
