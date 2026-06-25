//! Averaging device-support helper — matches C asyn's `*Average` DTYP variants.
//!
//! ## C reference
//!
//! C asyn has **no separate `asynInt32Average` interface**. The
//! `"asynInt32Average"` DTYP variant uses the regular `asynInt32`
//! interface; averaging is done on the device-support side inside
//! `interruptCallbackAverage` (`devAsynInt32.c:647-702`). The state is
//! just two scalars in `devPvt`:
//!
//! ```c
//! double sum;       // devAsynInt32.c:98
//! int    numAverage;
//! ```
//!
//! Every interrupt callback does:
//!
//! ```c
//! pPvt->numAverage++;
//! pPvt->sum += (double)value;
//! if (numAverage >= SVAL) {
//!     dval = sum / numAverage;     // arithmetic mean
//!     dval += (sum > 0.0) ? 0.5 : -0.5;  // round-to-nearest for Int32
//!     pPvt->numAverage = 0;
//!     pPvt->sum = 0.;
//! }
//! ```
//!
//! No bounded buffer, no drop-on-overflow, no oldest-sample eviction —
//! the accumulator simply grows in sum/count terms until the SVAL
//! threshold fires the mean computation and the reset.
//!
//! ## What this module provides
//!
//! [`SumAverager`] — a `(sum: f64, count: u64)` pair with the same
//! accumulate-then-reset semantic, available to Rust drivers that
//! want to mirror the C behaviour without re-implementing the
//! arithmetic. The interface is intentionally type-agnostic
//! (`push(sample: f64)`) so the same struct serves both Int32Average
//! and Float64Average DTYP equivalents.
//!
//! No trait surface is exposed: drivers that need the averaged
//! readback simply call `read_and_reset()` from their normal
//! `read_int32` / `read_float64` implementation when the DTYP is the
//! `*Average` variant — same as C devAsynInt32.

use std::sync::Mutex;

/// Sum-and-count averager. Matches C `devPvt.sum` + `devPvt.numAverage`
/// algorithm from `devAsynInt32.c:98-99`.
///
/// Locking is internal so the same instance can be shared across the
/// async interrupt callback and the synchronous record-process path
/// without external synchronisation.
pub struct SumAverager {
    state: Mutex<AverState>,
}

#[derive(Default)]
struct AverState {
    sum: f64,
    count: u64,
}

impl Default for SumAverager {
    fn default() -> Self {
        Self::new()
    }
}

