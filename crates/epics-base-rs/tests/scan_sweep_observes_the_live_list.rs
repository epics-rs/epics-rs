//! A scan sweep observes its list; it does not own a copy of it.
//!
//! C reference. `scanList` (`dbScan.c:998-1051`) opens with
//!
//!     /* When reading this code remember that the call to dbProcess can
//!      * result in the SCAN field being changed in an arbitrary number of
//!      * records. */
//!
//! and honours it: the cursor is re-read as `ellNext(&pse->node)` under
//! `psl->lock` on every step, and `psl->modified` drives a prev/next repair
//! walk (`:1030-1044`) for the case where the cursor's own element left the
//! list. `deleteFromList` (`dbScan.c:1097-1124`) unlinks the node and sets
//! `modified`, and an `SPC_SCAN` put reaches it through `scanDelete` on pass 0
//! (`dbAccess.c:127-131`), so a record whose SCAN is written earlier in the
//! same sweep is gone before the cursor arrives.
//!
//! The port's list is an ordered set rather than a linked list, so its cursor
//! is a key and "my element left the list" has one answer — the next key
//! strictly greater — instead of C's three. These cases pin that at the four
//! positions a mid-sweep removal can take relative to the cursor.
//!
//! `post_event` is the driver here because it is the public one; C reaches the
//! same `scanList` from `eventCallback` (`dbScan.c:459-465`) as it does from
//! `periodicTask`, and this port likewise has one sweep for both.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record, ScanType};
use epics_base_rs::types::EpicsValue;

/// What a probe does to the database while it is being processed.
type ScanAction = Box<dyn Fn(&PvDatabase) + Send + Sync>;

/// Counts its own `process()` calls, and optionally rewrites some other
/// record's SCAN while it is being processed — the port's stand-in for the
/// brief's `calcout` whose `OUT` is `B.SCAN`.
struct ProbeRecord {
    processes: Arc<AtomicUsize>,
    db: Arc<PvDatabase>,
    action: Option<ScanAction>,
}

impl Record for ProbeRecord {
    fn record_type(&self) -> &'static str {
        "scan_cursor_probe"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.processes.fetch_add(1, Ordering::SeqCst);
        if let Some(action) = &self.action {
            action(&self.db);
        }
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, _name: &str) -> Option<EpicsValue> {
        None
    }
    fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
        Err(epics_base_rs::error::CaError::FieldNotFound(name.into()))
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

/// Move `name` onto `scan`, the way a put to the SCAN field does: write the
/// field, then reindex. C `dbAccess.c:127-131` → `scanDelete`/`scanAdd`.
fn set_scan(db: &PvDatabase, name: &str, scan: ScanType) {
    let old = {
        let rec = db.get_record(name).expect("record present");
        let mut inst = rec.write();
        let old = inst.common.scan;
        inst.common.scan = scan;
        old
    };
    let phas = db
        .get_record(name)
        .expect("record present")
        .read()
        .common
        .phas;
    db.update_scan_index(name, old, scan, phas, phas);
}

/// Add an Event-scanned probe at `phas`, returning its process counter.
async fn probe(
    db: &Arc<PvDatabase>,
    name: &str,
    phas: i16,
    action: Option<ScanAction>,
) -> Arc<AtomicUsize> {
    let processes = Arc::new(AtomicUsize::new(0));
    db.add_record(
        name,
        Box::new(ProbeRecord {
            processes: processes.clone(),
            db: db.clone(),
            action,
        }),
    )
    .await
    .unwrap();
    db.get_record(name).unwrap().write().common.phas = phas;
    set_scan(db, name, ScanType::Event);
    processes
}

/// BOUNDARY: removed AFTER the cursor. `M` (PHAS 0) drops `V` (PHAS 1) off the
/// Event list before the sweep reaches it. C's `deleteFromList` has already
/// unlinked `V` when the cursor steps, so `V` does not process — the brief's
/// `calcout`/`calc` observable, where C leaves `B.VAL = 0`.
#[epics_macros_rs::epics_test]
async fn a_record_removed_ahead_of_the_cursor_does_not_process() {
    let db = Arc::new(PvDatabase::new());
    let mutator = probe(
        &db,
        "M",
        0,
        Some(Box::new(|db: &PvDatabase| {
            set_scan(db, "V", ScanType::Passive);
        })),
    )
    .await;
    let victim = probe(&db, "V", 1, None).await;

    db.post_event().await;

    assert_eq!(mutator.load(Ordering::SeqCst), 1, "the mutator itself runs");
    assert_eq!(
        victim.load(Ordering::SeqCst),
        0,
        "a record taken off the list mid-sweep must not be processed by that sweep"
    );
}

