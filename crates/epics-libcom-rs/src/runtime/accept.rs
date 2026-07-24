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
//! So there is no decision to make: **the loop always retries**, exactly as C
//! does. The only thing this type computes is how long to wait first.
//!
//! # There is no give-up, and that is C parity
//!
//! An earlier revision returned after 64 consecutive failures. That was a
//! deliberate deviation and it was wrong: C `rsrv` has no such path
//! (`caservertask.c:82-121` — `errlogPrintf`, `epicsThreadSleep(15.0)`,
//! `continue`, at every one of its three failure points, with
//! `cantProceed("Unreachable.  Perpetual thread.")` after the loop to say so).
//! On target the deviation meant an IOC that had seen a minute of `EMFILE`
//! stopped accepting **for the life of the process** while every other part of
//! it went on looking healthy — the worst available outcome, and unrecoverable
//! without a reboot, where C would have resumed the moment an fd freed.
//!
//! What is kept from that revision is the geometric backoff: [`FIRST`] to
//! [`CEILING`], which recovers from a transient blip about 15× faster than C's
//! flat 15 s while still capping the retry rate once saturated.
//!
//! # Saying so without a subscriber
//!
//! C prints on *every* failed accept, through `errlogPrintf`, which reaches
//! the console whatever the log configuration. Ours does both: a `tracing`
//! event per failure for a host with a subscriber (C parity, exactly), and an
//! `errlog` line on the 1st, 2nd, 4th, 8th … consecutive failure, which
//! reaches an RTEMS console that has no subscriber at all
//! ([`crate::runtime::log::errlog_sev_printf`]). Powers of two because the
//! backoff ceiling is 1 s where C's is 15 s: printing every failure at our
//! cadence would be 15× C's console traffic on a serial line. It needs no
//! clock, and it cannot suppress the first occurrence.

use std::time::Duration;

/// Delay after the first failure. Doubles per consecutive failure.
pub const FIRST: Duration = Duration::from_millis(25);

/// Longest delay between two accept attempts. Also the steady-state log rate
/// while a listener stays broken: one line per ceiling.
pub const CEILING: Duration = Duration::from_secs(1);

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
    /// Only `accept()` itself feeds this counter, and on RTEMS that is not
    /// where a loaded server actually fails. Measured under qemu at the
    /// connection ceiling: `accept()` **succeeds** and the per-connection
    /// thread spawn then fails with `EAGAIN`, 64 times in a row, without a
    /// single `accept()` failure — because this call has already run by then.
    /// So the backoff describes a broken *listener*, not a full *server*; the
    /// refusal path announces the latter.
    pub fn accepted(&mut self) {
        self.consecutive = 0;
    }

    /// An `accept()` failed. Returns how long to wait before the next attempt.
    ///
    /// There is no other outcome: the loop retries forever, as C does. The
    /// caller does the waiting, because only it knows whether it is a thread
    /// (`thread::sleep`) or a task (`tokio::time::sleep`).
    ///
    /// Announces the failure on the way past — see the module docs on why an
    /// `errlog` line at powers of two, and not the `tracing` event alone,
    /// is what an RTEMS console can actually receive.
    ///
    /// `#[must_use]`: dropping the delay is the original defect — a failure
    /// that is not waited on is the hot spin.
    #[must_use = "an accept failure must be backed off, never ignored"]
    pub fn failed(&mut self) -> Duration {
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive.is_power_of_two() {
            crate::runtime::log::errlog_sev_printf(
                crate::runtime::log::ErrlogSevEnum::Major,
                &format!(
                    "accept failed {} time(s) in a row; the server is not taking \
                     connections and keeps retrying",
                    self.consecutive
                ),
            );
        }
        // Doubling, saturating at the ceiling. `checked_shl`-free: the shift
        // amount is clamped so it can never reach the width of the type.
        let shift = (self.consecutive - 1).min(32);
        FIRST
            .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
            .min(CEILING)
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
        assert_eq!(b.failed(), FIRST);
    }

    #[test]
    fn the_delay_doubles_then_saturates_at_the_ceiling() {
        let mut b = AcceptBackoff::new();
        let seen: Vec<Duration> = (0..10).map(|_| b.failed()).collect();
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
    fn one_success_resets_the_history() {
        let mut b = AcceptBackoff::new();
        for _ in 0..64 {
            let _ = b.failed();
        }
        assert_eq!(b.consecutive_failures(), 64);
        b.accepted();
        assert_eq!(b.consecutive_failures(), 0);
        assert_eq!(b.failed(), FIRST);
    }

    /// The boundary this type exists to hold after the give-up path was
    /// removed: C `rsrv` never leaves its accept loop
    /// (`caservertask.c:82-121`, and `cantProceed("Unreachable.  Perpetual
    /// thread.")` after it). Past the old 64-failure budget, and far past it,
    /// there must still be a delay to wait — never an exit.
    #[test]
    fn the_loop_still_retries_long_after_the_old_give_up_budget() {
        let mut b = AcceptBackoff::new();
        for _ in 0..1_000 {
            let d = b.failed();
            assert!(
                d > Duration::ZERO && d <= CEILING,
                "a failure past the old budget must still yield a bounded wait, got {d:?}"
            );
        }
        assert_eq!(b.consecutive_failures(), 1_000);
    }

    /// The counter must not wrap into a zero delay after a very long outage.
    #[test]
    fn a_saturated_counter_still_waits_the_ceiling() {
        let mut b = AcceptBackoff {
            consecutive: u32::MAX - 1,
        };
        assert_eq!(b.failed(), CEILING);
        assert_eq!(b.failed(), CEILING, "saturating_add must not wrap to 0");
        assert_eq!(b.consecutive_failures(), u32::MAX);
    }

    /// The console announcement is bounded and cannot be silenced at the
    /// first occurrence — the property the RTEMS console needs, since the
    /// per-failure `tracing` event reaches nothing there.
    #[test]
    fn the_console_announcement_is_first_then_powers_of_two() {
        let announced: Vec<u32> = (1..=64u32).filter(|n| n.is_power_of_two()).collect();
        assert_eq!(announced, vec![1, 2, 4, 8, 16, 32, 64]);
    }
}
