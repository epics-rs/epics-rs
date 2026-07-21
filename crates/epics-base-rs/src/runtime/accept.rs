//! Backoff for a server accept loop, shared by every accept loop in the
//! workspace.
//!
//! # The defect this exists to close
//!
//! `accept()` can fail without the listener being broken — `EMFILE`/`ENFILE`
//! when the fd table is full, `ENOMEM`/`ENOBUFS` under memory pressure. On
//! those the pending connection **stays queued**, so a loop that logs and
//! immediately retries spins at 100% CPU and floods the log, at exactly the
//! moment the machine has no resources to spare. Two of this workspace's
//! accept loops did that.
//!
//! C `rsrv` has the shape: `epicsThreadSleep(15.0); continue;` at all three of
//! its failure points (`caservertask.c:92`, `:102`, `:118`).
//!
//! # Why the retry/give-up decision is not made from the error
//!
//! The obvious design is to classify the `io::Error` — retry `EMFILE`, return
//! on `EBADF`. **Measured on this host, that is not expressible:**
//!
//! | errno | `io::ErrorKind` |
//! |---|---|
//! | `EBADF` (9) — listener fd is gone | `Uncategorized` |
//! | `ENOTSOCK` (88) — not a socket | `Uncategorized` |
//! | `EMFILE` (24) — fd table full, *transient* | `Uncategorized` |
//! | `EINVAL` (22) | `InvalidInput` |
//!
//! The fatal cases and the transient one collapse into the same variant, so
//! `ErrorKind` cannot separate them. Matching `raw_os_error()` instead would
//! work per-platform, but the one kind that *is* distinguishable —
//! `InvalidInput` — is precisely what RTEMS returns spuriously from socket
//! calls while its libc omits the BSD `sin_len` byte, so a rule keyed on it
//! would make every RTEMS accept loop quit on its first call.
//!
//! So the decision is made from **behaviour over time, not from the error**:
//! a listener that has failed [`GIVE_UP_AFTER`] times in a row with no
//! successful accept in between is not serving anyone, whatever the errno
//! says. One success resets the count, which is what keeps a busy server
//! under sustained `EMFILE` alive — it accepts whenever an fd frees.
//!
//! # Deviation from C, deliberate
//!
//! C sleeps a flat 15 s and never returns; the thread is pinned forever on a
//! listener that will never accept again. This backs off geometrically from
//! [`FIRST`] to [`CEILING`] — recovering from a transient blip about 15×
//! faster than C while still capping the log at one line per second once
//! saturated — and returns after [`GIVE_UP_AFTER`] unbroken failures so a dead
//! listener does not pin a thread for the life of the IOC.

use std::time::Duration;

/// Delay after the first failure. Doubles per consecutive failure.
pub const FIRST: Duration = Duration::from_millis(25);

/// Longest delay between two accept attempts. Also the steady-state log rate
/// while a listener stays broken: one line per ceiling.
pub const CEILING: Duration = Duration::from_secs(1);

/// Consecutive failures — with **no** successful accept in between — after
/// which the listener is declared dead and the loop returns.
///
/// With [`FIRST`] and [`CEILING`] as they are, this is roughly a minute of
/// unbroken failure. Long enough that ordinary resource exhaustion recovers
/// (any single success resets the count), short enough that a thread parked on
/// a broken listener is not a permanent leak.
pub const GIVE_UP_AFTER: u32 = 64;

/// What the loop must do after a failed `accept()`.
///
/// `#[must_use]`: dropping this is the original defect — a failure that is
/// neither waited on nor returned from is the hot spin.
#[must_use = "an accept failure must be either backed off or returned from, never ignored"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptRetry {
    /// Wait this long, then accept again. The caller does the waiting, because
    /// only it knows whether it is a thread (`thread::sleep`) or a task
    /// (`tokio::time::sleep`).
    After(Duration),
    /// [`GIVE_UP_AFTER`] consecutive failures. Leave the loop.
    GiveUp,
}

/// Consecutive-failure counter for one accept loop.
///
/// Hold one per loop, call [`accepted`](Self::accepted) on every success and
/// [`failed`](Self::failed) on every failure.
#[derive(Debug, Clone, Default)]
pub struct AcceptBackoff {
    consecutive: u32,
}

