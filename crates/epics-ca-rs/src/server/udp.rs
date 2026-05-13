use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;

use crate::protocol::*;
use epics_base_rs::error::CaResult;
use epics_base_rs::net::{enable_so_rxq_ovfl_for_socket, recv_from_with_drop_count_socket};
use epics_base_rs::server::database::PvDatabase;

/// Run UDP search responders bound to one or more local interfaces.
///
/// Each interface gets its own task — having a dedicated socket per
/// interface lets the OS keep the broadcast routing straight on multi-NIC
/// hosts (matching C EPICS osiSockDiscoverInterfaces behaviour).
///
/// `ignore_addrs` filters out source addresses that should never receive
/// search replies (EPICS_CAS_IGNORE_ADDR_LIST).
pub async fn run_udp_search_responder(
    db: Arc<PvDatabase>,
    port: u16,
    tcp_port: u16,
    intf_addrs: Vec<Ipv4Addr>,
    ignore_addrs: Vec<Ipv4Addr>,
) -> CaResult<()> {
    let intfs = if intf_addrs.is_empty() {
        vec![Ipv4Addr::UNSPECIFIED]
    } else {
        intf_addrs
    };

    // Spawn one responder task per interface and wait for the first error.
    let mut handles = Vec::with_capacity(intfs.len());
    for ip in intfs {
        let db_t = db.clone();
        let ignore_t = ignore_addrs.clone();
        let handle = epics_base_rs::runtime::task::spawn(async move {
            run_single_responder(db_t, ip, port, tcp_port, ignore_t).await
        });
        handles.push(handle);
    }

    // Propagate the first error, abort the rest.
    let mut handles_iter = handles.into_iter();
    let result = if let Some(first) = handles_iter.next() {
        match first.await {
            Ok(inner) => inner,
            Err(e) => Err(epics_base_rs::error::CaError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            ))),
        }
    } else {
        Ok(())
    };
    for h in handles_iter {
        h.abort();
    }
    result
}

async fn run_single_responder(
    db: Arc<PvDatabase>,
    bind_ip: Ipv4Addr,
    port: u16,
    tcp_port: u16,
    ignore_addrs: Vec<Ipv4Addr>,
) -> CaResult<()> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // libcom commits 19146a5 + 5064931 + 65ef6e9: SO_REUSEADDR has dangerous
    // hijack semantics on Windows (any process can rebind), so it's POSIX-only.
    // For UDP datagram fanout (caRepeater + CA server sharing a port) Linux
    // requires BOTH SO_REUSEADDR and SO_REUSEPORT (different reuse classes
    // don't share); BSD/macOS need SO_REUSEPORT. Mirror libcom
    // epicsSocketEnableAddressUseForDatagramFanout and set both on Unix.
    #[cfg(not(windows))]
    {
        sock.set_reuse_address(true)?;
        #[cfg(unix)]
        sock.set_reuse_port(true)?;
    }
    // libcom commit 51191e6: Linux defaults IP_MULTICAST_ALL=1, which makes
    // a socket bound to 0.0.0.0 receive multicast for groups joined on ANY
    // socket on this host. Clear it so per-NIC search responders don't see
    // foreign multicast traffic. No-op on non-Linux.
    #[cfg(target_os = "linux")]
    {
        let _ = sock.set_multicast_all_v4(false);
    }
    sock.set_nonblocking(true)?;
    sock.bind(&std::net::SocketAddrV4::new(bind_ip, port).into())?;
    let socket = UdpSocket::from_std(sock.into())?;
    socket.set_broadcast(true)?;
    // EPICS_CA_MCAST_TTL (epics-base 3.16, f2a1834d). Only consulted by
    // the OS for multicast destinations; safe to apply unconditionally.
    let _ = socket.set_multicast_ttl_v4(epics_base_rs::runtime::net::ca_mcast_ttl());
    // pvxs `udp_collector.cpp::UDPCollector::UDPCollector` parity
    // (commit a064677e3625): opt the kernel into SO_RXQ_OVFL so each
    // recvmsg surfaces the per-socket dropped-datagram counter as a
    // cmsg. No-op on non-Linux. Diagnostic-only; failure to enable
    // is logged at trace and the responder continues normally.
    if let Err(e) = enable_so_rxq_ovfl_for_socket(&socket) {
        tracing::trace!(
            target: "epics_ca_rs::server::udp",
            %bind_ip,
            error = %e,
            "SO_RXQ_OVFL enable failed (non-fatal)"
        );
    }

    // 64 KB receive buffer — IPv4 maximum datagram size. The previous
    // 4 KB cap silently truncated bursts of multi-PV searches in
    // active facilities (each search message is ~24 bytes inc. PV
    // name; 4 KB held ~150 PVs while a typical site burst is many
    // hundreds, especially during gateway restart storms). 64 KB
    // matches the kernel ceiling without risking truncation.
    // Heap-allocated because 64 KB on the per-task stack is large
    // and the `Box<[u8]>` cost is amortized over the listener's
    // lifetime — one allocation, reused on every recv.
    let mut buf = vec![0u8; 64 * 1024];
    // Per-source-IP token bucket. Off by default; when
    // EPICS_CAS_UDP_SEARCH_RATE_LIMIT is set, drops excess packets to
    // mitigate amplification attacks where a tiny search reflects a
    // larger response.
    let udp_rl = UdpRateLimiter::from_env();

    // Tracks the previously-observed SO_RXQ_OVFL counter for this
    // socket. Logged on transitions only — pvxs `udp_collector.cpp:55-67`.
    let mut prev_drops: u32 = 0;

    loop {
        let (len, src, drops) = recv_from_with_drop_count_socket(&socket, &mut buf).await?;
        if drops != 0 && drops != prev_drops {
            tracing::debug!(
                target: "epics_ca_rs::server::udp",
                %bind_ip,
                prev = prev_drops,
                drops,
                "CA server UDP search responder buffer overflow"
            );
        }
        prev_drops = drops;
        if len < CaHeader::SIZE {
            continue;
        }

        // Apply ignore list (EPICS_CAS_IGNORE_ADDR_LIST). Any datagram
        // whose source IP appears in the list is silently dropped.
        if let SocketAddr::V4(v4) = src {
            if ignore_addrs.contains(v4.ip()) {
                continue;
            }
        }

        // Per-source-IP rate limit gate.
        if !udp_rl.allow(&src) {
            metrics::counter!("ca_server_udp_search_drops_total").increment(1);
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
                    if db.has_name(pv_name).await {
                        let server_ip = local_ip_for(src);
                        let mut resp = CaHeader::new(CA_PROTO_SEARCH);
                        resp.postsize = 8;
                        resp.data_type = tcp_port;
                        resp.count = 0;
                        resp.cid = u32::from_be_bytes(server_ip.octets());
                        resp.available = hdr.available;

                        let mut ver = CaHeader::new(CA_PROTO_VERSION);
                        ver.count = CA_MINOR_VERSION;

                        let mut reply = Vec::with_capacity(CaHeader::SIZE * 2 + 8);
                        reply.extend_from_slice(&ver.to_bytes());
                        reply.extend_from_slice(&resp.to_bytes());
                        let mut search_payload = [0u8; 8];
                        search_payload[0..2].copy_from_slice(&CA_MINOR_VERSION.to_be_bytes());
                        reply.extend_from_slice(&search_payload);

                        let _ = socket.send_to(&reply, src).await;
                    } else if hdr.data_type == CA_DO_REPLY {
                        // Client asked for an explicit negative reply
                        // (search header data_type == CA_DO_REPLY=10).
                        // Send CA_PROTO_NOT_FOUND so it doesn't have to
                        // wait for the search timeout.
                        let mut nf = CaHeader::new(CA_PROTO_NOT_FOUND);
                        nf.data_type = CA_DO_REPLY;
                        nf.count = CA_MINOR_VERSION;
                        nf.cid = hdr.available;
                        nf.available = hdr.available;
                        let _ = socket.send_to(&nf.to_bytes(), src).await;
                    }
                }
            }

            offset += msg_len;
        }
    }
}

