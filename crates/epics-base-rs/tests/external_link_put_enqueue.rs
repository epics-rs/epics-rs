//! External-link put parity with C `dbCa.c`: a record's CA / pva OUT
//! write is STAGED on the database's link-put queue and the record returns —
//! the wire write happens on the queue owner, never inside the record's
//! advisory write gate.
//!
//! C reference, both flavours:
//!
//! * fire-and-forget — `dbCaPutLink` (`dbCa.c:598-602`) delegates to
//!   `dbCaPutLinkCallback` with a NULL callback. That copies the value into
//!   the per-link `pputNative` cell, sets `link_action`, `addAction`s the link
//!   onto `workList`, signals `workListEvent` and RETURNS (`dbCa.c:567-595`).
//!   `ca_array_put` runs later, on the `dbCaTask` (`dbCa.c:1228-1231`); its
//!   failure reaches the operator through `errlogPrintf` (`dbCa.c:1240-1244`),
//!   never through the caller's status.
//! * completion — `dbCaPutLinkCallback` / `dbCaPutAsync` (`dbCa.c:508-513`)
//!   stage identically but carry `putCallback`; the task issues
//!   `ca_array_put_callback` (`dbCa.c:1233-1236`) and `putComplete`
//!   (`dbCa.c:994-1012`) later reports to the writer. Only a writer that
//!   asks for it gets it: a put-notify chain's `dbPutLink` is the plain one.
//!
//! The status C returns from either entry point is the STAGING status: -1 only
//! when the link is down or read-only (`dbCa.c:529-532`).
//!
//! Each test below is one boundary of that queue, not one narrative.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::{LinkPutOp, LinkRef, LinkSet, PutAdmission, PvDatabase};
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

/// An lset whose `put_value` blocks — for writes to `gated` only — until the
/// test hands it a permit, and records each write once that write has
/// COMPLETED. The completion ordering is what the queue owner controls, so
/// recording at completion is what makes the ordering assertions meaningful.
struct GateLset {
    /// Only writes to this PV block; every other PV completes at once, which
    /// is how the per-link independence boundary stays deterministic.
    gated: String,
    completed: Arc<Mutex<Vec<f64>>>,
    /// Signalled after each push to `completed` (`notify_one` keeps the
    /// permit, so a waiter that checks first and waits second misses none).
    settled: Arc<tokio::sync::Notify>,
    entered: tokio::sync::mpsc::UnboundedSender<f64>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl LinkSet for GateLset {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        true
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        self.get_cached_value(link)
    }
    async fn put_value(
        &self,
        link: LinkRef<'_>,
        value: EpicsValue,
        _: LinkPutOp,
    ) -> Result<(), String> {
        let name = link.name;
        let v = value.to_f64().expect("tests only put doubles");
        let _ = self.entered.send(v);
        if name == self.gated.as_bytes() {
            self.release
                .acquire()
                .await
                .expect("semaphore is never closed")
                .forget();
        }
        self.completed.lock().unwrap().push(v);
        self.settled.notify_one();
        Ok(())
    }
}

/// An lset that records every completed write immediately, and answers a
/// configurable put status. `admission` models C's `pca->isConnected &&
/// pca->hasWriteAccess` gate (`dbCa.c:529-532`).
struct RecordingLset {
    writes: Arc<Mutex<Vec<(f64, LinkPutOp)>>>,
    admission: PutAdmission,
    status: Result<(), String>,
}

impl RecordingLset {
    fn connected(writes: &Arc<Mutex<Vec<(f64, LinkPutOp)>>>) -> Self {
        Self {
            writes: Arc::clone(writes),
            admission: PutAdmission::Connected,
            status: Ok(()),
        }
    }
}

#[async_trait::async_trait]
impl LinkSet for RecordingLset {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        self.admission == PutAdmission::Connected
    }
    fn put_admission(&self, _: LinkRef<'_>) -> PutAdmission {
        self.admission
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        self.get_cached_value(link)
    }
    async fn put_value(
        &self,
        _: LinkRef<'_>,
        value: EpicsValue,
        op: LinkPutOp,
    ) -> Result<(), String> {
        self.writes
            .lock()
            .unwrap()
            .push((value.to_f64().expect("tests only put doubles"), op));
        self.status.clone()
    }
}

