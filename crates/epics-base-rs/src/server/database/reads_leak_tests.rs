//! The input stage's owned reads are freed every cycle: a leak of
//! [`Reads::Links`](super::processing) — the one `drop_reads` is the only
//! thing that frees — shows as heap growth over thousands of cycles.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;

use crate::server::ioc_builder::IocBuilder;
use crate::types::EpicsValue;

thread_local! {
    /// Live bytes allocated by this thread, net of its frees. Per thread so
    /// concurrently running tests cannot move each other's count.
    static LIVE: Cell<isize> = const { Cell::new(0) };
}

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the counter is a
// const-initialised thread-local `Cell`, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.with(|n| n.set(n.get() + layout.size() as isize));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.with(|n| n.set(n.get() - layout.size() as isize));
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.with(|n| n.set(n.get() + new_size as isize - layout.size() as isize));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const DB: &str = r#"
record(ao, "SRC") { field(VAL, "7") }
record(stringin, "SSRC") { field(VAL, "hi") }
record(waveform, "SRCW") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, [1,2,3,4]) }
record(waveform, "W") { field(NELM, "4") field(FTVL, "DOUBLE") field(INP, "SRCW NPP") }
record(ao, "AO") { field(OMSL, "closed_loop") field(DOL, "SRC NPP") }
record(calc, "CA") { field(INPA, "SRC NPP") field(CALC, "A") }
record(longin, "LI") { field(INP, "SRC NPP") }
record(stringin, "SI") { field(INP, "SSRC NPP") }
"#;

const CYCLES: usize = 3000;
const NAMES: [&str; 5] = ["W", "AO", "CA", "LI", "SI"];

async fn cycle(db: &super::PvDatabase, name: &str) {
    let mut visited = super::ProcStack::new();
    db.process_record_with_links(name, &mut visited)
        .await
        .expect("process");
}

#[test]
fn input_stage_reads_are_freed_every_cycle() {
    crate::runtime::task::test_ioc_thread(|| {
        crate::runtime::task::test_block_on(input_stage_reads_are_freed_every_cycle_body())
    });
}

async fn input_stage_reads_are_freed_every_cycle_body() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .expect("db")
        .build()
        .await
        .expect("ioc");
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    // Settle one-time growth (record caches, hash tables) before counting.
    for name in NAMES {
        for _ in 0..200 {
            cycle(&db, name).await;
        }
    }
    for name in NAMES {
        let before = LIVE.with(Cell::get);
        for _ in 0..CYCLES {
            cycle(&db, name).await;
        }
        let grown = LIVE.with(Cell::get) - before;
        assert!(
            crate::runtime::task::in_test_ioc_thread(),
            "counted off an IOC thread"
        );
        assert_eq!(
            grown, 0,
            "{name}: {grown} B of heap grown over {CYCLES} cycles"
        );
    }
    // Every read landed, so the zero above is not a stage that read nothing.
    let get = |pv: &'static str| db.get_pv(pv);
    assert_eq!(get("AO").unwrap(), EpicsValue::Double(7.0));
    assert_eq!(get("CA").unwrap(), EpicsValue::Double(7.0));
    assert_eq!(get("LI").unwrap(), EpicsValue::Long(7));
    assert_eq!(get("SI").unwrap(), EpicsValue::String("hi".into()));
    assert_eq!(
        get("W").unwrap(),
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0].into())
    );
}