impl SumAverager {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(AverState::default()),
        }
    }

    /// Accumulate one sample. C: `numAverage++; sum += (double)value;`
    /// (`devAsynInt32.c:665-666`).
    pub fn push(&self, sample: f64) {
        let mut s = self.state.lock().unwrap();
        s.sum += sample;
        // saturating_add avoids a debug-build overflow panic; C's `int`
        // counter wraps, and a saturated u64 count is no worse than that
        // for an averager that is never reset across 2^64 samples.
        s.count = s.count.saturating_add(1);
    }

    /// Return the arithmetic mean and clear sum/count.
    /// Empty accumulator returns `0.0` (matches C — the threshold
    /// check `numAverage >= SVAL` prevents zero-divide; if a caller
    /// pulls before any sample has arrived, the natural value is 0).
    pub fn read_and_reset(&self) -> f64 {
        let mut s = self.state.lock().unwrap();
        let m = if s.count == 0 {
            0.0
        } else {
            s.sum / s.count as f64
        };
        s.sum = 0.0;
        s.count = 0;
        m
    }

    /// Same mean computation without clearing — useful for diagnostics.
    pub fn peek(&self) -> f64 {
        let s = self.state.lock().unwrap();
        if s.count == 0 {
            0.0
        } else {
            s.sum / s.count as f64
        }
    }

    /// Current sample count.
    pub fn count(&self) -> u64 {
        self.state.lock().unwrap().count
    }

    /// Drop accumulated samples without computing a mean.
    pub fn reset(&self) {
        let mut s = self.state.lock().unwrap();
        s.sum = 0.0;
        s.count = 0;
    }

    /// Arithmetic mean + reset, or `None` when no samples have accumulated
    /// since the last drain. C `processAiAverage` treats `numAverage == 0`
    /// as "no data" — it sets `UDF_ALARM`/`INVALID`, `udf = 1`, and returns
    /// `-2` (leave VAL untouched) rather than averaging zero samples
    /// (`devAsynFloat64.c:721-725`). The `read_and_reset` family returns
    /// `0.0` there; this checked variant distinguishes "no data" so the
    /// caller can mirror C's return-`-2` path. Atomic (single lock) so the
    /// accumulating interrupt cannot race a sample in between the count
    /// check and the reset.
    pub fn read_and_reset_checked(&self) -> Option<f64> {
        let mut s = self.state.lock().unwrap();
        if s.count == 0 {
            return None;
        }
        let m = s.sum / s.count as f64;
        s.sum = 0.0;
        s.count = 0;
        Some(m)
    }

    /// Int32 variant of [`Self::read_and_reset_checked`] with C's
    /// round-to-nearest before truncation (`devAsynInt32.c:906-910` —
    /// `dval += (sum>0.0) ? 0.5 : -0.5`). `None` when no samples
    /// accumulated (C return `-2`).
    pub fn read_and_reset_int32_checked(&self) -> Option<i32> {
        let mut s = self.state.lock().unwrap();
        if s.count == 0 {
            return None;
        }
        let mut dval = s.sum / s.count as f64;
        dval += if s.sum > 0.0 { 0.5 } else { -0.5 };
        s.sum = 0.0;
        s.count = 0;
        Some(dval as i32)
    }

    /// Apply C's round-to-nearest for Int32 conversion
    /// (`devAsynInt32.c:680` — `dval += (sum>0.0) ? 0.5 : -0.5`),
    /// then truncate. Useful when the consuming record is an
    /// `aiRecord` reading an averaged Int32 channel.
    pub fn read_and_reset_int32(&self) -> i32 {
        let mut s = self.state.lock().unwrap();
        let n = s.count;
        let sum = s.sum;
        s.sum = 0.0;
        s.count = 0;
        if n == 0 {
            return 0;
        }
        let mut dval = sum / n as f64;
        dval += if sum > 0.0 { 0.5 } else { -0.5 };
        dval as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_of_four_samples() {
        let a = SumAverager::new();
        for v in [10.0, 20.0, 30.0, 40.0] {
            a.push(v);
        }
        assert_eq!(a.count(), 4);
        assert!((a.peek() - 25.0).abs() < 1e-9);
        assert!((a.read_and_reset() - 25.0).abs() < 1e-9);
        assert_eq!(a.count(), 0);
    }

    #[test]
    fn empty_returns_zero_mean() {
        let a = SumAverager::new();
        assert_eq!(a.peek(), 0.0);
        assert_eq!(a.read_and_reset(), 0.0);
    }

    #[test]
    fn accumulator_unbounded_no_drop_on_overflow() {
        // C asyn does NOT drop samples — sum/count grow until reset.
        // Verify the sum reflects every push (not a bounded window).
        let a = SumAverager::new();
        for v in 1..=100 {
            a.push(v as f64);
        }
        assert_eq!(a.count(), 100);
        // Sum of 1..=100 = 5050 → mean 50.5
        assert!((a.peek() - 50.5).abs() < 1e-9);
    }

    #[test]
    fn read_and_reset_int32_rounds_positive() {
        // C: dval += (sum>0) ? 0.5 : -0.5 → 25.49 + 0.5 = 25.99 → 25
        //                                  25.51 + 0.5 = 26.01 → 26
        let a = SumAverager::new();
        a.push(25.0);
        a.push(26.0);
        // mean = 25.5 → 25.5 + 0.5 = 26.0 → 26
        assert_eq!(a.read_and_reset_int32(), 26);
    }

    #[test]
    fn read_and_reset_int32_rounds_negative() {
        let a = SumAverager::new();
        a.push(-25.0);
        a.push(-26.0);
        // mean = -25.5 → -25.5 - 0.5 = -26.0 → -26
        assert_eq!(a.read_and_reset_int32(), -26);
    }

    #[test]
    fn read_and_reset_int32_empty_returns_zero() {
        let a = SumAverager::new();
        assert_eq!(a.read_and_reset_int32(), 0);
    }

    #[test]
    fn checked_drain_distinguishes_no_data_from_zero_mean() {
        // C `processAiAverage` keys on `numAverage == 0` (return -2, UDF), NOT
        // on the mean value — a genuine 0.0 mean (samples that sum to zero) is
        // data, while no samples at all is "no data". The checked variants must
        // return None ONLY when no samples accumulated.
        let a = SumAverager::new();
        assert_eq!(a.read_and_reset_checked(), None, "no samples → None");
        assert_eq!(a.read_and_reset_int32_checked(), None, "no samples → None");

        a.push(-5.0);
        a.push(5.0);
        // Mean is a true 0.0 — data, not "no data".
        assert_eq!(
            a.read_and_reset_checked(),
            Some(0.0),
            "samples summing to zero are data, not None"
        );
        assert_eq!(a.count(), 0, "checked drain resets");

        a.push(10.0);
        a.push(20.0);
        a.push(30.0);
        assert_eq!(a.read_and_reset_checked(), Some(20.0));
        assert_eq!(a.read_and_reset_checked(), None, "second drain has no data");
    }

    #[test]
    fn read_and_reset_int32_checked_rounds_then_resets() {
        let a = SumAverager::new();
        a.push(25.0);
        a.push(26.0);
        // mean 25.5 → +0.5 → 26.0 → 26 (same rounding as read_and_reset_int32).
        assert_eq!(a.read_and_reset_int32_checked(), Some(26));
        assert_eq!(a.count(), 0);
        assert_eq!(a.read_and_reset_int32_checked(), None);
    }

    #[test]
    fn reset_clears_without_computing() {
        let a = SumAverager::new();
        a.push(100.0);
        a.push(200.0);
        a.reset();
        assert_eq!(a.count(), 0);
        assert_eq!(a.peek(), 0.0);
    }
}
