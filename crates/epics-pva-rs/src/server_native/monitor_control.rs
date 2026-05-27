//! `MonitorControlOp` surface for custom [`super::source::ChannelSource`]
//! authors. Mirrors `pvxs::server::MonitorControlOp`:
//!
//! - `try_post(value)` — non-blocking send subject to the high
//!   watermark: `Err(OverHighWatermark)` when the producer-side
//!   count is at-or-over the configured high watermark,
//!   `Err(ChannelFull)` at the hard channel cap, `Err(ReceiverGone)`
//!   when the receiver was dropped. Mirrors pvxs `tryPost`.
//! - `force_post(value)` — unconditional send; ignores the high
//!   watermark (still `Err(ChannelFull)` / `Err(ReceiverGone)` at the
//!   hard cap or on drop). Mirrors pvxs `forcePost`.
//! - `set_high_watermark` / `set_low_watermark` — adjust the
//!   producer-side throttle thresholds at runtime.
//! - `is_paused()` / `set_paused(bool)` — observed flag the server's
//!   TCP loop flips via [`super::source::ChannelSource::notify_watermark`]
//!   ([`super::source::WatermarkKind::Pause`] on the LOW edge,
//!   [`super::source::WatermarkKind::Resume`] on the HIGH edge).
//!   Producers should consult before `try_post` to avoid spinning on
//!   a full outbox.
//!
//! Construct one per subscriber on the source side, hand the returned
//! `mpsc::Receiver<T>` to the server via the `subscribe()` return
//! path, and keep the `MonitorControlOp` for the producer-side. The
//! mpsc channel's bounded capacity is the hard backpressure ceiling;
//! the configured high/low watermarks are advisory thresholds the
//! runtime uses to drive the `notify_watermark` callback.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::sync::mpsc;

/// Result of a `try_post` / `force_post` attempt. `Ok(())` means
/// delivered; the error variants distinguish "receiver dropped"
/// (subscriber gone), "channel at hard cap", and "above high
/// watermark" so the caller can decide whether to discard, block, or
/// back off.
#[derive(Debug)]
pub enum PostError<T> {
    /// The receiving end of the monitor channel was dropped — the
    /// subscriber unsubscribed or its connection closed. The value
    /// is returned so the caller can decide whether to discard.
    ReceiverGone(T),
    /// The mpsc channel is at its hard cap (bounded `try_send`
    /// returned Full). Distinct from "above high watermark" — this
    /// is the unconditional ceiling beyond which `force_post` would
    /// also fail.
    ChannelFull(T),
    /// The producer-side high watermark has been reached. Caller
    /// should pause / coalesce. `force_post` ignores this gate.
    OverHighWatermark(T),
}

/// pvxs `MonitorControlOp` parity for source authors.
pub struct MonitorControlOp<T> {
    tx: mpsc::Sender<T>,
    /// Outstanding-event count visible to producers. Bumped on every
    /// successful `try_post` / `force_post`; producers can decrement
    /// it by hand when their internal accounting (e.g. ACK arrival)
    /// says the consumer drained N events.
    pending: Arc<AtomicUsize>,
    /// Producer-side advisory ceiling. `try_post` bails when
    /// `pending >= high_watermark`. Must be ≤ the bounded channel
    /// capacity for the gate to fire before backpressure does.
    high_watermark: Arc<AtomicUsize>,
    /// Companion to `high_watermark` — when the consumer drains
    /// `pending` back below this value the runtime fires the
    /// resume-edge `notify_watermark`. Defaults to 0.
    low_watermark: Arc<AtomicUsize>,
    /// Set by the runtime via [`Self::set_paused`] when the
    /// downstream subscriber has the outbox over the high watermark.
    /// Producers can short-circuit `try_post` calls via
    /// [`Self::is_paused`].
    paused: Arc<AtomicBool>,
}

