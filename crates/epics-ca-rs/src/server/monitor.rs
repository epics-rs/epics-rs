use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::tcp::OwnedWriteHalf;

use epics_base_rs::runtime::sync::{Mutex, mpsc};

use crate::protocol::*;
use epics_base_rs::server::pv::{MonitorEvent, ProcessVariable};
use epics_base_rs::types::encode_dbr;

/// Spawn a task that forwards monitor events from a PV subscription to the client TCP stream.
/// Returns a handle that can be used to cancel the subscription.
pub fn spawn_monitor_sender(
    _pv: Arc<ProcessVariable>,
    sub_id: u32,
    data_type: u16,
    writer: Arc<Mutex<BufWriter<OwnedWriteHalf>>>,
    mut rx: mpsc::Receiver<MonitorEvent>,
) -> tokio::task::JoinHandle<()> {
    epics_base_rs::runtime::task::spawn(async move {
        while let Some(event) = rx.recv().await {
            let payload = match encode_dbr(data_type, &event.snapshot) {
                Ok(bytes) => bytes,
                Err(_) => break,
            };
            let element_count = event.snapshot.value.count() as u32;
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
            if w.write_all(&hdr_bytes).await.is_err() {
                break;
            }
            if w.write_all(&padded).await.is_err() {
                break;
            }
            let _ = w.flush().await;
        }
    })
}
