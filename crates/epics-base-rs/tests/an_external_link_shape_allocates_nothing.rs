//! A scan cycle asks an external link for its DBF type, element count and
//! put admission without allocating: C reads them off the `caLink`
//! (`dbCaGetLinkDBFtype` / `dbCaGetNelements`, `dbCa.c:633-675`), whatever
//! units and description the channel's metadata carries. Counted with this
//! binary's own allocator on the scan thread, 100 cycles; a local CA link,
//! the target served by this database.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::error::CaError;
use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const DB: &str = r#"
record(ai, "X") { field(EGU, "mm") field(DESC, "d") }
record(scalcout, "SC") { field(CALC, "A+1") field(OUT, "X CA") field(OOPT, "Every Time") }
record(scalcout, "SCL") { field(CALC, "A+1") field(OUT, "X") field(OOPT, "Every Time") }
record(acalcout, "AC") { field(NELM, "1") field(CALC, "A+1") field(OUT, "X CA") field(OOPT, "Every Time") }
record(acalcout, "ACL") { field(NELM, "1") field(CALC, "A+1") field(OUT, "X") field(OOPT, "Every Time") }
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
            let deadline = Instant::now() + Duration::from_secs(10);
            while db.external_link_metadata(link).is_none() {
                assert!(Instant::now() < deadline, "{record}.{field} connects");
                epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
            }
        }
    }
    db
}

/// What 100 cycles of `rec` allocate on a scan thread.
fn allocs(db: &Arc<PvDatabase>, rec: &'static str) -> usize {
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
        ALLOCS.with(|n| n.set(0));
        COUNTING.with(|c| c.set(true));
        for _ in 0..100 {
            cycle(&mut visited);
        }
        COUNTING.with(|c| c.set(false));
        tx.send(ALLOCS.with(Cell::get)).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(20))
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
