//! R18-95: a put-notify that lands on a PACT record defers the WHOLE put.
//!
//! C `processNotifyCommon` (dbNotify.c:225-232) tests PACT ABOVE the put:
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
//!
//! R19-65 (the boundary set below): "PACT ends" is not one site. The deferral
//! must be consumed by whatever performs the PACT→idle transition, so there is
//! one test per release owner — `complete_async_record_inner`, the
//! ODLY/`ReprocessAfter` continuation, and each SIM/SDLY release inside
//! `check_simulation_mode` (simulated input, simulated output redirect, illegal
//! SIMM) — each with a put parked on that window and each asserting the record
//! is still usable afterwards. Before the fix only the first of those replayed;
//! the others stranded the put forever AND bricked the record for every later
//! put-notify.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::*;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
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
                post_write_fields: Vec::new(),
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

    fn declared_fields(&self) -> &'static [FieldDesc] {
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

    let rec = db.get_record("ASY").unwrap();
    assert!(
        rec.read().is_processing(),
        "the first pass returned AsyncPending: the record must be PACT"
    );

    Fixture { db, count, seen }
}

async fn val(db: &PvDatabase) -> i32 {
    let rec = db.get_record("ASY").unwrap();
    let inst = rec.read();
    match inst.record.get_field("VAL") {
        Some(EpicsValue::Long(v)) => v,
        other => panic!("ASY.VAL: {other:?}"),
    }
}

async fn rpro(db: &PvDatabase) -> bool {
    let rec = db.get_record("ASY").unwrap();
    let inst = rec.read();
    inst.common.rpro != 0
}

/// The deferral itself: while the record is PACT the put-notify writes NOTHING
/// — no value, no RPRO — and its callback has not fired.
#[epics_macros_rs::epics_test]
async fn put_notify_on_a_pact_record_writes_nothing() {
    let f = busy_record().await;

    let mut rx =
        f.db.put_record_field_from_ca("ASY", "VAL", EpicsValue::Long(7))
            .await
            .expect("the put is accepted (deferred), not refused")
            .into_handle()
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
#[epics_macros_rs::epics_test]
async fn deferred_put_is_replayed_and_completes_after_it() {
    let f = busy_record().await;

    let rx =
        f.db.put_record_field_from_ca("ASY", "VAL", EpicsValue::Long(7))
            .await
            .unwrap()
            .into_handle()
            .expect("deferred: receiver handed back");

    // The device round-trip finishes: C `dbNotifyCompletion` runs here.
    f.db.complete_async_record("ASY").await.unwrap();

    epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(5), rx)
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

/// C `dbNotify.c:213-220`: a record that already owns a put-notify puts the
/// next one on `restartList` (`ellSafeAdd`) — it is neither refused nor
/// written. Boundary: queue depth 1 → 2, the first depth the old single
/// `Option` could not hold.
#[epics_macros_rs::epics_test]
async fn a_second_put_notify_onto_a_deferred_record_queues_behind_it() {
    let f = busy_record().await;

    let first =
        f.db.put_record_field_from_ca("ASY", "VAL", EpicsValue::Long(7))
            .await
            .unwrap()
            .into_handle()
            .expect("first put deferred");
    let second =
        f.db.put_record_field_from_ca("ASY", "VAL", EpicsValue::Long(8))
            .await
            .expect("C queues the second put-notify; it never returns S_db_Blocked here")
            .into_handle()
            .expect("second put deferred");

    assert_eq!(
        val(&f.db).await,
        5,
        "both puts are BELOW the ownership test: a busy record keeps its old VAL"
    );

    f.db.complete_async_record("ASY").await.unwrap();

    expect_callback(first, "the first queued put").await;
    expect_callback(second, "the second queued put").await;

    assert_eq!(
        val(&f.db).await,
        8,
        "FIFO: 7 is replayed first, then 8, so 8 is the value left standing"
    );
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![5, 7, 8],
        "each replay processed the record with its own value, oldest first"
    );
}

