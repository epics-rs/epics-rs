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
//! producers, put-notify completion — instead PUSHES a fully-framed
//! `Vec<u8>` into the `Outbox`. The connection loop is the sole draining
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
//! contiguous `Vec<u8>` per message for the pre-existing abort-safety
//! invariant, so this is the shape they already produce.

use epics_base_rs::runtime::sync::mpsc;

/// Cloneable producer handle. Handed to every emit site (in-loop handlers
/// and spawned monitor / put-notify tasks). Its only capability is
/// [`push`](Outbox::push) — enqueue one complete frame.
#[derive(Clone)]
pub(crate) struct Outbox {
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

/// The draining end, owned solely by the connection loop alongside the
/// socket `BufWriter`. Not `Clone`: there is exactly one drain owner.
pub(crate) struct OutboxDrain {
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

/// Create a linked [`Outbox`] / [`OutboxDrain`] pair for one connection.
///
/// Unbounded because the sole draining owner pulls the queue empty after
/// every dispatch burst and whenever it is otherwise idle, so the queue
/// never holds more than one burst plus any concurrently-produced monitor
/// frames — the same bytes the old `BufWriter` would have buffered.
pub(crate) fn channel() -> (Outbox, OutboxDrain) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Outbox { tx }, OutboxDrain { rx })
}

impl Outbox {
    /// Enqueue one complete frame for the connection loop to write.
    ///
    /// Best-effort by design: once the connection loop has exited it drops
    /// the [`OutboxDrain`], and a push then silently discards the frame —
    /// exactly as a write to the already-torn-down socket was previously
    /// discarded. Producers never observe send failures because there is
    /// nothing actionable to do with a dead circuit.
    pub(crate) fn push(&self, frame: Vec<u8>) {
        let _ = self.tx.send(frame);
    }
}

impl OutboxDrain {
    /// Pull the next already-queued frame without waiting. Returns `None`
    /// when the queue is momentarily empty. The connection loop calls this
    /// in a tight loop to drain a whole burst before a single flush.
    pub(crate) fn try_next(&mut self) -> Option<Vec<u8>> {
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
    pub(crate) async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[epics_macros_rs::epics_test]
    async fn push_then_drain_preserves_frame_order() {
        let (outbox, mut drain) = channel();
        outbox.push(vec![1, 2, 3]);
        outbox.push(vec![4, 5]);
        // A clone is an equal producer; its frames interleave in push order.
        outbox.clone().push(vec![6]);

        assert_eq!(drain.try_next(), Some(vec![1, 2, 3]));
        assert_eq!(drain.try_next(), Some(vec![4, 5]));
        assert_eq!(drain.try_next(), Some(vec![6]));
        assert_eq!(drain.try_next(), None);
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
