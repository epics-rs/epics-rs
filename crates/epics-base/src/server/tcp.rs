use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpListener;
use crate::runtime::sync::{Mutex, RwLock};

use crate::error::{CaError, CaResult};
use crate::protocol::*;
use crate::server::access_security::{AccessLevel, AccessSecurityConfig};
use crate::server::database::{parse_pv_name, PvDatabase, PvEntry};
use crate::server::monitor::spawn_monitor_sender;
use crate::server::pv::ProcessVariable;
use crate::server::record::RecordInstance;
use crate::types::{encode_dbr, native_type_for_dbr, DbFieldType, EpicsValue};

#[derive(Clone)]
enum ChannelTarget {
    SimplePv(Arc<ProcessVariable>),
    RecordField {
        record: Arc<RwLock<RecordInstance>>,
        field: String,
    },
}

struct ChannelEntry {
    target: ChannelTarget,
    cid: u32,
}

struct SubscriptionEntry {
    target: ChannelTarget,
    sub_id: u32,
    data_type: u16,
    task: tokio::task::JoinHandle<()>,
}

struct ClientState {
    channels: HashMap<u32, ChannelEntry>,
    subscriptions: HashMap<u32, SubscriptionEntry>,
    channel_access: HashMap<u32, AccessLevel>,
    next_sid: AtomicU32,
    hostname: String,
    username: String,
    acf: Arc<Option<AccessSecurityConfig>>,
    tcp_port: u16,
    client_minor_version: u16,
}

impl ClientState {
    fn new(acf: Arc<Option<AccessSecurityConfig>>, tcp_port: u16) -> Self {
        Self {
            channels: HashMap::new(),
            subscriptions: HashMap::new(),
            channel_access: HashMap::new(),
            next_sid: AtomicU32::new(1),
            hostname: String::new(),
            username: String::new(),
            acf,
            tcp_port,
            client_minor_version: 0,
        }
    }

    fn alloc_sid(&self) -> u32 {
        self.next_sid.fetch_add(1, Ordering::Relaxed)
    }

    /// Compute access rights bits for a channel target.
    async fn compute_access(&self, target: &ChannelTarget) -> u32 {
        match target {
            ChannelTarget::SimplePv(_) => {
                if let Some(ref acf_cfg) = *self.acf {
                    match acf_cfg.check_access("DEFAULT", &self.hostname, &self.username) {
                        AccessLevel::ReadWrite => 3,
                        AccessLevel::Read => 1,
                        AccessLevel::NoAccess => 0,
                    }
                } else {
                    3
                }
            }
            ChannelTarget::RecordField { record, field: f } => {
                let instance = record.read().await;
                let is_ro = instance
                    .record
                    .field_list()
                    .iter()
                    .find(|fd| fd.name == f.as_str())
                    .map(|fd| fd.read_only)
                    .unwrap_or(false);
                if is_ro {
                    1
                } else if let Some(ref acf_cfg) = *self.acf {
                    let asg = &instance.common.asg;
                    match acf_cfg.check_access(asg, &self.hostname, &self.username) {
                        AccessLevel::ReadWrite => 3,
                        AccessLevel::Read => 1,
                        AccessLevel::NoAccess => 0,
                    }
                } else {
                    3
                }
            }
        }
    }
}

/// Run the TCP listener for CA connections.
pub async fn run_tcp_listener(
    db: Arc<PvDatabase>,
    port: u16,
    acf: Arc<Option<AccessSecurityConfig>>,
) -> CaResult<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;

    loop {
        let (stream, peer) = listener.accept().await?;
        let db = db.clone();
        let acf = acf.clone();
        crate::runtime::task::spawn(async move {
            if let Err(e) = handle_client(stream, db, acf, port).await {
                eprintln!("client {peer} error: {e}");
            }
        });
    }
}

