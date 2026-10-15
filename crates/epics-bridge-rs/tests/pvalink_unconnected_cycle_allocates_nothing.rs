//! A record whose pva link is down reads or refuses from state on every
//! cycle, as pvxs `pvaGetValue`/`pvaPutValue` return on `!self->valid()`
//! (`pvalink_lset.cpp:263-268`) without building anything. Counted with the
//! bridge's own pvalink lset on a primed IOC thread.
#![cfg(feature = "pvalink")]

// RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the exec-backend
// suite.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;

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

const DB: &str = r#"
record(ai, "PL:AI") { field(INP, {pva:"PL:NOPE"}) }
record(ai, "PL:AIURL") { field(INP, "pva://PL:NOPE") }
record(calc, "PL:CALC") { field(INPA, {pva:"PL:NOPE"}) field(CALC, "A") }
record(ao, "PL:AO") { field(OUT, {pva:"PL:NOPE"}) }
"#;

/// Allocations 100 cycles of `rec` make on a primed scan thread.
fn allocs(db: Arc<PvDatabase>, rec: &'static str) -> usize {
    let (tx, rx) = std::sync::mpsc::channel();
    MandatoryThread::new("scan-1", ThreadPriority::ScanLow, StackSizeClass::Big).spawn(move || {
        for _ in 0..3 {
            db.process_record_with_links_already_locked(rec, &mut ProcStack::new())
                .unwrap();
        }
        let mut visited = ProcStack::new();
        ALLOCS.with(|n| n.set(0));
        COUNTING.with(|c| c.set(true));
        for _ in 0..100 {
            db.process_record_with_links_already_locked(rec, &mut visited)
                .unwrap();
        }
        COUNTING.with(|c| c.set(false));
        tx.send(ALLOCS.with(Cell::get)).unwrap();
    });
    rx.recv().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unconnected_pva_link_cycle_allocates_nothing() {
    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |db| async move {
            epics_bridge_rs::pvalink::install_pvalink_resolver(&db).await;
            Vec::new()
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    for rec in ["PL:AI", "PL:AIURL", "PL:CALC", "PL:AO"] {
        assert_eq!(allocs(db.clone(), rec), 0, "{rec}");
    }
}
