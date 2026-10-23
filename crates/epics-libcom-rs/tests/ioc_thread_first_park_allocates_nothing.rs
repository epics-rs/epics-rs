//! An IOC thread's first park allocates nothing: `parking_lot_core` builds a
//! thread's park state there and grows its table to the live thread count, so
//! without `enter_ioc_thread` building it at entry the allocation lands in
//! whichever lock acquire first meets contention, under a record hold included.
//!
//! The table only grows at thresholds (live threads 6, 11, 22, 43, 86, 171,
//! 342, …), so one thread proves nothing. Each of `THREADS` IOC threads is
//! born only after the last one parked, and all stay alive until the end:
//! the live count climbs one by one through at least two thresholds, and the
//! thread that crosses one is the one that would allocate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::{Arc, Barrier, mpsc};
use std::time::Duration;

use epics_libcom_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const THREADS: usize = 400;

#[test]
fn the_first_park_of_an_ioc_thread_allocates_nothing() {
    let release = Arc::new(Barrier::new(THREADS + 1));
    let (tx, rx) = mpsc::channel();
    let mut threads = Vec::with_capacity(THREADS);
    for i in 0..THREADS {
        let release = Arc::clone(&release);
        let tx = tx.clone();
        threads.push(
            MandatoryThread::new(
                format!("park{i}"),
                ThreadPriority::Low,
                StackSizeClass::Small,
            )
            .spawn(move || {
                let lock = parking_lot::Mutex::new(());
                let mut guard = lock.lock();
                let cv = parking_lot::Condvar::new();
                COUNTING.with(|c| c.set(true));
                // A timed wait always parks, as a contended acquire does.
                let _ = cv.wait_for(&mut guard, Duration::from_millis(1));
                COUNTING.with(|c| c.set(false));
                tx.send(ALLOCS.with(Cell::get)).unwrap();
                release.wait();
            }),
        );
        let allocs = rx.recv().unwrap();
        assert_eq!(allocs, 0, "allocations in the first park of IOC thread {i}");
    }
    release.wait();
    for t in threads {
        t.join().unwrap();
    }
}
