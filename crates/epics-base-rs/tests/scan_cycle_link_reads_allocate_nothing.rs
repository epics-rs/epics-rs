//! A `scan-0.1` cycle of a record reading its links allocates nothing,
//! watched or not: C's `dbProcess` reaches each link through its parsed
//! `DBLINK` (`dbAccess.c` `dbGetLink`) and resolves no link metadata, which
//! `dbGet` reads only when a consumer asks (`dbDbLink.c:414-415`). Counted
//! per thread name with this binary's own allocator.
//!
//! Tokio only: the thread backend's scan driver allocates a waker per park
//! (`ThreadWaker::for_current_thread`), a count that varies per tick.

#![cfg(tokio_backend)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;

const SCAN_THREAD: &str = "scan-0.1";

thread_local! {
    /// 0 not yet asked, 1 `scan-0.1`, 2 any other thread. Asking may itself
    /// allocate; that reentry reads 2 and is not counted.
    static ON_SCAN: Cell<u8> = const { Cell::new(0) };
}

static ALLOCS: AtomicU64 = AtomicU64::new(0);

fn on_scan() -> bool {
    ON_SCAN.with(|s| match s.get() {
        0 => {
            s.set(2);
            let yes = std::thread::current().name() == Some(SCAN_THREAD);
            s.set(if yes { 1 } else { 2 });
            yes
        }
        v => v == 1,
    })
}

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the thread test reads a
// const-initialised thread-local and the counter is an atomic.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if on_scan() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if on_scan() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const N: usize = 20;

const TICK: &str = "record(calc, \"TICK\") { field(SCAN, \".1 second\") field(PHAS, \"-1\") field(CALC, \"VAL+1\") }\n";

/// `N` calcs reading `TICK` through INPA, the link that backs `A`'s metadata.
fn calcs() -> (String, Vec<String>) {
    let mut db = String::from(TICK);
    let mut pvs = Vec::new();
    for i in 0..N {
        db.push_str(&format!(
            "record(calc, \"C{i}\") {{ field(SCAN, \".1 second\") field(INPA, \"TICK\") \
             field(CALC, \"A+1\") }}\n"
        ));
        pvs.push(format!("C{i}"));
    }
    (db, pvs)
}

fn tick(db: &PvDatabase) -> u64 {
    match db.get_pv("TICK").expect("TICK") {
        EpicsValue::Double(v) => v as u64,
        other => panic!("TICK: {other:?}"),
    }
}

/// The counter and the tick, read in the idle gap after a sweep, so a window
/// holds whole sweeps only.
fn between_sweeps(db: &PvDatabase) -> (u64, u64) {
    let t = tick(db);
    while tick(db) == t {
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(40));
    (ALLOCS.load(Ordering::Relaxed), tick(db))
}

/// Allocations per tick on `scan-0.1`, over at least 20 ticks.
fn allocs_per_tick(db: &PvDatabase) -> (u64, u64) {
    std::thread::sleep(Duration::from_secs(1));
    let (a0, t0) = between_sweeps(db);
    std::thread::sleep(Duration::from_secs(2));
    let (a1, t1) = between_sweeps(db);
    let ticks = t1 - t0;
    assert!(ticks >= 10, "scan-0.1 ran {ticks} ticks in 2 s");
    (a1 - a0, ticks)
}

/// The cycle allocates nothing unwatched, and a subscriber adds nothing.
async fn assert_scan_allocates_nothing((db_text, pvs): (String, Vec<String>)) -> Arc<PvDatabase> {
    let db = IocBuilder::new()
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let scan = ScanOwner::start(db.clone());

    let (unwatched, ticks) = allocs_per_tick(&db);
    assert_eq!(
        unwatched, 0,
        "scan-0.1 allocated {unwatched} times in {ticks} ticks, unwatched"
    );

    let mut subs = Vec::new();
    for pv in &pvs {
        subs.push(
            DbSubscription::subscribe(&db, pv)
                .await
                .unwrap_or_else(|e| panic!("{pv} subscribes: {e:?}")),
        );
    }
    let (watched, ticks) = allocs_per_tick(&db);
    assert_eq!(
        watched,
        0,
        "scan-0.1 allocated {watched} times in {ticks} ticks, all {} watched",
        pvs.len()
    );
    for sub in &mut subs {
        assert!(sub.try_recv_event().is_ok(), "a watched record posted");
    }
    drop(scan);
    db
}

/// A post carries no metadata, so the cycle walks no INPA target for it.
#[epics_macros_rs::epics_test]
async fn a_calc_cycle_reading_inpa_allocates_nothing() {
    let db = assert_scan_allocates_nothing(calcs()).await;
    let c0 = db.get_pv("C0").expect("C0").to_f64().expect("double");
    assert!(c0 > 10.0, "C0 read TICK through INPA: {c0}");
}
