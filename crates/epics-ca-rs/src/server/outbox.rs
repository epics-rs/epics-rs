//! Per-connection wire outbox — the single sink for all server-produced
//! CA frames.
//!
//! # Why this exists (the dual-owner defect it removes)
//!
//! Before this, every emit site on a CA circuit shared one
//! `Arc<Mutex<BufWriter<W>>>`. The connection loop (`handle_client`) wrote
//! replies through it, AND the spawned monitor / put-notify completion
//! tasks reached into the SAME writer out-of-band, each taking
//! `writer.lock().await`. Two independent owners mutating one socket writer
//! is exactly the shared-mutable-writer problem: correct only by the
//! discipline of every site building a contiguous frame before locking, and
//! fragile under any future edit that splits a write.
//!
//! The structural fix (this module) removes the second owner *by
//! construction*: the raw socket writer becomes private to ONE owner, the
//! connection loop. Every emit site — request→reply handlers, the monitor
//! producers, put-notify completion — instead PUSHES one fully-framed
//! message into the `Outbox`. The connection loop is the sole draining
//! owner: it pulls framed bytes in arrival order and is the only code that
//! ever touches the socket.
//!
//! This mirrors what the PVA server already does
//! (`crates/epics-pva-rs/src/server_native/tcp.rs`: emit sites push framed
//! bytes to an mpsc channel that a single writer drains in arrival order).
//!
//! # Invariant
//!
//! - MUST: the socket writer is owned by exactly one task (the connection
//!   loop) and no other code writes the socket directly.
//! - Every producer holds only an `Outbox` handle and can do nothing but
//!   `push` a complete frame; it cannot observe or mutate the socket.
//!
//! A frame handed to `Outbox::push` MUST be a complete, independently
//! valid CA wire message (header + padded payload). The drain concatenates
//! frames back-to-back into the socket buffer, so a partial frame would
//! mis-align every following message. All server senders already build one
//! contiguous buffer per message for the pre-existing abort-safety
//! invariant, so this is the shape they already produce.
//!
//! Frames travel as `PooledFrame`, so the drain owner's drop of a written
//! frame is also what returns the connection's send buffer to its
//! `FramePool` for the next delivery — see `crate::server::frame`, which is
//! crate-private and so cannot be linked from this module's public docs.
//!
//! # Invariant: an asynchronous producer runs on credit
//!
//! - MUST: a producer that is not the connection loop MUST hold a [`Credit`]
//!   for every frame it enqueues, and only the drain owner may release one —
//!   by dropping the queued frame after its bytes are in the socket writer.
//!
//! Request→reply handlers are exempt and pass [`Credit::none`]: they run *in*
//! the connection loop, so a parked drain stops dispatch and no further reply
//! can be produced. They are self-limiting.
//!
//! Monitor producers are not. They pull from their own `EvQue` on their own
//! task, so with an unbounded queue a client that stops reading grows the
//! server without limit — measured at 9.34 kB/s for one 100 Hz subscription
//! (`doc/ca-stuck-reader-measurement.md`). Credit is what puts the socket back
//! in that loop: with none available the producer stops taking events out of
//! its ring, and the ring — which is bounded and coalesces, replacing a
//! monitor's last entry in place — absorbs the backlog. That is C's shape,
//! where `event_task` blocks on `SEND_LOCK` in front of a bounded `dbEvent`
//! ring, and it is what the blocking driver gets for free by writing the
//! socket directly under `send_lock` (`server::blocking`).
//!
//! Credit is taken *after* an event leaves the ring, never before: a producer
//! waiting for its next event must hold nothing, or a connection with more
//! subscriptions than [`MONITOR_CREDIT`] would exhaust the pool with an empty
//! queue and nothing left to drain to release it.

use crate::server::frame::{FramePool, PooledFrame};
use epics_base_rs::runtime::sync::mpsc;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Frames one connection's asynchronous producers may hold un-written.
///
/// Deep enough that a client keeping up never waits — the drain empties the
/// queue on every loop iteration — and small enough to bound a client that
/// does not. The worst case is this many frames of the largest DBR reply the
/// client subscribed to, on top of the connection's 64 KiB `BufWriter`.
pub(crate) const MONITOR_CREDIT: usize = 64;

/// Permission to occupy one slot in a connection's outbox.
///
/// Released by dropping — which the drain owner does once the frame it rode
/// in on has been written. A producer aborted while holding one releases it
/// the same way, so a cancelled subscription cannot strand credit.
pub(crate) struct Credit {
    /// Never read: the permit exists to be *held*, and releasing it is its
    /// `Drop`. Naming it `_permit` is what says so to the dead-code lint.
    _permit: Option<OwnedSemaphorePermit>,
}

impl Credit {
    /// For producers that run inside the connection loop and are therefore
    /// already bounded by it. See the module invariant.
    pub(crate) fn none() -> Self {
        Credit { _permit: None }
    }
}

