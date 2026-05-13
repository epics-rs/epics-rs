//! `asyn:FIFO` info-tag record-side ring buffer (asyn 2015 feature).
//!
//! Background: when a driver pushes interrupts faster than the EPICS
//! record can `process()` them (typical for fast hardware → slow
//! scan-rate records), individual sample values get overwritten in
//! the per-record latest-value cache and lost. C asyn added an
//! `info(asyn:FIFO, "N")` tag that allocates an N-deep ring buffer
//! per record so each interrupt is queued and the record drains one
//! per process cycle, preserving every sample.
//!
//! ## Behaviour
//!
//! - Capacity-bounded: `RingBuffer::new(N)` holds at most N entries.
//! - Drop-oldest on overflow (matches C asyn drop-old policy when
//!   the queue fills — the alternative drop-newest loses fresh data
//!   which defeats the purpose).
//! - Per-record overflow counter — exposed as `overruns()` so
//!   downstream alarm/diagnostic logic can react.
//! - Generic over the queued type (`InterruptValue`, `Vec<u8>`,
//!   `String`, `Vec<f64>` for waveforms / Long Strings — the cases
//!   the C `asyn:FIFO` was designed to protect).
//!
//! ## How records integrate
//!
//! The asyn record adapter calls `RingBuffer::push(value)` from the
//! interrupt subscriber task and `RingBuffer::pop()` from the record
//! `process()` callback. When `pop()` returns `None` the record
//! preserves its previous VAL (no spurious zeros).

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

/// Default ring depth when `info(asyn:FIFO, ...)` is set with no
/// explicit value. Matches C asyn's compiled default.
pub const DEFAULT_FIFO_DEPTH: usize = 100;

pub struct RingBuffer<T> {
    inner: Mutex<VecDeque<T>>,
    capacity: usize,
    overruns: AtomicU64,
}

impl<T> RingBuffer<T> {
    /// Construct a ring buffer with the given depth. A depth of 0
    /// is clamped to 1 — a zero-depth ring would drop every sample
    /// and provide no improvement over no buffer.
    pub fn new(depth: usize) -> Self {
        let capacity = depth.max(1);
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            overruns: AtomicU64::new(0),
        }
    }

    /// Push a value. When the buffer is full, drop the oldest entry
    /// and increment the overrun counter so the record/operator can
    /// detect under-sized buffers in production.
    pub fn push(&self, value: T) {
        let mut g = self.inner.lock();
        if g.len() >= self.capacity {
            g.pop_front();
            self.overruns.fetch_add(1, Ordering::Relaxed);
        }
        g.push_back(value);
    }

    /// Pop the oldest queued value, or `None` when empty. The record
    /// adapter calls this once per `process()` cycle.
    pub fn pop(&self) -> Option<T> {
        self.inner.lock().pop_front()
    }

    /// Drain every queued value into a fresh `Vec` (oldest first).
    /// Used when the record wants to flush the queue in one shot
    /// (e.g. compress record averaging across the whole window).
    pub fn drain_all(&self) -> Vec<T> {
        let mut g = self.inner.lock();
        g.drain(..).collect()
    }

    /// Number of currently-queued entries.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    /// Configured capacity (the value passed to [`Self::new`],
    /// floored at 1).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Total number of drop-oldest events since construction.
    /// Exported for diagnostic / alarm wiring — record adapters
    /// can post a STATE alarm when this counter advances.
    pub fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::Relaxed)
    }

    /// Reset the overrun counter. Operators clear this after
    /// addressing the under-sized buffer (e.g. tuning the
    /// `info(asyn:FIFO, "N")` value upward).
    pub fn reset_overruns(&self) {
        self.overruns.store(0, Ordering::Relaxed);
    }
}

