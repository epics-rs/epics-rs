use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::sync::Notify;

use epics_base_rs::runtime::sync::{Mutex, mpsc};

use crate::protocol::*;
use epics_base_rs::server::pv::{MonitorEvent, ProcessVariable};
use epics_base_rs::types::encode_dbr;

#[derive(Default)]
pub struct FlowControlGate {
    paused: AtomicBool,
    resumed: Notify,
}

impl FlowControlGate {
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        self.resumed.notify_waiters();
    }

    pub async fn wait_until_resumed(&self) {
        while self.paused.load(Ordering::Acquire) {
            self.resumed.notified().await;
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub async fn coalesce_while_paused(
        &self,
        rx: &mut mpsc::Receiver<MonitorEvent>,
        mut pending: MonitorEvent,
    ) -> Option<MonitorEvent> {
        while self.is_paused() {
            while let Ok(event) = rx.try_recv() {
                pending = event;
            }
            if !self.is_paused() {
                break;
            }
            tokio::select! {
                maybe_event = rx.recv() => match maybe_event {
                    Some(event) => pending = event,
                    None => return None,
                },
                _ = self.resumed.notified() => {}
            }
        }
        Some(pending)
    }
}

/// Spawn a task that forwards monitor events from a PV subscription to the client TCP stream.
/// Returns a handle that can be used to cancel the subscription.
///
/// Generic over the writer type so the same task body works for plain
/// `tokio::net::tcp::OwnedWriteHalf` and the TLS-wrapped
/// `WriteHalf<TlsStream<TcpStream>>` produced by the server's TLS
/// dispatch path.
pub fn spawn_monitor_sender<W>(
    pv: Arc<ProcessVariable>,
    sub_id: u32,
    data_type: u16,
    writer: Arc<Mutex<BufWriter<W>>>,
    flow_control: Arc<FlowControlGate>,
    mut rx: mpsc::Receiver<MonitorEvent>,
) -> tokio::task::JoinHandle<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    epics_base_rs::runtime::task::spawn(async move {
        loop {
            // Prefer any coalesced overflow value before blocking on the
            // mpsc — when the queue filled up while we were busy, the
            // newest value is parked there waiting for delivery.
            let next = if let Some(ev) = pv.pop_coalesced(sub_id).await {
                Some(ev)
            } else {
                rx.recv().await
            };
            let Some(mut event) = next else { break };
            if flow_control.is_paused() {
                let Some(coalesced) = flow_control.coalesce_while_paused(&mut rx, event).await
                else {
                    break;
                };
                event = coalesced;
            }
            if send_event(data_type, sub_id, &event, &writer)
                .await
                .is_err()
            {
                break;
            }
        }
    })
}

