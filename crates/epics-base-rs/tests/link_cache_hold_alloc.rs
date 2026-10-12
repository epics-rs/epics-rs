//! A `dbCommon` link's parse is built before the record's hold and the one it
//! replaces is dropped after it, as C `dbPutFieldLink` parses ahead of
//! `dbScanLock` (`dbAccess.c:1101`, `:1123`): the hold only moves pointers.
//! Counted with this binary's own allocator on the calling thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{LinkFieldType, RecordCell, parse_link_field};

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static FREES: Cell<usize> = const { Cell::new(0) };
    static HELD_ALLOCS: Cell<usize> = const { Cell::new(0) };
    static HELD_FREES: Cell<usize> = const { Cell::new(0) };
}

fn count(
    all: &'static std::thread::LocalKey<Cell<usize>>,
    held: &'static std::thread::LocalKey<Cell<usize>>,
) {
    if COUNTING.with(Cell::get) {
        all.with(|n| n.set(n.get() + 1));
        if RecordCell::live_borrows_on_this_thread() > 0 {
            held.with(|n| n.set(n.get() + 1));
        }
    }
}

// SAFETY: forwards every call to `System` unchanged; the counters are
// const-initialised thread-local `Cell`s, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(&ALLOCS, &HELD_ALLOCS);
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count(&ALLOCS, &HELD_ALLOCS);
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count(&FREES, &HELD_FREES);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// (allocations, frees, allocations under a hold, frees under a hold) made
/// by `f` on this thread.
fn counted<T>(f: impl FnOnce() -> T) -> (T, [usize; 4]) {
    start();
    let out = f();
    (out, stop())
}

fn start() {
    for c in [&ALLOCS, &FREES, &HELD_ALLOCS, &HELD_FREES] {
        c.with(|n| n.set(0));
    }
    COUNTING.with(|c| c.set(true));
}

fn stop() -> [usize; 4] {
    COUNTING.with(|c| c.set(false));
    [&ALLOCS, &FREES, &HELD_ALLOCS, &HELD_FREES].map(|c| c.with(Cell::get))
}

async fn ioc(db: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// The counter tells a hold from the rest; without it the tests below would
/// pass on nothing.
#[epics_macros_rs::epics_test]
async fn the_counter_sees_an_allocation_under_a_hold() {
    let db = ioc(r#"record(ao, "AO") {}"#).await;
    let rec = db.get_record("AO").unwrap();
    let (_, [allocs, _, held, _]) = counted(|| {
        let _inst = rec.write();
        drop(Vec::<u8>::with_capacity(8));
    });
    assert_eq!((allocs, held), (1, 1));
}

#[epics_macros_rs::epics_test]
async fn install_link_cache_allocates_and_frees_nothing_under_the_hold() {
    let db = ioc(r#"
record(ao, "AO") { field(OUT, "OLD:TARGET.VAL PP") }
record(ao, "OLD:TARGET") {}
record(ao, "NEW:TARGET") {}
"#)
    .await;
    let rec = db.get_record("AO").unwrap();

    let (link, [built, ..]) =
        counted(|| Arc::new(parse_link_field("NEW:TARGET.VAL PP", LinkFieldType::Out)));
    assert!(built > 0, "the parse is built here, before the hold");

    let (old, [allocs, frees, held_allocs, held_frees]) = counted(|| {
        let mut inst = rec.write();
        let old = inst
            .install_link_cache("OUT", link)
            .expect("OUT is a common link");
        assert!(inst.parsed_out.is_writable_out_link());
        old
    });
    assert_eq!(
        (allocs, frees, held_allocs, held_frees),
        (0, 0, 0, 0),
        "(allocs, frees, under the hold) installing OUT"
    );
    assert_eq!(Arc::strong_count(&old), 1, "the old parse came back whole");

    let ((), [_, frees, ..]) = counted(|| drop(old));
    assert!(frees > 0, "the old parse is freed here, after the hold");
}

/// iocInit's `dbInitLink` rewrite of a link to a record not in this IOC: the
/// write hold that installs the external parse allocates and frees nothing,
/// so the holds of a pass that rewrites one link cost what the same pass
/// costs with nothing to rewrite.
#[epics_macros_rs::epics_test]
async fn iocinit_link_rewrite_allocates_and_frees_nothing_under_the_hold() {
    let rewrites = ioc(r#"
record(ai, "AI") { field(INP, "ABSENT01") }
record(ai, "OTHER001") {}
"#)
    .await;
    let stays = ioc(r#"
record(ai, "AI") { field(INP, "LOCAL001") }
record(ai, "LOCAL001") {}
"#)
    .await;

    let mut held = Vec::new();
    for (db, converted) in [(&rewrites, 1), (&stays, 0)] {
        start();
        let n = db.initialize_link_locality().await;
        let [_, _, held_allocs, held_frees] = stop();
        assert_eq!(n, converted, "links made external");
        held.push((held_allocs, held_frees));
    }
    assert_eq!(
        held[0], held[1],
        "(allocs, frees) under holds: rewrite vs none"
    );
    assert_eq!(held[0].1, 0, "nothing is freed under a hold");
}
