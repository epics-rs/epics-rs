//! A scan cycle asks an external link for its DBF type, element count and
//! put admission without allocating: C reads them off the `caLink`
//! (`dbCaGetLinkDBFtype` / `dbCaGetNelements`, `dbCa.c:633-675`), whatever
//! units and description the channel's metadata carries. Counted with the
//! alloc binary's allocator on the scan thread, 100 cycles; a local CA link,
//! the target served by this database.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloc_count::thread;
use epics_base_rs::error::CaError;
use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;

const DB: &str = r#"
record(ai, "X") { field(EGU, "mm") field(DESC, "d") }
record(scalcout, "SC") { field(CALC, "A+1") field(OUT, "X CA") field(OOPT, "Every Time") }
record(scalcout, "SCL") { field(CALC, "A+1") field(OUT, "X") field(OOPT, "Every Time") }
record(acalcout, "AC") { field(NELM, "1") field(CALC, "A+1") field(OUT, "X CA") field(OOPT, "Every Time") }
record(acalcout, "ACL") { field(NELM, "1") field(CALC, "A+1") field(OUT, "X") field(OOPT, "Every Time") }
record(compress, "CC") { field(INP, "X CA") field(ALG, "Circular Buffer") field(NSAM, "8") }
"#;

/// [`DB`] after `iocInit`, every external link connected with metadata.
async fn ioc() -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.setup_cp_links().await;
    db.setup_external_link_opens().await;
    db.process_cp_links_on_connect().await;
    for record in db.all_record_names() {
        for (field, _, link) in db.record_link_fields(&record) {
            let Some(link) = link.external() else {
                continue;
            };
            let deadline = Instant::now() + fact_budget::FACT_BUDGET;
            while db.external_link_metadata(link).is_none() {
                assert!(Instant::now() < deadline, "{record}.{field} connects");
                epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
            }
        }
    }
    db
}

/// What 100 cycles of `rec` allocate on a scan thread.
fn allocs(db: &Arc<PvDatabase>, rec: &'static str) -> u64 {
    let (tx, rx) = std::sync::mpsc::channel();
    let db = db.clone();
    MandatoryThread::new("scan-1", ThreadPriority::ScanLow, StackSizeClass::Big).spawn(move || {
        let cycle = |visited: &mut ProcStack| {
            db.process_record_with_links_already_locked(rec, visited)
                .or_else(CaError::unless_process_status)
                .unwrap();
        };
        for _ in 0..3 {
            cycle(&mut ProcStack::new());
        }
        let mut visited = ProcStack::new();
        let window = thread::open();
        for _ in 0..100 {
            cycle(&mut visited);
        }
        tx.send(window.close().allocs).unwrap();
    });
    rx.recv_timeout(fact_budget::FACT_BUDGET)
        .expect("the cycle wedged")
}

/// scalcout and acalcout pick their OUT buffer from the target's type and
/// count (`scalcoutRecord.c` / `aCalcoutRecord.c` `execOutput`): a CA
/// target costs a cycle no more than the same record with a DB target, but
/// for the one name the put queue stages each put under.
#[epics_macros_rs::epics_test]
async fn a_ca_out_target_resolves_without_allocating() {
    let db = ioc().await;
    for (ca, local) in [("SC", "SCL"), ("AC", "ACL")] {
        let (ca_allocs, local_allocs) = (allocs(&db, ca), allocs(&db, local));
        assert!(
            ca_allocs <= local_allocs + 100,
            "{ca} {ca_allocs} against {local} {local_allocs}"
        );
    }
}

/// compress latches INPN from `dbGetNelements` each cycle
/// (`compressRecord.c:331-338`).
#[epics_macros_rs::epics_test]
async fn a_ca_inp_count_allocates_nothing() {
    let db = ioc().await;
    assert_eq!(allocs(&db, "CC"), 0);
}
