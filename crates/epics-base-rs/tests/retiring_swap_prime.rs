//! `prime_current_thread` takes `arc_swap`'s per-thread debt node at thread
//! start, so a cycle's first `RetiringSwap::load` allocates nothing. Counted
//! with this binary's own allocator; only the calling thread's allocations
//! inside `count` are seen.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use epics_base_rs::sync::{RetiringSwap, prime_current_thread};

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

/// One test, so the two threads run in order in one process: the unprimed
/// thread is still alive when the primed one starts, so its debt node is not
/// on the free list for the primed thread to reuse.
#[test]
fn a_primed_thread_loads_without_allocating() {
    let swap = Arc::new(RetiringSwap::new(Arc::new(7u32)));
    let (hold, held) = std::sync::mpsc::channel::<()>();
    let (first, first_allocs) = std::sync::mpsc::channel();
    let unprimed = {
        let swap = swap.clone();
        std::thread::spawn(move || {
            let (v, allocs) = count(|| *swap.load());
            first.send((v, allocs)).unwrap();
            held.recv().unwrap();
        })
    };
    let (v, allocs) = first_allocs.recv().unwrap();
    assert_eq!(v, 7);
    assert!(
        allocs > 0,
        "an unprimed thread's first load allocated nothing"
    );

    let primed = {
        let swap = swap.clone();
        std::thread::spawn(move || {
            prime_current_thread();
            count(|| *swap.load())
        })
    };
    assert_eq!(primed.join().unwrap(), (7, 0));
    hold.send(()).unwrap();
    unprimed.join().unwrap();
}