/// The OWNERSHIP arm, reached by a process-only notify (C `processGetRequest`,
/// the port's `process_record_with_notify` — QSRV's `record[process=true]`).
///
/// C `processNotifyCommon` (dbNotify.c:213-220) tests `precord->ppn` before it
/// looks at the request type, so a process-only notify onto an owned record
/// queues exactly like a put does. The port's restart list could hold only a
/// field-and-value put, so this entry had nowhere to wait and refused with an
/// `ECA_PUTCBINPROG` whose only sender in C is `write_notify_action`
/// (`rsrv/camessage.c:1701` at R7.0.10), a put-callback TIMEOUT, never a
/// second-request refusal. That refusal is now unreachable by construction:
/// the `CaError` variant that carried it has been deleted.
///
/// Boundary: `notify.is_some()` (owned), PACT held by that same notify.
#[epics_macros_rs::epics_test]
async fn process_notify_onto_an_owned_record_queues_instead_of_refusing() {
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

    // First process-notify takes the record: it installs the wait-set and the
    // record goes async under it, so `notify.is_some()` AND PACT.
    let first = db
        .process_record_with_notify("ASY")
        .await
        .expect("the first process-notify takes the record")
        .into_handle()
        .expect("the record went async, so a completion handle comes back");
    {
        let rec = db.get_record("ASY").unwrap();
        let inst = rec.read();
        assert!(inst.has_notify(), "the first notify owns the record");
        assert!(inst.is_processing(), "and holds it PACT");
    }

    // The second one must QUEUE, not refuse.
    let second = db
        .process_record_with_notify("ASY")
        .await
        .expect("C queues a notify arriving on an owned record; it never refuses here")
        .into_handle()
        .expect("a queued notify is async — it completes on the replay");

    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "queueing drives no extra process cycle"
    );

    db.complete_async_record("ASY").await.unwrap();

    expect_callback(first, "the owning process-notify").await;
    expect_callback(second, "the queued process-notify").await;
    assert_eq!(
        count.load(Ordering::Relaxed),
        2,
        "the replay is one further process cycle, not a re-put"
    );
    assert_eq!(
        val(&db).await,
        5,
        "a process-only notify writes nothing on the replay"
    );
}

/// The same ownership arm reached by a DBF LINK-field put.
///
/// Link fields are deliberately kept out of the PACT arm — a bare `sub` with an
/// empty `SNAM` parks PACT=TRUE forever (subRecord.c:119-122) and a link put
/// parked there would never be written. That exclusion used to cover the WHOLE
/// decision, so an owned record's link put fell through to the wait-set install
/// and got refused.
///
/// Boundary: owned (`notify.is_some()`) but NOT PACT — the arm the link-field
/// exclusion must not skip.
#[epics_macros_rs::epics_test]
async fn link_field_put_notify_onto_an_owned_idle_record_queues_instead_of_refusing() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("AI1", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // Own the record without making it PACT: this isolates the ownership arm
    // from the PACT arm, which is exactly what the link-field rule splits.
    let (owner_tx, _owner_rx) = epics_base_rs::runtime::sync::oneshot::channel();
    {
        let rec = db.get_record("AI1").unwrap();
        let mut inst = rec.write();
        inst.install_or_queue_notify(owner_tx)
            .expect("the record is free, so the wait-set installs");
        assert!(!inst.is_processing(), "owned, but idle");
    }

    let queued = db
        .put_record_field_from_ca("AI1", "INP", EpicsValue::String("7".into()))
        .await
        .expect("C queues a link-field put-notify onto an owned record; no refusal arm exists")
        .into_handle()
        .expect("a queued put-notify is async");
    drop(queued);

    let rec = db.get_record("AI1").unwrap();
    assert!(
        !matches!(rec.read().record.get_field("INP"), Some(EpicsValue::String(ref s)) if s == "7"),
        "C holds the queued value UNWRITTEN in the restartList node until the replay"
    );
}

