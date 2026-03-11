use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::ndarray::NDArray;

/// Type-erased blocking processor for inline array processing.
pub(crate) trait BlockingProcessFn: Send + Sync {
    fn process_and_publish(&self, array: &NDArray);
}

/// Tracks completion of array processing across multiple non-blocking plugins.
pub struct ArrayCompletion {
    remaining: AtomicUsize,
    done: std::sync::Mutex<bool>,
    condvar: std::sync::Condvar,
}

impl ArrayCompletion {
    /// Create a new tracker expecting `count` completions.
    pub fn new(count: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(count),
            done: std::sync::Mutex::new(count == 0),
            condvar: std::sync::Condvar::new(),
        }
    }

    /// Signal that one plugin has finished processing (or the message was dropped).
    pub fn signal_one(&self) {
        let prev = self.remaining.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            let mut done = self.done.lock().unwrap();
            *done = true;
            self.condvar.notify_all();
        }
    }

    /// Block until all plugins have signaled completion, or timeout expires.
    /// Returns `true` if all completed, `false` on timeout.
    pub fn wait(&self, timeout: Duration) -> bool {
        let done = self.done.lock().unwrap();
        if *done {
            return true;
        }
        let result = self.condvar.wait_timeout_while(done, timeout, |d| !*d).unwrap();
        *result.0
    }

    /// Block until all plugins have signaled completion (no timeout).
    pub fn wait_forever(&self) {
        let mut done = self.done.lock().unwrap();
        while !*done {
            done = self.condvar.wait(done).unwrap();
        }
    }
}

/// Array message with optional completion tracking.
/// When dropped, signals the completion tracker (if present).
pub struct ArrayMessage {
    pub array: Arc<NDArray>,
    pub(crate) completion: Option<Arc<ArrayCompletion>>,
}

impl Drop for ArrayMessage {
    fn drop(&mut self) {
        if let Some(c) = self.completion.take() {
            c.signal_one();
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
        // Non-blocking path
        let msg = ArrayMessage { array, completion: None };
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.dropped_count.fetch_add(1, Ordering::Relaxed);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// Send an array message with completion tracking. Used by `publish_and_wait`.
    pub(crate) fn send_msg(&self, msg: ArrayMessage) {
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // msg is dropped here → Drop fires → completion signaled
                self.dropped_count.fetch_add(1, Ordering::Relaxed);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                // msg is dropped here → Drop fires → completion signaled
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

    /// Publish an array and wait for all non-blocking plugins to finish processing.
    /// Blocking plugins are processed inline on the caller's thread.
    /// Returns immediately if no non-blocking plugins are connected.
    pub fn publish_and_wait(&self, array: Arc<NDArray>) {
        // Count non-blocking, enabled senders
        let nb_count = self.senders.iter()
            .filter(|s| s.is_enabled() && !s.is_blocking())
            .count();

        let completion = if nb_count > 0 {
            Some(Arc::new(ArrayCompletion::new(nb_count)))
        } else {
            None
        };

        for sender in &self.senders {
            if !sender.is_enabled() {
                continue;
            }
            if sender.is_blocking() {
                // Process inline (no tracker needed)
                if let Some(ref bp) = sender.blocking_processor {
                    bp.process_and_publish(&array);
                }
            } else {
                // Non-blocking: send with completion tracker
                let msg = ArrayMessage {
                    array: array.clone(),
                    completion: completion.clone(),
                };
                sender.send_msg(msg);
            }
        }

        // Wait for all non-blocking plugins to finish
        if let Some(c) = completion {
            c.wait_forever();
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
    fn test_publish_and_wait_basic() {
        // Verify publish_and_wait returns after receivers consume the message
        let (s1, mut r1) = ndarray_channel("P1", 10);
        let (s2, mut r2) = ndarray_channel("P2", 10);

        let mut output = NDArrayOutput::new();
        output.add(s1);
        output.add(s2);

        // Spawn consumers that drain after a short delay
        let h1 = std::thread::spawn(move || {
            let arr = r1.blocking_recv().unwrap();
            assert_eq!(arr.unique_id, 7);
        });
        let h2 = std::thread::spawn(move || {
            let arr = r2.blocking_recv().unwrap();
            assert_eq!(arr.unique_id, 7);
        });

        output.publish_and_wait(make_test_array(7));
        // If we get here, both plugins have finished
        h1.join().unwrap();
        h2.join().unwrap();
    }

    #[test]
    fn test_publish_and_wait_queue_full() {
        // When queue is full, dropped message signals completion → no deadlock
        let (s1, _r1) = ndarray_channel("P1", 1);

        let mut output = NDArrayOutput::new();
        output.add(s1);

        // Fill the channel
        output.publish(make_test_array(1));

        // This should not deadlock: queue full → msg dropped → completion signaled
        output.publish_and_wait(make_test_array(2));
    }

    #[test]
    fn test_publish_and_wait_no_plugins() {
        let output = NDArrayOutput::new();
        // Should return immediately with no senders
        output.publish_and_wait(make_test_array(1));
    }

    #[test]
    fn test_publish_and_wait_disabled_plugin() {
        let (s1, _r1) = ndarray_channel("P1", 10);

        let mut output = NDArrayOutput::new();
        output.add(s1);

        // Disable the sender
        output.senders[0].enabled.store(false, Ordering::Release);

        // Should return immediately since the only plugin is disabled
        output.publish_and_wait(make_test_array(1));
    }

    #[test]
    fn test_array_completion_signal() {
        let completion = Arc::new(ArrayCompletion::new(2));
        let c1 = completion.clone();
        let c2 = completion.clone();

        let h = std::thread::spawn(move || {
            completion.wait(Duration::from_secs(5))
        });

        c1.signal_one();
        c2.signal_one();

        assert!(h.join().unwrap());
    }

    #[test]
    fn test_array_completion_zero_count() {
        let completion = ArrayCompletion::new(0);
        // Should return immediately
        assert!(completion.wait(Duration::from_millis(1)));
    }

    #[test]
    fn test_array_message_drop_signals() {
        let completion = Arc::new(ArrayCompletion::new(1));
        let msg = ArrayMessage {
            array: make_test_array(1),
            completion: Some(completion.clone()),
        };
        drop(msg);
        assert!(completion.wait(Duration::from_millis(1)));
    }
}
