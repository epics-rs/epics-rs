//! An external forward link — an FLNK or a fanout `LNKn` — is dispatched from
//! what its parse holds, as C `dbScanFwdLink` hands `plink` to
//! `lset->scanForward` (`dbLink.c:475-481`): the cycle allocates nothing for
//! it. Counted with this binary's own allocator on the calling thread,
//! against the same record with the link left empty.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::server::database::{LinkSet, ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards every call to `System` unchanged; the counters are
// const-initialised thread-local `Cell`s, which never allocate.
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

/// A connected `pva` lset whose `scanForward` counts and allocates nothing.
struct Forwards(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl LinkSet for Forwards {
    fn is_connected(&self, _: &str) -> bool {
        true
    }
    async fn get_value(&self, _: &str) -> Option<EpicsValue> {
        None
    }
    fn scan_forward(&self, _: &str) -> Result<(), String> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

async fn ioc(db: &str) -> (Arc<PvDatabase>, Arc<AtomicUsize>) {
    let db = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let forwards = Arc::new(AtomicUsize::new(0));
    db.register_link_set("pva", Arc::new(Forwards(forwards.clone())))
        .await;
    db.ioc_init().await;
    (db, forwards)
}

/// Allocations one process of `rec` makes on this thread, after a warm-up.
async fn allocs(db: &PvDatabase, rec: &str) -> usize {
    let mut visited = ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
    let mut visited = ProcStack::new();
    ALLOCS.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
    COUNTING.with(|c| c.set(false));
    ALLOCS.with(Cell::get)
}

const DB: &str = r#"
record(calc, "BARE") { field(CALC, "VAL+1") }
record(calc, "PVA") { field(CALC, "VAL+1") field(FLNK, "pva://T:A") }
record(calc, "JSON") { field(CALC, "VAL+1") field(FLNK, {pva:{pv:"T:B", Q:4}}) }
record(fanout, "FNONE") {}
record(fanout, "FPVA") { field(LNK1, "pva://T:C") }
record(fanout, "FJSON") { field(LNK1, {pva:{pv:"T:D", Q:4}}) }
"#;

#[epics_macros_rs::epics_test]
async fn an_external_flnk_cycle_allocates_nothing_for_its_forward() {
    let (db, forwards) = ioc(DB).await;
    let bare = allocs(&db, "BARE").await;
    for rec in ["PVA", "JSON"] {
        let before = forwards.load(Ordering::Relaxed);
        assert_eq!(allocs(&db, rec).await, bare, "{rec}");
        assert_eq!(
            forwards.load(Ordering::Relaxed) - before,
            2,
            "{rec} forwarded"
        );
    }
}

#[epics_macros_rs::epics_test]
async fn a_pva_lnkn_dispatch_allocates_nothing() {
    let (db, forwards) = ioc(DB).await;
    // An empty fanout: a baseline that dispatches nothing, so whatever the
    // forwarding branch allocates shows.
    let bare = allocs(&db, "FNONE").await;
    for rec in ["FPVA", "FJSON"] {
        let before = forwards.load(Ordering::Relaxed);
        assert_eq!(allocs(&db, rec).await, bare, "{rec}");
        assert_eq!(
            forwards.load(Ordering::Relaxed) - before,
            2,
            "{rec} forwarded"
        );
    }
}