/// A queued put-notify whose client has gone (the receiver dropped) must not
/// wedge the queue: the record still drains, and the put behind it replays.
/// Boundary: the head of the queue has no live client.
#[epics_macros_rs::epics_test]
async fn a_queued_put_whose_client_vanished_does_not_block_the_queue() {
    let f = busy_record().await;

    // The CA connection dies while the put waits: C's `dbNotifyCancel` unlinks
    // it and `restartCheck` promotes the next one regardless.
    drop(
        f.db.put_record_field_from_ca("ASY", "VAL", EpicsValue::Long(7))
            .await
            .unwrap(),
    );
    let second =
        f.db.put_record_field_from_ca("ASY", "VAL", EpicsValue::Long(8))
            .await
            .unwrap()
            .into_handle()
            .expect("second put deferred");

    f.db.complete_async_record("ASY").await.unwrap();

    expect_callback(second, "the put behind an abandoned one").await;
    assert_eq!(val(&f.db).await, 8, "the surviving put's value still lands");
}

/// Deleting the record under a queued put-notify must complete the client, not
/// leak its completion: the sender drops with the record, which wakes the
/// receiver. Boundary: the queue is non-empty when the record disappears.
#[epics_macros_rs::epics_test]
async fn deleting_a_record_releases_its_queued_put_notifies() {
    let f = busy_record().await;

    let first =
        f.db.put_record_field_from_ca("ASY", "VAL", EpicsValue::Long(7))
            .await
            .unwrap()
            .into_handle()
            .expect("first put deferred");
    let second =
        f.db.put_record_field_from_ca("ASY", "VAL", EpicsValue::Long(8))
            .await
            .unwrap()
            .into_handle()
            .expect("second put deferred");

    assert!(f.db.remove_record("ASY").await, "ASY must exist to remove");

    for (rx, which) in [(first, "first"), (second, "second")] {
        let woken = epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .unwrap_or_else(|_| {
                panic!("the {which} queued put-notify leaked: its client is still waiting")
            });
        assert!(
            woken.is_err(),
            "the {which} put never ran, so its completion must arrive as a dropped \
             sender, not a success"
        );
    }
}

/// Await a deferred put's callback, failing the test rather than hanging.
async fn expect_callback(rx: epics_base_rs::runtime::sync::oneshot::Receiver<()>, site: &str) {
    epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(5), rx)
        .await
        .unwrap_or_else(|_| panic!("the put parked on the PACT window {site} released was never replayed — its callback never fired"))
        .expect("the completion sender must not be dropped");
}

/// A record whose first pass holds PACT for an ODLY-style reprocess window (C
/// `calcoutRecord.c:277-282`: `callbackRequestProcessCallbackDelayed` + `pact =
/// TRUE`) and completes on the continuation. This is the release site at
/// `processing.rs`'s `is_continuation` arm — NOT `complete_async_record_inner`.
struct OdlyRecord {
    val: i32,
    armed: bool,
}

impl Record for OdlyRecord {
    fn record_type(&self) -> &'static str {
        "odlyish"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if self.armed {
            self.armed = false;
            // PACT is held across the delay precisely because a ReprocessAfter
            // is scheduled to release it (the framework's construction-time
            // invariant).
            Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPending,
                actions: vec![ProcessAction::ReprocessAfter(
                    std::time::Duration::from_secs(100),
                )],
                device_did_compute: false,
                post_write_fields: Vec::new(),
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

    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }

    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }
}

