//! A CP/CPP holder is processed by a monitor POST from its source, never by
//! the bare fact that the source processed.
//!
//! C reference. `dbInitLink` tests the `CA`/`CP`/`CPP` modifier BEFORE
//! locality and short-circuits `dbDbInitLink` when one is present
//! (`dbLink.c:118-122`), so every CP link — including one naming a record in
//! this very IOC — is a CA link; the `isLocal` at `dbLink.c:128` is computed
//! only to choose the init-callback hint. That CA subscription is taken with
//! `DBE_VALUE | DBE_ALARM` (`dbCa.c:1225-1229` → `cadef.h:2010-2011`), and
//! `CA_DBPROCESS` is added only from its `eventCallback` (`dbCa.c:955-963`),
//! run as a bare `db_process` by the worker (`dbCa.c:1249-1257`). A source
//! cycle that publishes nothing therefore leaves the holder alone.
//!
//! The port keeps a local CP target in the `Db` shape (the `ca` link set lives
//! in `epics-ca-rs`, which `epics-base-rs` does not depend on) and restores
//! the C rule at the trigger instead. These cases pin that rule at its
//! boundaries: no post, a `DBE_VALUE` post, a post suppressed by `MDEL`, and
//! an alarm-only transition.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record, ScanType};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::{DbfCode, EpicsValue};

/// A Passive holder that counts its own `process()` calls — the observable
/// the CP trigger decides.
struct CountingRecord {
    val: f64,
    processes: Arc<AtomicUsize>,
}

impl Record for CountingRecord {
    fn record_type(&self) -> &'static str {
        "cp_trigger_count_test"
    }
    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.processes.fetch_add(1, Ordering::SeqCst);
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
                self.val = value.to_f64().unwrap_or(self.val);
                Ok(())
            }
            _ => Err(epics_base_rs::error::CaError::FieldNotFound(name.into())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        COUNTING_FIELDS
    }
}

/// The holder's `.dbd`, such as it is: a synthetic type still has to DECLARE
/// its `INP`, because "which fields on a record are links" is answered from
/// the declaration and nowhere else. `FieldDesc::new` cannot spell it —
/// it derives `declared_dbf` from the SERVED type, and all three link classes
/// serve as `DBF_STRING` — so the class is written out.
const fn inlink(name: &'static str) -> FieldDesc {
    FieldDesc {
        declared_dbf: DbfCode::Inlink,
        ..FieldDesc::new(name, epics_base_rs::types::DbFieldType::String, false)
    }
}

static COUNTING_FIELDS: &[FieldDesc] = &[
    inlink("INP"),
    FieldDesc::new("VAL", epics_base_rs::types::DbFieldType::Double, false),
];

/// `SRC` (ai, `MDEL = mdel`) with a Passive holder taking `INP = "SRC <mod>"`.
/// Returns the holder's process counter.
async fn cp_pair(db: &PvDatabase, modifier: &str, mdel: f64) -> Arc<AtomicUsize> {
    // What `register_record_type` does for a real type: publish the table to
    // the by-name registry `dbf_link_class` reads. `add_record` takes an
    // instance and no factory, so nothing else does it here.
    epics_base_rs::server::record::register_declared_fields(
        "cp_trigger_count_test",
        COUNTING_FIELDS,
    );
    let processes = Arc::new(AtomicUsize::new(0));
    let mut src = AiRecord::new(0.0);
    src.mdel = mdel;
    db.add_record("SRC", Box::new(src)).await.unwrap();
    db.add_record(
        "HOLD",
        Box::new(CountingRecord {
            val: 0.0,
            processes: processes.clone(),
        }),
    )
    .await
    .unwrap();
    {
        let rec = db.get_record("HOLD").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String(format!("SRC {modifier}").into()))
            .unwrap();
    }
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    processes
}

/// Drive `SRC` to `val` without going through a put that would process it
/// itself, then process it — one source cycle, whose posts decide the CP
/// dispatch.
async fn drive(db: &PvDatabase, val: f64) {
    {
        let rec = db.get_record("SRC").unwrap();
        let mut inst = rec.write();
        inst.record
            .put_field("VAL", EpicsValue::Double(val))
            .unwrap();
    }
    let mut visited = std::collections::HashSet::new();
    db.process_record_with_links("SRC", &mut visited, 0)
        .await
        .unwrap();
}

/// Settle `SRC` past its init alarm transition so later cycles publish
/// nothing but what the test drives. Returns the holder's count afterwards.
async fn settle(db: &PvDatabase, processes: &AtomicUsize) -> usize {
    drive(db, 0.0).await;
    processes.load(Ordering::SeqCst)
}

