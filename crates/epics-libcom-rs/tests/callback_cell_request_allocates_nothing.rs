//! A `CallbackCell` request allocates nothing, on the requesting thread or on
//! the band worker that runs it: C's `callbackRequest` queues the caller's own
//! `epicsCallback`, and the boxed `Callback` it replaces costs one allocation
//! per request. Counted per thread with this binary's own allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use epics_libcom_rs::runtime::background::{
    CallbackBody, CallbackCell, CallbackPool, CallbackPriority,
};

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// Set while the worker counts, read by its thread only.
static WORKER_ARMED: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);

fn counted() -> bool {
    ARMED.with(Cell::get)
}

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the gate reads a
// const-initialised thread-local and the counter is an atomic.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if counted() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if counted() {
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

const N: usize = 1000;

/// Arms its worker's counter on its first run and counts runs after that.
struct Body {
    runs: AtomicUsize,
}

impl CallbackBody for Body {
    fn run(&self) {
        ARMED.with(|a| a.set(WORKER_ARMED.load(Ordering::Acquire)));
        self.runs.fetch_add(1, Ordering::Release);
    }
}

#[test]
fn a_cell_request_and_its_run_allocate_nothing() {
    let pool = CallbackPool::with_config(16, 1);
    let cell = CallbackCell::new(Body {
        runs: AtomicUsize::new(0),
    });
    let wait_runs = |n: usize| {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while cell.body().runs.load(Ordering::Acquire) < n {
            assert!(std::time::Instant::now() < deadline, "run {n} never came");
            std::thread::yield_now();
        }
    };
    // Warm the band: its first run arms nothing.
    pool.request_cell(CallbackPriority::High, &cell).unwrap();
    wait_runs(1);
    WORKER_ARMED.store(true, Ordering::Release);

    for i in 0..N {
        ARMED.with(|a| a.set(true));
        pool.request_cell(CallbackPriority::High, &cell).unwrap();
        ARMED.with(|a| a.set(false));
        wait_runs(i + 2);
    }
    // One more run disarms the worker, then the count is read.
    WORKER_ARMED.store(false, Ordering::Release);
    pool.request_cell(CallbackPriority::High, &cell).unwrap();
    wait_runs(N + 2);
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "{N} cell requests and their runs allocated"
    );

    // The boxed request the cell replaces is counted by the same gate.
    let (tx, rx) = mpsc::channel();
    ARMED.with(|a| a.set(true));
    pool.request(
        CallbackPriority::High,
        Box::new(move || tx.send(()).unwrap()),
    )
    .unwrap();
    ARMED.with(|a| a.set(false));
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        ALLOCS.load(Ordering::Relaxed) > 0,
        "the counter missed a Box"
    );
}