/// Soft-Channel ao with the given OUT link and VAL.
async fn add_ao(db: &PvDatabase, name: &str, out: &str, val: f64) {
    db.add_record(name, Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv_no_process(format!("{name}.OUT"), EpicsValue::String(out.into()))
        .await
        .unwrap();
    let rec = db.get_record(name).expect("just added");
    let mut inst = rec.write();
    inst.common.udf = 0;
    inst.record
        .put_field("VAL", EpicsValue::Double(val))
        .unwrap();
}

async fn set_val(db: &PvDatabase, name: &str, val: f64) {
    let rec = db.get_record(name).expect("record exists");
    rec.write()
        .record
        .put_field("VAL", EpicsValue::Double(val))
        .unwrap();
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(name, &mut visited)
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
}

async fn alarm_of(db: &PvDatabase, name: &str) -> (u16, AlarmSeverity) {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

/// Boundary — enqueue-and-return. Record processing must complete while the
/// lset's `put_value` is still in flight. C: `dbCaPutLink` returns after
/// `addAction` (`dbCa.c:593-595`); `ca_array_put` runs on the `dbCaTask`.
///
/// This is the property the whole change exists for: the last network
/// suspension inside the record's advisory write gate is gone.
#[epics_macros_rs::epics_test]
async fn plain_put_returns_before_the_wire_write_completes() {
    let completed = Arc::new(Mutex::new(Vec::new()));
    let settled = Arc::new(tokio::sync::Notify::new());
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));

    let db = PvDatabase::new();
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    db.register_link_set(
        "ca",
        Arc::new(GateLset {
            gated: "REMOTE:OUT".to_string(),
            completed: Arc::clone(&completed),
            settled: Arc::clone(&settled),
            entered,
            release: Arc::clone(&release),
        }),
    )
    .await;
    db.process_cp_links_on_connect().await;

    add_ao(&db, "AO_ENQ", "REMOTE:OUT CA", 3.5).await;
    process(&db, "AO_ENQ").await;

    // process() has returned. The write is only now reaching the lset...
    let in_flight = entered_rx.recv().await.expect("the owner ran the write");
    assert_eq!(in_flight, 3.5);
    assert!(
        completed.lock().unwrap().is_empty(),
        "...and has NOT completed — so record processing did not wait for the \
         network write (C dbCa.c:593-595)"
    );
    assert_eq!(
        alarm_of(&db, "AO_ENQ").await,
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
        "a successfully STAGED put raises nothing; C's return status is the \
         staging status (dbCa.c:529-532), not the wire status"
    );

    release.add_permits(1);
    db.sync_external_link_puts().await;
    assert_eq!(
        completed.lock().unwrap().as_slice(),
        [3.5],
        "the dbCaSync barrier (dbCa.c:1126-1129) makes the write observable"
    );
}

/// Boundary — enqueue while disconnected. C refuses BEFORE staging anything
/// (`dbCa.c:529-532`: `if (!pca->isConnected || !pca->hasWriteAccess) return
/// -1`), so `put_value` must never run and the writing record must alarm in
/// this cycle (`dbLink.c:432-446` `setLinkAlarm`).
#[epics_macros_rs::epics_test]
async fn disconnected_link_stages_nothing_and_alarms() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let db = PvDatabase::new();
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    db.register_link_set(
        "ca",
        Arc::new(RecordingLset {
            writes: Arc::clone(&writes),
            admission: PutAdmission::Refused,
            status: Ok(()),
        }),
    )
    .await;
    db.process_cp_links_on_connect().await;

    add_ao(&db, "AO_DOWN", "REMOTE:OUT CA", 1.0).await;
    process(&db, "AO_DOWN").await;
    db.sync_external_link_puts().await;

    assert!(
        writes.lock().unwrap().is_empty(),
        "a put on a down link must not reach put_value at all — C refuses \
         before addAction (dbCa.c:529-532)"
    );
    assert_eq!(
        db.external_link_puts_completed(),
        0,
        "and nothing was staged, so the owner completed nothing"
    );
    assert_eq!(
        alarm_of(&db, "AO_DOWN").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "the refusal is the caller's status, so the record alarms this cycle"
    );
}

/// Boundary — "queue full". C has no queue depth per link: one `pputNative`
/// cell per link, and a put that arrives while one is pending overwrites it
/// and bumps `pca->nNoWrite` (`dbCa.c:582-583`). Latest-wins, never a backlog.
///
/// Here: put 1.0 goes in flight (gated), 2.0 is staged behind it, 3.0
/// overwrites 2.0. Exactly two writes reach the wire, in order, and the
/// middle value is dropped — which is `nNoWrite == 1`.
#[epics_macros_rs::epics_test]
async fn pending_put_is_overwritten_latest_wins() {
    let completed = Arc::new(Mutex::new(Vec::new()));
    let settled = Arc::new(tokio::sync::Notify::new());
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));

    let db = PvDatabase::new();
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    db.register_link_set(
        "ca",
        Arc::new(GateLset {
            gated: "REMOTE:OUT".to_string(),
            completed: Arc::clone(&completed),
            settled: Arc::clone(&settled),
            entered,
            release: Arc::clone(&release),
        }),
    )
    .await;
    db.process_cp_links_on_connect().await;

    add_ao(&db, "AO_COALESCE", "REMOTE:OUT CA", 1.0).await;
    process(&db, "AO_COALESCE").await;
    assert_eq!(
        entered_rx.recv().await,
        Some(1.0),
        "the first write is in flight and blocked"
    );

    set_val(&db, "AO_COALESCE", 2.0).await;
    process(&db, "AO_COALESCE").await;
    set_val(&db, "AO_COALESCE", 3.0).await;
    process(&db, "AO_COALESCE").await;

    assert_eq!(
        db.external_link_puts_coalesced(),
        1,
        "3.0 overwrote the pending 2.0 — C's pca->nNoWrite++ (dbCa.c:582-583)"
    );

    release.add_permits(2);
    db.sync_external_link_puts().await;
    assert_eq!(
        completed.lock().unwrap().as_slice(),
        [1.0, 3.0],
        "the in-flight value still lands, the superseded 2.0 never does, and \
         the surviving values keep their order on the link"
    );
}

