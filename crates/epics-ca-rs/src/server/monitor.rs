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

    let hdr_bytes = hdr.to_bytes_extended();
    let mut w = writer.lock().await;
    w.write_all(&hdr_bytes).await?;
    w.write_all(&padded).await?;
    w.flush().await?;
    Ok(())
}
