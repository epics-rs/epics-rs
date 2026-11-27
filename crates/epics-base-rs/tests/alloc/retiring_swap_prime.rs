//! `prime_current_thread` takes `arc_swap`'s per-thread debt node at thread
//! start, so a cycle's first `RetiringSwap::load` allocates nothing. Counted
//! with the alloc binary's allocator; only the calling thread's allocations
//! inside `count` are seen.

use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use alloc_count::thread;
use epics_base_rs::sync::{RetiringSwap, prime_current_thread};

fn count<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let (out, counts) = thread::counted(f);
    (out, counts.allocs)
}

/// An unprimed thread's first load, counted; the thread keeps its debt node
/// until `hold` is dropped.
fn unprimed_first_load(swap: &Arc<RetiringSwap<u32>>) -> (u64, mpsc::Sender<()>, JoinHandle<()>) {
    let swap = swap.clone();
    let (hold, held) = mpsc::channel::<()>();
    let (first, first_allocs) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        first.send(count(|| *swap.load())).unwrap();
        let _ = held.recv();
    });
    let (v, allocs) = first_allocs.recv().unwrap();
    assert_eq!(v, 7);
    (allocs, hold, thread)
}

/// The debt nodes of threads that have exited stay on `arc_swap`'s
/// process-wide list for any thread to claim without allocating, so unprimed
/// threads are held alive until one's first load allocates: the list then has
/// no node for the primed thread's load to claim.
#[test]
fn a_primed_thread_loads_without_allocating() {
    let swap = Arc::new(RetiringSwap::new(Arc::new(7u32)));
    let mut unprimed = Vec::new();
    loop {
        let (allocs, hold, thread) = unprimed_first_load(&swap);
        unprimed.push((hold, thread));
        if allocs > 0 {
            break;
        }
        assert!(
            unprimed.len() < 4096,
            "no unprimed thread's first load allocated"
        );
    }

    let primed = {
        let swap = swap.clone();
        std::thread::spawn(move || {
            prime_current_thread();
            count(|| *swap.load())
        })
    };
    assert_eq!(primed.join().unwrap(), (7, 0));
    for (hold, thread) in unprimed {
        drop(hold);
        thread.join().unwrap();
    }
}