impl AcceptBackoff {
    pub const fn new() -> Self {
        Self { consecutive: 0 }
    }

    /// A connection was accepted: the listener works, so forget the history.
    ///
    /// This is what makes the give-up rule safe under sustained fd exhaustion
    /// — a server that still lands the occasional connection never reaches
    /// [`GIVE_UP_AFTER`].
    pub fn accepted(&mut self) {
        self.consecutive = 0;
    }

    /// An `accept()` failed. Returns the delay to wait, or [`AcceptRetry::GiveUp`].
    pub fn failed(&mut self) -> AcceptRetry {
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive > GIVE_UP_AFTER {
            return AcceptRetry::GiveUp;
        }
        // Doubling, saturating at the ceiling. `checked_shl`-free: the shift
        // amount is clamped so it can never reach the width of the type.
        let shift = (self.consecutive - 1).min(32);
        let delay = FIRST
            .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
            .min(CEILING);
        AcceptRetry::After(delay)
    }

    /// Failures since the last success. Diagnostics and tests.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_failure_waits_and_does_not_spin() {
        let mut b = AcceptBackoff::new();
        assert_eq!(b.failed(), AcceptRetry::After(FIRST));
    }

    #[test]
    fn the_delay_doubles_then_saturates_at_the_ceiling() {
        let mut b = AcceptBackoff::new();
        let mut seen = Vec::new();
        for _ in 0..10 {
            match b.failed() {
                AcceptRetry::After(d) => seen.push(d),
                AcceptRetry::GiveUp => panic!("gave up inside the backoff ramp"),
            }
        }
        assert_eq!(seen[0], FIRST);
        assert_eq!(seen[1], FIRST * 2);
        assert_eq!(seen[2], FIRST * 4);
        // Monotone, and never past the ceiling.
        for w in seen.windows(2) {
            assert!(w[1] >= w[0], "backoff went backwards: {seen:?}");
        }
        assert_eq!(*seen.last().unwrap(), CEILING);
    }

    /// The boundary that keeps a busy server alive under sustained `EMFILE`:
    /// one accepted connection erases the whole failure history.
    #[test]
    fn one_success_resets_the_give_up_budget() {
        let mut b = AcceptBackoff::new();
        for _ in 0..GIVE_UP_AFTER {
            assert_ne!(b.failed(), AcceptRetry::GiveUp);
        }
        assert_eq!(b.consecutive_failures(), GIVE_UP_AFTER);
        b.accepted();
        assert_eq!(b.consecutive_failures(), 0);
        assert_eq!(b.failed(), AcceptRetry::After(FIRST));
    }

    /// The other boundary: exactly `GIVE_UP_AFTER` failures still retry, the
    /// next one returns.
    #[test]
    fn give_up_lands_one_past_the_budget() {
        let mut b = AcceptBackoff::new();
        for i in 1..=GIVE_UP_AFTER {
            assert_ne!(b.failed(), AcceptRetry::GiveUp, "gave up early at {i}");
        }
        assert_eq!(b.failed(), AcceptRetry::GiveUp);
    }

    /// Once it gives up it stays given up — a loop that ignores the first
    /// `GiveUp` must not be told to retry on the next failure.
    #[test]
    fn give_up_is_sticky() {
        let mut b = AcceptBackoff::new();
        for _ in 0..=GIVE_UP_AFTER {
            let _ = b.failed();
        }
        for _ in 0..5 {
            assert_eq!(b.failed(), AcceptRetry::GiveUp);
        }
    }

    /// A permanently broken listener must not pin a thread for the life of the
    /// IOC (C's `caservertask.c` behaviour, deliberately not copied), and must
    /// not give up so fast that a slow resource recovery is fatal.
    #[test]
    fn the_give_up_budget_is_about_a_minute_of_unbroken_failure() {
        let mut b = AcceptBackoff::new();
        let mut total = Duration::ZERO;
        while let AcceptRetry::After(d) = b.failed() {
            total += d;
        }
        assert!(
            total >= Duration::from_secs(30) && total <= Duration::from_secs(120),
            "give-up window is {total:?}; under 30s risks killing a recoverable \
             listener, over 120s is a thread parked on a dead one"
        );
    }
}
