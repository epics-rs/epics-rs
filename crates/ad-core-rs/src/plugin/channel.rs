use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crate::ndarray::NDArray;

/// Type-erased blocking processor for inline array processing.
pub(crate) trait BlockingProcessFn: Send + Sync {
    fn process_and_publish(&self, array: &NDArray);
}

/// Tracks the number of queued (in-flight) arrays across non-blocking plugins.
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

    /// Increment the queued count (called before try_send).
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

/// Array message with optional queued-array counter.
/// When dropped, decrements the counter (if present).
pub struct ArrayMessage {
    pub array: Arc<NDArray>,
    pub(crate) counter: Option<Arc<QueuedArrayCounter>>,
}

impl Drop for ArrayMessage {
    fn drop(&mut self) {
        if let Some(c) = self.counter.take() {
            c.decrement();
        }
    }
}

/// Sender held by upstream. Supports blocking and non-blocking modes.
#[derive(Clone)]
pub struct NDArraySender {
    tx: tokio::sync::mpsc::Sender<ArrayMessage>,
    port_name: String,
    dropped_count: Arc<AtomicU64>,
    enabled: Arc<AtomicBool>,
    blocking_mode: Arc<AtomicBool>,
    blocking_processor: Option<Arc<dyn BlockingProcessFn>>,
    queued_counter: Option<Arc<QueuedArrayCounter>>,
}

impl NDArraySender {
    /// Send an array downstream. Behavior depends on mode:
    /// - Disabled (`enable_callbacks=0`): silently dropped
    /// - Blocking (`blocking_callbacks=1`): processed inline on caller's thread
    /// - Non-blocking (default): queued for data thread (dropped if full)
    pub fn send(&self, array: Arc<NDArray>) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        if self.blocking_mode.load(Ordering::Acquire) {
            if let Some(ref bp) = self.blocking_processor {
                bp.process_and_publish(&array);
                return;
            }
        }
        // Non-blocking path: increment counter before try_send
        if let Some(ref c) = self.queued_counter {
            c.increment();
        }
        let msg = ArrayMessage {
            array,
            counter: self.queued_counter.clone(),
        };
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // msg dropped here → Drop fires → counter decremented (net 0)
                self.dropped_count.fetch_add(1, Ordering::Relaxed);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                // msg dropped here → Drop fires → counter decremented
            }
        }
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

    pub fn dropped_count(&self) -> u64 {
        self.dropped_count.load(Ordering::Relaxed)
    }

    /// Set the queued-array counter for tracking in-flight arrays.
    pub fn set_queued_counter(&mut self, counter: Arc<QueuedArrayCounter>) {
        self.queued_counter = Some(counter);
    }

    /// Configure blocking callback support. Used by plugin runtime.
    pub(crate) fn with_blocking_support(
        self,
        enabled: Arc<AtomicBool>,
        blocking_mode: Arc<AtomicBool>,
        blocking_processor: Arc<dyn BlockingProcessFn>,
    ) -> Self {
        Self {
            enabled,
            blocking_mode,
            blocking_processor: Some(blocking_processor),
            ..self
        }
    }
}

/// Receiver held by downstream plugin.
pub struct NDArrayReceiver {
    rx: tokio::sync::mpsc::Receiver<ArrayMessage>,
}

impl NDArrayReceiver {
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
            dropped_count: Arc::new(AtomicU64::new(0)),
            enabled: Arc::new(AtomicBool::new(true)),
            blocking_mode: Arc::new(AtomicBool::new(false)),
            blocking_processor: None,
            queued_counter: None,
        },
        NDArrayReceiver { rx },
    )
}

