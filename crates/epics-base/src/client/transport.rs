use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use crate::runtime::sync::{mpsc, Mutex};

use crate::channel::AccessRights;
use crate::protocol::*;

use super::types::{TransportCommand, TransportEvent};

struct ServerConnection {
    writer: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    _read_task: tokio::task::JoinHandle<()>,
}

pub(crate) async fn run_transport_manager(
    mut command_rx: mpsc::UnboundedReceiver<TransportCommand>,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
) {
    let mut connections: HashMap<SocketAddr, ServerConnection> = HashMap::new();

    while let Some(cmd) = command_rx.recv().await {
        match cmd {
            TransportCommand::CreateChannel {
                cid,
                pv_name,
                server_addr,
            } => {
                // Ensure we have a connection to this server
                if !connections.contains_key(&server_addr) {
                    match connect_server(server_addr, event_tx.clone()).await {
                        Some(conn) => {
                            connections.insert(server_addr, conn);
                        }
                        None => {
                            let _ = event_tx.send(TransportEvent::TcpClosed { server_addr });
                            continue;
                        }
                    }
                }

                // Check connection is still alive (task not finished)
                let alive = connections
                    .get(&server_addr)
                    .map(|c| !c._read_task.is_finished())
                    .unwrap_or(false);

                if !alive {
                    connections.remove(&server_addr);
                    match connect_server(server_addr, event_tx.clone()).await {
                        Some(conn) => {
                            connections.insert(server_addr, conn);
                        }
                        None => {
                            let _ = event_tx.send(TransportEvent::TcpClosed { server_addr });
                            continue;
                        }
                    }
                }

                let conn = connections.get(&server_addr).unwrap();
                let pv_payload = pad_string(&pv_name);
                let mut create_hdr = CaHeader::new(CA_PROTO_CREATE_CHAN);
                create_hdr.postsize = pv_payload.len() as u16;
                create_hdr.cid = cid;
                create_hdr.available = CA_MINOR_VERSION as u32;

                let writer = conn.writer.clone();
                let mut w = writer.lock().await;
                let ok = w.write_all(&create_hdr.to_bytes()).await.is_ok()
                    && w.write_all(&pv_payload).await.is_ok()
                    && w.flush().await.is_ok();
                if !ok {
                    drop(w);
                    connections.remove(&server_addr);
                    let _ = event_tx.send(TransportEvent::TcpClosed { server_addr });
                }
            }
            TransportCommand::ReadNotify {
                sid,
                data_type,
                count,
                ioid,
                server_addr,
            } => {
                if let Some(conn) = connections.get(&server_addr) {
                    let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
                    hdr.data_type = data_type;
                    hdr.cid = sid;
                    hdr.available = ioid;
                    if count > 0xFFFF {
                        hdr.set_payload_size(0, count);
                    } else {
                        hdr.count = count as u16;
                    }

                    let mut w = conn.writer.lock().await;
                    let _ = w.write_all(&hdr.to_bytes_extended()).await;
                    let _ = w.flush().await;
                }
            }
            TransportCommand::WriteNotify {
                sid,
                data_type,
                count,
                ioid,
                payload,
                server_addr,
            } => {
                if let Some(conn) = connections.get(&server_addr) {
                    let padded_len = align8(payload.len());
                    let mut padded = payload;
                    padded.resize(padded_len, 0);

                    let mut hdr = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
                    hdr.data_type = data_type;
                    hdr.cid = sid;
                    hdr.available = ioid;
                    hdr.set_payload_size(padded.len(), count);

                    let hdr_bytes = hdr.to_bytes_extended();
                    let mut w = conn.writer.lock().await;
                    let _ = w.write_all(&hdr_bytes).await;
                    let _ = w.write_all(&padded).await;
                    let _ = w.flush().await;
                }
            }
            TransportCommand::Subscribe {
                sid,
                data_type,
                count,
                subid,
                mask,
                server_addr,
            } => {
                if let Some(conn) = connections.get(&server_addr) {
                    let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
                    hdr.postsize = 16;
                    hdr.data_type = data_type;
                    hdr.cid = sid;
                    hdr.available = subid;
                    if count > 0xFFFF {
                        hdr.set_payload_size(16, count);
                    } else {
                        hdr.count = count as u16;
                    }

                    let mut mask_payload = [0u8; 16];
                    mask_payload[12..14].copy_from_slice(&mask.to_be_bytes());

                    let hdr_bytes = hdr.to_bytes_extended();
                    let mut w = conn.writer.lock().await;
                    let _ = w.write_all(&hdr_bytes).await;
                    let _ = w.write_all(&mask_payload).await;
                    let _ = w.flush().await;
                }
            }
            TransportCommand::Unsubscribe {
                sid,
                subid,
                data_type,
                server_addr,
            } => {
                if let Some(conn) = connections.get(&server_addr) {
                    let mut hdr = CaHeader::new(CA_PROTO_EVENT_CANCEL);
                    hdr.data_type = data_type;
                    hdr.cid = sid;
                    hdr.available = subid;

                    let mut w = conn.writer.lock().await;
                    let _ = w.write_all(&hdr.to_bytes()).await;
                    let _ = w.flush().await;
                }
            }
            TransportCommand::ClearChannel {
                cid,
                sid,
                server_addr,
            } => {
                if let Some(conn) = connections.get(&server_addr) {
                    let mut hdr = CaHeader::new(CA_PROTO_CLEAR_CHANNEL);
                    hdr.cid = sid;
                    hdr.available = cid;

                    let mut w = conn.writer.lock().await;
                    let _ = w.write_all(&hdr.to_bytes()).await;
                    let _ = w.flush().await;
                }
            }
        }
    }
}

