//! A `CallbackCell` request allocates nothing, on the requesting thread or on
//! the band worker that runs it: C's `callbackRequest` queues the caller's own
//! `epicsCallback`, and the boxed `Callback` it replaces costs one allocation
//! per request. Counted per thread with the alloc binary's allocator.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use alloc_count::named::{self, Name};
use alloc_count::thread;
use epics_libcom_rs::runtime::background::{
    CallbackBody, CallbackCell, CallbackPool, CallbackPriority,
};

/// The pool's one high-priority worker.
static WORKER: [Name; 1] = [Name::Exact("cbHigh")];

const N: usize = 1000;

/// Counts its runs.
struct Body {
    runs: AtomicUsize,
}

impl CallbackBody for Body {
    fn run(&self) {
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
    // Warm the band: once the second run has started, the worker's way back
    // from its first, which the old per-run arming left out too, is over.
    for n in 1..=2 {
        pool.request_cell(CallbackPriority::High, &cell).unwrap();
        wait_runs(n);
    }
    let worker = named::open(&WORKER);

    let mut requests = 0;
    for i in 0..N {
        let (r, counts) = thread::counted(|| pool.request_cell(CallbackPriority::High, &cell));
        r.unwrap();
        requests += counts.allocs;
        wait_runs(i + 3);
    }
    // One more run, so the worker's way back from the last counted one is in.
    pool.request_cell(CallbackPriority::High, &cell).unwrap();
    wait_runs(N + 3);
    let [worker] = worker.close().map(|c| c.allocs);
    assert_eq!(
        (requests, worker),
        (0, 0),
        "{N} cell requests and their runs allocated (requester, worker)"
    );

    // The boxed request the cell replaces is counted the same way.
    let (tx, rx) = mpsc::channel();
    let (r, boxed) = thread::counted(|| {
        pool.request(
            CallbackPriority::High,
            Box::new(move || tx.send(()).unwrap()),
        )
    });
    r.unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(boxed.allocs > 0, "the counter missed a Box");
}
