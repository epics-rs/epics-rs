//! A dedicated thread's loop allocates nothing at the yield its coop budget
//! forces mid-burst: the `block_on` driving it grows its deferred-waker list
//! there. A loop under a plain `block_on` is the control that the yield
//! counted grows it.

use alloc_count::thread;
use epics_libcom_rs::runtime::task::block_on_thread_loop;

/// Twice the budget tokio starts a poll with, so the burst meets the yield
/// it forces.
const BURST: usize = 256;

/// The allocations of a burst that never parks.
async fn burst() -> u64 {
    // Held across the awaits: both drivers poll it on the thread that made it.
    let window = thread::open();
    for _ in 0..BURST {
        tokio::task::coop::consume_budget().await;
    }
    window.close().allocs
}

fn runtime() -> tokio::runtime::Runtime {
    // RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — a current-thread runtime with no driver enabled needs no reactor and passes on the exec backend.
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

#[test]
fn a_thread_loops_forced_yield_allocates_nothing() {
    let control = std::thread::spawn(|| runtime().block_on(burst()))
        .join()
        .unwrap();
    assert!(
        control > 0,
        "a plain block_on's forced yield no longer grows anything"
    );
    let looped = std::thread::spawn(|| block_on_thread_loop(&runtime(), burst()))
        .join()
        .unwrap();
    assert_eq!(looped, 0);
}