async fn handle_client(
    stream: tokio::net::TcpStream,
    db: Arc<PvDatabase>,
    acf: Arc<Option<AccessSecurityConfig>>,
    tcp_port: u16,
) -> CaResult<()> {
    // Disable Nagle's algorithm to avoid extra latency for small control messages.
    let _ = stream.set_nodelay(true);

    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(BufWriter::new(writer)));
    let mut state = ClientState::new(acf, tcp_port);

    let mut buf = vec![0u8; 8192];
    let mut accumulated = Vec::new();

    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        accumulated.extend_from_slice(&buf[..n]);

        let mut offset = 0;
        while offset + CaHeader::SIZE <= accumulated.len() {
            let (hdr, hdr_size) = CaHeader::from_bytes_extended(&accumulated[offset..])?;
            let actual_post = hdr.actual_postsize();
            let padded_post = align8(actual_post);
            let msg_len = hdr_size + padded_post;

            if offset + msg_len > accumulated.len() {
                break;
            }

            let payload = if actual_post > 0 {
                accumulated[offset + hdr_size..offset + hdr_size + actual_post].to_vec()
            } else {
                Vec::new()
            };

            dispatch_message(&hdr, &payload, &mut state, &db, &writer).await?;
            offset += msg_len;
        }

        if offset > 0 {
            accumulated.drain(..offset);
        }
    }

    // Cleanup: cancel all subscriptions
    for (_, sub) in state.subscriptions.drain() {
        sub.task.abort();
        match &sub.target {
            ChannelTarget::SimplePv(pv) => {
                pv.remove_subscriber(sub.sub_id).await;
            }
            ChannelTarget::RecordField { record, .. } => {
                record.write().await.remove_subscriber(sub.sub_id);
            }
        }
    }

    Ok(())
}

