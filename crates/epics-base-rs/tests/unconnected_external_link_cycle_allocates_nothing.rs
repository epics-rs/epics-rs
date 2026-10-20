//! A record whose external link is not connected reads or refuses from
//! state on every cycle, as C's `dbCaGetLink` returns -1 on
//! `!pca->isConnected` (`dbCa.c:430-435`) and `dbCaPutLinkCallback` refuses
//! at `dbCa.c:529-532`: the `caLink` was built and its `CA_CONNECT` staged
//! once, at `dbCaAddLink` (`dbCa.c:389-414`) — for a DB link naming no local
//! record too, which `dbInitLink` makes a CA link (`dbLink.c:118-130`).
//! Counted on a primed IOC thread with this binary's own allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use epics_base_rs::server::database::{LinkRef, LinkSet, ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

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

/// A link set whose channels never connect.
struct Down;

#[async_trait::async_trait]
impl LinkSet for Down {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        false
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
}

const DB: &str = r#"
record(ai, "U:AI") { field(INP, {pva:"U:NOPE"}) }
record(ai, "U:AIURL") { field(INP, "pva://U:NOPE") }
record(ai, "U:AIJSON") { field(INP, {pva:{pv:"U:NOPE", Q:4}}) }
record(ai, "U:AICA") { field(INP, "ca://U:NOPE") }
record(ai, "U:AIDB") { field(INP, "U:NOT:HERE") }
record(ao, "U:AO") { field(OUT, {pva:"U:NOPE"}) }
record(ao, "U:AOJSON") { field(OUT, {pva:{pv:"U:NOPE", Q:4}}) }
record(ao, "U:AOCA") { field(OUT, "ca://U:NOPE") }
record(ao, "U:AODB") { field(OUT, "U:NOT:HERE") }
"#;

async fn ioc() -> Arc<PvDatabase> {
    let db = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .register_link_type("ca", |_db| async { Vec::new() })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.register_link_set("pva", Arc::new(Down)).await;
    db.register_link_set("ca", Arc::new(Down)).await;
    db.ioc_init().await;
    db
}

/// Allocations 100 cycles of `rec` make on a primed scan thread.
fn allocs(db: Arc<PvDatabase>, rec: &'static str) -> usize {
    let (tx, rx) = std::sync::mpsc::channel();
    MandatoryThread::new("scan-1", ThreadPriority::ScanLow, StackSizeClass::Big).spawn(move || {
        for _ in 0..3 {
            db.process_record_with_links_already_locked(rec, &mut ProcStack::new())
                .or_else(epics_base_rs::error::CaError::unless_process_status)
                .unwrap();
        }
        let mut visited = ProcStack::new();
        ALLOCS.with(|n| n.set(0));
        COUNTING.with(|c| c.set(true));
        for _ in 0..100 {
            db.process_record_with_links_already_locked(rec, &mut visited)
                .or_else(epics_base_rs::error::CaError::unless_process_status)
                .unwrap();
        }
        COUNTING.with(|c| c.set(false));
        tx.send(ALLOCS.with(Cell::get)).unwrap();
    });
    rx.recv().unwrap()
}

#[epics_macros_rs::epics_test]
async fn an_unconnected_external_inp_cycle_allocates_nothing() {
    let db = ioc().await;
    for rec in ["U:AI", "U:AIURL", "U:AIJSON", "U:AICA", "U:AIDB"] {
        assert_eq!(allocs(db.clone(), rec), 0, "{rec}");
    }
}

#[epics_macros_rs::epics_test]
async fn an_unconnected_external_out_cycle_allocates_nothing() {
    let db = ioc().await;
    for rec in ["U:AO", "U:AOJSON", "U:AOCA", "U:AODB"] {
        assert_eq!(allocs(db.clone(), rec), 0, "{rec}");
    }
}
