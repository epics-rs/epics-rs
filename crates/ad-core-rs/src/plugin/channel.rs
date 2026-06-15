use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::time::Duration;

use crate::ndarray::NDArray;

/// Tracks the number of queued (in-flight) arrays across plugins.
/// Used by drivers to perform a bounded wait at end of acquisition.
pub struct QueuedArrayCounter {
    count: AtomicUsize,
    mutex: parking_lot::Mutex<()>,
    condvar: parking_lot::Condvar,
}

impl QueuedArrayCounter {
    /// Create a new counter starting at zero.
    pub fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            mutex: parking_lot::Mutex::new(()),
            condvar: parking_lot::Condvar::new(),
        }
    }

    /// Increment the queued count (called before send).
    pub fn increment(&self) {
        self.count.fetch_add(1, Ordering::AcqRel);
    }

    /// Decrement the queued count. Notifies waiters when reaching zero.
    pub fn decrement(&self) {
        let prev = self.count.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            let _guard = self.mutex.lock();
            self.condvar.notify_all();
        }
    }

    /// Current queued count.
    pub fn get(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Wait until count reaches zero, or timeout expires.
    /// Returns `true` if count is zero, `false` on timeout.
    pub fn wait_until_zero(&self, timeout: Duration) -> bool {
        let mut guard = self.mutex.lock();
        if self.count.load(Ordering::Acquire) == 0 {
            return true;
        }
        !self
            .condvar
            .wait_while_for(
                &mut guard,
                |_| self.count.load(Ordering::Acquire) != 0,
                timeout,
            )
            .timed_out()
    }
}

impl Default for QueuedArrayCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Array message with optional queued-array counter and completion signal.
/// When dropped, decrements the counter (if present) — this signals that
/// the downstream plugin has finished processing the array.
pub struct ArrayMessage {
    pub array: Arc<NDArray>,
    pub(crate) counter: Option<Arc<QueuedArrayCounter>>,
    /// When Some, the sender awaits this to confirm downstream processing completed.
    /// Fired when ArrayMessage is dropped (i.e., after plugin process_array finishes).
    pub(crate) done_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for ArrayMessage {
    fn drop(&mut self) {
        if let Some(tx) = self.done_tx.take() {
            let _ = tx.send(());
        }
        if let Some(c) = self.counter.take() {
            c.decrement();
        }
    }
}

/// Outcome of a `publish` call, mirroring C++ `driverCallback` accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The array was enqueued (and, in blocking mode, processed).
    Delivered,
    /// `enable_callbacks` was 0 — array not sent (not a drop, not counted).
    Disabled,
    /// The downstream queue was full and the array was dropped. The caller
    /// must increment `DroppedArrays`, matching C++ `trySend` semantics.
    DroppedQueueFull,
    /// The downstream channel was closed (receiver gone).
    ChannelClosed,
}

/// Sender held by upstream.
///
/// # Default: drop-on-full (C++ parity)
///
/// By default `publish` uses a bounded `try_send`: when the downstream queue
/// is full the array is **dropped** and `PublishOutcome::DroppedQueueFull` is
/// returned, matching C++ `NDPluginDriver::driverCallback` `trySend` — a slow
/// plugin drops frames rather than back-pressuring the detector driver.
///
/// # `blocking_callbacks=1`: reliable opt-in
///
/// When `blocking_callbacks` is set, `publish` instead uses a reliable
/// `send().await` and waits for the downstream plugin to finish processing.
/// This is the explicit opt-in for "never drop, apply back-pressure"
/// behavior. It is NOT the default.
#[derive(Clone)]
pub struct NDArraySender {
    tx: tokio::sync::mpsc::Sender<ArrayMessage>,
    port_name: String,
    enabled: Arc<AtomicBool>,
    blocking_mode: Arc<AtomicBool>,
    queued_counter: Option<Arc<QueuedArrayCounter>>,
    /// Cumulative count of arrays dropped because this sender's downstream
    /// input queue was full. Owned by the downstream plugin (which publishes
    /// it to its `DROPPED_ARRAYS` param), shared back to every upstream
    /// sender that feeds this plugin — matching C++ `driverCallback` which
    /// increments the *receiving* plugin's `NDPluginDriverDroppedArrays`.
    dropped_arrays: Arc<AtomicI32>,
}