async fn connect_server(
    server_addr: SocketAddr,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
) -> Option<ServerConnection> {
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        TcpStream::connect(server_addr),
    )
    .await
    .ok()?
    .ok()?;

    let (reader, write_half) = stream.into_split();
    let writer = Arc::new(Mutex::new(write_half));

    // Send VERSION + HOST + CLIENT
    {
        let mut w = writer.lock().await;

        let mut version_hdr = CaHeader::new(CA_PROTO_VERSION);
        version_hdr.count = CA_MINOR_VERSION;
        w.write_all(&version_hdr.to_bytes()).await.ok()?;

        let hostname = crate::runtime::env::hostname();
        let host_payload = pad_string(&hostname);
        let mut host_hdr = CaHeader::new(CA_PROTO_HOST_NAME);
        host_hdr.postsize = host_payload.len() as u16;
        w.write_all(&host_hdr.to_bytes()).await.ok()?;
        w.write_all(&host_payload).await.ok()?;

        let username = crate::runtime::env::get("USER")
            .or_else(|| crate::runtime::env::get("USERNAME"))
            .unwrap_or_else(|| "unknown".to_string());
        let user_payload = pad_string(&username);
        let mut user_hdr = CaHeader::new(CA_PROTO_CLIENT_NAME);
        user_hdr.postsize = user_payload.len() as u16;
        w.write_all(&user_hdr.to_bytes()).await.ok()?;
        w.write_all(&user_payload).await.ok()?;

        w.flush().await.ok()?;
    }

    let read_task = crate::runtime::task::spawn(read_loop(reader, server_addr, event_tx, writer.clone()));

    Some(ServerConnection {
        writer,
        _read_task: read_task,
    })
}