/// Boundary — put ordering per link. Successive writes on ONE link, none of
/// them coalescing, must reach the wire in the order the records issued them.
/// C guarantees this by having a single `dbCaTask` service `workList` FIFO
/// (`dbCa.c:1115-1132`); ours by keeping at most one write in flight per link
/// and re-queueing a restaged value at its tail (`link_put_queue::finish`).
#[epics_macros_rs::epics_test]
async fn per_link_put_ordering_is_preserved() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let db = PvDatabase::new();
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    db.register_link_set("ca", Arc::new(RecordingLset::connected(&writes)))
        .await;
    db.process_cp_links_on_connect().await;

    add_ao(&db, "AO_ORDER", "REMOTE:OUT CA", 1.0).await;
    for v in [1.0, 2.0, 3.0, 4.0] {
        set_val(&db, "AO_ORDER", v).await;
        process(&db, "AO_ORDER").await;
        // Barrier per write, so none of them coalesce: this boundary is
        // ordering, the coalescing boundary is its own test above.
        db.sync_external_link_puts().await;
    }

    let seen: Vec<f64> = writes.lock().unwrap().iter().map(|(v, _)| *v).collect();
    assert_eq!(
        seen,
        [1.0, 2.0, 3.0, 4.0],
        "writes on one link must reach put_value in issue order"
    );
    assert_eq!(db.external_link_puts_coalesced(), 0);
}

/// Boundary — the completion flavour does NOT hold the writer either.
///
/// C `dbCaPutLinkCallback` stores `pca->putCallback`, calls `addAction` and
/// RETURNS (`dbCa.c:585-595`), identically to the plain flavour.
/// `putComplete` (`dbCa.c:994-1012`) later fires the callback, which is what
/// settles the writer's wait-set (sseq's `putCallbackCB`).
///
/// So: the put must return while `put_value` is still blocked, and the
/// wait-set must stay unsettled until the completion resolves.
#[epics_macros_rs::epics_test]
async fn async_put_completion_is_reported_to_its_writer() {
    let completed = Arc::new(Mutex::new(Vec::new()));
    let settled = Arc::new(tokio::sync::Notify::new());
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));

    let db = PvDatabase::new();
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    db.register_link_set(
        "ca",
        Arc::new(GateLset {
            gated: "REMOTE:OUT".to_string(),
            completed: Arc::clone(&completed),
            settled: Arc::clone(&settled),
            entered,
            release: Arc::clone(&release),
        }),
    )
    .await;
    db.process_cp_links_on_connect().await;

    add_ao(&db, "AO_NOTIFY", "REMOTE:OUT CA", 0.0).await;
    let unprocessed = alarm_of(&db, "AO_NOTIFY").await;
    let mut notify_rx = db
        .put_link_notify("AO_NOTIFY", "OUT", EpicsValue::Double(9.0))
        .await
        .expect("AO_NOTIFY is loaded");

    // The writer is back with the wire write still in flight.
    assert_eq!(
        entered_rx.recv().await,
        Some(9.0),
        "the completion-flavour write reached put_value on the queue owner"
    );
    assert!(
        completed.lock().unwrap().is_empty(),
        "the wire write has NOT completed, yet the put already returned — \
         C `dbCaPutLinkCallback` stages and returns (dbCa.c:585-595)"
    );
    assert!(
        notify_rx.try_recv().is_err(),
        "the writer's wait-set is still outstanding until putComplete \
         (dbCa.c:1056-1074), which is what the external put joined it for"
    );

    release.add_permits(1);
    db.sync_external_link_puts().await;
    assert_eq!(completed.lock().unwrap().as_slice(), [9.0]);
    notify_rx
        .await
        .expect("the completion settles the writer's wait-set");
    assert_eq!(
        alarm_of(&db, "AO_NOTIFY").await,
        unprocessed,
        "a completion that succeeded raises nothing"
    );
}

