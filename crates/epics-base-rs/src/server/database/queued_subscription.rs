//! A subscription whose handler sends each value to a bounded queue a
//! client's own thread reads, as sscan's `saveData` and motor's
//! `motorUtil` event handlers send to an `epicsMessageQueue`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::ca_client::{ClientChannel, ClientSubscription};
use crate::runtime::sync::{Notify, mpsc};
use crate::runtime::task::block_on_sync;
use crate::server::snapshot::Snapshot;

/// An `evid` whose handler sends to a queue. Its drop ends the handler's
/// wait for room, and the value waiting goes unqueued, before the
/// subscription's drop waits for that handler: the thread that drops it
/// may be the queue's only reader. C's `ca_clear_subscription` waits there
/// for good.
pub struct QueuedSubscription {
    cleared: Arc<Cleared>,
    _sub: ClientSubscription,
}

impl QueuedSubscription {
    /// `ca_add_masked_array_event` of `count` elements of `dbr_type` whose
    /// handler `f` sends each value to `tx` through the queue it is given.
    pub async fn new<T: Send + 'static>(
        ch: &dyn ClientChannel,
        dbr_type: u16,
        count: u32,
        mask: u16,
        tx: mpsc::Sender<T>,
        f: impl Fn(Snapshot, &SubscriptionQueue<T>) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let cleared = Arc::new(Cleared::default());
        let queue = SubscriptionQueue {
            tx,
            gone: cleared.clone(),
        };
        let callback = Box::new(move |s: Snapshot| f(s, &queue));
        let sub = ch.subscribe(dbr_type, count, mask, callback).await?;
        Ok(Self { cleared, _sub: sub })
    }
}

impl Drop for QueuedSubscription {
    fn drop(&mut self) {
        self.cleared.set();
    }
}

/// The queue as a [`QueuedSubscription`]'s handler sends to it.
pub struct SubscriptionQueue<T> {
    tx: mpsc::Sender<T>,
    gone: Arc<Cleared>,
}

impl<T> SubscriptionQueue<T> {
    /// `epicsMessageQueueSend`: whether `msg` was queued. The wait for room
    /// holds the context's delivering thread, as C's does, until the
    /// subscription drops.
    pub fn send(&self, msg: T) -> bool {
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(msg)) => block_on_sync(async {
                crate::runtime::select! {
                    sent = self.tx.send(msg) => sent.is_ok(),
                    () = self.gone.wait() => false,
                }
            })
            .unwrap_or(false),
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// `epicsMessageQueueTrySend`: whether `msg` was queued.
    pub fn try_send(&self, msg: T) -> bool {
        self.tx.try_send(msg).is_ok()
    }
}

/// Set when its [`QueuedSubscription`] drops.
#[derive(Default)]
struct Cleared {
    set: AtomicBool,
    notify: Notify,
}

impl Cleared {
    fn set(&self) {
        self.set.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        // Made before the load, so a `set` between the two still wakes it.
        let notified = self.notify.notified();
        if !self.set.load(Ordering::SeqCst) {
            notified.await;
        }
    }
}