/// Cloneable producer handle. Handed to every emit site (in-loop handlers
/// and spawned monitor / put-notify tasks). Its only capabilities are
/// [`push`](Outbox::push) — enqueue one complete frame — and [`pool`](Outbox::pool),
/// the connection's send buffer to build that frame in.
#[derive(Clone)]
pub(crate) struct Outbox {
    tx: mpsc::UnboundedSender<QueuedFrame>,
    pool: Arc<FramePool>,
    credit: Arc<Semaphore>,
}

/// One queued frame, plus the [`Credit`] its producer spent to enqueue it.
///
/// Dereferences to the frame, so the drain owner writes it exactly as before;
/// dropping it is what both returns the send buffer to the [`FramePool`] and
/// releases the producer to build its next frame.
pub(crate) struct QueuedFrame {
    frame: PooledFrame,
    _credit: Credit,
}

impl std::ops::Deref for QueuedFrame {
    type Target = PooledFrame;

    fn deref(&self) -> &PooledFrame {
        &self.frame
    }
}

/// The draining end, owned solely by the connection loop alongside the
/// socket `BufWriter`. Not `Clone`: there is exactly one drain owner.
pub(crate) struct OutboxDrain {
    rx: mpsc::UnboundedReceiver<QueuedFrame>,
}

/// Create a linked [`Outbox`] / [`OutboxDrain`] pair for one connection.
///
/// The channel itself is unbounded: the sole draining owner pulls it empty
/// after every dispatch burst and whenever it is otherwise idle, and the
/// in-loop producers that feed it stop as soon as it does not. What bounds
/// the queue is [`Credit`], which the out-of-loop producers must hold — see
/// the module invariant. Bounding the channel instead would deadlock the
/// connection loop, which is both a producer and the only consumer.
pub(crate) fn channel() -> (Outbox, OutboxDrain) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        Outbox {
            tx,
            pool: Arc::new(FramePool::new()),
            credit: Arc::new(Semaphore::new(MONITOR_CREDIT)),
        },
        OutboxDrain { rx },
    )
}

impl Outbox {
    /// This connection's send buffer, for [`FrameBuf::acquire`](crate::server::frame::FrameBuf::acquire).
    ///
    /// Shared by every clone of the handle, so all of a connection's producers
    /// draw on the one buffer and the drain owner's `Drop` of the written frame
    /// hands it back to whichever of them asks next.
    pub(crate) fn pool(&self) -> &Arc<FramePool> {
        &self.pool
    }

    /// Enqueue one complete frame for the connection loop to write.
    ///
    /// Best-effort by design: once the connection loop has exited it drops
    /// the [`OutboxDrain`], and a push then silently discards the frame —
    /// exactly as a write to the already-torn-down socket was previously
    /// discarded. Producers never observe send failures because there is
    /// nothing actionable to do with a dead circuit. A discarded frame still
    /// returns its buffer to the pool on drop; the pool dies with the
    /// connection either way.
    pub(crate) fn push(&self, frame: impl Into<PooledFrame>) {
        self.push_with(frame, Credit::none());
    }

    /// Enqueue one complete frame, spending `credit`.
    ///
    /// Same best-effort contract as [`push`](Outbox::push). Kept synchronous
    /// on purpose: the whole frame is handed over in one `send` with no await
    /// between sealing it and enqueuing it, so a producer aborted mid-flight
    /// can only be cut at a frame boundary.
    pub(crate) fn push_with(&self, frame: impl Into<PooledFrame>, credit: Credit) {
        let _ = self.tx.send(QueuedFrame {
            frame: frame.into(),
            _credit: credit,
        });
    }

    /// Wait for room in this connection's outbox, for a producer that runs
    /// outside the connection loop.
    ///
    /// Call this only once the event is already out of the producer's ring —
    /// see the module invariant for why holding credit while idle deadlocks.
    /// The semaphore is never closed, so this resolves to a real credit or
    /// stays pending until the drain owner releases one.
    pub(crate) async fn reserve(&self) -> Credit {
        match Arc::clone(&self.credit).acquire_owned().await {
            Ok(permit) => Credit {
                _permit: Some(permit),
            },
            // Unreachable: nothing closes this semaphore. Degrade to unbounded
            // rather than dropping a client's update if that ever changes.
            Err(_) => Credit::none(),
        }
    }
}

impl OutboxDrain {
    /// Pull the next already-queued frame without waiting. Returns `None`
    /// when the queue is momentarily empty. The connection loop calls this
    /// in a tight loop to drain a whole burst before a single flush.
    pub(crate) fn try_next(&mut self) -> Option<QueuedFrame> {
        self.rx.try_recv().ok()
    }