/// Boundary twin — a completion flavour whose WIRE write fails does NOT alarm
/// the source record; it settles the writer's wait-set and goes to errlog.
///
/// C's `putComplete` reads `pca->putCallback` and calls it, discarding
/// `arg.status` entirely (`dbCa.c:994-1012`) — the callback runs whether the
/// remote accepted the put or not. The wire status reaches the operator on the
/// task instead (`errlogPrintf`, `dbCa.c:1238-1244`, below the
/// `CA_PUT`/`CA_PUT_CALLBACK` fork so it covers both flavours). The record's
/// own alarm comes from the STAGING gate (`dbCa.c:529-532` →
/// `dbLink.c:432-446` `setLinkAlarm`), which this write passed.
///
/// If the queue owner ever dropped a completion instead of resolving it, this
/// test would hang rather than fail — which is why `PutCompletion` resolves on
/// `Drop` as well as on the success path.
#[epics_macros_rs::epics_test]
async fn async_put_wire_failure_settles_the_writer_without_alarming() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let db = PvDatabase::new();
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    db.register_link_set(
        "ca",
        Arc::new(RecordingLset {
            writes: Arc::clone(&writes),
            admission: PutAdmission::Connected,
            status: Err("remote refused the put".to_string()),
        }),
    )
    .await;
    db.process_cp_links_on_connect().await;

    add_ao(&db, "AO_NOTIFY_ERR", "REMOTE:OUT CA", 0.0).await;
    let unprocessed = alarm_of(&db, "AO_NOTIFY_ERR").await;
    let notify_rx = db
        .put_link_notify("AO_NOTIFY_ERR", "OUT", EpicsValue::Double(4.0))
        .await
        .expect("AO_NOTIFY_ERR is loaded");
    db.sync_external_link_puts().await;

    assert_eq!(
        writes.lock().unwrap().len(),
        1,
        "the write was attempted on the wire"
    );
    notify_rx.await.expect(
        "a failed wire write still settles the writer — C putComplete \
                 discards arg.status and calls the callback regardless",
    );
    assert_eq!(
        alarm_of(&db, "AO_NOTIFY_ERR").await,
        unprocessed,
        "the record's alarm comes from the staging gate, not the wire status \
         (dbCa.c:529-532 / dbLink.c:432-446)"
    );
}

/// Boundary — distinct links are independent. One slow target must not block
/// writes to every other link. C gets this free (`ca_array_put` only queues
/// into libca's send buffer); ours needs the owner to keep one in-flight write
/// PER LINK rather than one globally.
#[epics_macros_rs::epics_test]
async fn a_blocked_link_does_not_stall_other_links() {
    let completed = Arc::new(Mutex::new(Vec::new()));
    let settled = Arc::new(tokio::sync::Notify::new());
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));

    let db = PvDatabase::new();
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    db.register_link_set(
        "ca",
        Arc::new(GateLset {
            // Only SLOW:OUT blocks; FAST:OUT completes as soon as the owner
            // hands it over.
            gated: "SLOW:OUT".to_string(),
            completed: Arc::clone(&completed),
            settled: Arc::clone(&settled),
            entered,
            release: Arc::clone(&release),
        }),
    )
    .await;
    db.process_cp_links_on_connect().await;

    add_ao(&db, "AO_SLOW", "SLOW:OUT CA", 1.0).await;
    add_ao(&db, "AO_FAST", "FAST:OUT CA", 2.0).await;

    process(&db, "AO_SLOW").await;
    assert_eq!(entered_rx.recv().await, Some(1.0), "SLOW:OUT is blocked");

    process(&db, "AO_FAST").await;
    assert_eq!(
        entered_rx.recv().await,
        Some(2.0),
        "FAST:OUT reached the lset while SLOW:OUT was still in flight"
    );
    let fast_done = async {
        while !completed.lock().unwrap().contains(&2.0) {
            settled.notified().await;
        }
    };
    if epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(30), fast_done)
        .await
        .is_err()
    {
        panic!(
            "FAST:OUT not completed 30 s after it reached the lset; completed = {:?}",
            completed.lock().unwrap()
        );
    }
    let mid = completed.lock().unwrap().clone();
    assert_eq!(
        mid,
        [2.0],
        "FAST:OUT completed while SLOW:OUT was still blocked — the owner keeps \
         one in-flight write PER LINK, not one globally"
    );

    release.add_permits(1);
    db.sync_external_link_puts().await;
    let mut done = completed.lock().unwrap().clone();
    done.sort_by(f64::total_cmp);
    assert_eq!(done, [1.0, 2.0]);
}