impl<T> MonitorControlOp<T> {
    /// Build a control op with a bounded mpsc channel of the given
    /// capacity. Returns the control op (give to the producer) and
    /// the receiver (give to the server).
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<T>) {
        let (tx, rx) = mpsc::channel(capacity);
        let op = Self {
            tx,
            pending: Arc::new(AtomicUsize::new(0)),
            high_watermark: Arc::new(AtomicUsize::new(capacity / 2)),
            low_watermark: Arc::new(AtomicUsize::new(0)),
            paused: Arc::new(AtomicBool::new(false)),
        };
        (op, rx)
    }

    /// Build a control op around an externally-created channel. Use
    /// when the source already owns the mpsc Sender (e.g. a
    /// pre-existing fan-out registry).
    pub fn from_sender(tx: mpsc::Sender<T>, watermark: usize) -> Self {
        Self {
            tx,
            pending: Arc::new(AtomicUsize::new(0)),
            high_watermark: Arc::new(AtomicUsize::new(watermark)),
            low_watermark: Arc::new(AtomicUsize::new(0)),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Non-blocking send subject to the high watermark gate. Mirrors
    /// pvxs `MonitorControlOp::tryPost`. Returns `Ok(())` when
    /// delivered, `Err(_)` when refused — caller should back off and
    /// retry once `is_paused()` clears.
    pub fn try_post(&self, value: T) -> Result<(), PostError<T>> {
        let p = self.pending.load(Ordering::Relaxed);
        let hw = self.high_watermark.load(Ordering::Relaxed);
        if hw > 0 && p >= hw {
            return Err(PostError::OverHighWatermark(value));
        }
        match self.tx.try_send(value) {
            Ok(()) => {
                self.pending.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(v)) => Err(PostError::ChannelFull(v)),
            Err(mpsc::error::TrySendError::Closed(v)) => Err(PostError::ReceiverGone(v)),
        }
    }

    /// Unconditional send (ignores the high watermark). Will still
    /// fail if the bounded channel is at hard capacity — the
    /// underlying mpsc has no "force overwrite" semantics. Mirrors
    /// pvxs `MonitorControlOp::forcePost`.
    pub fn force_post(&self, value: T) -> Result<(), PostError<T>> {
        match self.tx.try_send(value) {
            Ok(()) => {
                self.pending.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(v)) => Err(PostError::ChannelFull(v)),
            Err(mpsc::error::TrySendError::Closed(v)) => Err(PostError::ReceiverGone(v)),
        }
    }

    /// Adjust the producer-side high watermark. 0 disables the gate
    /// (every `try_post` falls back to channel capacity).
    pub fn set_high_watermark(&self, n: usize) {
        self.high_watermark.store(n, Ordering::Relaxed);
    }

    /// Adjust the low-watermark advisory threshold. Server uses it to
    /// decide when to fire the resume-edge `notify_watermark`.
    pub fn set_low_watermark(&self, n: usize) {
        self.low_watermark.store(n, Ordering::Relaxed);
    }

    pub fn high_watermark(&self) -> usize {
        self.high_watermark.load(Ordering::Relaxed)
    }

    pub fn low_watermark(&self) -> usize {
        self.low_watermark.load(Ordering::Relaxed)
    }

    /// Current producer-side pending count. Decrement via
    /// [`Self::note_consumed`] when the runtime / consumer ACKs N
    /// events. Used by the high-watermark gate.
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    /// Tell the control op N events have been consumed downstream.
    /// Decrements the pending counter; the high-watermark gate
    /// reopens once `pending < high_watermark`.
    pub fn note_consumed(&self, n: usize) {
        // Saturating subtract — never go negative.
        let mut cur = self.pending.load(Ordering::Relaxed);
        loop {
            let new = cur.saturating_sub(n);
            match self
                .pending
                .compare_exchange_weak(cur, new, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Signal the runtime crossed up through the high watermark.
    /// Producers consult [`Self::is_paused`] to short-circuit post()
    /// rather than burning the watermark check on every send.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

impl<T> Clone for MonitorControlOp<T> {
    /// Cloning shares the same producer state (Arc-internal), so
    /// every clone observes a single `pending` / `paused` history.
    /// This matches pvxs's `shared_ptr<MonitorControlOp>` semantics.
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            pending: self.pending.clone(),
            high_watermark: self.high_watermark.clone(),
            low_watermark: self.low_watermark.clone(),
            paused: self.paused.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn try_post_succeeds_below_high_watermark() {
        let (op, mut rx) = MonitorControlOp::<i32>::channel(8);
        op.set_high_watermark(4);
        for i in 0..3 {
            op.try_post(i).expect("try_post");
        }
        assert_eq!(op.pending(), 3);
        // Drain so the channel doesn't fill up — pending only tracks
        // producer-side accounting.
        let _ = rx.recv().await;
    }

    #[tokio::test]
    async fn try_post_refuses_at_high_watermark() {
        let (op, _rx) = MonitorControlOp::<i32>::channel(8);
        op.set_high_watermark(2);
        op.try_post(1).expect("first");
        op.try_post(2).expect("second");
        match op.try_post(3) {
            Err(PostError::OverHighWatermark(_)) => {}
            other => panic!("expected OverHighWatermark, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn force_post_bypasses_watermark() {
        let (op, _rx) = MonitorControlOp::<i32>::channel(8);
        op.set_high_watermark(1);
        op.try_post(1).expect("first");
        op.force_post(2).expect("force_post bypasses HW");
        assert_eq!(op.pending(), 2);
    }

    #[tokio::test]
    async fn note_consumed_drops_pending_for_subsequent_try_post() {
        let (op, _rx) = MonitorControlOp::<i32>::channel(8);
        op.set_high_watermark(2);
        op.try_post(1).expect("first");
        op.try_post(2).expect("second");
        op.note_consumed(2);
        assert_eq!(op.pending(), 0);
        op.try_post(3).expect("after consumed");
    }

    #[tokio::test]
    async fn paused_flag_observable_independently() {
        let (op, _rx) = MonitorControlOp::<i32>::channel(8);
        assert!(!op.is_paused());
        op.set_paused(true);
        assert!(op.is_paused());
        op.set_paused(false);
        assert!(!op.is_paused());
    }

    #[tokio::test]
    async fn closed_receiver_returns_receiver_gone() {
        let (op, rx) = MonitorControlOp::<i32>::channel(8);
        drop(rx);
        match op.try_post(1) {
            Err(PostError::ReceiverGone(_)) => {}
            other => panic!("expected ReceiverGone, got {other:?}"),
        }
    }
}