/// BOUNDARY: source processes, publishes nothing. `MDEL = 0` and `0 > 0` is
/// false, so an unchanged value posts no `DBE_VALUE`; the alarm has already
/// settled, so no `DBE_ALARM` either. The holder must not move.
///
/// This is the brief's `SRC`/`HOLD` climb: pre-fix the holder was processed
/// from the tail of every source cycle, so a 1 Hz `SRC` walked `HOLD` up by
/// one per second where C leaves it fixed.
#[epics_macros_rs::epics_test]
async fn unchanged_source_cycle_does_not_process_the_cp_holder() {
    let db = PvDatabase::new();
    let processes = cp_pair(&db, "CP", 0.0).await;
    let settled = settle(&db, &processes).await;

    for _ in 0..5 {
        drive(&db, 0.0).await;
    }

    assert_eq!(
        processes.load(Ordering::SeqCst),
        settled,
        "a source cycle that posts nothing must not process a CP holder"
    );
}

/// BOUNDARY: the value moves past `MDEL`, so `monitor()` posts `DBE_VALUE`.
/// One post, one dispatch.
#[epics_macros_rs::epics_test]
async fn a_value_post_processes_the_cp_holder_once() {
    let db = PvDatabase::new();
    let processes = cp_pair(&db, "CP", 0.0).await;
    let settled = settle(&db, &processes).await;

    drive(&db, 1.0).await;

    assert_eq!(
        processes.load(Ordering::SeqCst),
        settled + 1,
        "a DBE_VALUE post must process the CP holder exactly once"
    );
}

/// BOUNDARY: the value moves but stays INSIDE `MDEL`, so the deadband
/// suppresses the `DBE_VALUE` post. C's subscription sees nothing, so the
/// holder must not process — a changed VAL is not the trigger, a post is.
#[epics_macros_rs::epics_test]
async fn a_move_inside_mdel_does_not_process_the_cp_holder() {
    let db = PvDatabase::new();
    let processes = cp_pair(&db, "CP", 10.0).await;
    let settled = settle(&db, &processes).await;

    drive(&db, 1.0).await;

    assert_eq!(
        processes.load(Ordering::SeqCst),
        settled,
        "a move inside MDEL posts nothing, so no CP dispatch"
    );
}

/// BOUNDARY: alarm movement with no value movement. `recGblResetAlarms`
/// returns `val_mask = DBE_ALARM`, which rides on the VAL post even when the
/// deadband suppressed `DBE_VALUE` — the second half of C's
/// `DBE_VALUE | DBE_ALARM` subscription. The very first source cycle is
/// exactly this case (UDF/INVALID → NO_ALARM), so it must dispatch.
#[epics_macros_rs::epics_test]
async fn an_alarm_only_transition_processes_the_cp_holder() {
    let db = PvDatabase::new();
    let processes = cp_pair(&db, "CP", 10.0).await;

    drive(&db, 0.0).await;

    assert!(
        processes.load(Ordering::SeqCst) >= 1,
        "the init alarm transition posts DBE_ALARM and must dispatch"
    );
}

/// BOUNDARY: the CPP passive gate is unchanged by the post gate. A CPP
/// holder whose SCAN is not Passive is skipped even when the source posts
/// (C `dbCa.c:958-962`: `pvlOptCPP` adds `CA_DBPROCESS` only for
/// `precord->scan == 0`).
#[epics_macros_rs::epics_test]
async fn a_cpp_holder_that_is_not_passive_is_still_skipped_on_a_post() {
    let db = PvDatabase::new();
    let processes = cp_pair(&db, "CPP", 0.0).await;
    {
        let rec = db.get_record("HOLD").unwrap();
        rec.write().common.scan = ScanType::SEC1;
    }
    let settled = settle(&db, &processes).await;

    drive(&db, 1.0).await;

    assert_eq!(
        processes.load(Ordering::SeqCst),
        settled,
        "a non-Passive CPP holder is reached by its own scan, not by CP dispatch"
    );
}

/// BOUNDARY: a Passive CPP holder does dispatch on a post — the same gate,
/// the other side of the CPP branch.
#[epics_macros_rs::epics_test]
async fn a_passive_cpp_holder_dispatches_on_a_post() {
    let db = PvDatabase::new();
    let processes = cp_pair(&db, "CPP", 0.0).await;
    let settled = settle(&db, &processes).await;

    drive(&db, 1.0).await;

    assert_eq!(
        processes.load(Ordering::SeqCst),
        settled + 1,
        "a Passive CPP holder is processed by the source's post"
    );
}