/// Fan-out: broadcasts arrays to multiple downstream receivers.
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

    /// Publish an array to all downstream receivers.
    pub fn publish(&self, array: Arc<NDArray>) {
        for sender in &self.senders {
            sender.send(array.clone());
        }
    }

    pub fn total_dropped(&self) -> u64 {
        self.senders.iter().map(|s| s.dropped_count()).sum()
    }

    pub fn num_senders(&self) -> usize {
        self.senders.len()
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

    #[test]
    fn test_send_receive_basic() {
        let (sender, mut receiver) = ndarray_channel("TEST", 10);
        sender.send(make_test_array(1));
        sender.send(make_test_array(2));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let a1 = receiver.recv().await.unwrap();
            assert_eq!(a1.unique_id, 1);
            let a2 = receiver.recv().await.unwrap();
            assert_eq!(a2.unique_id, 2);
        });
    }

    #[test]
    fn test_back_pressure_drops() {
        let (sender, _receiver) = ndarray_channel("TEST", 2);
        // Fill the channel
        sender.send(make_test_array(1));
        sender.send(make_test_array(2));
        // This should be dropped
        sender.send(make_test_array(3));
        sender.send(make_test_array(4));

        assert_eq!(sender.dropped_count(), 2);
    }

    #[test]
    fn test_fanout_three_receivers() {
        let (s1, mut r1) = ndarray_channel("P1", 10);
        let (s2, mut r2) = ndarray_channel("P2", 10);
        let (s3, mut r3) = ndarray_channel("P3", 10);

        let mut output = NDArrayOutput::new();
        output.add(s1);
        output.add(s2);
        output.add(s3);

        output.publish(make_test_array(42));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert_eq!(r1.recv().await.unwrap().unique_id, 42);
            assert_eq!(r2.recv().await.unwrap().unique_id, 42);
            assert_eq!(r3.recv().await.unwrap().unique_id, 42);
        });
    }

    #[test]
    fn test_fanout_total_dropped() {
        let (s1, _r1) = ndarray_channel("P1", 1);
        let (s2, _r2) = ndarray_channel("P2", 1);

        let mut output = NDArrayOutput::new();
        output.add(s1);
        output.add(s2);

        // Fill both channels
        output.publish(make_test_array(1));
        // Both full now
        output.publish(make_test_array(2));

        assert_eq!(output.total_dropped(), 2);
    }

    #[test]
    fn test_fanout_remove() {
        let (s1, _r1) = ndarray_channel("P1", 10);
        let (s2, _r2) = ndarray_channel("P2", 10);

        let mut output = NDArrayOutput::new();
        output.add(s1);
        output.add(s2);
        assert_eq!(output.num_senders(), 2);

        output.remove("P1");
        assert_eq!(output.num_senders(), 1);
    }

    #[test]
    fn test_blocking_recv() {
        let (sender, mut receiver) = ndarray_channel("TEST", 10);

        let handle = std::thread::spawn(move || {
            let arr = receiver.blocking_recv().unwrap();
            arr.unique_id
        });

        sender.send(make_test_array(99));
        let id = handle.join().unwrap();
        assert_eq!(id, 99);
    }

    #[test]
    fn test_channel_closed_on_receiver_drop() {
        let (sender, receiver) = ndarray_channel("TEST", 10);
        drop(receiver);
        // Sending to closed channel should not panic
        sender.send(make_test_array(1));
        assert_eq!(sender.dropped_count(), 0); // closed, not "dropped"
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

    #[test]
    fn test_send_increments_counter() {
        let counter = Arc::new(QueuedArrayCounter::new());
        let (mut sender, _receiver) = ndarray_channel("TEST", 10);
        sender.set_queued_counter(counter.clone());

        sender.send(make_test_array(1));
        assert_eq!(counter.get(), 1);
        sender.send(make_test_array(2));
        assert_eq!(counter.get(), 2);
    }

    #[test]
    fn test_send_queue_full_no_net_increment() {
        let counter = Arc::new(QueuedArrayCounter::new());
        let (mut sender, _receiver) = ndarray_channel("TEST", 1);
        sender.set_queued_counter(counter.clone());

        sender.send(make_test_array(1)); // fills queue
        assert_eq!(counter.get(), 1);
        sender.send(make_test_array(2)); // queue full → dropped → net 0 change
        assert_eq!(counter.get(), 1);
    }

    #[test]
    fn test_message_drop_decrements() {
        let counter = Arc::new(QueuedArrayCounter::new());
        counter.increment();
        let msg = ArrayMessage {
            array: make_test_array(1),
            counter: Some(counter.clone()),
        };
        assert_eq!(counter.get(), 1);
        drop(msg);
        assert_eq!(counter.get(), 0);
    }
}
