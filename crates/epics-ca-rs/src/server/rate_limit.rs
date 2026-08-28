//! Per-client rate limiting via a token bucket.
//!
//! C rsrv has flow control (the priority gate that pauses monitor
//! sends when the kernel TCP buffer fills) but no notion of *refusing*
//! traffic from an abusive client — a runaway loop spamming `caput`
//! or `event_add` at line rate is forwarded into the record-processing
//! path until something further up trips. This module fills that gap:
//! every accepted CA message draws one token, and the bucket refills
//! at a configurable steady-state rate. When the bucket is empty the
//! caller's policy chooses what to do (drop the message, disconnect).
//!
//! The bucket is per-connection (per `ClientState`) so a misbehaving
//! client cannot starve well-behaved ones. The hot-path check is one
//! atomic load + one branch when rate limiting is disabled.

use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

/// Token bucket. `capacity` is the max burst; `refill_per_sec` is the
/// long-term sustainable rate. Both are configured at construction
/// time and immutable afterwards.
#[derive(Debug)]
pub struct RateLimiter {
    capacity: u64,
    refill_per_sec: u64,
    /// Tokens × 1000 (millitokens) so per-millisecond refill avoids
    /// integer-division zeroes at low rates. A bucket of 100 tokens
    /// is `100_000` here.
    millitokens: AtomicU64,
    /// Last refill instant; `last_refill_micros` since the bucket was
    /// created. The base `Instant` is captured once at construction.
    base: Instant,
    last_refill_micros: AtomicU64,
}

impl RateLimiter {
    /// Build a bucket. `capacity == 0` is treated as "disabled" by
    /// `try_acquire` returning Ok(()) unconditionally.
    pub fn new(capacity: u64, refill_per_sec: u64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            millitokens: AtomicU64::new(capacity.saturating_mul(1000)),
            base: Instant::now(),
            last_refill_micros: AtomicU64::new(0),
        }
    }

    /// Whether limiting is enabled. Pure helper for callers that want
    /// to skip work entirely when off.
    pub fn enabled(&self) -> bool {
        self.capacity > 0
    }

    /// Try to draw one token. Returns `Ok(())` when a token was
    /// available (or rate limiting is disabled), `Err(())` when the
    /// bucket is empty.
    #[allow(clippy::result_unit_err)]
    pub fn try_acquire(&self) -> Result<(), ()> {
        if !self.enabled() {
            return Ok(());
        }
        self.refill();
        // CAS-loop subtract one token (= 1000 millitokens).
        loop {
            let cur = self.millitokens.load(Ordering::Acquire);
            if cur < 1000 {
                return Err(());
            }
            let next = cur - 1000;
            if self
                .millitokens
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    /// Add tokens corresponding to elapsed time. Idempotent — called
    /// before every acquire; refills are bounded by `capacity`.
    fn refill(&self) {
        let now_micros = self.base.elapsed().as_micros() as u64;
        let last = self.last_refill_micros.load(Ordering::Acquire);
        if now_micros <= last {
            return;
        }
        let delta_micros = now_micros - last;
        // refill_per_sec tokens / 1_000_000 µs × delta = tokens added.
        // Multiply both sides by 1_000 (millitokens) and rearrange to
        // stay in u64. Use saturating ops so we never wrap.
        let added_milli = self
            .refill_per_sec
            .saturating_mul(delta_micros)
            .saturating_div(1_000);
        if added_milli == 0 {
            return;
        }
        // CAS-update last_refill_micros so two threads don't both add.
        if self
            .last_refill_micros
            .compare_exchange(last, now_micros, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return; // someone else refilled
        }
        let cap_milli = self.capacity.saturating_mul(1000);
        let mut cur = self.millitokens.load(Ordering::Acquire);
        loop {
            let next = cur.saturating_add(added_milli).min(cap_milli);
            match self.millitokens.compare_exchange_weak(
                cur,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => cur = observed,
            }
        }
    }
}

/// Configuration loaded from the environment. The defaults disable
/// rate limiting.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateLimitConfig {
    /// Steady-state rate (msgs/sec). Zero disables.
    pub msgs_per_sec: u64,
    /// Burst capacity (max tokens at any point). Zero disables.
    pub burst: u64,
    /// How many consecutive drops cause the connection to be torn
    /// down. Zero disables disconnect-on-strike.
    pub strike_threshold: u32,
}

impl RateLimitConfig {
    pub fn from_env() -> Self {
        let msgs_per_sec = epics_base_rs::runtime::env::get("EPICS_CAS_RATE_LIMIT_MSGS_PER_SEC")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let burst = epics_base_rs::runtime::env::get("EPICS_CAS_RATE_LIMIT_BURST")
            .and_then(|s| s.parse().ok())
            .unwrap_or(if msgs_per_sec > 0 {
                msgs_per_sec * 4
            } else {
                0
            });
        let strike_threshold = epics_base_rs::runtime::env::get("EPICS_CAS_RATE_LIMIT_STRIKES")
            .and_then(|s| s.parse().ok())
            .unwrap_or(100);
        Self {
            msgs_per_sec,
            burst,
            strike_threshold,
        }
    }

    pub fn build(&self) -> Option<RateLimiter> {
        if self.msgs_per_sec == 0 || self.burst == 0 {
            return None;
        }
        Some(RateLimiter::new(self.burst, self.msgs_per_sec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_always_ok() {
        let rl = RateLimiter::new(0, 0);
        for _ in 0..1000 {
            assert!(rl.try_acquire().is_ok());
        }
    }

    #[test]
    fn empty_bucket_rejects() {
        let rl = RateLimiter::new(2, 1);
        assert!(rl.try_acquire().is_ok());
        assert!(rl.try_acquire().is_ok());
        assert!(rl.try_acquire().is_err());
    }

    #[test]
    fn refills_over_time() {
        let rl = RateLimiter::new(10, 1000); // 1 token/ms
        for _ in 0..10 {
            rl.try_acquire().unwrap();
        }
        assert!(rl.try_acquire().is_err());
        std::thread::sleep(Duration::from_millis(15));
        // After 15 ms at 1 token/ms we should have ~10 again (capped).
        for _ in 0..5 {
            assert!(rl.try_acquire().is_ok());
        }
    }

    #[test]
    fn config_from_env_defaults_disabled() {
        // Don't touch process env in tests; just check defaults.
        let cfg = RateLimitConfig::default();
        assert!(cfg.build().is_none());
    }
}
