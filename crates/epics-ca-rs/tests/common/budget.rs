#![allow(dead_code)] // Included by `#[path]` into suites that use only some of it.

//! The one budget a test in this crate may put on a fact it expects to happen.
//!
//! # Why one number, and why this number
//!
//! A per-call literal like `Duration::from_secs(5)` is not a bound on the code
//! under test — it is a bet on how much CPU the machine will give this process
//! while the rest of the suite runs. Two of those bets have already been lost
//! on this workspace, both in the 5 s band and both unreproducible on a rerun:
//!
//! - `epics-ca-rs::calink ca_link_init_ready_flips_after_metadata_fetch`,
//!   `calink.rs:150` — "init_ready stayed false on a connected link" after 5 s,
//!   once in two full-workspace runs; 0/20 on an isolated rerun at 1-min load
//!   231–286.
//! - `epics-ca-rs::asg_inp_change_reevaluates_access
//!   opening_the_gate_grants_write_to_a_connected_client`,
//!   `asg_inp_change_reevaluates_access.rs:122` — "no CA_PROTO_ACCESS_RIGHTS
//!   carrying write=true within 5 s", at 1-min load 447→563 while every
//!   sibling panel ran its own workspace gate.
//!
//! Neither is a defect in the code they cover. An isolated rerun cannot see a
//! contention failure, which is exactly why the first sighting of each survived
//! as "load".
//!
//! [`FACT_BUDGET`] is **derived, not chosen**. `.config/nextest.toml` kills a
//! test at `slow-timeout.period × terminate-after`: 30 s × 4 = 120 s on
//! `profile.default`, 60 s × 4 = 240 s on `profile.ci` and `profile.interop`.
//! That kill is the clock that should decide a wait, because it is the only one
//! that scales with nothing an individual test controls. A test-local budget
//! exists for one reason — so a genuine failure reports with the test's own
//! message instead of a bare SIGTERM — so it is set to half the shortest kill,
//! leaving the other half for the test to print what it saw.
//!
//! It is **not** a performance assertion. A test that needs to assert a
//! latency asserts it on a measured elapsed time, not by picking a small
//! budget and hoping the scheduler cooperates.
//!
//! # What still carries its own number
//!
//! A duration that is the evidence rather than a bound on it: a per-attempt
//! bound inside a retry loop whose expiry retries rather than fails, a proxy's
//! per-datagram forwarding bound, and an assertion whose subject *is* an
//! elapsed time (`blocking_real_record_e2e.rs`'s `read_at < ODLY`). Those are
//! listed in the sweep that introduced this file.
//!
//! One entry left that list. "The window a test spends proving something did
//! *not* happen" used to be on it, and it was the one case where a duration
//! could never be evidence — see [`barrier`].

use std::time::Duration;

/// Half of nextest's shortest terminate-after (30 s × 4 = 120 s) — see the
/// module docs for the derivation and for what deliberately does not use it.
pub const FACT_BUDGET: Duration = Duration::from_secs(60);

/// Absence closes on a causal barrier, never on a window.
///
/// # The defect this replaces
///
/// `sleep(250 ms); assert!(nothing_arrived())` asserts that the code under
/// test did not do something. It cannot: the window is evidence about the
/// scheduler, not about the protocol. Reintroduce the behaviour the test
/// denies and the test still passes whenever the peer is 251 ms slow than the
/// run that set the number — so the test can only ever fail *flakily*, and
/// never fails for the reason it was written. Twenty-three of these were
/// standing in this crate.
///
/// # The rule
///
/// An absence claim must close on an observation that is causally ordered
/// **after** the denied event on every path where the denied event occurs.
/// In practice: send a probe whose reply the peer can only produce after it
/// has produced anything it was going to produce for the earlier stimulus,
/// then require the probe's reply to arrive with nothing denied before it.
///
/// A CA circuit gives that for free — one TCP stream, one server-side message
/// thread, replies in order — so a `CA_PROTO_READ_NOTIFY` round trip is a
/// barrier for every frame the server was going to send earlier.
///
/// # The one duration
///
/// [`FACT_BUDGET`], bounding the wait for the *barrier*. That is a presence
/// wait: a peer that never answers fails the test with the test's own message
/// rather than a bare SIGTERM. No caller passes a duration, so no call site
/// can re-open the defect by choosing a number.
pub mod barrier {
    use std::fmt::Debug;
    use std::time::{Duration, Instant};

    fn vacuous<T: Debug>(what: &str, seen: &[T]) -> ! {
        panic!(
            "{what}: the barrier never arrived within {:?}, so the absence \
             claim proves nothing; saw {seen:?}",
            super::FACT_BUDGET
        )
    }

    fn violated<T: Debug>(what: &str, item: &T, seen: &[T]) -> ! {
        panic!("{what}: {item:?} arrived before the barrier; saw {seen:?} first")
    }

    /// Read an ordered source until `barrier` matches, failing if `denied`
    /// matches first. Returns everything observed, barrier included.
    ///
    /// `next(remaining)` yields the next observation, or `None` when the
    /// source ended or `remaining` elapsed with nothing — either way the
    /// barrier did not arrive, which fails the test rather than passing it.
    pub fn until<T: Debug>(
        what: &str,
        denied: impl Fn(&T) -> bool,
        barrier: impl Fn(&T) -> bool,
        mut next: impl FnMut(Duration) -> Option<T>,
    ) -> Vec<T> {
        let deadline = Instant::now() + super::FACT_BUDGET;
        let mut seen: Vec<T> = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                vacuous(what, &seen);
            }
            let Some(item) = next(remaining) else {
                vacuous(what, &seen);
            };
            if denied(&item) {
                violated(what, &item, &seen);
            }
            let done = barrier(&item);
            seen.push(item);
            if done {
                return seen;
            }
        }
    }

    /// [`until`] for a source read by an async call.
    pub async fn until_async<T: Debug>(
        what: &str,
        denied: impl Fn(&T) -> bool,
        barrier: impl Fn(&T) -> bool,
        mut next: impl AsyncFnMut(Duration) -> Option<T>,
    ) -> Vec<T> {
        let deadline = Instant::now() + super::FACT_BUDGET;
        let mut seen: Vec<T> = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                vacuous(what, &seen);
            }
            let Some(item) = next(remaining).await else {
                vacuous(what, &seen);
            };
            if denied(&item) {
                violated(what, &item, &seen);
            }
            let done = barrier(&item);
            seen.push(item);
            if done {
                return seen;
            }
        }
    }
}
