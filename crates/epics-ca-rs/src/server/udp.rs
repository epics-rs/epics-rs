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
    let socket = bind_responder_socket(bind_ip, port)?;
    let socket = Arc::new(socket);

    // C `caservertask.c::start_tcp_server_tasks` (lines 670-708) opens a
    // *second* UDP responder bound to the interface's broadcast address
    // whenever the primary socket is bound to a specific (non-INADDR_ANY)
    // interface IP. The comment at line 671 documents the BSD-sockets
    // oddity: a unicast-bound socket on POSIX does NOT receive UDP
    // datagrams whose destination is the interface's broadcast addr —
    // only the secondary socket bound to the broadcast addr will.
    // Without this second responder, every libca client SEARCH that
    // targets the broadcast network address (the default
    // `EPICS_CA_ADDR_LIST` fan-out shape) goes unanswered on a Rust IOC
    // configured with a specific `EPICS_CAS_INTF_ADDR_LIST` entry —
    // PVs become invisible to broadcast clients despite the server
    // running and accepting unicast searches.
    //
    // On Windows the kernel behaviour differs (a specific-IP-bound
    // socket receives broadcasts), so C `caservertask.c:670, 728`
    // guards the secondary socket with `#if !(_WIN32 || __CYGWIN__)`.
    // Mirror that gate.
    let bcast_socket: Option<Arc<UdpSocket>> = {
        #[cfg(any(windows, target_os = "windows"))]
        {
            None
        }
        #[cfg(not(any(windows, target_os = "windows")))]
        {
            super::addr_list::broadcast_for_ip(bind_ip).and_then(|bcast_ip| {
                match bind_responder_socket(bcast_ip, port) {
                    Ok(s) => Some(Arc::new(s)),
                    Err(e) => {
                        tracing::warn!(
                            target: "epics_ca_rs::server::udp",
                            %bind_ip,
                            %bcast_ip,
                            error = %e,
                            "CA server bcast responder bind failed; broadcast SEARCHes \
                             to this interface will not be answered"
                        );
                        None
                    }
                }
            })
        }
    };

    let udp_rl = Arc::new(UdpRateLimiter::from_env());
    let primary = recv_loop(
        socket.clone(),
        db.clone(),
        bind_ip,
        tcp_port,
        ignore_addrs.clone(),
        udp_rl.clone(),
    );

    match bcast_socket {
        Some(bsock) => {
            let secondary = recv_loop(bsock, db, bind_ip, tcp_port, ignore_addrs, udp_rl);
            // First task to error wins; the other is dropped when this
            // future returns. tokio::try_join is `Drop` on cancel, so the
            // surviving loop's recv() future cancels cleanly.
            tokio::try_join!(primary, secondary).map(|_| ())
        }
        None => primary.await,
    }
}

/// Build and configure the per-bind UDP socket. Centralised so the
/// primary (interface IP) and secondary (interface broadcast addr)
/// sockets share identical socket-option setup.
fn bind_responder_socket(bind_ip: Ipv4Addr, port: u16) -> CaResult<UdpSocket> {
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
    Ok(socket)
}