impl NDArraySender {
    /// Publish an array downstream.
    ///
    /// - `enable_callbacks=0`: returns `Disabled`, array not sent.
    /// - `blocking_callbacks=0` (default): bounded `try_send` — on a full queue
    ///   the array is dropped and `DroppedQueueFull` is returned (C++ parity).
    /// - `blocking_callbacks=1`: reliable `send().await` + awaits downstream
    ///   processing completion (explicit opt-in, never drops).
    pub async fn publish(&self, array: Arc<NDArray>) -> PublishOutcome {
        self.publish_inner(array, true).await
    }

    /// Publish for the scatter reroute path. Mirrors C++ `NDPluginScatter`'s
    /// `auxStatus` protocol: scatter sets `auxStatus=asynOverflow` for every
    /// node except the last so that a full-queue consumer is *rerouted past*
    /// without counting a dropped array — the receiving `driverCallback` reads
    /// `auxStatus==asynOverflow` as `ignoreQueueFull` and skips the
    /// `DroppedArrays++` (NDPluginDriver.cpp:406,433-442). Only the last node
    /// (`is_last`) is allowed to actually drop and count the array.
    pub async fn publish_scatter(&self, array: Arc<NDArray>, is_last: bool) -> PublishOutcome {
        self.publish_inner(array, is_last).await
    }

    /// Shared publish body. `count_drop` controls whether a full-queue drop
    /// increments `DroppedArrays`: `true` for the normal broadcast path (C++
    /// `ignoreQueueFull=false`), `false` for a scatter reroute attempt that is
    /// not the last node (C++ `ignoreQueueFull=true`).
    async fn publish_inner(&self, array: Arc<NDArray>, count_drop: bool) -> PublishOutcome {
        if !self.enabled.load(Ordering::Acquire) {
            return PublishOutcome::Disabled;
        }

        let blocking = self.blocking_mode.load(Ordering::Acquire);

        if !blocking {
            // Drop-on-full path (C++ trySend). Build the message only on the
            // way into try_send so a full queue does not touch the counter.
            if let Some(ref c) = self.queued_counter {
                c.increment();
            }
            let msg = ArrayMessage {
                array,
                counter: self.queued_counter.clone(),
                done_tx: None,
            };
            return match self.tx.try_send(msg) {
                Ok(()) => PublishOutcome::Delivered,
                // `msg` is dropped here → counter decremented by ArrayMessage::drop.
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    if count_drop {
                        self.dropped_arrays.fetch_add(1, Ordering::AcqRel);
                    }
                    PublishOutcome::DroppedQueueFull
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    PublishOutcome::ChannelClosed
                }
            };
        }

        // Reliable blocking path: never drops, awaits completion.
        if let Some(ref c) = self.queued_counter {
            c.increment();
        }
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let msg = ArrayMessage {
            array,
            counter: self.queued_counter.clone(),
            done_tx: Some(done_tx),
        };
        if self.tx.send(msg).await.is_err() {
            // Channel closed — counter was decremented by ArrayMessage::drop
            return PublishOutcome::ChannelClosed;
        }
        let _ = done_rx.await;
        PublishOutcome::Delivered
    }

    /// Whether this sender's plugin has callbacks enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Whether this sender's plugin is in blocking mode.
    pub fn is_blocking(&self) -> bool {
        self.blocking_mode.load(Ordering::Acquire)
    }

    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    /// Set the queued-array counter for tracking in-flight arrays.
    pub fn set_queued_counter(&mut self, counter: Arc<QueuedArrayCounter>) {
        self.queued_counter = Some(counter);
    }

    /// Attach the downstream plugin's shared `DroppedArrays` counter so that
    /// a full-queue drop on this sender is accounted to that plugin (C++ parity).
    pub fn set_dropped_arrays_counter(&mut self, counter: Arc<AtomicI32>) {
        self.dropped_arrays = counter;
    }

    /// The shared `DroppedArrays` counter for this sender's downstream queue.
    pub fn dropped_arrays_counter(&self) -> &Arc<AtomicI32> {
        &self.dropped_arrays
    }

    /// Current capacity (free slots) of the downstream input queue.
    pub fn capacity(&self) -> usize {
        self.tx.capacity()
    }

    /// Maximum capacity of the downstream input queue.
    pub fn max_capacity(&self) -> usize {
        self.tx.max_capacity()
    }

    /// Set the enabled/blocking mode flags (used by plugin runtime wiring).
    pub(crate) fn set_mode_flags(
        &mut self,
        enabled: Arc<AtomicBool>,
        blocking_mode: Arc<AtomicBool>,
    ) {
        self.enabled = enabled;
        self.blocking_mode = blocking_mode;
    }
}

