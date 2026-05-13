//! Averaging device interfaces (asyn Issue #30).
//!
//! Mirror of C asyn's `asynInt32Average` / `asynFloat64Average`. A
//! driver registering these interfaces accumulates samples internally
//! and exposes an averaged readback on demand:
//!
//! - `read_*_average(user)` returns the mean of every sample the
//!   driver has buffered since the previous read AND clears the
//!   accumulator (sample-and-reset semantic — matches C asyn).
//! - `read_*_average_peek(user)` returns the mean without clearing,
//!   for clients that want to observe the rolling average without
//!   disturbing the next caller's window.
//!
//! Records that use this interface (typically an `ai` with
//! `DTYP="asynInt32Average"`) get a fresh window-mean on every
//! process cycle — useful for hardware oversampling drivers that
//! tick faster than the record scan rate.
//!
//! ## Implementation notes
//!
//! - The interface trait is intentionally small: just `read` +
//!   `peek` + `reset`. Drivers maintain their own buffer; helpers
//!   below provide a ready-to-use `RingAverager<T>` for the common
//!   case (fixed-size circular buffer, drop oldest sample on full).
//! - `f64` is the canonical accumulator type because it can hold an
//!   `i32` sample range without precision loss and the mean
//!   computation is one floating-point divide regardless of the
//!   stored type.

use crate::error::AsynResult;
use crate::user::AsynUser;
use std::collections::VecDeque;
use std::sync::Mutex;

/// `asynInt32Average` interface — averages i32 samples, returns f64.
pub trait AsynInt32Average: Send + Sync {
    /// Return the mean of buffered samples and clear the accumulator.
    /// Returns `0.0` for an empty buffer (canonical C asyn behaviour;
    /// callers can distinguish "no data" via `peek_count` if needed).
    fn read_int32_average(&mut self, user: &AsynUser) -> AsynResult<f64>;

    /// Return the mean without clearing the accumulator. Idempotent
    /// — multiple peeks between reads yield the same value.
    fn peek_int32_average(&self, user: &AsynUser) -> AsynResult<f64>;

    /// How many samples are currently buffered.
    fn peek_count(&self, user: &AsynUser) -> AsynResult<usize>;

    /// Drop every buffered sample without computing a mean. Useful
    /// when an operator wants to start a fresh averaging window
    /// without taking the sample-and-reset side-effect on a read.
    fn reset_average(&mut self, user: &AsynUser) -> AsynResult<()>;
}

/// `asynFloat64Average` interface — averages f64 samples, returns f64.
pub trait AsynFloat64Average: Send + Sync {
    fn read_float64_average(&mut self, user: &AsynUser) -> AsynResult<f64>;
    fn peek_float64_average(&self, user: &AsynUser) -> AsynResult<f64>;
    fn peek_count(&self, user: &AsynUser) -> AsynResult<usize>;
    fn reset_average(&mut self, user: &AsynUser) -> AsynResult<()>;
}

/// Reusable fixed-size circular sample buffer for drivers that want
/// the canonical "accumulate, drop oldest on full" averaging behavior.
/// Generic over the sample type — both `i32` and `f64` work via
/// `Into<f64>` for the mean computation.
pub struct RingAverager<T: Copy + Into<f64>> {
    samples: Mutex<VecDeque<T>>,
    capacity: usize,
}

impl<T: Copy + Into<f64>> RingAverager<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity: capacity.max(1),
        }
    }

    /// Append a sample. When the buffer is full the oldest sample is
    /// dropped — matching C asyn's drop-on-overflow policy.
    pub fn push(&self, sample: T) {
        let mut g = self.samples.lock().unwrap();
        if g.len() >= self.capacity {
            g.pop_front();
        }
        g.push_back(sample);
    }

    /// Return the mean and clear the buffer.
    pub fn read_and_reset(&self) -> f64 {
        let mut g = self.samples.lock().unwrap();
        let m = compute_mean(&g);
        g.clear();
        m
    }

    /// Return the mean without clearing.
    pub fn peek(&self) -> f64 {
        let g = self.samples.lock().unwrap();
        compute_mean(&g)
    }

    /// Current sample count (≤ capacity).
    pub fn count(&self) -> usize {
        self.samples.lock().unwrap().len()
    }

    /// Drop every buffered sample.
    pub fn reset(&self) {
        self.samples.lock().unwrap().clear();
    }
}

fn compute_mean<T: Copy + Into<f64>>(q: &VecDeque<T>) -> f64 {
    if q.is_empty() {
        return 0.0;
    }
    let sum: f64 = q.iter().map(|s| (*s).into()).sum();
    sum / q.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_int32_mean_of_window() {
        let r: RingAverager<i32> = RingAverager::new(8);
        for v in [10, 20, 30, 40] {
            r.push(v);
        }
        assert_eq!(r.count(), 4);
        assert!((r.peek() - 25.0).abs() < 1e-9);
        // Reading must clear.
        assert!((r.read_and_reset() - 25.0).abs() < 1e-9);
        assert_eq!(r.count(), 0);
        // Empty mean is 0.0 (canonical C asyn behaviour).
        assert_eq!(r.peek(), 0.0);
        assert_eq!(r.read_and_reset(), 0.0);
    }

    #[test]
    fn ring_drops_oldest_on_overflow() {
        let r: RingAverager<i32> = RingAverager::new(3);
        for v in [1, 2, 3, 4, 5] {
            r.push(v);
        }
        assert_eq!(r.count(), 3);
        // window is now [3, 4, 5] → mean 4.0
        assert!((r.peek() - 4.0).abs() < 1e-9);
    }

    #[test]
    fn ring_float64_mean() {
        let r: RingAverager<f64> = RingAverager::new(4);
        for v in [1.0, 2.0, 3.0] {
            r.push(v);
        }
        assert!((r.peek() - 2.0).abs() < 1e-9);
        assert!((r.read_and_reset() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn reset_does_not_consume_or_compute_mean() {
        let r: RingAverager<i32> = RingAverager::new(4);
        r.push(100);
        r.push(200);
        r.reset();
        assert_eq!(r.count(), 0);
        assert_eq!(r.peek(), 0.0);
    }

    #[test]
    fn zero_capacity_is_clamped_to_one() {
        // RingAverager(0) → still functions with capacity=1 (no panic).
        let r: RingAverager<i32> = RingAverager::new(0);
        r.push(99);
        assert_eq!(r.count(), 1);
        assert!((r.peek() - 99.0).abs() < 1e-9);
    }
}
