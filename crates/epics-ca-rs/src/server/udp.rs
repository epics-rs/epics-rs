use socket2::{Domain, Protocol, Socket, Type};
use std::sync::Arc;
use tokio::net::UdpSocket;

use crate::protocol::*;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;

/// Run the UDP search responder on the given port.
/// Listens for CA_PROTO_SEARCH requests and responds if the PV exists.
pub async fn run_udp_search_responder(
    db: Arc<PvDatabase>,
    port: u16,
    tcp_port: u16,
) -> CaResult<()> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(target_os = "macos")]
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&std::net::SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, port).into())?;
    let socket = UdpSocket::from_std(sock.into())?;
    socket.set_broadcast(true)?;

    let mut buf = [0u8; 4096];

    loop {
        let (len, src) = socket.recv_from(&mut buf).await?;
        if len < CaHeader::SIZE {
            continue;
        }

        let mut offset = 0;
        while offset + CaHeader::SIZE <= len {
            let hdr = match CaHeader::from_bytes(&buf[offset..]) {
                Ok(h) => h,
                Err(_) => break,
            };
            let payload_size = align8(hdr.postsize as usize);
            let msg_len = CaHeader::SIZE + payload_size;

            if offset + msg_len > len {
                break;
            }

            if hdr.cmmd == CA_PROTO_SEARCH {
                let payload_start = offset + CaHeader::SIZE;
                let payload_end = payload_start + hdr.postsize as usize;
                let payload = &buf[payload_start..payload_end];

                // Extract PV name (null-terminated)
                let pv_name_end = payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(payload.len());
                if let Ok(pv_name) = std::str::from_utf8(&payload[..pv_name_end]) {
                    // Check both simple PVs and record base names
                    if db.has_name(pv_name).await {
                        // Build search response
                        // cid = server IP (0xFFFFFFFF = "use sender's IP")
                        // available = echo client's search ID (from request's available field)
                        let mut resp = CaHeader::new(CA_PROTO_SEARCH);
                        resp.postsize = 8;
                        resp.data_type = tcp_port;
                        resp.count = 0;
                        resp.cid = 0xFFFFFFFF;
                        resp.available = hdr.available;

                        // Version header first
                        let mut ver = CaHeader::new(CA_PROTO_VERSION);
                        ver.count = CA_MINOR_VERSION;

                        let mut reply = Vec::with_capacity(CaHeader::SIZE * 2 + 8);
                        reply.extend_from_slice(&ver.to_bytes());
                        reply.extend_from_slice(&resp.to_bytes());
                        // libca's UDP search reply parser only trusts the appended
                        // minor version when postsize >= sizeof(ca_uint32_t).
                        // Use an 8-byte aligned payload like the C server.
                        let mut search_payload = [0u8; 8];
                        search_payload[0..2].copy_from_slice(&CA_MINOR_VERSION.to_be_bytes());
                        reply.extend_from_slice(&search_payload);

                        let _ = socket.send_to(&reply, src).await;
                    }
                }
            }

            offset += msg_len;
        }
    }
}
