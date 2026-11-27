//! The `tests/*.rs` of `epics-libcom-rs` that stall a thread inside an
//! allocation, built as one binary around the allocator that does it: a
//! replaced global allocator would sit under every test of `integration`, and
//! `alloc`'s counts every allocation.
//!
//! A thread armed with [`Gate::arm_this_thread`] stalls inside its next
//! allocation of [`CHUNK_SIZED`] bytes or more until [`Gate::release`];
//! everything else is forwarded unchanged.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fact_budget::FACT_BUDGET;

mod a_stalled_queue_growth_holds_up_nobody;

/// Nothing on the request path allocates this much but the run-queue chunk a
/// growth makes: the first one past a 64-slot ring is 128 nodes.
const CHUNK_SIZED: usize = 2048;

/// One stall, owned by one test.
#[derive(Default)]
struct Gate {
    stalled: AtomicBool,
    released: AtomicBool,
}

thread_local! {
    static ARMED: Cell<Option<&'static Gate>> = const { Cell::new(None) };
}

impl Gate {
    fn new() -> &'static Gate {
        Box::leak(Box::default())
    }

    /// Stall this thread's next chunk-sized allocation until [`Gate::release`].
    fn arm_this_thread(&'static self) {
        ARMED.with(|armed| armed.set(Some(self)));
    }

    fn wait_stalled(&self, never: &str) {
        let deadline = epics_libcom_rs::runtime::time::deadline_from_now(FACT_BUDGET);
        while !self.stalled.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "{never}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn release(&self) {
        self.released.store(true, Ordering::Release);
    }
}

struct StallArmed;

unsafe impl GlobalAlloc for StallArmed {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() >= CHUNK_SIZED
            && let Some(gate) = ARMED.with(Cell::take)
        {
            gate.stalled.store(true, Ordering::Release);
            while !gate.released.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: StallArmed = StallArmed;