async fn read_loop(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    server_addr: SocketAddr,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    writer: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
) {
    let mut buf = vec![0u8; 8192];
    let mut accumulated = Vec::new();

    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => {
                let _ = event_tx.send(TransportEvent::TcpClosed { server_addr });
                return;
            }
            Ok(n) => n,
        };

        accumulated.extend_from_slice(&buf[..n]);

        let mut offset = 0;
        while offset + CaHeader::SIZE <= accumulated.len() {
            let (hdr, hdr_size) = match CaHeader::from_bytes_extended(&accumulated[offset..]) {
                Ok(v) => v,
                Err(_) => break,
            };
            let actual_post = hdr.actual_postsize();
            let msg_len = hdr_size + align8(actual_post);

            if offset + msg_len > accumulated.len() {
                break;
            }

            let data_start = offset + hdr_size;

            match hdr.cmmd {
                CA_PROTO_VERSION => {}
                CA_PROTO_ACCESS_RIGHTS => {
                    let _ = event_tx.send(TransportEvent::AccessRightsChanged {
                        cid: hdr.cid,
                        access: AccessRights::from_u32(hdr.available),
                    });
                }
                CA_PROTO_CREATE_CHAN => {
                    let _ = event_tx.send(TransportEvent::ChannelCreated {
                        cid: hdr.cid,
                        sid: hdr.available,
                        data_type: hdr.data_type,
                        element_count: hdr.actual_count(),
                        access: AccessRights::from_u32(0x3), // default; real access comes via ACCESS_RIGHTS
                        server_addr,
                    });
                }
                CA_PROTO_READ_NOTIFY => {
                    let data = accumulated[data_start..data_start + actual_post].to_vec();
                    let _ = event_tx.send(TransportEvent::ReadResponse {
                        ioid: hdr.available,
                        data_type: hdr.data_type,
                        count: hdr.actual_count(),
                        data,
                    });
                }
                CA_PROTO_WRITE_NOTIFY => {
                    // Per spec: param1 (cid field) = ECA status, param2 (available field) = IOID
                    let _ = event_tx.send(TransportEvent::WriteResponse {
                        ioid: hdr.available,
                        status: hdr.cid,
                    });
                }
                CA_PROTO_EVENT_ADD => {
                    let data = accumulated[data_start..data_start + actual_post].to_vec();
                    let _ = event_tx.send(TransportEvent::MonitorData {
                        subid: hdr.available,
                        data_type: hdr.data_type,
                        count: hdr.actual_count(),
                        data,
                    });
                }
                CA_PROTO_ECHO => {
                    // Respond inline
                    let echo_hdr = CaHeader::new(CA_PROTO_ECHO);
                    let mut w = writer.lock().await;
                    let _ = w.write_all(&echo_hdr.to_bytes()).await;
                    let _ = w.flush().await;
                }
                CA_PROTO_CREATE_CH_FAIL => {
                    let _ = event_tx.send(TransportEvent::ChannelCreateFailed {
                        cid: hdr.cid,
                    });
                }
                CA_PROTO_ERROR => {
                    // Payload contains original request header (16 bytes) + error message string
                    let orig_cmd = if actual_post >= 16 {
                        let orig_hdr_bytes = &accumulated[data_start..data_start + 16];
                        Some(u16::from_be_bytes([orig_hdr_bytes[0], orig_hdr_bytes[1]]))
                    } else {
                        None
                    };
                    let msg = if actual_post > 16 {
                        let msg_bytes = &accumulated[data_start + 16..data_start + actual_post];
                        let end = msg_bytes.iter().position(|&b| b == 0).unwrap_or(msg_bytes.len());
                        String::from_utf8_lossy(&msg_bytes[..end]).to_string()
                    } else {
                        String::new()
                    };
                    eprintln!("CA server error: cmd={:?} msg={}", orig_cmd, msg);
                    let _ = event_tx.send(TransportEvent::ServerError {
                        original_request: orig_cmd,
                        message: msg,
                    });
                }
                CA_PROTO_SERVER_DISCONN => {
                    let _ = event_tx.send(TransportEvent::ServerDisconnect {
                        cid: hdr.cid,
                        server_addr,
                    });
                }
                _ => {}
            }

            offset += msg_len;
        }

        if offset > 0 {
            accumulated.drain(..offset);
        }
    }
}
