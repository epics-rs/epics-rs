//! A lend of a record to device code allocates nothing on release, whatever
//! the type. The release compares the metadata cache's source fields with
//! what the cache was built from, so every source must read in place: a
//! string source cloned out by name was a malloc per I/O Intr callback, and
//! per cycle on the scan thread for a held-API device. Counted with this
//! binary's own allocator on the calling thread, every string source set.

mod module_records;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::dbd_generated::{RECORD_TYPES, record_fields};
use epics_base_rs::types::DbFieldType;

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

/// The metadata cache's string sources.
const STRING_SOURCES: &[&str] = &[
    "DESC", "EGU", "ZNAM", "ONAM", "ZRST", "ONST", "TWST", "THST", "FRST", "FVST", "SXST", "SVST",
    "EIST", "NIST", "TEST", "ELST", "TVST", "TTST", "FTST", "FFST",
];

fn record_of(record_type: &str) -> String {
    let fields: String = record_fields(record_type)
        .unwrap_or_default()
        .iter()
        .chain(epics_base_rs::server::record::dbd_generated::DB_COMMON_FIELDS)
        .filter(|f| f.dbf_type == DbFieldType::String && STRING_SOURCES.contains(&f.name))
        .map(|f| format!(r#" field({}, "s{}")"#, f.name, f.name))
        .collect();
    format!(r#"record({record_type}, "R:{record_type}") {{{fields} }}"#)
}

#[test]
fn a_lend_allocates_nothing_on_any_record_type() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_lend_allocates_nothing_on_any_record_type_body(),
        )
    });
}

async fn a_lend_allocates_nothing_on_any_record_type_body() {
    let db: String = RECORD_TYPES.iter().map(|t| record_of(t) + "\n").collect();
    let builder = module_records::factories()
        .into_iter()
        .fold(IocBuilder::new(), |b, (name, factory)| {
            b.register_record_type(&name, factory)
        });
    let (db, _) = builder
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let handle = db.async_handle();
    let mut allocating = Vec::new();
    for record_type in RECORD_TYPES {
        let name = format!("R:{record_type}");
        for _ in 0..10 {
            handle.with_record(&name, |_| {});
        }
        const N: usize = 100;
        ALLOCS.with(|n| n.set(0));
        COUNTING.with(|c| c.set(true));
        for _ in 0..N {
            handle.with_record(&name, |_| {});
        }
        COUNTING.with(|c| c.set(false));
        let allocs = ALLOCS.with(Cell::get);
        if allocs != 0 {
            allocating.push(format!("{record_type}: {allocs} in {N} lends"));
        }
    }
    assert!(
        epics_base_rs::runtime::task::in_test_ioc_thread(),
        "counted off an IOC thread"
    );
    assert!(allocating.is_empty(), "{allocating:#?}");
}
