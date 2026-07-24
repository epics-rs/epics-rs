//! `MonitorControlOp` — a producer-side monitor output for custom
//! [`super::source::ChannelSource`] authors. It is *inspired by*
//! `pvxs::server::MonitorControlOp` but is deliberately a simpler
//! primitive: a bounded mpsc queue with one advisory high/low watermark
//! pair. It does NOT reproduce pvxs's full `servermon` flow control —
//! there is no separate queue-`limit` vs pipeline-`window`, no
//! coalesce-to-tail `post()`, and `force_post` cannot over-fill past the
//! channel cap (tokio mpsc has no over-fill). Source authors who need
//! exact pvxs monitor semantics must layer those on top.
//!
//! - `try_post(value)` — non-blocking send subject to the high
//!   watermark: `Err(OverHighWatermark)` when `pending` is at-or-over
//!   the configured high watermark, `Err(ChannelFull)` at the hard
//!   channel cap, `Err(ReceiverGone)` when the receiver was dropped.
//! - `force_post(value)` — send ignoring the high watermark (still
//!   `Err(ChannelFull)` / `Err(ReceiverGone)` at the hard cap or on
//!   drop).
//! - `set_high_watermark` / `set_low_watermark` — adjust the advisory
//!   thresholds at runtime. The caller is responsible for keeping
//!   `low <= high`; they are independent stores.
//! - `is_paused()` / `set_paused(bool)` — observed flag the server's
//!   TCP loop flips via [`super::source::ChannelSource::notify_watermark`]
//!   ([`super::source::WatermarkKind::Pause`] on the LOW edge,
//!   [`super::source::WatermarkKind::Resume`] on the HIGH edge).
//!   Producers should consult before `try_post` to avoid spinning on
//!   a full outbox.
//!
//! `pending` accounting is symmetric and automatic on the `channel()`
//! path: a successful `try_post` / `force_post` increments it, and the
//! paired [`MonitorReceiver`]'s `recv` / `try_recv` decrements it when
//! the consumer pulls a value off the queue. So `pending` tracks queue
//! occupancy (sent-but-not-yet-consumed) and the high-watermark gate
//! reopens by itself as the consumer drains — no manual bookkeeping. The
//! watermark is advisory: under concurrent producer clones the load-then-
//! send is not atomic, so `pending` can momentarily overshoot the
//! watermark, but never the hard channel cap (`ChannelFull` is the real
//! ceiling).
//!
//! Construct one per subscriber on the source side, hand the returned
//! [`MonitorReceiver`] to the server via the `subscribe()` return path,
//! and keep the `MonitorControlOp` for the producer side. The
//! [`MonitorControlOp::from_sender`] path is for when the source already
//! owns an mpsc `Sender` (a pre-existing fan-out registry); there the
//! receiver is not owned by this type, so the external consumer must
//! call [`MonitorControlOp::note_consumed`] to decrement `pending`.

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
    /// Outstanding-event count (sent but not yet consumed). Bumped on
    /// every successful `try_post` / `force_post`; decremented by the
    /// paired [`MonitorReceiver`] on `recv` (channel path) or by hand
    /// via `note_consumed` (from_sender path).
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
    /// capacity. Returns the control op (give to the producer) and the
    /// [`MonitorReceiver`] (give to the server) whose `recv` decrements
    /// `pending`, keeping the watermark accounting symmetric.
    pub fn channel(capacity: usize) -> (Self, MonitorReceiver<T>) {
        let (tx, rx) = mpsc::channel(capacity);
        let pending = Arc::new(AtomicUsize::new(0));
        let op = Self {
            tx,
            pending: pending.clone(),
            high_watermark: Arc::new(AtomicUsize::new(capacity / 2)),
            low_watermark: Arc::new(AtomicUsize::new(0)),
            paused: Arc::new(AtomicBool::new(false)),
        };
        (op, MonitorReceiver { rx, pending })
    }

    /// Build a control op around an externally-created channel. Use
    /// when the source already owns the mpsc Sender (e.g. a
    /// pre-existing fan-out registry). The receiver is not owned by this
    /// type, so the external consumer must call [`Self::note_consumed`]
    /// to keep `pending` accurate — there is no [`MonitorReceiver`] to
    /// decrement it automatically.
    pub fn from_sender(tx: mpsc::Sender<T>, watermark: usize) -> Self {
        Self {
            tx,
            pending: Arc::new(AtomicUsize::new(0)),
            high_watermark: Arc::new(AtomicUsize::new(watermark)),
            low_watermark: Arc::new(AtomicUsize::new(0)),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Non-blocking send subject to the high-watermark gate. Returns
    /// `Ok(())` when delivered, `Err(_)` when refused — caller should
    /// back off and retry once `is_paused()` clears or the consumer
    /// drains below the watermark.
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

    /// Send ignoring the high watermark. Unlike pvxs `forcePost` it
    /// cannot over-fill: it still fails with `ChannelFull` at the hard
    /// channel cap, since the underlying tokio mpsc has no over-fill /
    /// force-overwrite semantics.
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

    /// Current pending count — values sent but not yet consumed. On the
    /// [`Self::channel`] path the paired [`MonitorReceiver`] keeps this
    /// current automatically; see [`Self::note_consumed`] for the
    /// [`Self::from_sender`] path.
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    /// Manually tell the control op N events were consumed downstream.
    /// Needed **only** on the [`Self::from_sender`] path, where the
    /// receiver is owned externally and cannot decrement `pending`
    /// itself. On the [`Self::channel`] path the [`MonitorReceiver`]
    /// already decrements on `recv`/`try_recv`, so calling this there
    /// would double-count.
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

/// Consumer end of a [`MonitorControlOp::channel`]. Wraps the mpsc
/// `Receiver` and owns the *decrement* half of the `pending` accounting:
/// every value pulled off the queue drops `pending` by one, so the
/// producer's high-watermark gate reopens as the consumer drains without
/// any manual `note_consumed`. This is the symmetric counterpart to the
/// `try_post` / `force_post` increment — the same actor that performs the
/// real reverse operation (consume) owns the reverse accounting.
pub struct MonitorReceiver<T> {
    rx: mpsc::Receiver<T>,
    pending: Arc<AtomicUsize>,
}

impl<T> MonitorReceiver<T> {
    /// Decrement `pending` (saturating at 0) for one consumed value.
    fn note_one(&self) {
        let mut cur = self.pending.load(Ordering::Relaxed);
        loop {
            let new = cur.saturating_sub(1);
            match self
                .pending
                .compare_exchange_weak(cur, new, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Await the next value. Decrements `pending` when one is returned;
    /// `None` (channel closed and drained) leaves the counter untouched.
    pub async fn recv(&mut self) -> Option<T> {
        let v = self.rx.recv().await;
        if v.is_some() {
            self.note_one();
        }
        v
    }

    /// Non-blocking pull. Decrements `pending` only on a delivered value.
    pub fn try_recv(&mut self) -> Result<T, mpsc::error::TryRecvError> {
        let r = self.rx.try_recv();
        if r.is_ok() {
            self.note_one();
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[epics_macros_rs::epics_test]
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

    #[epics_macros_rs::epics_test]
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

    #[epics_macros_rs::epics_test]
    async fn force_post_bypasses_watermark() {
        let (op, _rx) = MonitorControlOp::<i32>::channel(8);
        op.set_high_watermark(1);
        op.try_post(1).expect("first");
        op.force_post(2).expect("force_post bypasses HW");
        assert_eq!(op.pending(), 2);
    }

    #[epics_macros_rs::epics_test]
    async fn note_consumed_drops_pending_on_from_sender_path() {
        // from_sender owns no MonitorReceiver, so the external consumer
        // decrements via note_consumed.
        let (tx, _rx) = mpsc::channel::<i32>(8);
        let op = MonitorControlOp::from_sender(tx, 2);
        op.try_post(1).expect("first");
        op.try_post(2).expect("second");
        assert!(matches!(
            op.try_post(3),
            Err(PostError::OverHighWatermark(_))
        ));
        op.note_consumed(2);
        assert_eq!(op.pending(), 0);
        op.try_post(3).expect("after consumed");
    }

    #[epics_macros_rs::epics_test]
    async fn recv_decrements_pending_and_reopens_gate_without_manual_note() {
        // Boundary: the high-watermark gate must reopen as the consumer
        // drains, with no note_consumed call. (Regression: the receiver
        // used to be a bare mpsc::Receiver, so pending only fell via a
        // manual note_consumed that nothing called — the gate latched
        // closed permanently once pending first reached the watermark.)
        let (op, mut rx) = MonitorControlOp::<i32>::channel(8);
        op.set_high_watermark(2);
        op.try_post(1).expect("first");
        op.try_post(2).expect("second");
        assert_eq!(op.pending(), 2);
        assert!(matches!(
            op.try_post(3),
            Err(PostError::OverHighWatermark(_))
        ));

        // Consumer pulls one value off the queue — pending drops by one
        // on recv alone.
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(op.pending(), 1, "recv must decrement pending");

        // Gate reopened without any note_consumed.
        op.try_post(3).expect("gate reopened after recv");
        assert_eq!(op.pending(), 2);
    }

    #[epics_macros_rs::epics_test]
    async fn try_recv_also_decrements_pending() {
        let (op, mut rx) = MonitorControlOp::<i32>::channel(8);
        op.try_post(10).expect("post");
        assert_eq!(op.pending(), 1);
        assert_eq!(rx.try_recv().expect("try_recv"), 10);
        assert_eq!(op.pending(), 0, "try_recv must decrement pending");
        assert!(rx.try_recv().is_err(), "empty after drain");
        assert_eq!(op.pending(), 0, "empty try_recv must not underflow");
    }

    #[epics_macros_rs::epics_test]
    async fn paused_flag_observable_independently() {
        let (op, _rx) = MonitorControlOp::<i32>::channel(8);
        assert!(!op.is_paused());
        op.set_paused(true);
        assert!(op.is_paused());
        op.set_paused(false);
        assert!(!op.is_paused());
    }

    #[epics_macros_rs::epics_test]
    async fn closed_receiver_returns_receiver_gone() {
        let (op, rx) = MonitorControlOp::<i32>::channel(8);
        drop(rx);
        match op.try_post(1) {
            Err(PostError::ReceiverGone(_)) => {}
            other => panic!("expected ReceiverGone, got {other:?}"),
        }
    }
}