/// Boundary: PACT released by the ODLY/`ReprocessAfter` continuation, deferral
/// PRESENT. The measured defect — `caput -c ASY.A 7` into a `calcout ODLY=20`
/// window left A=5, the callback never fired, and every later put-callback on
/// that record was refused.
#[epics_macros_rs::epics_test]
async fn deferred_put_is_replayed_when_the_odly_continuation_releases_pact() {
    let db = PvDatabase::new();
    db.add_record(
        "ODL",
        Box::new(OdlyRecord {
            val: 5,
            armed: true,
        }),
    )
    .await
    .unwrap();

    let mut v1 = HashSet::new();
    db.process_record_with_links("ODL", &mut v1, 0)
        .await
        .unwrap();
    let rec = db.get_record("ODL").unwrap();
    assert!(
        rec.read().is_processing(),
        "the ODLY window holds PACT (C keeps the record ACTIVE on the watchdog)"
    );

    let rx = db
        .put_record_field_from_ca("ODL", "VAL", EpicsValue::Long(7))
        .await
        .expect("the put is accepted (deferred)")
        .into_handle()
        .expect("a deferred put-notify hands back a receiver");

    // The delay expires: the continuation ends the cycle and releases PACT.
    let mut v2 = HashSet::new();
    db.process_record_continuation("ODL", &mut v2, 0)
        .await
        .unwrap();

    expect_callback(rx, "the ODLY continuation").await;
    assert!(
        !rec.read().is_processing(),
        "the continuation left the record idle"
    );
    assert_eq!(
        rec.read().record.get_field("VAL"),
        Some(EpicsValue::Long(7)),
        "the deferred value must be written by the replay"
    );

    // And the record is not bricked: the parked slot was consumed, so the next
    // put-callback is accepted rather than refused.
    db.put_record_field_from_ca("ODL", "VAL", EpicsValue::Long(9))
        .await
        .expect("a later put-notify on the now-idle record must be accepted");
    assert_eq!(
        rec.read().record.get_field("VAL"),
        Some(EpicsValue::Long(9))
    );
}

/// `SDLY_AI`: SIMM=YES via SIML, SIOL=42, SDLY≥0 → the fresh cycle holds PACT
/// and defers the SIOL round-trip. PACT is released inside
/// `check_simulation_mode`, on the `Simulated` tail.
async fn sdly_input_record() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SRC", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();
    let mut ai = AiRecord::new(0.0);
    ai.siml = "SW".to_string();
    ai.siol = "SRC".to_string();
    ai.sdly = 100.0; // async; the real timer cannot fire inside the test
    db.add_record("SIMAI", Box::new(ai)).await.unwrap();
    db
}