async fn send_event<W: AsyncWrite + Unpin + Send + 'static>(
    data_type: u16,
    sub_id: u32,
    event: &MonitorEvent,
    writer: &Arc<Mutex<BufWriter<W>>>,
) -> std::io::Result<()> {
    let payload = encode_dbr(data_type, &event.snapshot)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "encode"))?;
    // CA-268: DBR_CLASS_NAME wire payload is always one fixed 40-byte
    // string regardless of the underlying value count. Same override
    // already applied at the GET / send_monitor_snapshot / RecordField
    // event loop sites. SimplePv channels carry no record_type, so
    // class_name stays None and the body is 40 zero bytes — matches
    // IOC behaviour for synthetic channels.
    let element_count = if data_type == epics_base_rs::types::DBR_CLASS_NAME {
        1
    } else {
        event.snapshot.value.count() as u32
    };
    let mut padded = payload;
    padded.resize(align8(padded.len()), 0);

    let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
    // C client TCP parser requires 8-byte aligned postsize
    hdr.set_payload_size(padded.len(), element_count);
    hdr.data_type = data_type;
    hdr.cid = 1; // ECA_NORMAL status
    hdr.available = sub_id;

    // Abort-safety: this runs inside a monitor task that
    // `handle_client` may `task.abort()` (EVENT_CANCEL / CLEAR_CHANNEL
    // / disconnect cleanup). `tokio::abort()` drops the task at the
    // next await point. If the header and payload were written in two
    // separate `write_all` awaits, an abort landing between them would
    // leave an orphan header in the shared BufWriter, mis-framing every
    // subsequent message the next lock holder ships. Build the whole
    // CA_PROTO_EVENT_ADD frame as ONE contiguous buffer and issue a
    // single `write_all`, so an abort can only land at a frame boundary
    // (before or after the complete write), never mid-frame. The flush
    // stays separate: an aborted flush merely leaves whole frames
    // buffered, which the next lock holder flushes — harmless.
    let hdr_bytes = hdr.to_bytes_extended();
    let mut frame = Vec::with_capacity(hdr_bytes.len() + padded.len());
    frame.extend_from_slice(&hdr_bytes);
    frame.extend_from_slice(&padded);
    let mut w = writer.lock().await;
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Mock `AsyncWrite` that records the length of every `poll_write`
    /// batch it receives. Wrapped in a zero-capacity `BufWriter`, each
    /// `write_all` is forwarded straight through (tokio's `BufWriter`
    /// bypasses its buffer when the input is at least as large as the
    /// buffer capacity), so the recorded batches map 1:1 to the
    /// `write_all` calls `send_event` issues.
    #[derive(Default)]
    struct RecordingWriter {
        /// One entry per `poll_write` batch — the bytes delivered.
        batches: Vec<Vec<u8>>,
    }

    impl AsyncWrite for RecordingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.batches.push(buf.to_vec());
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Abort-safety regression: `send_event` must emit the CA_PROTO_EVENT_ADD
    /// header and payload as ONE contiguous `write_all`. A split across two
    /// `write_all` awaits would let a `task.abort()` land between them,
    /// leaving an orphan header in the shared `BufWriter` and mis-framing
    /// every subsequent message. A true abort-race is non-deterministic to
    /// schedule, so this asserts the structural property that makes the
    /// race impossible: exactly one write batch, equal to the full frame.
    #[tokio::test]
    async fn send_event_writes_frame_in_single_write_all() {
        use epics_base_rs::server::pv::MonitorEvent;
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::{DBR_LONG, EpicsValue};

        // Zero-capacity BufWriter: every write_all forwards directly to the
        // RecordingWriter, so batch count == write_all count.
        let writer = Arc::new(Mutex::new(BufWriter::with_capacity(
            0,
            RecordingWriter::default(),
        )));

        let snapshot = Snapshot::new(
            EpicsValue::Long(42),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let event = MonitorEvent {
            snapshot,
            origin: 0,
        };

        send_event(DBR_LONG, 7, &event, &writer)
            .await
            .expect("send_event must succeed");

        let guard = writer.lock().await;
        let batches = &guard.get_ref().batches;

        // Exactly one write batch — header and payload are not split.
        assert_eq!(
            batches.len(),
            1,
            "send_event must issue exactly one write_all (got {} batches: {:?})",
            batches.len(),
            batches.iter().map(|b| b.len()).collect::<Vec<_>>(),
        );

        let frame = &batches[0];

        // A single scalar DBR_LONG (4 bytes -> 8 padded, count 1) stays
        // under the 0xFFFF extended-header threshold, so the frame is a
        // standard 16-byte header followed by the padded payload. The
        // single batch must be exactly that complete frame.
        assert!(
            frame.len() >= 16,
            "frame shorter than a CA header: {} bytes",
            frame.len(),
        );
        let payload_size = u16::from_be_bytes([frame[2], frame[3]]) as usize;
        assert_ne!(
            payload_size, 0xFFFF,
            "test value unexpectedly produced an extended header",
        );
        assert_eq!(
            16 + payload_size,
            frame.len(),
            "header-declared payload size ({payload_size}) plus header (16) \
             must equal the contiguous frame length ({})",
            frame.len(),
        );
        // Payload is 8-byte aligned (C client TCP parser requirement).
        assert_eq!(payload_size % 8, 0, "payload not 8-byte aligned");
    }
}