async fn dispatch_message(
    hdr: &CaHeader,
    payload: &[u8],
    state: &mut ClientState,
    db: &Arc<PvDatabase>,
    writer: &Arc<Mutex<BufWriter<OwnedWriteHalf>>>,
) -> CaResult<()> {
    match hdr.cmmd {
        CA_PROTO_VERSION => {
            state.client_minor_version = hdr.count;
            let mut resp = CaHeader::new(CA_PROTO_VERSION);
            resp.data_type = 1;
            resp.count = CA_MINOR_VERSION;
            resp.cid = 1;
            let mut w = writer.lock().await;
            w.write_all(&resp.to_bytes()).await?;
            w.flush().await?;
        }

        CA_PROTO_HOST_NAME => {
            let end = payload
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len());
            state.hostname = String::from_utf8_lossy(&payload[..end]).to_string();
            // Re-evaluate access rights for all existing channels
            reeval_access_rights(state, writer).await?;
        }

        CA_PROTO_CLIENT_NAME => {
            let end = payload
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len());
            state.username = String::from_utf8_lossy(&payload[..end]).to_string();
            // Re-evaluate access rights for all existing channels
            reeval_access_rights(state, writer).await?;
        }

        CA_PROTO_CREATE_CHAN => {
            // Pre-CA-4.4 clients send claims with no PV name (postsize=0).
            // Silently ignore these, matching C server behavior (camessage.c:1204).
            // The client will retry with v4.4+ format after receiving our VERSION.
            if hdr.actual_postsize() <= 1 {
                return Ok(());
            }

            let end = payload
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len());
            let pv_name = String::from_utf8_lossy(&payload[..end]).to_string();
            let client_cid = hdr.cid;
            let (_base, field_raw) = parse_pv_name(&pv_name);
            let field = field_raw.to_ascii_uppercase();

            if let Some(entry) = db.find_entry(&pv_name).await {
                let sid = state.alloc_sid();

                let (dbr_type, element_count, target) = match entry {
                    PvEntry::Simple(pv) => {
                        let value = pv.get().await;
                        (value.dbr_type(), value.count() as u32, ChannelTarget::SimplePv(pv))
                    }
                    PvEntry::Record(rec) => {
                        let instance = rec.read().await;
                        // Use resolve_field for 3-level priority
                        let value = instance.resolve_field(&field);
                        match value {
                            Some(v) => (
                                v.dbr_type(),
                                v.count() as u32,
                                ChannelTarget::RecordField {
                                    record: rec.clone(),
                                    field: field.clone(),
                                },
                            ),
                            None => {
                                // Field not found — send CREATE_CH_FAIL
                                let mut fail = CaHeader::new(CA_PROTO_CREATE_CH_FAIL);
                                fail.cid = client_cid;
                                let mut w = writer.lock().await;
                                w.write_all(&fail.to_bytes()).await?;
                                w.flush().await?;
                                return Ok(());
                            }
                        }
                    }
                };

                let access = state.compute_access(&target).await;
                let access_level = match access {
                    3 => AccessLevel::ReadWrite,
                    1 => AccessLevel::Read,
                    _ => AccessLevel::NoAccess,
                };

                state.channels.insert(
                    sid,
                    ChannelEntry {
                        target,
                        cid: client_cid,
                    },
                );
                state.channel_access.insert(sid, access_level);

                let mut ar = CaHeader::new(CA_PROTO_ACCESS_RIGHTS);
                ar.cid = client_cid;
                ar.available = access;

                let mut resp = CaHeader::new(CA_PROTO_CREATE_CHAN);
                resp.data_type = dbr_type as u16;
                resp.cid = client_cid;
                resp.available = sid;
                resp.set_payload_size(0, element_count);

                let mut w = writer.lock().await;
                w.write_all(&ar.to_bytes()).await?;
                w.write_all(&resp.to_bytes_extended()).await?;
                w.flush().await?;
            } else {
                // PV not found — send CREATE_CH_FAIL
                let mut fail = CaHeader::new(CA_PROTO_CREATE_CH_FAIL);
                fail.cid = client_cid;
                let mut w = writer.lock().await;
                w.write_all(&fail.to_bytes()).await?;
                w.flush().await?;
            }
        }

        CA_PROTO_READ_NOTIFY => {
            let sid = hdr.cid;
            let ioid = hdr.available;
            let requested_type = hdr.data_type;

            let entry = match state.channels.get(&sid) {
                Some(e) => e,
                None => {
                    send_cmd_error(writer, CA_PROTO_READ_NOTIFY, requested_type, ECA_BADCHID, ioid).await?;
                    return Ok(());
                }
            };

            let snapshot = get_full_snapshot(&entry.target).await;
            if let Some(snapshot) = snapshot {
                let data = match encode_dbr(requested_type, &snapshot) {
                    Ok(d) => d,
                    Err(_) => {
                        send_cmd_error(writer, CA_PROTO_READ_NOTIFY, requested_type, ECA_BADTYPE, ioid).await?;
                        return Ok(());
                    }
                };
                let element_count = snapshot.value.count() as u32;
                let mut padded = data;
                padded.resize(align8(padded.len()), 0);

                let mut resp = CaHeader::new(CA_PROTO_READ_NOTIFY);
                // C client TCP parser requires 8-byte aligned postsize
                resp.set_payload_size(padded.len(), element_count);
                resp.data_type = requested_type;
                resp.cid = ECA_NORMAL;
                resp.available = ioid;

                let mut w = writer.lock().await;
                w.write_all(&resp.to_bytes_extended()).await?;
                w.write_all(&padded).await?;
                w.flush().await?;
            }
        }

        CA_PROTO_WRITE | CA_PROTO_WRITE_NOTIFY => {
            let sid = hdr.cid;
            let ioid = hdr.available;
            let is_notify = hdr.cmmd == CA_PROTO_WRITE_NOTIFY;

            let write_type = match DbFieldType::from_u16(hdr.data_type) {
                Ok(t) => t,
                Err(_) => {
                    if is_notify {
                        send_cmd_error(writer, CA_PROTO_WRITE_NOTIFY, hdr.data_type, ECA_BADTYPE, ioid).await?;
                    }
                    return Ok(());
                }
            };

            let entry = match state.channels.get(&sid) {
                Some(e) => e,
                None => {
                    if is_notify {
                        send_cmd_error(writer, CA_PROTO_WRITE_NOTIFY, hdr.data_type, ECA_BADCHID, ioid).await?;
                    }
                    return Ok(());
                }
            };

            // Check access level
            let access = state.channel_access.get(&sid).copied().unwrap_or(AccessLevel::ReadWrite);
            if access != AccessLevel::ReadWrite {
                if is_notify {
                    let mut resp = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
                    resp.data_type = write_type as u16;
                    resp.count = 1;
                    resp.cid = ECA_NOWTACCESS;
                    resp.available = ioid;
                    let mut w = writer.lock().await;
                    w.write_all(&resp.to_bytes()).await?;
                    w.flush().await?;
                }
                return Ok(());
            }

            let count = hdr.actual_count() as usize;
            let new_value = match EpicsValue::from_bytes_array(write_type, payload, count) {
                Ok(v) => v,
                Err(_) => {
                    if is_notify {
                        send_cmd_error(writer, CA_PROTO_WRITE_NOTIFY, hdr.data_type, ECA_BADTYPE, ioid).await?;
                    }
                    return Ok(());
                }
            };

            let write_result: Result<(), CaError> = match &entry.target {
                ChannelTarget::SimplePv(pv) => {
                    pv.set(new_value).await;
                    Ok(())
                }
                ChannelTarget::RecordField { record, field } => {
                    let name = record.read().await.name.clone();
                    db.put_record_field_from_ca(&name, field, new_value).await
                }
            };

            // F1: CA_PROTO_WRITE (cmd=4) is fire-and-forget — no response
            if is_notify {
                let eca_status = match &write_result {
                    Ok(()) => ECA_NORMAL,
                    Err(e) => e.to_eca_status(),
                };

                // F6: Per spec, param1=status, param2=IOID
                let mut resp = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
                resp.data_type = write_type as u16;
                resp.count = 1;
                resp.cid = eca_status;
                resp.available = ioid;

                let mut w = writer.lock().await;
                w.write_all(&resp.to_bytes()).await?;
                w.flush().await?;
            }
        }

        CA_PROTO_EVENT_ADD => {
            let sid = hdr.cid;
            let sub_id = hdr.available;
            let requested_type = hdr.data_type;

            let native_type = match native_type_for_dbr(requested_type) {
                Ok(t) => t,
                Err(_) => {
                    send_cmd_error(writer, CA_PROTO_EVENT_ADD, requested_type, ECA_BADTYPE, sub_id).await?;
                    return Ok(());
                }
            };

            let mask = if payload.len() >= 14 {
                u16::from_be_bytes([payload[12], payload[13]])
            } else {
                DBE_VALUE | DBE_ALARM
            };

            let entry = match state.channels.get(&sid) {
                Some(e) => e,
                None => {
                    send_cmd_error(writer, CA_PROTO_EVENT_ADD, requested_type, ECA_BADCHID, sub_id).await?;
                    return Ok(());
                }
            };

            {
                match &entry.target {
                    ChannelTarget::SimplePv(pv) => {
                        let rx = pv.add_subscriber(sub_id, native_type, mask).await;

                        // Send initial value
                        let snap = pv.snapshot().await;
                        send_monitor_snapshot(
                            writer,
                            sub_id,
                            requested_type,
                            &snap,
                        ).await?;

                        let task = spawn_monitor_sender(
                            pv.clone(),
                            sub_id,
                            requested_type,
                            writer.clone(),
                            rx,
                        );

                        state.subscriptions.insert(
                            sub_id,
                            SubscriptionEntry {
                                target: ChannelTarget::SimplePv(pv.clone()),
                                sub_id,
                                data_type: requested_type,
                                task,
                            },
                        );
                    }
                    ChannelTarget::RecordField { record, field } => {
                        let mut instance = record.write().await;
                        let rx =
                            instance.add_subscriber(field, sub_id, native_type, mask);

                        // Send initial value with full metadata
                        if let Some(snap) = instance.snapshot_for_field(field) {
                            send_monitor_snapshot(
                                writer,
                                sub_id,
                                requested_type,
                                &snap,
                            ).await?;
                        }

                        let writer_clone = writer.clone();
                        let task = crate::runtime::task::spawn(async move {
                            let mut rx = rx;
                            while let Some(event) = rx.recv().await {
                                let payload_bytes = match encode_dbr(
                                    requested_type,
                                    &event.snapshot,
                                ) {
                                    Ok(bytes) => bytes,
                                    Err(_) => break,
                                };
                                let element_count = event.snapshot.value.count() as u32;
                                let mut padded = payload_bytes;
                                padded.resize(align8(padded.len()), 0);

                                let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
                                // C client TCP parser requires 8-byte aligned postsize
                                hdr.set_payload_size(padded.len(), element_count);
                                hdr.data_type = requested_type;
                                hdr.cid = 1; // ECA_NORMAL
                                hdr.available = sub_id;

                                let hdr_bytes = hdr.to_bytes_extended();
                                let mut w = writer_clone.lock().await;
                                if w.write_all(&hdr_bytes).await.is_err() {
                                    break;
                                }
                                if w.write_all(&padded).await.is_err() {
                                    break;
                                }
                                let _ = w.flush().await;
                            }
                        });

                        state.subscriptions.insert(
                            sub_id,
                            SubscriptionEntry {
                                target: ChannelTarget::RecordField {
                                    record: record.clone(),
                                    field: field.clone(),
                                },
                                sub_id,
                                data_type: requested_type,
                                task,
                            },
                        );
                    }
                }
            }
        }


        CA_PROTO_EVENT_CANCEL => {
            let sub_id = hdr.available;
            if let Some(sub) = state.subscriptions.remove(&sub_id) {
                sub.task.abort();
                match &sub.target {
                    ChannelTarget::SimplePv(pv) => {
                        pv.remove_subscriber(sub.sub_id).await;
                    }
                    ChannelTarget::RecordField { record, .. } => {
                        record.write().await.remove_subscriber(sub.sub_id);
                    }
                }

                // Per spec: send final EVENT_ADD response with count=0
                let mut resp = CaHeader::new(CA_PROTO_EVENT_ADD);
                resp.data_type = sub.data_type;
                resp.count = 0;
                resp.cid = ECA_NORMAL;
                resp.available = sub_id;
                let mut w = writer.lock().await;
                w.write_all(&resp.to_bytes()).await?;
                w.flush().await?;
            }
        }

        CA_PROTO_EVENTS_OFF | CA_PROTO_EVENTS_ON => {
            // Flow control from client — acknowledge silently (no-op).
            // Sending CA_PROTO_ERROR would confuse C libca clients.
        }

        CA_PROTO_READ_SYNC => {
            // READ_SYNC is a barrier/flush for previously queued responses.
            let mut w = writer.lock().await;
            w.flush().await?;
        }

        CA_PROTO_ECHO => {
            let resp = CaHeader::new(CA_PROTO_ECHO);
            let mut w = writer.lock().await;
            w.write_all(&resp.to_bytes()).await?;
            w.flush().await?;
        }

        CA_PROTO_SEARCH => {
            // TCP search — only supported for clients with minor version >= 4
            if state.client_minor_version < 4 {
                return Ok(());
            }
            let end = payload
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len());
            let pv_name = String::from_utf8_lossy(&payload[..end]).to_string();

            if db.has_name(&pv_name).await {
                // Reply: data_type = tcp_port, cid = 0xFFFFFFFF, available = client's cid
                // 8-byte payload containing CA_MINOR_VERSION as u16
                let mut resp = CaHeader::new(CA_PROTO_SEARCH);
                resp.data_type = state.tcp_port;
                resp.set_payload_size(8, 0);
                resp.cid = 0xFFFF_FFFF;
                resp.available = hdr.available;

                let mut search_payload = [0u8; 8];
                search_payload[0..2].copy_from_slice(&CA_MINOR_VERSION.to_be_bytes());

                let mut w = writer.lock().await;
                w.write_all(&resp.to_bytes_extended()).await?;
                w.write_all(&search_payload).await?;
                w.flush().await?;
            }
            // Not found → silent (matches C server behavior)
        }

        CA_PROTO_CLEAR_CHANNEL => {
            let sid = hdr.cid;
            let cid = hdr.available;
            if let Some(_entry) = state.channels.remove(&sid) {
                let mut resp = CaHeader::new(CA_PROTO_CLEAR_CHANNEL);
                resp.cid = sid;
                resp.available = cid;
                let mut w = writer.lock().await;
                w.write_all(&resp.to_bytes()).await?;
                w.flush().await?;
            }
        }

        _ => {
            // Unknown command — send CA_PROTO_ERROR with ECA status and original header
            let error_msg = format!("Unsupported command {}", hdr.cmmd);
            send_ca_error(writer, hdr, ECA_INTERNAL, &error_msg).await?;
        }
    }

    Ok(())
}
async fn get_full_snapshot(
    target: &ChannelTarget,
) -> Option<crate::server::snapshot::Snapshot> {
    match target {
        ChannelTarget::SimplePv(pv) => Some(pv.snapshot().await),
        ChannelTarget::RecordField { record, field } => {
            record.read().await.snapshot_for_field(field)
        }
    }
}