/// Wait for the queued replay (C's restart callback) to land its value.
async fn await_replayed_value(db: &PvDatabase, name: &str, want: f64) {
    for _ in 0..500 {
        if let Ok(EpicsValue::Double(v)) = db.get_pv(name)
            && (v - want).abs() < 1e-10
        {
            return;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{name}: the deferred put was never replayed — VAL never reached {want}");
}

/// Boundary: PACT released by the SIM/SDLY input continuation, deferral PRESENT.
///
/// The replayed put processes the record, which — still `SIMM=YES, SDLY=100` —
/// legitimately re-enters the SDLY window, so its callback fires on THAT cycle's
/// continuation, not this one. That is the deferral being closed under its own
/// restart, and it is exactly what the strand looked nothing like: before the
/// fix VAL stayed at its pre-put value forever.
#[epics_macros_rs::epics_test]
async fn deferred_put_is_replayed_when_the_sdly_input_continuation_releases_pact() {
    let db = sdly_input_record().await;

    let mut v1 = HashSet::new();
    db.process_record_with_links("SIMAI", &mut v1, 0)
        .await
        .unwrap();
    let rec = db.get_record("SIMAI").unwrap();
    assert!(
        rec.read().is_processing(),
        "SDLY holds PACT across the simulated read"
    );

    let rx = db
        .put_record_field_from_ca("SIMAI", "VAL", EpicsValue::Double(7.0))
        .await
        .expect("the put is accepted (deferred)")
        .into_handle()
        .expect("a deferred put-notify hands back a receiver");

    let mut v2 = HashSet::new();
    db.process_record_continuation("SIMAI", &mut v2, 0)
        .await
        .unwrap();

    // The release replayed the put: the value lands on the record the SIOL read
    // left idle.
    await_replayed_value(&db, "SIMAI", 7.0).await;

    // The replay's own process re-armed SDLY; its continuation completes the
    // put-notify.
    let mut v3 = HashSet::new();
    db.process_record_continuation("SIMAI", &mut v3, 0)
        .await
        .unwrap();
    expect_callback(rx, "the simulated-input continuation").await;
    assert!(!rec.read().is_processing());
}

/// Boundary: PACT released by the SIM/SDLY OUTPUT continuation (the
/// `RedirectOutputToSiol` arm), deferral PRESENT.
#[epics_macros_rs::epics_test]
async fn deferred_put_is_replayed_when_the_sdly_output_continuation_releases_pact() {
    let db = PvDatabase::new();
    db.add_record("SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SINK", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mut ao = AoRecord::new(1.0);
    ao.siml = "SW".to_string();
    ao.siol = "SINK".to_string();
    ao.sdly = 100.0;
    db.add_record("SIMAO", Box::new(ao)).await.unwrap();

    let mut v1 = HashSet::new();
    db.process_record_with_links("SIMAO", &mut v1, 0)
        .await
        .unwrap();
    let rec = db.get_record("SIMAO").unwrap();
    assert!(
        rec.read().is_processing(),
        "SDLY holds PACT across the simulated write"
    );

    let rx = db
        .put_record_field_from_ca("SIMAO", "VAL", EpicsValue::Double(7.0))
        .await
        .expect("the put is accepted (deferred)")
        .into_handle()
        .expect("a deferred put-notify hands back a receiver");

    let mut v2 = HashSet::new();
    db.process_record_continuation("SIMAO", &mut v2, 0)
        .await
        .unwrap();

    await_replayed_value(&db, "SIMAO", 7.0).await;

    let mut v3 = HashSet::new();
    db.process_record_continuation("SIMAO", &mut v3, 0)
        .await
        .unwrap();
    expect_callback(rx, "the simulated-output continuation").await;
    assert!(!rec.read().is_processing());
}

/// Boundary: PACT released by the illegal-SIMM arm of the continuation (C's
/// `switch (prec->simm) default:` — `recGblSetSevr(SOFT_ALARM, INVALID)`, no
/// SIOL round-trip), deferral PRESENT. The cycle ends in an alarm, but the put
/// parked on the window it just released must still be replayed.
#[epics_macros_rs::epics_test]
async fn deferred_put_is_replayed_when_the_illegal_simm_continuation_releases_pact() {
    let db = sdly_input_record().await;

    let mut v1 = HashSet::new();
    db.process_record_with_links("SIMAI", &mut v1, 0)
        .await
        .unwrap();
    let rec = db.get_record("SIMAI").unwrap();
    assert!(rec.read().is_processing());

    let rx = db
        .put_record_field_from_ca("SIMAI", "VAL", EpicsValue::Double(7.0))
        .await
        .expect("the put is accepted (deferred)")
        .into_handle()
        .expect("a deferred put-notify hands back a receiver");

    // SIMM goes out of menu DURING the delay: the continuation's `switch` takes
    // the `default:` arm (C re-reads SIMM only when `!pact`, so the continuation
    // sees the new value).
    {
        let mut inst = rec.write();
        inst.record.put_field("SIMM", EpicsValue::Short(7)).unwrap();
    }

    let mut v2 = HashSet::new();
    db.process_record_continuation("SIMAI", &mut v2, 0)
        .await
        .unwrap();

    // The put parked on that window is replayed even though the cycle ended in
    // SOFT_ALARM with no SIOL round-trip at all.
    await_replayed_value(&db, "SIMAI", 7.0).await;

    // The replay's own process re-resolved SIMM from SIML (C `recGblGetSimm`
    // runs on every `!pact` entry), so the record is back in SIMM=YES and
    // re-armed SDLY; its continuation completes the put-notify.
    let mut v3 = HashSet::new();
    db.process_record_continuation("SIMAI", &mut v3, 0)
        .await
        .unwrap();
    expect_callback(rx, "the illegal-SIMM continuation").await;
    assert!(!rec.read().is_processing());
}

/// The fire-and-forget route is NOT deferred: C `dbPutField` on a PACT record
/// writes the value and raises RPRO (dbAccess.c:1260-1274). Only `dbPutNotify`
/// waits — the gate must not swallow the ordinary put.
#[epics_macros_rs::epics_test]
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
