//! A `DBR_STRING` link read of an enum VAL renders one state label: a
//! stringin reading an mbbi with 16 states, or a bo, allocates no more per
//! process than one reading another stringin.
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
record(mbbi, "M") {
    field(VAL, "3")
    field(ZRST, "s0") field(ONST, "s1") field(TWST, "s2") field(THST, "s3")
    field(FRST, "s4") field(FVST, "s5") field(SXST, "s6") field(SVST, "s7")
    field(EIST, "s8") field(NIST, "s9") field(TEST, "s10") field(ELST, "s11")
    field(TVST, "s12") field(TTST, "s13") field(FTST, "s14") field(FFST, "s15")
}
record(bo, "B") { field(VAL, "1") field(ZNAM, "off") field(ONAM, "on") }
record(stringin, "S") { field(VAL, "s3") }
record(stringin, "SI_M") { field(INP, "M NPP") }
record(stringin, "SI_B") { field(INP, "B NPP") }
record(stringin, "SI_S") { field(INP, "S NPP") }
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
fn a_string_read_of_an_enum_val_allocates_as_a_string_source() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_string_read_of_an_enum_val_allocates_as_a_string_source_body(),
        )
    });
}

async fn a_string_read_of_an_enum_val_allocates_as_a_string_source_body() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    // 20 and 6 while the read built the whole `EnumStringForm`.
    for rec in ["SI_S", "SI_M", "SI_B"] {
        assert_eq!(allocs_over_1000(&db, rec).await, 2 * 1000, "{rec}");
    }
}