async fn send_monitor_snapshot(
    writer: &Arc<Mutex<BufWriter<OwnedWriteHalf>>>,
    sub_id: u32,
    data_type: u16,
    snapshot: &crate::server::snapshot::Snapshot,
) -> CaResult<()> {
    let data = encode_dbr(data_type, snapshot)?;
    let element_count = snapshot.value.count() as u32;
    let mut padded = data;
    padded.resize(align8(padded.len()), 0);

    let mut resp = CaHeader::new(CA_PROTO_EVENT_ADD);
    // C client TCP parser requires 8-byte aligned postsize
    resp.set_payload_size(padded.len(), element_count);
    resp.data_type = data_type;
    resp.cid = 1; // ECA_NORMAL
    resp.available = sub_id;

    let mut w = writer.lock().await;
    w.write_all(&resp.to_bytes_extended()).await?;
    w.write_all(&padded).await?;
    w.flush().await?;
    Ok(())
}

/// Re-evaluate and re-send CA_PROTO_ACCESS_RIGHTS for all open channels.
/// Called when hostname or username changes.
async fn reeval_access_rights(
    state: &mut ClientState,
    writer: &Arc<Mutex<BufWriter<OwnedWriteHalf>>>,
) -> CaResult<()> {
    if state.channels.is_empty() {
        return Ok(());
    }
    // Collect channel info first to avoid borrow conflict with compute_access
    let chan_info: Vec<(u32, u32, ChannelTarget)> = state
        .channels
        .iter()
        .map(|(&sid, entry)| (sid, entry.cid, entry.target.clone()))
        .collect();

    let mut w = writer.lock().await;
    for (sid, cid, target) in chan_info {
        let new_access = state.compute_access(&target).await;
        let new_level = match new_access {
            3 => AccessLevel::ReadWrite,
            1 => AccessLevel::Read,
            _ => AccessLevel::NoAccess,
        };
        state.channel_access.insert(sid, new_level);
        let mut ar = CaHeader::new(CA_PROTO_ACCESS_RIGHTS);
        ar.cid = cid;
        ar.available = new_access;
        w.write_all(&ar.to_bytes()).await?;
    }
    w.flush().await?;
    Ok(())
}

