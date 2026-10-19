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
//! run as a bare `db_process` by the worker (`dbCa.c:1314-1321`). A source
//! cycle that publishes nothing therefore leaves the holder alone.
//!
//! The port makes the same link, served in-process by the calink `dbContext`
//! twin. These cases pin the rule at its boundaries: no post, a `DBE_VALUE`
//! post, a post suppressed by `MDEL`, and an alarm-only transition.
//!
//! The holder is processed on the dbCa worker, so a presence waits for it.
//! An absence closes on one of two facts: `SRC` posted no `DBE_VALUE |
//! DBE_ALARM` event, which is the link's only trigger; or the witness `WIT`,
//! `SRC`'s last CP edge, was processed by the same pass that decided `HOLD`.

mod calink_ioc;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record, ScanType};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

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

/// The process counts of `HOLD` and of the witness `WIT`.
struct Pair {
    hold: Arc<AtomicUsize>,
    wit: Arc<AtomicUsize>,
}

impl Pair {
    fn hold(&self) -> usize {
        self.hold.load(Ordering::SeqCst)
    }

    /// Wait for the pass that processes `WIT` for the `n`th time; it has
    /// decided `HOLD` too.
    async fn pass(&self, n: usize) {
        calink_ioc::until(&format!("WIT processed {n} times"), || {
            self.wit.load(Ordering::SeqCst) == n
        })
        .await;
    }
}

async fn add_holder(db: &PvDatabase, name: &str, inp: &str) -> Arc<AtomicUsize> {
    let processes = Arc::new(AtomicUsize::new(0));
    db.add_record(
        name,
        Box::new(CountingRecord {
            val: 0.0,
            processes: processes.clone(),
        }),
    )
    .await
    .unwrap();
    db.put_pv_no_process(format!("{name}.INP"), EpicsValue::String(inp.into()))
        .await
        .unwrap();
    processes
}

/// `SRC` (ai, `MDEL = mdel`) with a Passive holder taking `INP = "SRC <mod>"`
/// and, after it, the witness `WIT` taking `SRC CP`. `hold_scan` is set
/// before the link connects.
async fn cp_pair(db: &PvDatabase, modifier: &str, mdel: f64, hold_scan: ScanType) -> Pair {
    // What `register_record_type` does for a real type: publish the table to
    // the by-name registry `dbf_link_class` reads. `add_record` takes an
    // instance and no factory, so nothing else does it here.
    epics_base_rs::server::record::register_declared_fields(
        "cp_trigger_count_test",
        COUNTING_FIELDS,
    );
    let mut src = AiRecord::new(0.0);
    src.mdel = mdel;
    db.add_record("SRC", Box::new(src)).await.unwrap();
    let hold = add_holder(db, "HOLD", &format!("SRC {modifier}")).await;
    db.get_record("HOLD").unwrap().write().common.scan = hold_scan;
    let wit = add_holder(db, "WIT", "SRC CP").await;
    db.initialize_link_locality().await;
    calink_ioc::connect(db).await;
    let pair = Pair { hold, wit };
    pair.pass(1).await;
    pair
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
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("SRC", &mut visited)
        .await
        .unwrap();
}

/// Settle `SRC` past its init alarm transition so later cycles publish
/// nothing but what the test drives. Returns the holder's count afterwards.
async fn settle(db: &PvDatabase, pair: &Pair) -> usize {
    drive(db, 0.0).await;
    pair.pass(2).await;
    pair.hold()
}

/// Drive `SRC` to `val` and assert it posted no event the link takes.
async fn drive_silent(db: &PvDatabase, val: f64) {
    let mut rx = db
        .get_record("SRC")
        .unwrap()
        .write()
        .add_subscriber(
            "VAL",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::ALARM).bits(),
        )
        .unwrap();
    drive(db, val).await;
    assert!(
        rx.try_recv().is_err(),
        "SRC posted a DBE_VALUE|DBE_ALARM event, the CP link's trigger"
    );
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
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    let pair = cp_pair(&db, "CP", 0.0, ScanType::Passive).await;
    let settled = settle(&db, &pair).await;

    for _ in 0..5 {
        drive_silent(&db, 0.0).await;
    }

    assert_eq!(
        pair.hold(),
        settled,
        "a source cycle that posts nothing must not process a CP holder"
    );
}

/// BOUNDARY: the value moves past `MDEL`, so `monitor()` posts `DBE_VALUE`.
/// One post, one dispatch.
#[epics_macros_rs::epics_test]
async fn a_value_post_processes_the_cp_holder_once() {
    let db = PvDatabase::new();
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    let pair = cp_pair(&db, "CP", 0.0, ScanType::Passive).await;
    let settled = settle(&db, &pair).await;

    drive(&db, 1.0).await;
    pair.pass(3).await;

    assert_eq!(
        pair.hold(),
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
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    let pair = cp_pair(&db, "CP", 10.0, ScanType::Passive).await;
    let settled = settle(&db, &pair).await;

    drive_silent(&db, 1.0).await;

    assert_eq!(
        pair.hold(),
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
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    let pair = cp_pair(&db, "CP", 10.0, ScanType::Passive).await;
    let connected = pair.hold();

    drive(&db, 0.0).await;
    pair.pass(2).await;

    assert_eq!(
        pair.hold(),
        connected + 1,
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
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    let pair = cp_pair(&db, "CPP", 0.0, ScanType::SEC1).await;
    let settled = settle(&db, &pair).await;

    drive(&db, 1.0).await;
    pair.pass(3).await;

    assert_eq!(
        (settled, pair.hold()),
        (0, 0),
        "a non-Passive CPP holder is reached by its own scan, not by CP dispatch"
    );
}

/// BOUNDARY: a Passive CPP holder does dispatch on a post — the same gate,
/// the other side of the CPP branch.
#[epics_macros_rs::epics_test]
async fn a_passive_cpp_holder_dispatches_on_a_post() {
    let db = PvDatabase::new();
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    let pair = cp_pair(&db, "CPP", 0.0, ScanType::Passive).await;
    let settled = settle(&db, &pair).await;

    drive(&db, 1.0).await;
    pair.pass(3).await;

    assert_eq!(
        pair.hold(),
        settled + 1,
        "a Passive CPP holder is processed by the source's post"
    );
}