/// Determine the local interface IP that would route to `remote`.
/// Creates a temporary unconnected UDP socket and "connects" it (no data
/// is sent — this just lets the OS pick the outgoing interface via routing).
fn local_ip_for(remote: SocketAddr) -> Ipv4Addr {
    let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") else {
        return Ipv4Addr::UNSPECIFIED;
    };
    if sock.connect(remote).is_err() {
        return Ipv4Addr::UNSPECIFIED;
    }
    match sock.local_addr() {
        Ok(SocketAddr::V4(a)) => *a.ip(),
        _ => Ipv4Addr::UNSPECIFIED,
    }
}

/// Per-source-IP token bucket on the UDP search responder. Mitigates
/// amplification (a tiny SEARCH eliciting a much larger SEARCH_REPLY
/// across many records) and absurd loops from misconfigured clients.
///
/// Disabled when neither env var is set; the cost is one IP-equality
/// comparison per packet otherwise. The implementation is a fixed
/// 1-second sliding window — coarse but cheap; replace with
/// per-IP token buckets if a finer policy is ever needed.
struct UdpRateLimiter {
    enabled: bool,
    cap_per_sec: u32,
    counts:
        std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, (std::time::Instant, u32)>>,
}

impl UdpRateLimiter {
    fn from_env() -> Self {
        let cap = epics_base_rs::runtime::env::get("EPICS_CAS_UDP_SEARCH_RATE_LIMIT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0u32);
        Self {
            enabled: cap > 0,
            cap_per_sec: cap,
            counts: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn allow(&self, src: &SocketAddr) -> bool {
        if !self.enabled {
            return true;
        }
        let ip = src.ip();
        let now = std::time::Instant::now();
        let mut counts = self.counts.lock().unwrap();
        let entry = counts.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= std::time::Duration::from_secs(1) {
            entry.0 = now;
            entry.1 = 0;
        }
        if entry.1 >= self.cap_per_sec {
            return false;
        }
        entry.1 += 1;
        // Periodic GC: prune stale entries every 1024 packets to keep
        // the map bounded under DDoS conditions where sources rotate.
        if counts.len() > 4096 {
            let cutoff = now - std::time::Duration::from_secs(5);
            counts.retain(|_, (t, _)| *t >= cutoff);
        }
        true
    }
}
