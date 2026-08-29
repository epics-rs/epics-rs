//! The absence primitive's own falsifiability proof.
//!
//! `budget::barrier` exists because `sleep(N); assert!(nothing_happened())`
//! passes whether the code works or not. A replacement that could itself pass
//! vacuously would buy nothing, so each case below is the one a windowed
//! assertion gets wrong: the barrier that never came, and the denied event
//! that arrived on the far side of the window.

use std::time::Duration;

#[path = "common/budget.rs"]
mod budget;

/// Baseline: the barrier arrives with nothing denied before it.
#[test]
fn a_clean_run_returns_everything_up_to_the_barrier() {
    let mut src = vec!["hello", "world", "PROBE"].into_iter();
    let seen = budget::barrier::until(
        "clean",
        |s: &&str| *s == "DENIED",
        |s: &&str| *s == "PROBE",
        |_| src.next(),
    );
    assert_eq!(seen, vec!["hello", "world", "PROBE"]);
}

/// The whole point: the denied event fails the test, deterministically,
/// however long it took to arrive. A window would have passed if it were slow.
#[test]
#[should_panic(expected = "\"DENIED\" arrived before the barrier")]
fn a_denied_event_before_the_barrier_fails() {
    let mut src = vec!["hello", "DENIED", "PROBE"].into_iter();
    budget::barrier::until(
        "denied-first",
        |s: &&str| *s == "DENIED",
        |s: &&str| *s == "PROBE",
        |_| src.next(),
    );
}

/// The claim is only about what precedes the barrier. Anything after it is
/// outside the ordering the probe established, so it must not fail the test —
/// otherwise callers would need a second window to bound "after".
#[test]
fn a_denied_event_after_the_barrier_is_not_the_claim() {
    let mut src = vec!["PROBE", "DENIED"].into_iter();
    let seen = budget::barrier::until(
        "denied-after",
        |s: &&str| *s == "DENIED",
        |s: &&str| *s == "PROBE",
        |_| src.next(),
    );
    assert_eq!(seen, vec!["PROBE"]);
}

/// A source that ends without the barrier proves nothing about absence, so it
/// fails rather than passing on an empty observation — the exact way a
/// windowed assertion passes when the peer is simply dead.
#[test]
#[should_panic(expected = "the barrier never arrived")]
fn a_barrier_that_never_arrives_is_vacuous_and_fails() {
    let mut src = vec!["hello"].into_iter();
    budget::barrier::until(
        "no-barrier",
        |s: &&str| *s == "DENIED",
        |s: &&str| *s == "PROBE",
        |_| src.next(),
    );
}

/// The caller is handed the budget that remains, not a slice of it: one call
/// that blocks for the whole budget must be the last one.
#[test]
fn the_remaining_budget_shrinks_across_reads() {
    let mut handed: Vec<Duration> = Vec::new();
    let mut n = 0;
    let seen = budget::barrier::until(
        "budget",
        |_: &u8| false,
        |v: &u8| *v == 2,
        |remaining| {
            handed.push(remaining);
            std::thread::sleep(Duration::from_millis(20));
            n += 1;
            Some(n)
        },
    );
    assert_eq!(seen, vec![1, 2]);
    assert!(handed[0] <= budget::FACT_BUDGET, "{handed:?}");
    assert!(handed[1] < handed[0], "the budget must shrink: {handed:?}");
}

/// The async twin carries the same rule; the suites that read an `mpsc` or a
/// socket are async and must not need a second, weaker primitive.
///
/// Gated out of the exec-backend suite because the read closure it stands in
/// for is a `tokio::time::timeout`, and the `exec_backend` background executor
/// starts no tokio timer. The async twin itself has no callers there: the
/// suites that use it are async and are gated out for the same reason.
#[cfg(tokio_backend)]
#[tokio::test]
#[should_panic(expected = "\"DENIED\" arrived before the barrier")]
async fn the_async_twin_fails_on_a_denied_event_too() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<&str>();
    for f in ["hello", "DENIED", "PROBE"] {
        tx.send(f).unwrap();
    }
    budget::barrier::until_async(
        "async-denied",
        |s: &&str| *s == "DENIED",
        |s: &&str| *s == "PROBE",
        async |remaining| {
            tokio::time::timeout(remaining, rx.recv())
                .await
                .ok()
                .flatten()
        },
    )
    .await;
}
