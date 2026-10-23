//! The over-run report of a periodic scan thread allocates nothing on it: C's
//! `periodicTask` prints it with `errlogPrintf` (`dbScan.c:830-841`), which
//! formats into the buffer errlog allocated at init. A slow host trips the
//! report in any allocation-counted window. Counted per thread name in this
//! binary's own allocator, across over-runs forced by holding TICK's write
//! lock past each deadline until the report fires.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::log::{
    errlog_add_listener, errlog_console_paints, errlog_flush, errlog_remove_listener,
    errlog_vprintf,
};
use epics_base_rs::server::ioc_builder::IocBuilder;

const SCAN_THREAD: &str = "scan-0.1";

thread_local! {
    /// 0 not yet asked, 1 `scan-0.1`, 2 any other thread. Asking may itself
    /// allocate; that reentry reads 2 and is not counted.
    static ON_SCAN: Cell<u8> = const { Cell::new(0) };
}

static ARMED: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static REPORTED: AtomicBool = AtomicBool::new(false);

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

fn count() {
    if ARMED.load(Ordering::Relaxed) && on_scan() {
        CALLS.fetch_add(1, Ordering::Relaxed);
    }
}

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the thread test reads a
// const-initialised thread-local and the counters are atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count();
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const DB: &str = r#"record(calc, "TICK") { field(SCAN, ".1 second") field(CALC, "VAL+1") }"#;

#[epics_macros_rs::epics_test]
async fn an_over_run_report_allocates_nothing_on_the_scan_thread() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let ticks = || db.get_pv("TICK").unwrap().to_f64().unwrap();
    // An IOC has printed through errlog long before a list over-runs: the
    // console and the `tracing` callsite exist, and the worker's listener
    // snapshot is sized.
    let listener = errlog_add_listener(|m| {
        if m.contains("Over-runs have now happened") {
            REPORTED.store(true, Ordering::SeqCst);
        }
    });
    errlog_console_paints();
    errlog_vprintf(format_args!("warm\n"));
    errlog_flush();

    let owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    let start = Instant::now();
    while ticks() < 5.0 {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "scan never ticked"
        );
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }

    ARMED.store(true, Ordering::SeqCst);
    // C reports the tenth over-run in a row once 10 s have passed since the
    // thread started (`dbScan.c:830`): hold every sweep past its deadline
    // until it does.
    let stop = Arc::new(AtomicBool::new(false));
    let holder = {
        let (stop, rec) = (stop.clone(), db.get_record("TICK").unwrap());
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let held = rec.write();
                std::thread::sleep(Duration::from_millis(150));
                drop(held);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let start = Instant::now();
    while !REPORTED.load(Ordering::SeqCst) {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "no over-run report"
        );
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    stop.store(true, Ordering::SeqCst);
    holder.join().unwrap();
    // Two sweeps past the one that reported.
    let reported = ticks();
    while ticks() < reported + 2.0 {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    ARMED.store(false, Ordering::SeqCst);
    drop(owner);
    errlog_remove_listener(listener);

    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        0,
        "allocations and frees on scan-0.1 across over-runs and their report"
    );
}
