//! A callback request that finds its band's run-queue supply empty grows the
//! supply on its own thread. A thread descheduled in the middle of that
//! growth — a low-priority requester preempted inside the chunk allocation —
//! must not hold up any other requester: C allocates its queue in
//! `callbackInit` (`callback.c:306`), so no request waits on an allocation.

use std::sync::{Arc, mpsc};

use epics_libcom_rs::runtime::background::{
    CallbackBody, CallbackCell, CallbackPool, CallbackPriority,
};
use fact_budget::FACT_BUDGET;

use crate::Gate;

/// Reports its own name each time it runs.
struct Ran(mpsc::Sender<&'static str>, &'static str);

impl CallbackBody for Ran {
    fn run(&self) {
        let _ = self.0.send(self.1);
    }
}

#[test]
fn a_request_is_queued_and_run_while_another_requesters_growth_is_stalled() {
    // A 64-slot ring fills the arena's first chunk exactly, so the band has no
    // run-queue node to spare and its first cell request grows it.
    let pool = CallbackPool::with_config(64, 1);
    let handle = pool.handle();
    let gate = Gate::new();
    let (ran, runs) = mpsc::channel();
    let a = CallbackCell::new(Ran(ran.clone(), "A"));
    let b = CallbackCell::new(Ran(ran, "B"));

    let stalled_requester = {
        let (handle, a) = (handle.clone(), Arc::clone(&a));
        std::thread::spawn(move || {
            gate.arm_this_thread();
            handle.request_cell(CallbackPriority::High, &a)
        })
    };
    gate.wait_stalled("A's request never grew the band's run-queue supply");

    let (returned, b_returned) = mpsc::channel();
    let other_requester = {
        let b = Arc::clone(&b);
        std::thread::spawn(move || {
            let _ = returned.send(handle.request_cell(CallbackPriority::High, &b));
        })
    };
    let b_meanwhile = b_returned.recv_timeout(FACT_BUDGET);
    let b_ran = match b_meanwhile {
        Ok(_) => runs.recv_timeout(FACT_BUDGET).ok(),
        Err(_) => None,
    };

    gate.release();
    let a_result = stalled_requester.join().unwrap();
    other_requester.join().unwrap();

    assert!(
        matches!(b_meanwhile, Ok(Ok(()))),
        "B's request waited for A's stalled growth: {b_meanwhile:?}"
    );
    assert_eq!(b_ran, Some("B"), "B's callback did not run while A stalled");
    assert!(a_result.is_ok(), "A's request failed: {a_result:?}");
    assert_eq!(
        runs.recv_timeout(FACT_BUDGET),
        Ok("A"),
        "A's callback did not run once its growth resumed"
    );
}
