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
        loop {
            // Register the resume waiter eagerly with `enable()` BEFORE
            // re-reading the pause flag. `Notify::notified()` does not
            // register until first polled and `notify_waiters()` stores
            // no permit, so without this a `resume()` firing between the
            // `paused.load()` and the `.await` would be lost and leave
            // this blocked until the next resume. `resume()` stores
            // `paused = false` (Release) before notifying, so if we still
            // observe `paused` here the broadcast has not fired yet and is
            // guaranteed to land on this enabled waiter. Same lost-wake-
            // safe pattern as `coalesce_while_paused` and
            // `Channel::wait_until_inactive`.
            let resumed = self.resumed.notified();
            tokio::pin!(resumed);
            resumed.as_mut().enable();
            if !self.paused.load(Ordering::Acquire) {
                return;
            }
            resumed.await;
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
        loop {
            // Register the resume waiter eagerly BEFORE re-reading the
            // pause flag. `resume()` stores `paused = false` (Release)
            // *before* `notify_waiters()`, so if we still observe
            // `is_paused()` here the broadcast has not fired yet and is
            // guaranteed to land on this already-enabled waiter. This
            // closes the lost-wake gap where an EVENTS_ON arriving
            // between the recheck and the `select!` await would otherwise
            // be dropped — `Notify::notified()` does not register until
            // first polled, and `notify_waiters()` stores no permit.
            // `notify_waiters` (broadcast) is required rather than
            // `notify_one`: one circuit-level gate fans out to every
            // monitor task on the connection, and `notify_one` would wake
            // only one of them. Same pattern as
            // `Channel::wait_until_inactive`.
            let resumed = self.resumed.notified();
            tokio::pin!(resumed);
            resumed.as_mut().enable();
            if !self.is_paused() {
                break;
            }
            // Collapse the backlog to the latest value while paused.
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
                _ = resumed => {}
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
/// R2-12: `data_count` is the original EVENT_ADD request count. When
/// non-zero, every monitor delivery echoes this in the header and
/// zero-pads short payloads up to `dbr_buffer_size(type, native,
/// count)` — matches C `read_reply` which keeps the request count
/// and pads (or uses `snapshot.value.count()` when the request was
/// autosize=0).
#[allow(clippy::too_many_arguments)]
pub fn spawn_monitor_sender<W>(
    pv: Arc<ProcessVariable>,
    sub_id: u32,
    data_type: u16,
    data_count: u32,
    writer: Arc<Mutex<BufWriter<W>>>,
    flow_control: Arc<FlowControlGate>,
    mut rx: mpsc::Receiver<MonitorEvent>,
    denied: Arc<AtomicBool>,
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
            // C `casAccessRightsCB` (`rsrv/camessage.c:1080-1095`)
            // suppresses event deliveries with `db_event_disable`
            // while read access is denied (without tearing the
            // subscription down). Producer keeps running so a
            // later re-enable resumes the same camonitor; we just
            // drop the event silently.
            if denied.load(Ordering::Acquire) {
                continue;
            }
            if send_event(data_type, data_count, sub_id, &event, &writer)
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
    data_count: u32,
    sub_id: u32,
    event: &MonitorEvent,
    writer: &Arc<Mutex<BufWriter<W>>>,
) -> std::io::Result<()> {
    let mut payload = encode_dbr(data_type, &event.snapshot)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "encode"))?;
    // CA-268: DBR_CLASS_NAME wire payload is always one fixed 40-byte
    // string regardless of the underlying value count. Same override
    // already applied at the GET / send_monitor_snapshot / RecordField
    // event loop sites. SimplePv channels carry no record_type, so
    // class_name stays None and the body is 40 zero bytes — matches
    // IOC behaviour for synthetic channels.
    //
    // R2-12: when the EVENT_ADD request set an explicit count, every
    // monitor delivery echoes that count and zero-pads the payload up
    // to `dbr_buffer_size(type, native, count)` (C `read_reply`
    // `rsrv/camessage.c:507-571` parity). The helper returns the
    // header count to use; `data_count == 0` means autosize (use the
    // live snapshot count).
    // R2-12 refinement: enforce request count in BOTH directions —
    // pad when requested > actual AND truncate when requested <
    // actual. C `read_reply` (`rsrv/camessage.c:507-571`) sizes
    // the payload to `dbr_size_n(type, request_count)` either way.
    let actual_count = event.snapshot.value.count() as u32;
    let element_count = if data_type == epics_base_rs::types::DBR_CLASS_NAME {
        1
    } else if data_count == 0 {
        actual_count
    } else if let Ok(native) = epics_base_rs::types::native_type_for_dbr(data_type) {
        let meta_size = epics_base_rs::types::dbr_buffer_size(data_type, native, 0);
        let target_size = meta_size + (data_count as usize) * native.element_size();
        if data_count > actual_count {
            let cur = payload.len();
            if cur < target_size {
                payload.extend(std::iter::repeat_n(0u8, target_size - cur));
            }
        } else if data_count < actual_count && payload.len() > target_size {
            payload.truncate(target_size);
        }
        data_count
    } else {
        data_count
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

        // data_count = 0 means autosize (use snapshot's actual count);
        // matches every pre-R2-12 producer caller.
        send_event(DBR_LONG, 0, 7, &event, &writer)
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

    /// FlowControlGate (EVENTS_OFF/EVENTS_ON) pause/resume boundaries.
    /// The gate is the single owner of the monitor pause transition that
    /// both `spawn_monitor_sender` and the record-field monitor loop
    /// acquire through `coalesce_while_paused`. Tested by boundary
    /// (paused vs not at entry, backlog squash, channel open vs closed,
    /// resume wake) rather than by narrative scenario.
    mod flow_control_gate {
        use super::*;
        use epics_base_rs::server::pv::MonitorEvent;
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::EpicsValue;

        fn ev(v: i32) -> MonitorEvent {
            MonitorEvent {
                snapshot: Snapshot::new(
                    EpicsValue::Long(v),
                    0,
                    0,
                    std::time::SystemTime::UNIX_EPOCH,
                ),
                origin: 0,
            }
        }

        fn value_of(e: &MonitorEvent) -> i32 {
            match e.snapshot.value {
                EpicsValue::Long(v) => v,
                ref other => panic!("expected Long, got {other:?}"),
            }
        }

        /// Not paused at entry → the pending value is returned at once,
        /// without waiting on any rx event or resume.
        #[tokio::test]
        async fn not_paused_at_entry_returns_pending_immediately() {
            let gate = FlowControlGate::default();
            // No sender is ever used; default gate is not paused.
            let (_tx, mut rx) = mpsc::channel::<MonitorEvent>(1);
            let got = gate.coalesce_while_paused(&mut rx, ev(7)).await;
            assert_eq!(value_of(&got.expect("returns pending")), 7);
        }

        /// Paused → backlog squashes to the latest into `pending`; the
        /// resume flushes only that latest value (no per-event frame).
        #[tokio::test]
        async fn coalesce_to_latest_while_paused_then_resume_flushes() {
            let gate = Arc::new(FlowControlGate::default());
            gate.pause();
            let (tx, mut rx) = mpsc::channel::<MonitorEvent>(8);
            let g2 = gate.clone();
            let task = epics_base_rs::runtime::task::spawn(async move {
                g2.coalesce_while_paused(&mut rx, ev(1)).await
            });
            // Feed newer values while paused; each yield lets the gate
            // absorb the value into `pending` and park again.
            for v in [2i32, 3, 4] {
                tx.send(ev(v)).await.unwrap();
                tokio::task::yield_now().await;
            }
            gate.resume();
            let got = task.await.unwrap().expect("resume delivers latest");
            assert_eq!(
                value_of(&got),
                4,
                "coalesce yields only the latest value on resume"
            );
        }

        /// Paused + the source channel closes → `None` (caller ends the
        /// subscription), not a hang.
        #[tokio::test]
        async fn channel_close_while_paused_returns_none() {
            let gate = Arc::new(FlowControlGate::default());
            gate.pause();
            let (tx, mut rx) = mpsc::channel::<MonitorEvent>(1);
            let g2 = gate.clone();
            let task = epics_base_rs::runtime::task::spawn(async move {
                g2.coalesce_while_paused(&mut rx, ev(1)).await
            });
            tokio::task::yield_now().await; // let the consumer park in the select
            drop(tx); // closes the channel; rx.recv() resolves to None
            assert!(
                task.await.unwrap().is_none(),
                "channel close while paused yields None"
            );
        }

        /// A resume delivered while the consumer is parked must wake it
        /// and flush the held value WITHOUT requiring a further rx event.
        /// The held value is the only one ever sent, so a lost resume
        /// wake would strand the consumer forever.
        #[tokio::test]
        async fn resume_flushes_held_without_further_event() {
            let gate = Arc::new(FlowControlGate::default());
            gate.pause();
            let (tx, mut rx) = mpsc::channel::<MonitorEvent>(4);
            let g2 = gate.clone();
            let task = epics_base_rs::runtime::task::spawn(async move {
                g2.coalesce_while_paused(&mut rx, ev(1)).await
            });
            // One value arrives during the pause, then the source goes
            // quiet — only a resume can release the consumer.
            tx.send(ev(5)).await.unwrap();
            tokio::task::yield_now().await;
            gate.resume();
            let got = tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .expect("resume must wake a parked consumer (no lost wake)")
                .unwrap()
                .expect("resume delivers the held value");
            assert_eq!(value_of(&got), 5);
        }

        /// `wait_until_resumed` (used by external callers of the gate)
        /// must return after a resume even when the resume races the
        /// consumer's entry — the eager `enable()` closes the same
        /// lost-wake gap as `coalesce_while_paused`.
        #[tokio::test]
        async fn wait_until_resumed_unblocks_on_resume() {
            let gate = Arc::new(FlowControlGate::default());
            gate.pause();
            let g2 = gate.clone();
            let task =
                epics_base_rs::runtime::task::spawn(async move { g2.wait_until_resumed().await });
            tokio::task::yield_now().await; // let it park
            gate.resume();
            tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .expect("wait_until_resumed must return after resume")
                .unwrap();
        }
    }
}
