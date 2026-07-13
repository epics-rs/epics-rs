//! R18-95: a put-notify that lands on a PACT record defers the WHOLE put.
//!
//! C `processNotifyCommon` (dbNotify.c:225-231) tests PACT ABOVE the put:
//!
//! ```c
//! if (precord->ppn && pnotify->paddr->precord != precord) { ... }
//! if (precord->pact) {                      /* busy: write NOTHING */
//!     pnotify->state = notifyRestartCallbackRequested;
//!     ellAdd(&precord->ppnr->restartList, &pnotify->restartNode);
//!     return;
//! }
//! ... putCallback(...)                      /* the value is written HERE */
//! ```
//!
//! so the value is not written, RPRO is not raised, and the callback is not
//! joined to the in-flight cycle's wait-set. `dbNotifyCompletion` replays the
//! whole put when the record goes idle.
//!
//! Oracle — softIoc 7.0.10.1-DEV, `ASY` a calcout with `ODLY=4`, `A=5`,
//! driven by `caput -c ASY.A 7` one second into a running 4 s delay:
//!
//! ```text
//! t=1.0s   caput -c ASY.A 7          (blocks)
//! t=2.0s   ASY.A=5  ASY.PACT=1  ASY.RPRO=0   <- nothing written, no RPRO
//! t=4.0s   ASY.A=7                            <- the put is replayed
//! t=6.9s   caput returns; ASY.A=7  ASY.VAL=7  <- callback AFTER the restart
//! ```
//!
//! The port wrote the value immediately, set RPRO, and joined the running
//! cycle's wait-set — so the callback fired at the end of a cycle that never
//! saw the value, breaking "callback ⟹ your value was processed".

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::*;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// A record whose first `process()` goes async (the device round-trip C holds
/// PACT for) and whose later passes complete synchronously. It snapshots `VAL`
/// on every pass, so the test can say exactly which value each cycle saw.
struct AsyncOnceRecord {
    val: i32,
    pending: bool,
    process_count: Arc<AtomicU32>,
    seen_by_process: Arc<std::sync::Mutex<Vec<i32>>>,
}

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Long, false)];

impl Record for AsyncOnceRecord {
    fn record_type(&self) -> &'static str {
        "asynconce"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.process_count.fetch_add(1, Ordering::Relaxed);
        self.seen_by_process.lock().unwrap().push(self.val);
        if self.pending {
            self.pending = false;
            Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPending,
                actions: Vec::new(),
                device_did_compute: false,
            })
        } else {
            Ok(ProcessOutcome::complete())
        }
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
        FIELDS
    }

    /// VAL is a `pp(TRUE)` field: a put to it processes the Passive record.
    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }
}

struct Fixture {
    db: Arc<PvDatabase>,
    count: Arc<AtomicU32>,
    seen: Arc<std::sync::Mutex<Vec<i32>>>,
}

