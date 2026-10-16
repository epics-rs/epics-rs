//! DOL and SIOL render at the source only when the record's request is a
//! `DBR_STRING`: an ao DOL or ai SIOL from an mbbi must not build the label.
//! Counted with this binary's own allocator over 1000 processes; only the
//! calling thread's allocations inside `count` are seen.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
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

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const DB: &str = r#"
record(mbbi, "M") { field(VAL, "1") field(ZRST, "zero") field(ONST, "one") }
record(bo, "B") { field(VAL, "1") field(ZNAM, "off") field(ONAM, "on") }
record(ai, "A") { field(VAL, "2.5") field(PREC, "2") }
record(ao, "AO_M") { field(DOL, "M NPP") field(OMSL, "closed_loop") }
record(ao, "AO_B") { field(DOL, "B NPP") field(OMSL, "closed_loop") }
record(ao, "AO_A") { field(DOL, "A NPP") field(OMSL, "closed_loop") }
record(ai, "AI_M") { field(SIMM, "YES") field(SIOL, "M NPP") }
record(ai, "AI_A") { field(SIMM, "YES") field(SIOL, "A NPP") }
"#;

async fn allocs_over_1000(db: &PvDatabase, rec: &str) -> usize {
    let mut stack = ProcStack::new();
    db.process_record_with_links(rec, &mut stack).await.unwrap();
    ALLOCS.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    for _ in 0..1000 {
        let mut stack = ProcStack::new();
        db.process_record_with_links(rec, &mut stack).await.unwrap();
    }
    COUNTING.with(|c| c.set(false));
    assert!(
        epics_base_rs::runtime::task::in_test_ioc_thread(),
        "counted off an IOC thread"
    );
    ALLOCS.with(Cell::get)
}

#[test]
fn a_numeric_dol_or_siol_read_builds_no_source_render() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_numeric_dol_or_siol_read_builds_no_source_render_body(),
        )
    });
}

async fn a_numeric_dol_or_siol_read_builds_no_source_render_body() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    // The b74df5911 counts, which no source class may raise: one allocation
    // per process, the release build's, since the debug build's slot text
    // check compares the lent text.
    for (rec, per_process) in [
        ("AO_M", 1),
        ("AO_B", 1),
        ("AO_A", 1),
        ("AI_M", 1),
        ("AI_A", 1),
    ] {
        assert_eq!(
            allocs_over_1000(&db, rec).await,
            per_process * 1000,
            "{rec}"
        );
    }
}