/// Parse the `info(asyn:FIFO, "value")` tag string. Accepts:
/// - decimal positive integer (`"100"`) → that depth.
/// - empty / `"yes"` / `"true"` → [`DEFAULT_FIFO_DEPTH`].
/// - `"0"` / `"no"` / `"false"` → `None` (FIFO disabled — adapter
///   keeps the historic latest-value-only path).
/// - unparseable → `None` with a debug log (caller's responsibility
///   to surface the bad config to the operator).
pub fn parse_fifo_tag(raw: &str) -> Option<usize> {
    let s = raw.trim();
    if s.is_empty() {
        return Some(DEFAULT_FIFO_DEPTH);
    }
    match s.to_ascii_lowercase().as_str() {
        "yes" | "true" | "on" => return Some(DEFAULT_FIFO_DEPTH),
        "no" | "false" | "off" | "0" => return None,
        _ => {}
    }
    match s.parse::<i64>() {
        Ok(n) if n > 0 => Some(n as usize),
        Ok(_) => None,
        Err(_) => {
            tracing::debug!(
                target: "asyn_rs::asyn_record::fifo",
                raw = %raw,
                "asyn:FIFO tag value not parseable; FIFO disabled"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_basic() {
        let r: RingBuffer<i32> = RingBuffer::new(4);
        r.push(1);
        r.push(2);
        r.push(3);
        assert_eq!(r.len(), 3);
        assert_eq!(r.pop(), Some(1));
        assert_eq!(r.pop(), Some(2));
        assert_eq!(r.pop(), Some(3));
        assert_eq!(r.pop(), None);
    }

    #[test]
    fn overflow_drops_oldest_and_counts() {
        let r: RingBuffer<i32> = RingBuffer::new(3);
        for v in [1, 2, 3, 4, 5] {
            r.push(v);
        }
        assert_eq!(r.len(), 3);
        assert_eq!(r.overruns(), 2, "two oldest values dropped");
        assert_eq!(r.pop(), Some(3));
        assert_eq!(r.pop(), Some(4));
        assert_eq!(r.pop(), Some(5));
    }

    #[test]
    fn drain_all_returns_oldest_first() {
        let r: RingBuffer<i32> = RingBuffer::new(8);
        for v in [10, 20, 30] {
            r.push(v);
        }
        assert_eq!(r.drain_all(), vec![10, 20, 30]);
        assert!(r.is_empty());
    }

    #[test]
    fn reset_overruns() {
        let r: RingBuffer<i32> = RingBuffer::new(2);
        r.push(1);
        r.push(2);
        r.push(3);
        assert!(r.overruns() > 0);
        r.reset_overruns();
        assert_eq!(r.overruns(), 0);
    }

    #[test]
    fn zero_depth_clamps_to_one() {
        let r: RingBuffer<i32> = RingBuffer::new(0);
        assert_eq!(r.capacity(), 1);
        r.push(99);
        assert_eq!(r.len(), 1);
        r.push(100);
        assert_eq!(r.len(), 1);
        assert_eq!(r.pop(), Some(100));
    }

    #[test]
    fn parse_tag_decimal_depth() {
        assert_eq!(parse_fifo_tag("100"), Some(100));
        assert_eq!(parse_fifo_tag("  50 "), Some(50));
        assert_eq!(parse_fifo_tag("1"), Some(1));
    }

    #[test]
    fn parse_tag_truthy_aliases() {
        assert_eq!(parse_fifo_tag(""), Some(DEFAULT_FIFO_DEPTH));
        assert_eq!(parse_fifo_tag("yes"), Some(DEFAULT_FIFO_DEPTH));
        assert_eq!(parse_fifo_tag("YES"), Some(DEFAULT_FIFO_DEPTH));
        assert_eq!(parse_fifo_tag("true"), Some(DEFAULT_FIFO_DEPTH));
        assert_eq!(parse_fifo_tag("on"), Some(DEFAULT_FIFO_DEPTH));
    }

    #[test]
    fn parse_tag_falsy_aliases() {
        assert_eq!(parse_fifo_tag("no"), None);
        assert_eq!(parse_fifo_tag("false"), None);
        assert_eq!(parse_fifo_tag("0"), None);
        assert_eq!(parse_fifo_tag("off"), None);
    }

    #[test]
    fn parse_tag_negative_or_garbage_disables() {
        assert_eq!(parse_fifo_tag("-5"), None);
        assert_eq!(parse_fifo_tag("abc"), None);
        assert_eq!(parse_fifo_tag("12.5"), None);
    }
}