/// Receiver held by downstream plugin.
pub struct NDArrayReceiver {
    rx: tokio::sync::mpsc::Receiver<ArrayMessage>,
}

impl NDArrayReceiver {
    /// Number of currently buffered (pending) messages in the input queue.
    pub fn pending(&self) -> usize {
        self.rx.len()
    }

    /// Maximum capacity of the input queue.
    pub fn max_capacity(&self) -> usize {
        self.rx.max_capacity()
    }

    /// Number of free slots in the input queue (`max_capacity - pending`).
    pub fn capacity(&self) -> usize {
        self.rx.capacity()
    }

    /// Blocking receive (for use in std::thread data processing loops).
    pub fn blocking_recv(&mut self) -> Option<Arc<NDArray>> {
        self.rx.blocking_recv().map(|msg| msg.array.clone())
    }

    /// Async receive.
    pub async fn recv(&mut self) -> Option<Arc<NDArray>> {
        self.rx.recv().await.map(|msg| msg.array.clone())
    }

    /// Receive the full ArrayMessage (crate-internal). The message's Drop
    /// will signal completion when the caller is done with it.
    pub(crate) async fn recv_msg(&mut self) -> Option<ArrayMessage> {
        self.rx.recv().await
    }
}

/// Create a matched sender/receiver pair.
pub fn ndarray_channel(port_name: &str, queue_size: usize) -> (NDArraySender, NDArrayReceiver) {
    let (tx, rx) = tokio::sync::mpsc::channel(queue_size.max(1));
    (
        NDArraySender {
            tx,
            port_name: port_name.to_string(),
            enabled: Arc::new(AtomicBool::new(true)),
            blocking_mode: Arc::new(AtomicBool::new(false)),
            queued_counter: None,
            dropped_arrays: Arc::new(AtomicI32::new(0)),
        },
        NDArrayReceiver { rx },
    )
}

/// Fan-out: publishes arrays to multiple downstream receivers.
pub struct NDArrayOutput {
    senders: Vec<NDArraySender>,
}

impl NDArrayOutput {
    pub fn new() -> Self {
        Self {
            senders: Vec::new(),
        }
    }

    pub fn add(&mut self, sender: NDArraySender) {
        self.senders.push(sender);
    }

    pub fn remove(&mut self, port_name: &str) {
        self.senders.retain(|s| s.port_name != port_name);
    }

    /// Remove a sender by port name and return it (if found).
    pub fn take(&mut self, port_name: &str) -> Option<NDArraySender> {
        let idx = self.senders.iter().position(|s| s.port_name == port_name)?;
        Some(self.senders.swap_remove(idx))
    }

    /// Publish an array to all downstream receivers (async, concurrent).
    ///
    /// Each sender publishes independently. Returns the per-sender outcomes
    /// so the caller can count `DroppedArrays` for any downstream whose queue
    /// was full (C++ `driverCallback` semantics).
    pub async fn publish(&self, array: Arc<NDArray>) -> Vec<PublishOutcome> {
        let futs = self.senders.iter().map(|s| s.publish(array.clone()));
        futures_util::future::join_all(futs).await
    }

    /// Publish an array to a single downstream receiver by index (for scatter/round-robin).
    pub async fn publish_to(&self, index: usize, array: Arc<NDArray>) -> Option<PublishOutcome> {
        if let Some(sender) = self.senders.get(index % self.senders.len().max(1)) {
            Some(sender.publish(array).await)
        } else {
            None
        }
    }

    pub fn num_senders(&self) -> usize {
        self.senders.len()
    }

