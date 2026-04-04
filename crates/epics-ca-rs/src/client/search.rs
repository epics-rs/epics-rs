use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use epics_base_rs::runtime::sync::mpsc;

use crate::protocol::*;

use super::types::{SearchRequest, SearchResponse};

const MIN_RETRY: Duration = Duration::from_millis(30);
const MAX_RETRY: Duration = Duration::from_secs(30);

#[allow(dead_code)]
struct PendingSearch {
    cid: u32,
    pv_name: String,
    packet: Vec<u8>,
    next_retry: Instant,
    backoff: Duration,
}

pub(crate) async fn run_search_engine(
    addr_list: Vec<SocketAddr>,
    mut request_rx: mpsc::UnboundedReceiver<SearchRequest>,
    response_tx: mpsc::UnboundedSender<SearchResponse>,
) {
    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = socket.set_broadcast(true);
    // Increase OS socket receive buffer for search response bursts.
    // Matches C EPICS libca which also sets SO_RCVBUF.
    #[cfg(unix)]
    {
        use std::os::fd::BorrowedFd;
        let fd = unsafe { BorrowedFd::borrow_raw(socket.as_raw_fd()) };
        let sock_ref = socket2::SockRef::from(&fd);
        let _ = sock_ref.set_recv_buffer_size(256 * 1024);
    }

    let mut pending: HashMap<u32, PendingSearch> = HashMap::new();
    let mut recv_buf = [0u8; 65536];

    loop {
        // Compute next retry deadline
        let next_deadline = pending
            .values()
            .map(|p| p.next_retry)
            .min()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

        let sleep = epics_base_rs::runtime::task::sleep_until(next_deadline);

        tokio::select! {
            req = request_rx.recv() => {
                let Some(req) = req else { return };
                match req {
                    SearchRequest::Search { cid, pv_name } => {
                        let packet = build_search_packet(cid, &pv_name);
                        // Send immediately
                        for addr in &addr_list {
                            let _ = socket.send_to(&packet, addr).await;
                        }
                        pending.insert(cid, PendingSearch {
                            cid,
                            pv_name,
                            packet,
                            next_retry: Instant::now() + MIN_RETRY,
                            backoff: MIN_RETRY,
                        });
                    }
                    SearchRequest::Cancel { cid } => {
                        pending.remove(&cid);
                    }
                }
            }
            result = socket.recv_from(&mut recv_buf) => {
                let Ok((len, src)) = result else { continue };
                if len < CaHeader::SIZE { continue; }

                let mut offset = 0;
                while offset + CaHeader::SIZE <= len {
                    let Ok(hdr) = CaHeader::from_bytes(&recv_buf[offset..]) else { break };
                    match hdr.cmmd {
                        CA_PROTO_SEARCH => {
                            let server_port = hdr.data_type;
                            // cid field = server IP (0xFFFFFFFF = use sender's IP)
                            let server_ip = if hdr.cid == 0xFFFFFFFF {
                                src.ip()
                            } else {
                                std::net::IpAddr::V4(Ipv4Addr::from(hdr.cid.to_be_bytes()))
                            };
                            let server_addr = SocketAddr::new(server_ip, server_port as u16);
                            // available field = echoed search ID
                            let cid = hdr.available;
                            if pending.remove(&cid).is_some() {
                                let _ = response_tx.send(SearchResponse::Found { cid, server_addr });
                            }
                        }
                        _ => {}
                    }
                    offset += CaHeader::SIZE + align8(hdr.postsize as usize);
                }
            }
            _ = sleep => {
                // Retry pending searches that are due
                let now = Instant::now();
                for p in pending.values_mut() {
                    if p.next_retry <= now {
                        for addr in &addr_list {
                            let _ = socket.send_to(&p.packet, addr).await;
                        }
                        p.backoff = (p.backoff * 2).min(MAX_RETRY);
                        p.next_retry = now + p.backoff;
                    }
                }
            }
        }
    }
}

fn build_search_packet(cid: u32, pv_name: &str) -> Vec<u8> {
    let pv_payload = pad_string(pv_name);

    let mut version_hdr = CaHeader::new(CA_PROTO_VERSION);
    version_hdr.count = CA_MINOR_VERSION;

    let mut search_hdr = CaHeader::new(CA_PROTO_SEARCH);
    search_hdr.postsize = pv_payload.len() as u16;
    search_hdr.data_type = CA_DO_REPLY;
    search_hdr.count = CA_MINOR_VERSION;
    search_hdr.cid = cid;
    search_hdr.available = cid;

    let mut packet = Vec::new();
    packet.extend_from_slice(&version_hdr.to_bytes());
    packet.extend_from_slice(&search_hdr.to_bytes());
    packet.extend_from_slice(&pv_payload);
    packet
}
