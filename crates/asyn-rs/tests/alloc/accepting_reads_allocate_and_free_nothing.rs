//! `InterruptManager::accepting` runs from `call_param_callbacks`, which a
//! synchronous port's write reaches under the record's hold. This pins that
//! the read allocates and frees nothing, for a bound and an unbound port and
//! after an open gate was dropped, and that it returns the right answer.
//! It does not detect a lock: an allocation-free mutex read passes. That
//! property is by construction (an atomic count and an append-only chain).

use alloc_count::{Counts, thread};
use asyn_rs::interrupt::InterruptManager;
use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

#[test]
fn an_owned_and_an_ownerless_port_read_without_allocating_or_freeing() {
    let ownerless = InterruptManager::new();
    let live = InterruptGate::opened();
    let open = InterruptManager::owned_by(
        live.clone(),
        asyn_rs::port::InterfaceMask::ALL,
        asyn_rs::port::InterfaceMask::ALL,
    );
    // An owner that was open and is dropped before the reads.
    let gone = InterruptGate::opened();
    let closed = InterruptManager::owned_by(
        gone.clone(),
        asyn_rs::port::InterfaceMask::ALL,
        asyn_rs::port::InterfaceMask::ALL,
    );
    gone.set(false);
    drop(gone);

    let (counts, Counts { allocs, frees, .. }) = thread::counted(|| {
        let mut n = (0, 0, 0);
        for _ in 0..1000 {
            n.0 += usize::from(ownerless.accepting());
            n.1 += usize::from(open.accepting());
            n.2 += usize::from(closed.accepting());
        }
        n
    });
    assert_eq!(counts, (0, 1000, 0), "ownerless and closed read false");
    assert_eq!((allocs, frees), (0, 0));
    drop(live);
}
