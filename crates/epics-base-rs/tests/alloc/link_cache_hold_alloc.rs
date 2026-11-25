//! A `dbCommon` link's parse is built before the record's hold and the one it
//! replaces is dropped after it, as C `dbPutFieldLink` parses ahead of
//! `dbScanLock` (`dbAccess.c:1101`, `:1123`): the hold only moves pointers.
//! Counted with the alloc binary's allocator on the calling thread.

use std::collections::HashMap;
use std::sync::Arc;

use alloc_count::thread;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{LinkFieldType, RecordCell, parse_link_field};

/// Whether this thread holds a record now.
fn holding() -> bool {
    RecordCell::live_borrows_on_this_thread() > 0
}

/// (allocations, frees, allocations under a hold, frees under a hold) made
/// by `f` on this thread; a realloc is an allocation, not a free.
fn counted<T>(f: impl FnOnce() -> T) -> (T, [u64; 4]) {
    let window = thread::open_split(holding);
    let out = f();
    (out, stop(window))
}

fn stop(window: thread::Window) -> [u64; 4] {
    let (all, held) = window.close_split();
    assert!(
        epics_base_rs::runtime::task::in_test_ioc_thread(),
        "counted off an IOC thread"
    );
    [all.allocs, all.deallocs, held.allocs, held.deallocs]
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
#[test]
fn the_counter_sees_an_allocation_under_a_hold() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            the_counter_sees_an_allocation_under_a_hold_body(),
        )
    });
}

async fn the_counter_sees_an_allocation_under_a_hold_body() {
    let db = ioc(r#"record(ao, "AO") {}"#).await;
    let rec = db.get_record("AO").unwrap();
    let (_, [allocs, _, held, _]) = counted(|| {
        let _inst = rec.write();
        drop(Vec::<u8>::with_capacity(8));
    });
    assert_eq!((allocs, held), (1, 1));
}

#[test]
fn install_link_cache_allocates_and_frees_nothing_under_the_hold() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            install_link_cache_allocates_and_frees_nothing_under_the_hold_body(),
        )
    });
}

async fn install_link_cache_allocates_and_frees_nothing_under_the_hold_body() {
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
#[test]
fn iocinit_link_rewrite_allocates_and_frees_nothing_under_the_hold() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            iocinit_link_rewrite_allocates_and_frees_nothing_under_the_hold_body(),
        )
    });
}

async fn iocinit_link_rewrite_allocates_and_frees_nothing_under_the_hold_body() {
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
        let window = thread::open_split(holding);
        let n = db.initialize_link_locality().await;
        let [_, _, held_allocs, held_frees] = stop(window);
        assert_eq!(n, converted, "links made external");
        held.push((held_allocs, held_frees));
    }
    assert_eq!(
        held[0], held[1],
        "(allocs, frees) under holds: rewrite vs none"
    );
    assert_eq!(held[0].1, 0, "nothing is freed under a hold");
}
