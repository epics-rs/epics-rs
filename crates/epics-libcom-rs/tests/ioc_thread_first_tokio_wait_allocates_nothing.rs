//! An IOC thread's first wait in tokio outside a runtime allocates nothing:
//! tokio builds the thread's parker there, and a port actor makes that wait
//! when its channel's `try_recv` meets a send half done, mid-cycle. A thread
//! without the prologue is the control that the wait counted builds it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

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

/// The allocations of this thread's first tokio wait.
fn first_wait() -> usize {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tx.send(()).unwrap();
    COUNTING.with(|c| c.set(true));
    rx.blocking_recv().unwrap();
    COUNTING.with(|c| c.set(false));
    ALLOCS.with(Cell::get)
}

#[test]
fn the_first_tokio_wait_of_an_ioc_thread_allocates_nothing() {
    let control = std::thread::spawn(first_wait).join().unwrap();
    assert!(
        control > 0,
        "a plain thread's first wait no longer builds anything"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    MandatoryThread::new("tokiowait", ThreadPriority::Low, StackSizeClass::Small)
        .spawn(move || tx.send(first_wait()).unwrap())
        .join()
        .unwrap();
    assert_eq!(rx.recv().unwrap(), 0);
}