/// `ASY` PACT=1, mid device round-trip, exactly as the oracle's ODLY window.
async fn busy_record() -> Fixture {
    let db = Arc::new(PvDatabase::new());
    let count = Arc::new(AtomicU32::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    db.add_record(
        "ASY",
        Box::new(AsyncOnceRecord {
            val: 5,
            pending: true,
            process_count: count.clone(),
            seen_by_process: seen.clone(),
        }),
    )
    .await
    .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("ASY", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("ASY").await.unwrap();
    assert!(
        rec.read().await.is_processing(),
        "the first pass returned AsyncPending: the record must be PACT"
    );

    Fixture { db, count, seen }
}

async fn val(db: &PvDatabase) -> i32 {
    let rec = db.get_record("ASY").await.unwrap();
    let inst = rec.read().await;
    match inst.record.get_field("VAL") {
        Some(EpicsValue::Long(v)) => v,
        other => panic!("ASY.VAL: {other:?}"),
    }
}

async fn rpro(db: &PvDatabase) -> bool {
    let rec = db.get_record("ASY").await.unwrap();
    let inst = rec.read().await;
    inst.common.rpro
}

/// The deferral itself: while the record is PACT the put-notify writes NOTHING
/// — no value, no RPRO — and its callback has not fired.
#[tokio::test]
async fn put_notify_on_a_pact_record_writes_nothing() {
    let f = busy_record().await;

    let mut rx =
        f.db.put_record_field_from_ca("ASY", "VAL", EpicsValue::Long(7))
            .await
            .expect("the put is accepted (deferred), not refused")
            .expect("a deferred put-notify hands back a receiver to await");

    assert_eq!(
        val(&f.db).await,
        5,
        "C writes the value in putCallback, BELOW the PACT test: a busy record keeps its old VAL"
    );
    assert!(
        !rpro(&f.db).await,
        "dbNotify's PACT arm sets no RPRO — the restart list carries the put, not a reprocess flag"
    );
    assert!(
        rx.try_recv().is_err(),
        "the callback must not fire on a cycle that never saw the value"
    );
    assert_eq!(
        f.count.load(Ordering::Relaxed),
        1,
        "and the deferral drives no extra process cycle"
    );
}

/// The restart: when the record's async work completes, `dbNotifyCompletion`
/// replays the whole put — value, process, and only then the callback.
#[tokio::test]
async fn deferred_put_is_replayed_and_completes_after_it() {
    let f = busy_record().await;

    let rx =
        f.db.put_record_field_from_ca("ASY", "VAL", EpicsValue::Long(7))
            .await
            .unwrap()
            .expect("deferred: receiver handed back");

    // The device round-trip finishes: C `dbNotifyCompletion` runs here.
    f.db.complete_async_record("ASY").await.unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(5), rx)
        .await
        .expect("the put-notify callback must fire after the restarted put")
        .expect("the completion sender must not be dropped");

    assert_eq!(
        val(&f.db).await,
        7,
        "the restart wrote the deferred value into the now-idle record"
    );

    // The invariant the whole finding is about: the callback fired only after a
    // process cycle that SAW the value. The in-flight cycle saw 5; the restart's
    // cycle saw 7.
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(
        seen.last(),
        Some(&7),
        "the last process cycle before the callback must have seen the put's value, got {seen:?}"
    );
    assert!(
        !rpro(&f.db).await,
        "the restart is a put, not an RPRO reprocess: RPRO stays clear throughout"
    );
}

/// C `dbNotify.c:213-217`: a record already owning a put-notify restart refuses
/// the next one (`S_db_Blocked` → ECA_PUTCBINPROG), rather than silently
/// dropping the first caller's sender.
#[tokio::test]
async fn a_second_put_notify_onto_a_deferred_record_is_refused() {
    let f = busy_record().await;

    let _rx =
        f.db.put_record_field_from_ca("ASY", "VAL", EpicsValue::Long(7))
            .await
            .unwrap()
            .expect("first put deferred");

    let err =
        f.db.put_record_field_from_ca("ASY", "VAL", EpicsValue::Long(8))
            .await
            .expect_err("a second put-notify onto the same deferred record is refused");
    assert!(
        matches!(err, CaError::PutCallbackInProgress(_)),
        "expected PutCallbackInProgress (C S_db_Blocked), got {err:?}"
    );
}

/// The fire-and-forget route is NOT deferred: C `dbPutField` on a PACT record
/// writes the value and raises RPRO (dbAccess.c:1263-1277). Only `dbPutNotify`
/// waits — the gate must not swallow the ordinary put.
#[tokio::test]
async fn a_fire_and_forget_put_on_a_pact_record_still_writes_and_sets_rpro() {
    let f = busy_record().await;

    f.db.put_record_field_from_ca_no_notify("ASY", "VAL", EpicsValue::Long(7))
        .await
        .unwrap();

    assert_eq!(
        val(&f.db).await,
        7,
        "dbPutField writes into a busy record — the deferral is the notify route's alone"
    );
    assert!(
        rpro(&f.db).await,
        "and marks it for the reprocess that carries the value to the device"
    );
}