    /// Await the next frame. Used by the connection loop's idle `select!`
    /// arm so a monitor / put-notify frame produced while the loop is
    /// blocked on the socket read is written promptly. Resolves to `None`
    /// only if every [`Outbox`] handle has been dropped, which cannot
    /// happen while the loop still holds its own handle.
    ///
    /// Used only by the async connection loop (`tcp::handle_client`); the
    /// blocking driver drains synchronously via [`OutboxDrain::try_next`].
    /// Host-only (not `epics_embedded_target`).
    #[cfg(not(epics_embedded_target))]
    pub(crate) async fn recv(&mut self) -> Option<QueuedFrame> {
        self.rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The next frame's bytes, for assertions.
    fn next(drain: &mut OutboxDrain) -> Option<Vec<u8>> {
        drain.try_next().map(|f| f.to_vec())
    }

    #[epics_macros_rs::epics_test]
    async fn push_then_drain_preserves_frame_order() {
        let (outbox, mut drain) = channel();
        outbox.push(vec![1, 2, 3]);
        outbox.push(vec![4, 5]);
        // A clone is an equal producer; its frames interleave in push order.
        outbox.clone().push(vec![6]);

        assert_eq!(next(&mut drain).as_deref(), Some(&[1, 2, 3][..]));
        assert_eq!(next(&mut drain).as_deref(), Some(&[4, 5][..]));
        assert_eq!(next(&mut drain).as_deref(), Some(&[6][..]));
        assert!(next(&mut drain).is_none());
    }

    // The invariant boundary is "credit remaining": 0 vs > 0, crossed in both
    // directions, and crossed only by the two actors allowed to move it.

    #[epics_macros_rs::epics_test]
    async fn a_reserve_below_the_bound_does_not_wait() {
        let (outbox, _drain) = channel();
        for i in 0..MONITOR_CREDIT {
            let credit = outbox.reserve().await;
            outbox.push_with(vec![i as u8], credit);
        }
        // Exactly MONITOR_CREDIT frames fit without the drain running at all.
    }

    #[epics_macros_rs::epics_test]
    async fn a_reserve_at_the_bound_waits_for_the_drain() {
        let (outbox, mut drain) = channel();
        for i in 0..MONITOR_CREDIT {
            let credit = outbox.reserve().await;
            outbox.push_with(vec![i as u8], credit);
        }

        // Credit is exhausted: the next producer must park, not queue.
        let mut blocked = Box::pin(outbox.reserve());
        assert!(
            futures_util::poll!(blocked.as_mut()).is_pending(),
            "reserve must park once MONITOR_CREDIT frames are un-written"
        );

        // Only the drain owner releases credit, and only by dropping a frame
        // it has taken — draining one admits exactly one more producer.
        drop(drain.try_next().expect("a queued frame"));
        assert!(
            futures_util::poll!(blocked.as_mut()).is_ready(),
            "dropping one written frame must release exactly one credit"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn holding_a_queued_frame_does_not_release_its_credit() {
        let (outbox, mut drain) = channel();
        for i in 0..MONITOR_CREDIT {
            let credit = outbox.reserve().await;
            outbox.push_with(vec![i as u8], credit);
        }
        // Taken from the queue but NOT yet written: the drain owner still
        // holds it, so the producer stays parked. This is the boundary that
        // makes credit track bytes the socket has accepted rather than
        // bytes dequeued.
        let in_flight = drain.try_next().expect("a queued frame");
        let mut blocked = Box::pin(outbox.reserve());
        assert!(futures_util::poll!(blocked.as_mut()).is_pending());
        drop(in_flight);
        assert!(futures_util::poll!(blocked.as_mut()).is_ready());
    }

    #[epics_macros_rs::epics_test]
    async fn an_in_loop_push_never_consumes_credit() {
        let (outbox, _drain) = channel();
        // Request→reply handlers are self-limiting and exempt; however many
        // they enqueue, a monitor producer still gets its full allowance.
        for i in 0..(MONITOR_CREDIT * 4) {
            outbox.push(vec![i as u8]);
        }
        for i in 0..MONITOR_CREDIT {
            let credit = outbox.reserve().await;
            outbox.push_with(vec![i as u8], credit);
        }
    }

    #[epics_macros_rs::epics_test]
    async fn an_aborted_producer_releases_the_credit_it_never_spent() {
        let (outbox, _drain) = channel();
        for i in 0..(MONITOR_CREDIT - 1) {
            let credit = outbox.reserve().await;
            outbox.push_with(vec![i as u8], credit);
        }
        // Reserved, then the task dies before building its frame.
        drop(outbox.reserve().await);
        // The allowance is intact: credit follows frames, not reservations.
        let credit = outbox.reserve().await;
        outbox.push_with(vec![0xff], credit);
        let mut blocked = Box::pin(outbox.reserve());
        assert!(futures_util::poll!(blocked.as_mut()).is_pending());
    }

    #[epics_macros_rs::epics_test]
    async fn push_after_drain_dropped_is_silently_discarded() {
        let (outbox, drain) = channel();
        drop(drain);
        // No panic, no observable failure — matches best-effort socket write
        // to a torn-down circuit.
        outbox.push(vec![9, 9, 9]);
    }
}