async fn recv_loop(
    socket: Arc<UdpSocket>,
    db: Arc<PvDatabase>,
    bind_ip: Ipv4Addr,
    tcp_port: u16,
    ignore_addrs: Vec<Ipv4Addr>,
    udp_rl: Arc<UdpRateLimiter>,
) -> CaResult<()> {
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
        // Per-datagram client sequence (captured from a leading VERSION
        // header whose `m_dataType == sequenceNoIsValid`). Echoed in any
        // VERSION reply we emit for this datagram so the client can
        // discard stale responses arriving after its search timer
        // expired (C `cas_send_dg_msg`, `caserverio.c:194-197`). Stays
        // `None` for peers that don't prepend a VERSION or that send
        // the older non-flagged form; the reply VERSION then carries
        // the default zero seq with the flag cleared.
        let mut client_seq: Option<u32> = None;
        while offset + CaHeader::SIZE <= len {
            let hdr = match CaHeader::from_bytes(&buf[offset..]) {
                Ok(h) => h,
                Err(_) => break,
            };
            // C `rsrv/camessage.c:2452` rejects misaligned `m_postsize`.
            // UDP path drops silently (no error response). Without this
            // check, the `align8(postsize)` advancement would jump
            // into the next message's body, mis-parsing chained
            // SEARCH datagrams.
            if (hdr.postsize as usize) & 0x7 != 0 {
                break;
            }
            let payload_size = hdr.postsize as usize;
            let msg_len = CaHeader::SIZE + payload_size;

            if offset + msg_len > len {
                break;
            }

            // C UDP dispatcher (camessage.c:2505-2516) allows only
            // udp_version_action (cmd 0) and search_reply_udp (cmd 6)
            // to succeed. Every other cmd index in the udpJumpTable
            // is bound to bad_udp_cmd_action which returns RSRV_ERROR
            // — the dispatcher loop then `break`s out, dropping the
            // rest of this datagram. Pre-fix Rust just advanced
            // `offset` and parsed the next message regardless;
            // a peer could chain a junk cmd before a SEARCH and the
            // chained SEARCH would still be processed even though
            // C IOC would have stopped parsing at the junk cmd.
            //
            // VERSION's UDP handler (udp_version_action, camessage.c:
            // 2094-2110) is a no-op for the stateless Rust responder:
            // it only stored per-client minor_version_number +
            // seqNoOfReq in C; Rust doesn't track UDP-per-datagram
            // state, so we just allow the VERSION header to pass and
            // continue.
            if hdr.cmmd != CA_PROTO_VERSION && hdr.cmmd != CA_PROTO_SEARCH {
                break;
            }
            if hdr.cmmd == CA_PROTO_VERSION {
                // C `udp_version_action` (rsrv/camessage.c:2094-2110)
                // stores `pclient->seqNoOfReq = m_cid` and the version
                // when the leading VERSION header marks the seq valid
                // (`m_dataType == sequenceNoIsValid`, caProto.h:128).
                // Capture it here so the SEARCH-reply branch can
                // populate its VERSION echo and match
                // `cas_send_dg_msg` byte-for-byte.
                //
                // R2-10: C `udp_version_action` returns RSRV_ERROR on
                // `!CA_VSUPPORTED(m_count)` and the UDP dispatcher
                // breaks out of the current datagram on any non-OK
                // status. Pre-fix Rust accepted any VERSION and
                // happily kept parsing later messages in the same
                // datagram — a malformed VERSION-first datagram could
                // still elicit a Rust SEARCH reply where rsrv would
                // have dropped the rest. Mirror C: bad version
                // breaks the per-datagram parse.
                const CA_MINIMUM_SUPPORTED_VERSION: u16 = 4;
                if hdr.count < CA_MINIMUM_SUPPORTED_VERSION {
                    break;
                }
                if hdr.data_type == 1 {
                    client_seq = Some(hdr.cid);
                }
            }
            if hdr.cmmd == CA_PROTO_SEARCH {
                // C `search_reply_udp` (rsrv/camessage.c:2151-2154)
                // rejects unsupported minor versions BEFORE the
                // empty-name check. `CA_VSUPPORTED(minor) = minor >= 4`
                // (CA_MINIMUM_SUPPORTED_VERSION in caProto.h:34). C
                // returns RSRV_ERROR which skips the reply. Ancient
                // libca clients (pre-V4.4) parse search replies with a
                // different layout; emitting our V4.13 reply confuses
                // them or worse, fabricates a usable channel they
                // can't actually open.
                // R2-10: C `search_reply_udp` (camessage.c:2151-2154)
                // returns RSRV_ERROR on unsupported minor version and
                // the UDP dispatcher breaks out of the datagram. Pre-
                // fix Rust skipped only the offending SEARCH and kept
                // parsing later messages in the same datagram, so a
                // malformed SEARCH-first datagram could still elicit
                // a Rust reply for a later message where rsrv would
                // have dropped the rest. Match C: bad-version SEARCH
                // ends the per-datagram parse.
                const CA_MINIMUM_SUPPORTED_VERSION: u16 = 4;
                if hdr.count < CA_MINIMUM_SUPPORTED_VERSION {
                    break;
                }
                // C `search_reply_udp` (rsrv/camessage.c:2159) rejects
                // SEARCH whose `m_postsize <= 1` ("empty PV name in UDP
                // search request") and silently returns RSRV_OK. The
                // null-terminator alone is 1 byte; a usable PV name
                // needs at least one non-null byte plus the terminator
                // (postsize >= 2). Without this guard the Rust path
                // would parse `pv_name = ""` from an attacker's empty-
                // postsize SEARCH burst and call `db.has_name("")` on
                // every datagram — wasted lookups + a non-trivial
                // amplification vector if a record happened to be
                // named "" (impossible in practice, but the C side
                // documents the reject and we match it).
                if hdr.postsize <= 1 {
                    offset += msg_len;
                    continue;
                }
                let payload_start = offset + CaHeader::SIZE;
                let payload_end = payload_start + hdr.postsize as usize;
                let payload = &buf[payload_start..payload_end];

                // Extract PV name (null-terminated)
                // R2-33: C `search_reply_udp` forces
                // `pName[mp->m_postsize - 1] = '\0'`. Cap the
                // NUL search at `postsize - 1` so an unterminated
                // peer name is treated as a `postsize - 1` byte
                // name (matching rsrv) rather than the full
                // payload (Rust pre-fix).
                let scan_end = payload.len().saturating_sub(1).max(0);
                let pv_name_end = payload[..scan_end]
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(scan_end);
                if let Ok(pv_name) = std::str::from_utf8(&payload[..pv_name_end]) {
                    if db.has_name(pv_name).await {
                        // C parity: `search_reply_udp`
                        // (`rsrv/camessage.c:2193-2207`) sets
                        // `sid = ~0U` (INADDR_BROADCAST), telling
                        // the client to use the UDP packet's source
                        // address as the server IP. The previous
                        // code embedded a probe-derived
                        // `local_ip_for(src)` which (a) diverged from
                        // C byte-for-byte and (b) could resolve to
                        // the wrong interface on multi-homed hosts —
                        // the probe binds 0.0.0.0:0 and `connect`s
                        // to the client, but the kernel's outgoing-
                        // interface choice may not match the
                        // interface the client used to reach us.
                        // Using the sentinel delegates the IP
                        // determination to the receiver, which gets
                        // it right by construction (the UDP source
                        // IP is whatever the client sees on the
                        // reply packet).
                        let mut resp = CaHeader::new(CA_PROTO_SEARCH);
                        resp.postsize = 8;
                        resp.data_type = tcp_port;
                        resp.count = 0;
                        resp.cid = u32::MAX; // ~0U — "use UDP source address"
                        resp.available = hdr.available;

                        let mut ver = CaHeader::new(CA_PROTO_VERSION);
                        ver.count = CA_MINOR_VERSION;
                        // C `cas_send_dg_msg` (`caserverio.c:194-197`)
                        // echoes `pclient->seqNoOfReq` in `m_cid` with
                        // `m_dataType = sequenceNoIsValid` (1) on
                        // CA_V411+. Required for libca's stale-
                        // response filter (`udpiiu.cpp:badSeqNumber`)
                        // — without the echo, libca's search-timer
                        // validation falls through and a reply that
                        // arrived after the timer expired is accepted
                        // anyway. Only set when the request datagram
                        // carried a flagged VERSION; older clients
                        // see ver.cid = 0 / data_type = 0 (no flag),
                        // matching the pre-V411 C wire form.
                        if let Some(seq) = client_seq {
                            ver.cid = seq;
                            ver.data_type = 1;
                        }

                        let mut reply = Vec::with_capacity(CaHeader::SIZE * 2 + 8);
                        reply.extend_from_slice(&ver.to_bytes());
                        reply.extend_from_slice(&resp.to_bytes());
                        let mut search_payload = [0u8; 8];
                        search_payload[0..2].copy_from_slice(&CA_MINOR_VERSION.to_be_bytes());
                        reply.extend_from_slice(&search_payload);

                        let _ = socket.send_to(&reply, src).await;
                    }
                    // C parity: `search_reply_udp` (rsrv/camessage.c:2167)
                    // silently returns on `dbChannelTest` failure for ALL
                    // UDP searches — there is no DO_REPLY branch on the
                    // UDP path. Only `search_reply_tcp` honours the flag
                    // and emits CA_PROTO_NOT_FOUND. Emitting NOT_FOUND
                    // here surprised C libca clients running through a
                    // name-server-list iteration: a UDP NOT_FOUND from a
                    // peer would short-circuit the broadcast search,
                    // missing IOCs that hadn't responded yet.
                }
            }

            offset += msg_len;
        }
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
