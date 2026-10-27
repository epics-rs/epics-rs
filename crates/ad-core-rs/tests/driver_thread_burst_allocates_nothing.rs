//! A driver's acquisition burst on its `run_thread_named` thread allocates
//! nothing once warm. Each frame takes a queued command, replaces
//! `pArrays[0]` (simDetector.cpp:694-699) and publishes it with a `trySend`
//! (NDPluginDriver.cpp:423); with every command ready the loop never parks,
//! so its coop budget runs out mid-burst and forces a yield.
//!
//! Only the driver's thread is counted, and not its fill of the frame.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ad_core_rs::ndarray::{NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::DriverPool;
use ad_core_rs::plugin::channel::{ArrayPublisher, NDArrayOutput, ndarray_channel};
use ad_core_rs::runtime as rt;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

const DRIVER: &str = "AllocCountDet";

thread_local! {
    /// The driver's fill of the frame, which is its compute and not the loop.
    static EXEMPT: Cell<bool> = const { Cell::new(false) };
    /// 0 not yet asked, 1 the driver's thread, 2 any other. Asking may itself
    /// allocate; that reentry reads 2 and is not counted.
    static ON_DRIVER: Cell<u8> = const { Cell::new(0) };
}

fn on_driver() -> bool {
    ON_DRIVER.with(|d| match d.get() {
        0 => {
            d.set(2);
            let yes = std::thread::current().name() == Some(DRIVER);
            d.set(if yes { 1 } else { 2 });
            yes
        }
        v => v == 1,
    })
}

struct Counting;

impl Counting {
    fn note(&self) {
        if COUNTING.load(Ordering::Relaxed)
            && !EXEMPT.try_with(Cell::get).unwrap_or(false)
            && on_driver()
        {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// SAFETY: every method forwards to `System` unchanged; the counter is an
// atomic, const-initialised thread-locals never allocate, and the one name
// lookup reenters as another thread's.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.note();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        self.note();
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn exempt<R>(f: impl FnOnce() -> R) -> R {
    EXEMPT.with(|e| e.set(true));
    let r = f();
    EXEMPT.with(|e| e.set(false));
    r
}

/// Past the 33rd frame, where the plugin queue grows the second of the two
/// 32-slot blocks it then recycles, and short of the 128 units of budget a
/// poll starts with, so the yield it forces lands in the count.
const WARM: usize = 40;
const FRAMES: usize = 512;

#[test]
fn a_driver_threads_frame_burst_allocates_nothing() {
    let (commands, mut cmd_rx) = rt::command_channel(WARM + FRAMES);
    for _ in 0..WARM + FRAMES {
        commands.try_send(()).unwrap();
    }

    let (sender, mut receiver) = ndarray_channel("SINK", 1);
    let seen = Arc::new(AtomicUsize::new(0));
    let consumer = {
        let seen = Arc::clone(&seen);
        std::thread::spawn(move || {
            while let Some(frame) = receiver.blocking_recv() {
                drop(frame);
                seen.fetch_add(1, Ordering::Release);
            }
        })
    };
    let mut output = NDArrayOutput::new();
    output.add(sender);
    let publisher = ArrayPublisher::new(Arc::new(parking_lot::Mutex::new(output)));
    let pool = DriverPool::standalone(0);

    rt::run_thread_named(DRIVER, move || async move {
        for i in 0..WARM + FRAMES {
            cmd_rx.recv().await.unwrap();
            if i == WARM {
                ALLOCS.store(0, Ordering::SeqCst);
                COUNTING.store(true, Ordering::SeqCst);
            }
            let frame = exempt(|| {
                pool.owner()
                    .replace_array(0, || {
                        pool.alloc(
                            vec![NDDimension::new(64), NDDimension::new(64)],
                            NDDataType::UInt16,
                        )
                        .ok()
                    })
                    .unwrap()
            });
            publisher.publish(frame).await;
            // Spun, not awaited: a park would hand the loop a fresh budget.
            while seen.load(Ordering::Acquire) <= i {
                std::thread::yield_now();
            }
        }
        COUNTING.store(false, Ordering::SeqCst);
    })
    .join()
    .unwrap();
    consumer.join().unwrap();

    assert_eq!(
        ALLOCS.load(Ordering::SeqCst),
        0,
        "{FRAMES} frames on the driver thread after {WARM} warm-up ones"
    );
}