/// BOUNDARY: moved to a DIFFERENT list. `V` leaves the Event list for a
/// periodic one rather than for Passive. C's `scanDelete`+`scanAdd` pair puts
/// it on another `scan_list` entirely, so this sweep must not reach it either
/// — the case a "re-check SCAN is still Event" guard over a snapshot would
/// also have caught, and the case a "re-check it is still non-Passive" guard
/// would not.
#[epics_macros_rs::epics_test]
async fn a_record_moved_to_another_list_does_not_process_on_this_one() {
    let db = Arc::new(PvDatabase::new());
    let mutator = probe(
        &db,
        "M",
        0,
        Some(Box::new(|db: &PvDatabase| {
            set_scan(db, "V", ScanType::Sec1);
        })),
    )
    .await;
    let victim = probe(&db, "V", 1, None).await;

    db.post_event().await;

    assert_eq!(mutator.load(Ordering::SeqCst), 1, "the mutator itself runs");
    assert_eq!(
        victim.load(Ordering::SeqCst),
        0,
        "a record moved to another scan list must not be processed by this list's sweep"
    );
}

/// BOUNDARY: removed BEHIND the cursor. `V` (PHAS 0) has already processed
/// when `M` (PHAS 1) drops it; the removal is behind the cursor and changes
/// nothing about this sweep, which must still reach the record after `M`.
#[epics_macros_rs::epics_test]
async fn a_record_removed_behind_the_cursor_still_processed_once() {
    let db = Arc::new(PvDatabase::new());
    let victim = probe(&db, "V", 0, None).await;
    let mutator = probe(
        &db,
        "M",
        1,
        Some(Box::new(|db: &PvDatabase| {
            set_scan(db, "V", ScanType::Passive);
        })),
    )
    .await;
    let tail = probe(&db, "T", 2, None).await;

    db.post_event().await;

    assert_eq!(
        victim.load(Ordering::SeqCst),
        1,
        "a record already visited is unaffected by its later removal"
    );
    assert_eq!(mutator.load(Ordering::SeqCst), 1, "the mutator itself runs");
    assert_eq!(
        tail.load(Ordering::SeqCst),
        1,
        "the sweep continues past a removal behind the cursor"
    );
}

/// BOUNDARY: removed AT the cursor — the cursor's own element leaves the list
/// while the cursor stands on it. C's `psl->modified` repair walk exists for
/// exactly this (`dbScan.c:1030-1044`): `pse` is gone, so C resumes from its
/// predecessor's successor.
///
/// The removal comes from `M`'s FLNK target rather than from `M` itself
/// because a record cannot rewrite its own SCAN inside its own cycle here —
/// `process_record_with_links_body` holds the record's write guard across
/// `record.process()`, where C's `dbScanLock` is a recursive `epicsMutex`. The
/// FLNK tail runs after that guard drops and before the cursor's next step, so
/// it lands in the same window a CA put or `dbpf` from another thread would.
#[epics_macros_rs::epics_test]
async fn a_record_removed_at_the_cursor_does_not_stall_the_sweep() {
    let db = Arc::new(PvDatabase::new());
    let head = probe(&db, "H", 0, None).await;
    let mutator = probe(&db, "M", 1, None).await;
    let tail = probe(&db, "T", 2, None).await;

    // `K` is Passive: it exists only as M's FLNK tail.
    let knife = Arc::new(AtomicUsize::new(0));
    db.add_record(
        "K",
        Box::new(ProbeRecord {
            processes: knife.clone(),
            db: db.clone(),
            action: Some(Box::new(|db: &PvDatabase| {
                set_scan(db, "M", ScanType::Passive);
            })),
        }),
    )
    .await
    .unwrap();
    db.get_record("M")
        .unwrap()
        .write()
        .put_common_field("FLNK", EpicsValue::String("K".into()))
        .unwrap();

    db.post_event().await;

    assert_eq!(head.load(Ordering::SeqCst), 1, "the record ahead runs once");
    assert_eq!(
        mutator.load(Ordering::SeqCst),
        1,
        "the cursor record runs once"
    );
    assert_eq!(knife.load(Ordering::SeqCst), 1, "the FLNK tail ran");
    assert_eq!(
        tail.load(Ordering::SeqCst),
        1,
        "the sweep resumes past a cursor whose own element left the list"
    );
}