    /// Clone the senders list (for publishing outside a lock in async context).
    pub(crate) fn senders_clone(&self) -> Vec<NDArraySender> {
        self.senders.clone()
    }
}

/// Cloneable async handle for publishing arrays to downstream plugins.
///
/// This is the public API for driver acquisition tasks.
/// Internally it snapshots the sender list, releases the lock, then
/// publishes to all senders concurrently.
///
/// # Example
/// ```ignore
/// if config.array_callbacks {
///     publisher.publish(Arc::new(frame)).await;
/// }
/// ```
#[derive(Clone)]
pub struct ArrayPublisher {
    output: Arc<parking_lot::Mutex<NDArrayOutput>>,
}

impl ArrayPublisher {
    /// Create a publisher backed by the given output.
    pub fn new(output: Arc<parking_lot::Mutex<NDArrayOutput>>) -> Self {
        Self { output }
    }

    /// Publish an array to all downstream plugins (async, concurrent fan-out).
    ///
    /// Returns the per-downstream outcomes — a `DroppedQueueFull` entry means
    /// that downstream plugin's input queue was full and the array was dropped
    /// (C++ `driverCallback` `trySend`). The driver should count those as
    /// `DroppedArrays`.
    pub async fn publish(&self, array: Arc<NDArray>) -> Vec<PublishOutcome> {
        let senders = self.output.lock().senders_clone();
        let futs = senders.iter().map(|s| s.publish(array.clone()));
        futures_util::future::join_all(futs).await
    }
}

impl Default for NDArrayOutput {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndarray::{NDArray, NDDataType, NDDimension};

