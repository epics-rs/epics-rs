//! Token-bucket throttler for limiting plugin output byte rate.
//!
//! Port of C++ `throttler.cpp` / `throttler.h`. Used by the plugin runtime
//! to enforce `MaxByteRate` on the output path: each emitted array consumes
//! tokens equal to its byte size; an array that cannot be paid for is dropped
//! and counted into `DroppedOutputArrays`.

use std::time::Instant;

/// Token-bucket rate limiter.
///
/// `limit` is the maximum tokens (bytes) per second. The bucket refills
/// continuously and is capped at `limit`. A `limit` of `0` disables
/// throttling entirely (`try_take` always succeeds).
pub struct Throttler {
    /// Max tokens per second (bucket capacity).
    limit: f64,
    /// Currently available tokens.
    available: f64,
    /// How much to refill per millisecond (`limit / 1000`).
    refill_amount: f64,
    /// When the last refill happened.
    last_refill: Instant,
}

impl Throttler {
    /// Create a throttler with the given per-second token limit.
    pub fn new(limit: f64) -> Self {
        let mut t = Self {
            limit: 0.0,
            available: 0.0,
            refill_amount: 0.0,
            last_refill: Instant::now(),
        };
        t.reset(limit);
        t
    }

    /// Reset the throttler with a new per-second limit. Refills the bucket
    /// to full. Matches C++ `Throttler::reset` (called by `writeFloat64`
    /// when `MaxByteRate` changes).
    pub fn reset(&mut self, limit: f64) {
        self.limit = limit;
        self.available = limit;
        self.refill_amount = limit / 1000.0;
        self.last_refill = Instant::now();
    }

    /// Refill the bucket based on elapsed time, returning available tokens.
    fn refill(&mut self) -> f64 {
        let now = Instant::now();
        let refill_count = (now.duration_since(self.last_refill).as_secs_f64() * 1000.0) as i64;
        if refill_count != 0 {
            self.available =
                self.limit.min(self.available + refill_count as f64 * self.refill_amount);
            self.last_refill = now;
        }
        self.available
    }

    /// Try to take `tokens` from the bucket. Returns `true` if the bucket
    /// had enough tokens (and they were consumed), `false` otherwise.
    ///
    /// When `limit == 0` throttling is disabled and this always returns `true`.
    pub fn try_take(&mut self, tokens: f64) -> bool {
        if self.limit == 0.0 {
            return true;
        }
        if tokens > self.refill() {
            return false;
        }
        self.available -= tokens;
        true
    }
}

impl Default for Throttler {
    fn default() -> Self {
        Self::new(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_disabled_when_zero_limit() {
        let mut t = Throttler::new(0.0);
        // Any size always passes when throttling is disabled.
        assert!(t.try_take(1_000_000_000.0));
        assert!(t.try_take(1_000_000_000.0));
    }

    #[test]
    fn test_bucket_drains_then_refuses() {
        // 1000 tokens/sec. Bucket starts full at 1000.
        let mut t = Throttler::new(1000.0);
        assert!(t.try_take(1000.0)); // drains the bucket
        // Immediately after, the bucket is empty — a non-trivial take fails.
        assert!(!t.try_take(500.0));
    }

    #[test]
    fn test_refill_over_time() {
        let mut t = Throttler::new(1000.0);
        assert!(t.try_take(1000.0)); // empty
        std::thread::sleep(Duration::from_millis(200));
        // After 200ms at 1000/sec, ~200 tokens refilled.
        assert!(t.try_take(150.0));
    }

    #[test]
    fn test_reset_refills_to_full() {
        let mut t = Throttler::new(1000.0);
        assert!(t.try_take(1000.0)); // empty
        t.reset(2000.0);
        // After reset the bucket is full at the new limit.
        assert!(t.try_take(2000.0));
    }
}
