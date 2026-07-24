//! The real-time scheduling switch is off on a HOSTED target unless someone
//! asks for it. (On RTEMS the default is the other way — see
//! `task::DEFAULT_POLICY` for why the two targets differ. This is an
//! integration test binary, so it only ever runs hosted.)
//!
//! `task::apply_to_current_thread` can put an IOC thread into a
//! SCHED_FIFO band. Doing that without being asked is wrong twice over: on a
//! desktop the request fails (no `CAP_SYS_NICE`, `RLIMIT_RTPRIO` 0), and on a
//! box where it *succeeds* an unattended RT band can starve the machine. So
//! the guarantee is not "the request usually fails" but "the request is never
//! made" — see `task::RT_PRIORITY_ENV`.
//!
//! This lives in its own integration binary on purpose. The claim is about a
//! whole *process* making zero scheduler calls, and
//! `task::sched_calls_made` is a process-global count; a unit test
//! sharing a process with switch-on tests could not assert zero. One test per
//! file keeps the process pure under every runner.

use epics_libcom_rs::runtime::task::{
    PriorityApplied, RT_PRIORITY_ENV, RtPolicy, ThreadPriority, apply_to_current_thread,
    sched_calls_made,
};

#[test]
fn a_default_process_never_calls_the_os_scheduler() {
    assert_eq!(
        std::env::var_os(RT_PRIORITY_ENV),
        None,
        "this test measures the default; run it with {RT_PRIORITY_ENV} unset"
    );
    assert_eq!(
        RtPolicy::current(),
        RtPolicy::Disabled,
        "an unset switch must resolve to off on a hosted target"
    );

    // Every priority a runtime or background thread carries today: the
    // callback bands (ScanLow-1 / ScanLow+4 / ScanHigh+1), the delayed-callback
    // timer (ScanHigh), the scanOnce worker (ScanLow), and the blocking CA
    // server's receiver/sender pair (CAServerLow, CAServerLow-1).
    let wired = [
        ThreadPriority::Custom(ThreadPriority::ScanLow.value() - 1),
        ThreadPriority::Custom(ThreadPriority::ScanLow.value() + 4),
        ThreadPriority::Custom(ThreadPriority::ScanHigh.value() + 1),
        ThreadPriority::ScanHigh,
        ThreadPriority::ScanLow,
        ThreadPriority::CaServerLow,
        ThreadPriority::Custom(ThreadPriority::CaServerLow.value() - 1),
    ];
    for p in wired {
        assert_eq!(
            apply_to_current_thread(p),
            PriorityApplied::Disabled,
            "{p:?} must not reach the scheduler with the switch off"
        );
    }

    // Off other threads too — the IOC applies these from freshly spawned
    // workers, not from the thread that read the switch.
    let workers: Vec<_> = wired
        .into_iter()
        .map(|p| std::thread::spawn(move || apply_to_current_thread(p)))
        .collect();
    for w in workers {
        assert_eq!(
            w.join().expect("worker panicked"),
            PriorityApplied::Disabled
        );
    }

    assert_eq!(
        sched_calls_made(),
        0,
        "a default process must make no pthread_setschedparam call at all — \
         not even the one-off range probe"
    );
}
