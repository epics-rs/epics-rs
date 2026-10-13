//! `InterruptManager::accepting` runs from `call_param_callbacks`, which a
//! synchronous port's write reaches under the record's hold. This pins that
//! the read allocates and frees nothing, for a bound and an unbound port and
//! after an open gate was dropped, and that it returns the right answer.
//! It does not detect a lock: an allocation-free mutex read passes. That
//! property is by construction (an atomic count and an append-only chain).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use asyn_rs::interrupt::InterruptManager;
use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static FREES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            FREES.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn counted<R>(f: impl FnOnce() -> R) -> (R, usize, usize) {
    let (a0, f0) = (ALLOCS.load(Ordering::SeqCst), FREES.load(Ordering::SeqCst));
    COUNTING.with(|c| c.set(true));
    let r = f();
    COUNTING.with(|c| c.set(false));
    (
        r,
        ALLOCS.load(Ordering::SeqCst) - a0,
        FREES.load(Ordering::SeqCst) - f0,
    )
}

#[test]
fn a_bound_and_an_unbound_port_read_without_allocating_or_freeing() {
    let bound = InterruptManager::new(4);
    let unbound = InterruptManager::new(4);
    let mine = InterruptGate::new();
    bound.bind_gate(mine.clone());
    bound.bind_gate(InterruptGate::new());

    // A gate that was open and is dropped before the reads: a design that
    // keeps a registry of gates frees it on the read side.
    let gone = InterruptGate::opened();
    drop(gone);
    let live = InterruptGate::opened();

    let (open, allocs, frees) = counted(|| {
        let mut open = 0;
        for _ in 0..1000 {
            open += usize::from(unbound.accepting()) + usize::from(bound.accepting());
        }
        open
    });
    assert_eq!(
        open, 1000,
        "unbound reads the live gate, bound its closed own"
    );
    assert_eq!((allocs, frees), (0, 0));
    drop(live);
}
