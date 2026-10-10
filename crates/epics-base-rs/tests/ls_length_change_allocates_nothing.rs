//! A long-string VAL whose length moves reuses its buffer. C allocates VAL
//! and OVAL once at SIZV (lsoRecord.c:63-64) and `memcpy`s into them, so a
//! cycle whose DOL text alternates between "0" and "10" costs what one
//! alternating between "0" and "1" does. Counted with this binary's own
//! allocator on the calling thread, nobody subscribed.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

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

/// Allocations per process cycle of `C`, which forward-links the lso and the
/// lsi that read its VAL as text.
async fn per_cycle(calc: &str) -> usize {
    let db = IocBuilder::new()
        .db_string(
            &format!(
                r#"
record(calc, "C") {{ field(CALC, "{calc}") field(FLNK, "F") }}
record(fanout, "F") {{ field(LNK1, "LSO") field(LNK2, "LSI") }}
record(lso, "LSO") {{ field(SIZV, "64") field(OMSL, "closed_loop") field(DOL, "C") }}
record(lsi, "LSI") {{ field(SIZV, "64") field(INP, "C") }}
"#
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for _ in 0..10 {
        db.process_record("C").await.unwrap();
    }
    const N: usize = 100;
    ALLOCS.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    for _ in 0..N {
        db.process_record("C").await.unwrap();
    }
    COUNTING.with(|c| c.set(false));
    ALLOCS.with(Cell::get) / N
}

#[epics_macros_rs::epics_test]
async fn a_val_length_change_allocates_nothing() {
    let moving = per_cycle("VAL?0:10").await;
    let still = per_cycle("VAL?0:1").await;
    assert_eq!(moving, still, "length-alternating cycle vs same-length");
}
