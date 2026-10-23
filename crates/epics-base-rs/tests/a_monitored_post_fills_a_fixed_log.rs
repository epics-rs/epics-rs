//! A record's post fills one fixed `db_field_log` per subscriber from its
//! own fields (`db_create_field_log`, `dbEvent.c:700-738`): stat, sevr, the
//! amsg cut to 39 bytes and a NUL, time, utag, and a scalar value. Nothing
//! else is built, so a monitored post allocates no more than an unmonitored
//! one. Counted with this binary's own allocator on the calling thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Wake};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::{EventUser, no_context};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static FREES: Cell<usize> = const { Cell::new(0) };
}

fn count(n: &'static std::thread::LocalKey<Cell<usize>>) {
    if COUNTING.with(Cell::get) {
        n.with(|n| n.set(n.get() + 1));
    }
}

// SAFETY: forwards every call to `System` unchanged; the counters are
// const-initialised thread-local `Cell`s, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(&ALLOCS);
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count(&ALLOCS);
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count(&FREES);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const DB: &str = r#"
record(calc, "C") { field(CALC, "A") }
record(ai, "AI") { field(HIGH, "5") field(HSV, "MINOR") }
"#;

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// Allocations and frees per `dbPut` of `C.VAL`, whose non-pp value field
/// posts from `dbPut` itself (`dbAccess.c:1414`).
async fn per_put(db: &PvDatabase) -> (usize, usize) {
    for n in 0..10 {
        db.put_pv("C.VAL", EpicsValue::Double(f64::from(n)))
            .await
            .unwrap();
    }
    const N: usize = 100;
    ALLOCS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    for n in 0..N {
        db.put_pv("C.VAL", EpicsValue::Double(n as f64))
            .await
            .unwrap();
    }
    COUNTING.with(|c| c.set(false));
    assert!(
        epics_base_rs::runtime::task::in_test_ioc_thread(),
        "counted off an IOC thread"
    );
    (ALLOCS.with(Cell::get) / N, FREES.with(Cell::get) / N)
}

/// The post a `dbPut` makes to a monitored field costs it nothing over the
/// same put with no monitor.
#[test]
fn a_dbput_post_to_a_monitor_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_dbput_post_to_a_monitor_allocates_nothing_body(),
        )
    });
}

async fn a_dbput_post_to_a_monitor_allocates_nothing_body() {
    let db = ioc().await;
    let plain = per_put(&db).await;
    let rec = db.get_record("C").unwrap();
    let mut reader = rec
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .expect("subscribe");
    let watched = per_put(&db).await;
    assert_eq!(watched, plain, "(allocs, frees) per put, monitored vs not");
    let mut last = None;
    while let Ok(ev) = reader.try_recv() {
        last = ev.value().as_value().cloned();
    }
    assert_eq!(last, Some(EpicsValue::Double(99.0)));
}

/// `strncpy(pLog->amsg, prec->amsg, DB_AMSG_SIZE-1)` with the last byte NUL
/// (`dbEvent.c:708-709`): a message of 39 bytes or more arrives as its first
/// 39.
#[test]
fn an_amsg_reaches_a_monitor_cut_to_39_bytes() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(an_amsg_reaches_a_monitor_cut_to_39_bytes_body())
    });
}

async fn an_amsg_reaches_a_monitor_cut_to_39_bytes_body() {
    let db = ioc().await;
    let rec = db.get_record("C").unwrap();
    let mut reader = rec
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::ALARM.bits())
        .expect("subscribe");
    for len in [39, 40, 41] {
        let text: String = ('a'..='z').cycle().take(len).collect();
        {
            let mut inst = rec.write();
            inst.common.amsg.set(&text);
            inst.notify_field("VAL", EventMask::ALARM);
        }
        let ev = reader.try_recv().expect("an alarm post");
        assert_eq!(ev.amsg(), &text[..39], "a {len}-byte amsg");
    }
}

struct Wakes(AtomicUsize);

impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A cycle that posts VAL and an alarm field reaches a parked circuit
/// consumer as one wake, and each log carries `caEventMask & select`
/// (`dbEvent.c:896-900`).
#[test]
fn a_cycle_posting_val_and_sevr_wakes_once_with_narrowed_masks() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_cycle_posting_val_and_sevr_wakes_once_with_narrowed_masks_body(),
        )
    });
}

async fn a_cycle_posting_val_and_sevr_wakes_once_with_narrowed_masks_body() {
    let db = ioc().await;
    let rec = db.get_record("AI").unwrap();
    let user = EventUser::new();
    let (mut val, mut sevr) = {
        let mut inst = rec.write();
        let select = (EventMask::VALUE | EventMask::ALARM).bits();
        let val = inst
            .add_subscriber_on(&user, "VAL", 1, DbFieldType::Double, select, no_context())
            .expect("VAL");
        let sevr = inst
            .add_subscriber_on(&user, "SEVR", 2, DbFieldType::Enum, select, no_context())
            .expect("SEVR");
        (val, sevr)
    };
    let wakes = Arc::new(Wakes(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(wakes.clone());
    assert!(
        user.poll_next(&mut Context::from_waker(&waker))
            .is_pending()
    );

    db.put_record_field_from_ca_no_notify("AI", "VAL", EpicsValue::Double(10.0))
        .await
        .expect("put");

    assert_eq!(wakes.0.load(Ordering::SeqCst), 1, "one wake per cycle");
    let val = val.try_recv().expect("a VAL post");
    // ai posts VAL with DBE_VALUE|DBE_LOG|DBE_ALARM on entering alarm; the
    // log keeps only what this monitor selected.
    assert_eq!(val.mask(), EventMask::VALUE | EventMask::ALARM);
    assert_eq!(val.value().as_value(), Some(&EpicsValue::Double(10.0)));
    let sevr = sevr.try_recv().expect("a SEVR post");
    assert_eq!(sevr.mask(), EventMask::VALUE);
    assert_eq!(sevr.sevr(), 1);
}
