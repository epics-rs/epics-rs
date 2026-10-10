//! aCalcout's `afterCalc` array post (`take_process_posted_fields`) with no
//! subscriber must cost nothing: C's `db_post_events` finds no monitor, and
//! the port has no change detector that reads AA..LL. Counted with this
//! binary's own allocator on the calling thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

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

/// Allocations per steady-state process cycle of an unwatched acalcout.
async fn per_cycle(calc: &str) -> usize {
    let db = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::new()))
        .db_string(
            &format!(r#"record(acalcout, "AC") {{ field(NELM, "4") field(CALC, "{calc}") }}"#),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for _ in 0..10 {
        db.process_record("AC").await.unwrap();
    }
    const N: usize = 100;
    ALLOCS.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    for _ in 0..N {
        db.process_record("AC").await.unwrap();
    }
    COUNTING.with(|c| c.set(false));
    ALLOCS.with(Cell::get) / N
}

/// The store's own array temporary is the one allocation an AMASK cycle may
/// add over a cycle that stores nothing; the post itself adds none.
#[epics_macros_rs::epics_test]
async fn an_unwatched_amask_post_allocates_nothing() {
    let stores = per_cycle("AA:=IX;VAL+1").await;
    let plain = per_cycle("VAL+1").await;
    assert!(
        stores <= plain + 1,
        "AMASK cycle {stores} allocs vs {plain} without a store"
    );
}
