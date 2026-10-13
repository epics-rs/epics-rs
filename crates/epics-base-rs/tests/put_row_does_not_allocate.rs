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
    assert!(
        epics_base_rs::runtime::task::in_test_ioc_thread(),
        "counted off an IOC thread"
    );
    (out, ALLOCS.with(Cell::get))
}

#[test]
fn the_counter_sees_this_threads_allocations() {
    epics_base_rs::runtime::task::test_ioc_thread(the_counter_sees_this_threads_allocations_body);
}

fn the_counter_sees_this_threads_allocations_body() {
    let (v, allocs) = count(|| Vec::<u8>::with_capacity(8));
    drop(v);
    assert_eq!(allocs, 1);
}

/// A string into a string field passes through unconverted; the old row test
/// cloned the field's `PvString` to ask whether it was an array.
#[test]
fn a_string_put_into_a_string_field_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_string_put_into_a_string_field_allocates_nothing_body(),
        )
    });
}

async fn a_string_put_into_a_string_field_allocates_nothing_body() {
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
#[test]
fn a_string_put_into_a_scalar_dbaddr_field_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_string_put_into_a_scalar_dbaddr_field_allocates_nothing_body(),
        )
    });
}

async fn a_string_put_into_a_scalar_dbaddr_field_allocates_nothing_body() {
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
#[test]
fn a_string_over_the_field_size_is_cut_in_place() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_string_over_the_field_size_is_cut_in_place_body(),
        )
    });
}

async fn a_string_over_the_field_size_is_cut_in_place_body() {
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

/// A number into a string field renders `cvtDoubleToString` into the put's
/// own `MAX_STRING_SIZE` buffer, as C's `cvt_d_st` does into the field
/// (`dbFastLinkConv.c:1333`), then copies it into the reserved slot; the
/// field name resolves to its declared spelling. Neither allocates.
#[test]
fn a_number_put_into_a_string_field_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_number_put_into_a_string_field_allocates_nothing_body(),
        )
    });
}

async fn a_number_put_into_a_string_field_allocates_nothing_body() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(stringout, "SO") { field(VAL, "old") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    for (field, value, want) in [
        ("VAL", EpicsValue::Double(3.25), "3.250000"),
        ("VAL", EpicsValue::Double(1e20), " 1.000000e+20"),
        ("val", EpicsValue::Float(-2.5), "-2.500000"),
        ("Val", EpicsValue::Long(-7), "-7"),
    ] {
        let held = db.lock_for_put("SO", field, &value);
        let (put, allocs) = count(|| db.put_pv_already_locked(held, value));
        put.unwrap();
        assert_eq!(allocs, 0, "{field} {want}");
        assert_eq!(
            db.get_pv("SO").unwrap(),
            EpicsValue::String(PvString::from(want))
        );
    }
}

/// A lean `Str` store copies into the slot `RecordCell::new` reserved to the
/// field's cap, so no length up to the cap grows it; 40 bytes is cut to 39
/// (`putStringString`, `dbConvert.c:916-926`).
#[test]
fn a_lean_string_put_into_a_fresh_record_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_lean_string_put_into_a_fresh_record_allocates_nothing_body(),
        )
    });
}

async fn a_lean_string_put_into_a_fresh_record_allocates_nothing_body() {
    let (db, _) = IocBuilder::new()
        .db_string(r#"record(stringout, "SO") {}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for len in [1, 39, 40] {
        let text = "abcdefghijklmnopqrstuvwxyz0123456789ABCD"[..len].to_string();
        let value = EpicsValue::String(PvString::from(text.as_str()));
        let held = db.lock_for_put("SO", "VAL", &value);
        let (put, allocs) = count(|| db.put_pv_already_locked(held, value));
        put.unwrap();
        assert_eq!(allocs, 0, "{len} bytes");
        assert_eq!(
            db.get_pv("SO").unwrap(),
            EpicsValue::String(PvString::from(&text[..len.min(39)]))
        );
    }
}

/// With no subscriber on the field, the put returns from its post before
/// reading the field, as C `db_post_events` does at `mlis.count == 0`
/// (`dbEvent.c:885`). Lean `Str` arm only: the general arm still clones
/// the value it stores.
#[test]
fn a_put_into_an_unsubscribed_string_field_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_put_into_an_unsubscribed_string_field_allocates_nothing_body(),
        )
    });
}

async fn a_put_into_an_unsubscribed_string_field_allocates_nothing_body() {
    let (db, _) = IocBuilder::new()
        .db_string(r#"record(stringout, "SO") {}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for (rec, field) in [("SO", "IVOV"), ("SO", "VAL")] {
        for text in ["warm", "next"] {
            let value = EpicsValue::String(PvString::from(text));
            let held = db.lock_for_put(rec, field, &value);
            let (put, allocs) = count(|| db.put_pv_already_locked(held, value));
            put.unwrap();
            if text == "next" {
                assert_eq!(allocs, 0, "{rec}.{field}");
            }
        }
        assert_eq!(
            db.get_pv(&format!("{rec}.{field}")).unwrap(),
            EpicsValue::String(PvString::from("next"))
        );
    }
}
