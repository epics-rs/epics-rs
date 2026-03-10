use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::ndarray::NDArray;

/// Sender held by upstream. Non-blocking send with drop-newest policy.
#[derive(Clone)]
pub struct NDArraySender {
    tx: tokio::sync::mpsc::Sender<Arc<NDArray>>,
    port_name: String,
    dropped_count: Arc<AtomicU64>,
}

impl NDArraySender {
    /// Try to send an array. If the channel is full, increment dropped_count and discard.
    pub fn send(&self, array: Arc<NDArray>) {
        match self.tx.try_send(array) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.dropped_count.fetch_add(1, Ordering::Relaxed);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped_count.load(Ordering::Relaxed)
    }
}

/// Receiver held by downstream plugin.
pub struct NDArrayReceiver {
    rx: tokio::sync::mpsc::Receiver<Arc<NDArray>>,
}

impl NDArrayReceiver {
    /// Blocking receive (for use in std::thread data processing loops).
    pub fn blocking_recv(&mut self) -> Option<Arc<NDArray>> {
        self.rx.blocking_recv()
    }

    /// Async receive.
    pub async fn recv(&mut self) -> Option<Arc<NDArray>> {
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
}