    fn make_test_array(id: i32) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        Arc::new(arr)
    }

    #[tokio::test]
    async fn test_publish_receive_basic() {
        let (sender, mut receiver) = ndarray_channel("TEST", 10);
        sender.publish(make_test_array(1)).await;
        sender.publish(make_test_array(2)).await;

        let a1 = receiver.recv().await.unwrap();
        assert_eq!(a1.unique_id, 1);
        let a2 = receiver.recv().await.unwrap();
        assert_eq!(a2.unique_id, 2);
    }

    #[tokio::test]
    async fn test_publish_blocking_no_drop() {
        // In blocking_callbacks mode, reliable send().await is used: even a
        // queue of 1 must not drop — the producer back-pressures instead.
        let (sender, mut receiver) = ndarray_channel("TEST", 1);
        sender.blocking_mode.store(true, Ordering::Release);

        let s = sender.clone();
        let pub_handle = tokio::spawn(async move {
            s.publish(make_test_array(1)).await;
            s.publish(make_test_array(2)).await;
            s.publish(make_test_array(3)).await;
        });

        // Receive all 3 — no drops in blocking mode.
        let a1 = receiver.recv().await.unwrap();
        assert_eq!(a1.unique_id, 1);
        let a2 = receiver.recv().await.unwrap();
        assert_eq!(a2.unique_id, 2);
        let a3 = receiver.recv().await.unwrap();
        assert_eq!(a3.unique_id, 3);

        pub_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_publish_drops_on_full_queue() {
        // B1: default (non-blocking) mode drops on a full queue and reports
        // DroppedQueueFull, matching C++ trySend.
        let (sender, _receiver) = ndarray_channel("TEST", 1);

        // First publish fills the queue.
        assert_eq!(
            sender.publish(make_test_array(1)).await,
            PublishOutcome::Delivered
        );
        // Second publish finds the queue full → dropped + counted.
        assert_eq!(
            sender.publish(make_test_array(2)).await,
            PublishOutcome::DroppedQueueFull
        );
    }

    #[tokio::test]
    async fn test_drop_on_full_does_not_leak_counter() {
        // A dropped array must not leave the queued-array counter incremented.
        let counter = Arc::new(QueuedArrayCounter::new());
        let (mut sender, _receiver) = ndarray_channel("TEST", 1);
        sender.set_queued_counter(counter.clone());

        sender.publish(make_test_array(1)).await; // delivered, counter=1
        assert_eq!(counter.get(), 1);
        let outcome = sender.publish(make_test_array(2)).await; // dropped
        assert_eq!(outcome, PublishOutcome::DroppedQueueFull);
        // Counter must still be 1 — the dropped message decremented on drop.
        assert_eq!(counter.get(), 1);
    }

    #[tokio::test]
    async fn test_blocking_callbacks_completion_wait() {
        let (sender, mut receiver) = ndarray_channel("TEST", 10);
        sender.blocking_mode.store(true, Ordering::Release);

        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();

        // Spawn receiver that takes some time to process
        let recv_handle = tokio::spawn(async move {
            let msg = receiver.recv_msg().await.unwrap();
            assert_eq!(msg.array.unique_id, 42);
            // Simulate processing time
            tokio::time::sleep(Duration::from_millis(50)).await;
            completed_clone.store(true, Ordering::Release);
            // msg dropped here → done_tx fires
        });

        // publish() should wait for completion
        sender.publish(make_test_array(42)).await;

        // By the time publish returns, downstream should have completed
        assert!(completed.load(Ordering::Acquire));

        recv_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_fanout_three_receivers() {
        let (s1, mut r1) = ndarray_channel("P1", 10);
        let (s2, mut r2) = ndarray_channel("P2", 10);
        let (s3, mut r3) = ndarray_channel("P3", 10);

        let mut output = NDArrayOutput::new();
        output.add(s1);
        output.add(s2);
        output.add(s3);

        output.publish(make_test_array(42)).await;

        assert_eq!(r1.recv().await.unwrap().unique_id, 42);
        assert_eq!(r2.recv().await.unwrap().unique_id, 42);
        assert_eq!(r3.recv().await.unwrap().unique_id, 42);
    }

    #[test]
    fn test_blocking_recv() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (sender, mut receiver) = ndarray_channel("TEST", 10);

        let handle = std::thread::spawn(move || {
            let arr = receiver.blocking_recv().unwrap();
            arr.unique_id
        });

        rt.block_on(sender.publish(make_test_array(99)));
        let id = handle.join().unwrap();
        assert_eq!(id, 99);
    }

    #[tokio::test]
    async fn test_channel_closed_on_receiver_drop() {
        let (sender, receiver) = ndarray_channel("TEST", 10);
        drop(receiver);
        // Sending to closed channel should not panic
        sender.publish(make_test_array(1)).await;
    }

    #[test]
    fn test_queued_counter_basic() {
        let counter = QueuedArrayCounter::new();
        assert_eq!(counter.get(), 0);
        counter.increment();
        assert_eq!(counter.get(), 1);
        counter.increment();
        assert_eq!(counter.get(), 2);
        counter.decrement();
        assert_eq!(counter.get(), 1);
        counter.decrement();
        assert_eq!(counter.get(), 0);
    }

    #[test]
    fn test_queued_counter_wait_until_zero() {
        let counter = Arc::new(QueuedArrayCounter::new());
        counter.increment();
        counter.increment();

        let c = counter.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            c.decrement();
            std::thread::sleep(Duration::from_millis(10));
            c.decrement();
        });

        assert!(counter.wait_until_zero(Duration::from_secs(5)));
        h.join().unwrap();
    }

    #[test]
    fn test_queued_counter_wait_timeout() {
        let counter = Arc::new(QueuedArrayCounter::new());
        counter.increment();
        assert!(!counter.wait_until_zero(Duration::from_millis(10)));
    }

    #[tokio::test]
    async fn test_publish_increments_counter() {
        let counter = Arc::new(QueuedArrayCounter::new());
        let (mut sender, mut _receiver) = ndarray_channel("TEST", 10);
        sender.set_queued_counter(counter.clone());

        sender.publish(make_test_array(1)).await;
        assert_eq!(counter.get(), 1);
        sender.publish(make_test_array(2)).await;
        assert_eq!(counter.get(), 2);
    }

    #[tokio::test]
    async fn test_message_drop_decrements() {
        let counter = Arc::new(QueuedArrayCounter::new());
        counter.increment();
        let msg = ArrayMessage {
            array: make_test_array(1),
            counter: Some(counter.clone()),
            done_tx: None,
        };
        assert_eq!(counter.get(), 1);
        drop(msg);
        assert_eq!(counter.get(), 0);
    }
}