/// Send a command-specific zero-payload error response.
/// Used for READ_NOTIFY, WRITE_NOTIFY, and EVENT_ADD error replies.
async fn send_cmd_error(
    writer: &Arc<Mutex<BufWriter<OwnedWriteHalf>>>,
    cmd: u16,
    data_type: u16,
    eca_status: u32,
    ioid_or_subid: u32,
) -> CaResult<()> {
    let mut resp = CaHeader::new(cmd);
    resp.data_type = data_type;
    resp.count = 0;
    resp.cid = eca_status;
    resp.available = ioid_or_subid;
    let mut w = writer.lock().await;
    w.write_all(&resp.to_bytes()).await?;
    w.flush().await?;
    Ok(())
}

/// Send a CA_PROTO_ERROR response with the original header and an error message.
async fn send_ca_error(
    writer: &Arc<Mutex<BufWriter<OwnedWriteHalf>>>,
    original_hdr: &CaHeader,
    eca_status: u32,
    message: &str,
) -> CaResult<()> {
    let error_msg_bytes = pad_string(message);
    let payload_size = CaHeader::SIZE + error_msg_bytes.len();

    let mut resp = CaHeader::new(CA_PROTO_ERROR);
    resp.set_payload_size(payload_size, 0);
    resp.cid = eca_status;

    let mut w = writer.lock().await;
    w.write_all(&resp.to_bytes_extended()).await?;
    w.write_all(&original_hdr.to_bytes()).await?;
    w.write_all(&error_msg_bytes).await?;
    w.flush().await?;
    Ok(())
}
