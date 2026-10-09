//! The put row is picked under the destination record's lock, so it must not
//! clone the field to learn its shape. Counted with this binary's own
//! allocator; only the calling thread's allocations inside `count` are seen.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{coerce_put_value, dbput_coerce_value};
use epics_base_rs::types::c_parse::Converted;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};

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

fn count<T>(f: impl FnOnce() -> T) -> (T, usize) {
    ALLOCS.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (out, ALLOCS.with(Cell::get))
}

#[test]
fn the_counter_sees_this_threads_allocations() {
    let (v, allocs) = count(|| Vec::<u8>::with_capacity(8));
    drop(v);
    assert_eq!(allocs, 1);
}

/// A string into a string field passes through unconverted; the old row test
/// cloned the field's `PvString` to ask whether it was an array.
#[epics_macros_rs::epics_test]
async fn a_string_put_into_a_string_field_allocates_nothing() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(stringout, "SO") { field(VAL, "old") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let rec = db.get_record("SO").unwrap();
    let guard = rec.read();
    let value = EpicsValue::String(PvString::from("new"));
    let (put, allocs) =
        count(|| coerce_put_value(&*guard.record, "VAL", DbFieldType::String, value));
    assert!(put.is_ok());
    assert_eq!(allocs, 0);
}

/// A one-element string into a scalar-held `SPC_DBADDR` field is `putString*`
/// on that one element; it used to be packed into a one-element array first.
#[epics_macros_rs::epics_test]
async fn a_string_put_into_a_scalar_dbaddr_field_allocates_nothing() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(mbbo, "N") { field(VAL, "3") }
record(aSub, "AS") { field(FTA, "LONG") field(NOA, "1") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    for (pv, field, target, want) in [
        ("N", "VAL", DbFieldType::UShort, EpicsValue::UShort(2)),
        ("AS", "A", DbFieldType::Long, EpicsValue::Long(2)),
    ] {
        let rec = db.get_record(pv).unwrap();
        let guard = rec.read();
        let value = EpicsValue::String(PvString::from("2"));
        let (put, allocs) =
            count(|| dbput_coerce_value(&*guard.record, field, target, value).unwrap());
        assert!(
            matches!(put, Converted::Stored(ref v) if *v == want),
            "{pv}.{field}"
        );
        assert_eq!(allocs, 0, "{pv}.{field}");
    }
}

/// `putStringString` keeps `size - 1` bytes, cut from the request itself.
/// softIoc: `caput A.EGU abcdefghijklmnopqrstuvwxyz` reads `abcdefghijklmno`.
#[epics_macros_rs::epics_test]
async fn a_string_over_the_field_size_is_cut_in_place() {
    let (db, _) = IocBuilder::new()
        .db_string(r#"record(ai, "A") {}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let rec = db.get_record("A").unwrap();
    let guard = rec.read();
    let value = EpicsValue::String(PvString::from("abcdefghijklmnopqrstuvwxyz"));
    let (put, allocs) =
        count(|| coerce_put_value(&*guard.record, "EGU", DbFieldType::String, value).unwrap());
    assert!(
        matches!(put, Converted::Stored(EpicsValue::String(ref s)) if s.as_bytes() == b"abcdefghijklmno")
    );
    assert_eq!(allocs, 0);
}
