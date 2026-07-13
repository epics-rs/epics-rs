use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;

use crate::protocol::*;
use epics_base_rs::error::CaResult;
use epics_base_rs::net::{enable_so_rxq_ovfl_for_socket, recv_from_with_drop_count_socket};
use epics_base_rs::server::database::PvDatabase;

/// Decide the UDP responder sockets to open: one `(bind_ip,
/// mcast_groups)` spec per CA `casIntfAddrList` interface entry.
///
/// **Invariant.** C `caservertask.c:621-668` opens exactly
/// ONE UDP socket per `casIntfAddrList` entry — `conf->udp`, bound
/// to that entry's address (a specific IP *or* `INADDR_ANY`) — and
/// joins every `casMCastAddrList` group on THAT SAME socket via
/// `IP_ADD_MEMBERSHIP`. C never opens a separate per-multicast-group
/// socket.
///
/// Pre-fix Rust kept C parity for specific interfaces but, for a
/// wildcard (`0.0.0.0`) interface, emptied the interface's group
/// list and spawned one EXTRA `0.0.0.0:port` socket per multicast
/// group. With `SO_REUSEADDR`/`SO_REUSEPORT` datagram fanout, an
/// ordinary broadcast (and, on stacks without reuseport
/// load-balancing, unicast) CA SEARCH reached the primary wildcard
/// responder AND every extra multicast responder — each emitting
/// its own `send_to(reply, src)`, so one request got duplicate
/// replies. `IP_MULTICAST_ALL=0` filters only multicast group
/// cross-talk, not unicast/broadcast.
///
/// This function enforces the invariant structurally: every
/// returned spec carries the FULL multicast group list, and there
/// is exactly one spec per interface entry — there is no code path
/// that produces an extra group-only responder. A wildcard
/// interface is therefore the single owner of ordinary
/// unicast/broadcast SEARCH traffic, matching C's `conf->udp`.
fn plan_responder_specs(
    intf_addrs: Vec<Ipv4Addr>,
    mcast_addrs: &[Ipv4Addr],
) -> Vec<(Ipv4Addr, Vec<Ipv4Addr>)> {
    let intfs = if intf_addrs.is_empty() {
        vec![Ipv4Addr::UNSPECIFIED]
    } else {
        intf_addrs
    };
    intfs
        .into_iter()
        .map(|bind_ip| (bind_ip, mcast_addrs.to_vec()))
        .collect()
}

/// A UDP search responder whose sockets are already bound (and whose
/// multicast groups are already joined).
///
/// Counterpart of [`super::tcp::BoundTcp`]: only [`bind_udp_responders`]
/// can produce one, and every `CaServer` construction path does so
/// before returning the server. A bound UDP socket already queues
/// datagrams in the kernel, so a SEARCH that arrives before
/// `CaServer::run()` is polled is answered once the recv loop starts
/// rather than dropped.
#[derive(Clone)]
pub struct BoundResponder {
    bind_ip: Ipv4Addr,
    /// Primary socket, bound to the interface entry's address.
    socket: Arc<UdpSocket>,
    /// Secondary socket bound to the interface's broadcast address —
    /// see the comment in [`bind_udp_responders`]. `None` on Windows
    /// and for wildcard interfaces.
    bcast: Option<Arc<UdpSocket>>,
}

/// The UDP search responders of one server, all bound to one port.
#[derive(Clone)]
pub struct BoundUdp {
    responders: Vec<BoundResponder>,
    port: u16,
}

impl BoundUdp {
    /// The UDP port every responder is bound to. With a requested port
    /// of 0 this is the ephemeral port the kernel chose — the value a
    /// client must put in `EPICS_CA_ADDR_LIST` to reach this server.
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Bind one UDP search responder per `EPICS_CAS_INTF_ADDR_LIST`
/// interface, joining every configured multicast group on it.
///
/// Each interface gets its own socket — having a dedicated socket per
/// interface lets the OS keep the broadcast routing straight on multi-NIC
/// hosts (matching C EPICS osiSockDiscoverInterfaces behaviour).
///
/// A requested `port` of 0 means "any free UDP port": the first
/// responder's kernel-assigned port is then imposed on the remaining
/// interfaces, since a CA server answers SEARCHes on one port and its
/// clients are pointed at exactly one. This is the race-free way to
/// take a port — a caller that probes for a free port first and passes
/// the number can still lose it to another socket in between.
pub fn bind_udp_responders(
    port: u16,
    intf_addrs: Vec<Ipv4Addr>,
    mcast_addrs: &[Ipv4Addr],
) -> CaResult<BoundUdp> {
    let mut responders = Vec::new();
    let mut actual_port = if port == 0 { None } else { Some(port) };
    for (bind_ip, mcast_groups) in plan_responder_specs(intf_addrs, mcast_addrs) {
        let responder = bind_single_responder(bind_ip, actual_port.unwrap_or(0), &mcast_groups)?;
        if actual_port.is_none() {
            actual_port = Some(responder.socket.local_addr()?.port());
        }
        responders.push(responder);
    }
    // `plan_responder_specs` always yields at least one spec (an empty
    // interface list becomes the wildcard entry), so the port is set.
    let port = actual_port.ok_or_else(|| {
        epics_base_rs::error::CaError::Io(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "CAS: no UDP search responder bound",
        ))
    })?;
    Ok(BoundUdp { responders, port })
}

/// Run the recv loops of responders already bound by
/// [`bind_udp_responders`].
///
/// `ignore_addrs` filters out source addresses that should never receive
/// search replies (EPICS_CAS_IGNORE_ADDR_LIST).
pub async fn run_udp_search_responder(
    db: Arc<PvDatabase>,
    bound: BoundUdp,
    tcp_port: u16,
    ignore_addrs: Vec<Ipv4Addr>,
) -> CaResult<()> {
    let responders = bound.responders;
    let mut handles = Vec::with_capacity(responders.len());

    for responder in responders {
        let db_t = db.clone();
        let ignore_t = ignore_addrs.clone();
        let handle = epics_base_rs::runtime::task::spawn(async move {
            run_single_responder(db_t, responder, tcp_port, ignore_t).await
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

fn bind_single_responder(
    bind_ip: Ipv4Addr,
    port: u16,
    mcast_groups: &[Ipv4Addr],
) -> CaResult<BoundResponder> {
    let socket = bind_responder_socket(bind_ip, port)?;
    // The port the primary actually took — identical to `port`, except
    // for an ephemeral (0) request, where the kernel chose it. Every
    // other socket of this responder must bind that same port: a CA
    // server answers SEARCHes on exactly one port.
    let port = socket.local_addr()?.port();
    // Join each multicast group on this
    // responder's own socket via `IP_ADD_MEMBERSHIP` with
    // `imr_interface = bind_ip`. C `caservertask.c:633-665` joins
    // every `casMCastAddrList` group on the single `conf->udp`
    // socket of each `casIntfAddrList` entry, regardless of whether
    // that entry is a specific IP or `INADDR_ANY` — there is no
    // separate per-group socket. A wildcard `0.0.0.0` `bind_ip`
    // joins on the kernel default interface (matching C's
    // `imr_interface = INADDR_ANY` for a wildcard `conf->udpAddr`).
    // Joining here, on the one responder socket, keeps a single
    // owner for ordinary unicast/broadcast SEARCH traffic so a
    // request is answered exactly once. Per-(intf, group) failures
    // are logged and skipped — `caservertask.c:659-660`
    // `errlogPrintf`s and continues; the IOC stays up.
    for group in mcast_groups {
        match socket.join_multicast_v4(*group, bind_ip) {
            Ok(()) => tracing::debug!(
                target: "epics_ca_rs::server::udp",
                %bind_ip,
                group = %group,
                "joined multicast group on responder socket"
            ),
            Err(e) => tracing::warn!(
                target: "epics_ca_rs::server::udp",
                %bind_ip,
                group = %group,
                error = %e,
                "CA server IP_ADD_MEMBERSHIP failed — \
                 SEARCH on this group will not reach this NIC"
            ),
        }
    }
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
    let bcast: Option<Arc<UdpSocket>> = {
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

    Ok(BoundResponder {
        bind_ip,
        socket,
        bcast,
    })
}

async fn run_single_responder(
    db: Arc<PvDatabase>,
    responder: BoundResponder,
    tcp_port: u16,
    ignore_addrs: Vec<Ipv4Addr>,
) -> CaResult<()> {
    let BoundResponder {
        bind_ip,
        socket,
        bcast,
    } = responder;
    let udp_rl = Arc::new(UdpRateLimiter::from_env());
    let primary = recv_loop(
        socket,
        db.clone(),
        bind_ip,
        tcp_port,
        ignore_addrs.clone(),
        udp_rl.clone(),
    );

    match bcast {
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
    //
    // Only for a *well-known* port, which is the case the fanout exists
    // for. On an ephemeral bind (port 0) there is nothing to share, and
    // the flags are actively harmful: Linux may hand bind(0) a port that
    // already belongs to a reuse-compatible socket, silently joining its
    // SO_REUSEPORT group — the kernel then load-balances arriving
    // datagrams across the group, so SEARCHes for this server land on an
    // unrelated socket and go unanswered. Leaving the flags off makes the
    // kernel pick a port this socket exclusively owns.
    #[cfg(not(windows))]
    if port != 0 {
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
    // Name the socket in the error: a CA server binds several, and
    // "Address already in use" with no address is unactionable for an
    // operator whose UDP search port is taken by another process.
    sock.bind(&std::net::SocketAddrV4::new(bind_ip, port).into())
        .map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("CA server UDP search responder bind {bind_ip}:{port}: {e}"),
            )
        })?;
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

// the standalone `run_multicast_responder` was
// removed. It bound a *second* `0.0.0.0:port` socket per multicast
// group; with `SO_REUSEADDR`/`SO_REUSEPORT` datagram fanout that
// extra socket also caught ordinary unicast/broadcast CA SEARCH
// traffic and emitted duplicate replies. C `caservertask.c:633-665`
// has no such per-group socket — it joins every `casMCastAddrList`
// group on the single `conf->udp` socket of each interface entry.
// `run_single_responder` now owns the joins for both specific and
// wildcard interfaces, matching C.

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

    // secondary buffer for peek-and-drain across queued
    // inbounds. Heap-allocated once and reused on every iteration.
    let mut peek_buf = vec![0u8; 64 * 1024];

    loop {
        // C `cast_server.c:171-179`: a UDP recv error never exits the cast
        // server thread — it logs and sleeps 1 s, then keeps receiving. On
        // Windows, `WSAECONNRESET` (os error 10054) surfaces here whenever an
        // earlier send from this socket drew an ICMP port-unreachable
        // (KB263823); on Linux a connected-socket ICMP shows as
        // `ECONNREFUSED`. Propagating the error instead killed the SEARCH
        // responder for the rest of the server's life.
        let (len, src, drops) = match recv_from_with_drop_count_socket(&socket, &mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "epics_ca_rs::server::udp",
                    %bind_ip,
                    "CAS: UDP recv error: {e}"
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
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

        // per-(src, batch) state that survives across queued
        // same-src inbounds. Pre-fix Rust created a fresh send_buf
        // per inbound and flushed immediately, so a search storm of
        // N small datagrams from the same client yielded N reply
        // datagrams. C `cast_server.c:266-281` only flushes when
        // FIONREAD reports the recv queue is drained OR the peer
        // changes. Mirror that: accumulate same-src replies across
        // peeked inbounds; flush on peer change or queue drain.
        let mut current_src = src;
        // Owned copy of the current inbound so subsequent
        // `try_recv_from(&mut peek_buf)` peeks don't conflict with
        // the parse borrow. Reused via `clear()` + `extend_from_slice`.
        let mut current_buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        current_buf.extend_from_slice(&buf[..len]);
        // Per-datagram client sequence (captured from a leading VERSION
        // header whose `m_dataType == sequenceNoIsValid`). Echoed in any
        // VERSION reply we emit for this datagram so the client can
        // discard stale responses arriving after its search timer
        // expired (C `cas_send_dg_msg`, `caserverio.c:194-197`). Stays
        // `None` for peers that don't prepend a VERSION or that send
        // the older non-flagged form; the reply VERSION then carries
        // the default zero seq with the flag cleared.
        let mut client_seq: Option<u32> = None;
        // largest VERSION minor seen in this inbound. C
        // `udp_version_action` sets `pclient->minor_version_number`
        // unconditionally on every CA_VSUPPORTED VERSION; `cas_send_
        // dg_msg` consults `CA_V411(minor_version_number)` at flush
        // time. Pre-fix Rust gated the placeholder on
        // `client_seq.is_some()` which only fires for VERSIONs with
        // `dataType == sequenceNoIsValid`, missing the case where a
        // V4.13 client sends `m_count >= 11` without the seq flag.
        let mut client_minor: Option<u16> = None;
        // accumulator for the single outbound
        // datagram. C `cast_server.c:163-281` + `caserverio.c:185-201`
        // reuse one send buffer across all SEARCH matches and flush
        // once via `cas_send_dg_msg`. We pre-seed a VERSION
        // placeholder at byte 0 of every fresh batch (matching
        // `rsrv_version_reply`); at flush time we either fill in the
        // final seq fields (CA_V411 peer) or strip the first 16 bytes
        // (pre-V4.11 peer), matching `cas_send_dg_msg`'s gate.
        let mut send_buf: Vec<u8> = Vec::new();
        // match C's `MAX_UDP_SEND = 1024` (`caProto.h:66`).
        // `cas_copy_in_header` (`caserverio.c:280-294`) flushes when
        // the next message would push `stk > maxstk`, so C never
        // builds a UDP reply datagram larger than ~1024 bytes.
        // Third-party CA implementations (Java CAJ, asyncio-ca,
        // embedded ports) may assume that contract and truncate the
        // tail of larger replies. libca peers pre-allocate
        // `recvBuf[MAX_UDP_RECV]` so they tolerate larger, but the
        // wire-byte parity argument favors matching the C constant.
        const UDP_FLUSH_THRESHOLD: usize = 1024;
        'parse: loop {
            let mut offset = 0;
            while offset + CaHeader::SIZE <= current_buf.len() {
                let hdr = match CaHeader::from_bytes(&current_buf[offset..]) {
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

                if offset + msg_len > current_buf.len() {
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
                    // C `udp_version_action` returns RSRV_ERROR on
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
                    // track the largest VERSION minor seen so the
                    // flush-time placeholder strip/keep decision matches
                    // `CA_V411(minor_version_number)` regardless of
                    // whether the inbound's leading frame is VERSION,
                    // SEARCH, or chained.
                    client_minor = Some(client_minor.unwrap_or(0).max(hdr.count));
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
                    // C `search_reply_udp` (camessage.c:2151-2154)
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
                    let payload = &current_buf[payload_start..payload_end];

                    // Extract PV name (null-terminated)
                    // C `search_reply_udp` forces
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
                        // thread the datagram source address
                        // into the search resolver so the CA gateway can
                        // apply host-scoped `.pvlist` `DENY FROM host`
                        // admission at search time (parity with C
                        // `pvExistTest` passing the client host to
                        // `gateAs::findEntry`).
                        if db.has_name_from(pv_name, Some(src)).await {
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
                            // Placeholder VERSION header — `cid` and
                            // `data_type` get patched at flush time once
                            // we know whether the inbound carried a
                            // CA_V411 VERSION.

                            let resp_bytes = resp.to_bytes();
                            let mut search_payload = [0u8; 8];
                            search_payload[0..2].copy_from_slice(&CA_MINOR_VERSION.to_be_bytes());

                            // accumulate into send_buf
                            // and ALWAYS pre-seed a VERSION placeholder
                            // at byte 0 of a fresh batch — matching
                            // `rsrv_version_reply`'s up-front seed.
                            // `cas_send_dg_msg` decides at flush time
                            // whether to keep (CA_V411 peer) or strip
                            // (pre-V4.11 peer) those 16 bytes. The
                            // placeholder always being present means a
                            // chained inbound that puts SEARCH before
                            // VERSION still gets a VERSION-led reply.
                            // Flush before append if the next reply
                            // would push us over the MTU; the post-
                            // flush re-seed (handled in `flush_send_buf`)
                            // mirrors C's per-flush re-seed.
                            const SEARCH_REPLY_LEN: usize = CaHeader::SIZE + 8;
                            if !send_buf.is_empty()
                                && send_buf.len() + SEARCH_REPLY_LEN > UDP_FLUSH_THRESHOLD
                            {
                                flush_send_buf(
                                    &socket,
                                    current_src,
                                    &mut send_buf,
                                    client_minor,
                                    client_seq,
                                    &bind_ip,
                                )
                                .await;
                            }
                            if send_buf.is_empty() {
                                send_buf.extend_from_slice(&ver.to_bytes());
                            }
                            send_buf.extend_from_slice(&resp_bytes);
                            send_buf.extend_from_slice(&search_payload);
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
            // peek for queued inbounds. C `cast_server.c:266-281`
            // calls `socket_ioctl(FIONREAD, &nchars)` after each
            // `camessage()` and ONLY flushes `cas_send_dg_msg` when
            // `nchars == 0` or the peer changes. Mirror that — drain
            // queued same-src inbounds into the same `send_buf` so a
            // search storm of N small same-peer datagrams yields ONE
            // outbound, not N. Different-src peeks flush the current
            // batch and start a fresh one for the new src.
            //
            // a queued datagram that is rejected (short header,
            // ignore-list, rate limit) must NOT restart the parser
            // over the *previous* datagram's bytes still sitting in
            // `current_buf`, and must NOT leave `current_src` pointing
            // at the rejected peer. C `cast_server.c` always overwrites
            // `client->recv.buf` with `recvfrom` before `camessage()`
            // runs, and an ignored datagram sets `status = -1` so the
            // whole parse + `client->addr` update is skipped. Mirror
            // that: drain-and-discard rejected datagrams in this inner
            // loop without touching `current_src` / `current_buf`;
            // only an accepted datagram performs the peer-change flush,
            // replaces `current_buf`, and re-enters `'parse`.
            let next_datagram = loop {
                match socket.try_recv_from(&mut peek_buf) {
                    Ok((peek_len, peek_src)) => {
                        // C `cast_server.c` requires a full caHdr before
                        // `camessage()` will parse anything; a short
                        // datagram yields no SEARCH work. Discard it
                        // and try to drain the next queued datagram —
                        // do NOT re-parse `current_buf`.
                        if peek_len < CaHeader::SIZE {
                            continue;
                        }
                        // Ignore-list / rate-limit rejections discard
                        // the datagram without changing peer state —
                        // C skips `camessage()` for `casIgnoreAddrs`
                        // hits via `status = -1`.
                        if let SocketAddr::V4(v4) = peek_src {
                            if ignore_addrs.contains(v4.ip()) {
                                continue;
                            }
                        }
                        if !udp_rl.allow(&peek_src) {
                            metrics::counter!("ca_server_udp_search_drops_total").increment(1);
                            continue;
                        }
                        break Some((peek_len, peek_src));
                    }
                    Err(_) => break None, // recv queue drained
                }
            };
            match next_datagram {
                Some((peek_len, peek_src)) => {
                    if peek_src != current_src {
                        // Peer change: flush current batch to the old
                        // src, then reset batch state for the new src.
                        flush_send_buf(
                            &socket,
                            current_src,
                            &mut send_buf,
                            client_minor,
                            client_seq,
                            &bind_ip,
                        )
                        .await;
                        current_src = peek_src;
                        client_seq = None;
                        client_minor = None;
                    }
                    // Replace `current_buf` with the accepted
                    // datagram's bytes BEFORE re-entering `'parse` so
                    // the parser never reprocesses the previous
                    // datagram under a new `current_src`.
                    current_buf.clear();
                    current_buf.extend_from_slice(&peek_buf[..peek_len]);
                    continue 'parse;
                }
                None => break 'parse, // recv queue drained
            }
        } // 'parse
        // flush the accumulated SEARCH
        // replies as a single outbound datagram. `cas_send_dg_msg`
        // does the same after each batch is fully parsed.
        if !send_buf.is_empty() {
            flush_send_buf(
                &socket,
                current_src,
                &mut send_buf,
                client_minor,
                client_seq,
                &bind_ip,
            )
            .await;
        }
    }
}

/// send the accumulated SEARCH-reply batch.
///
/// If `client_minor >= 11` (CA_V411 peer), patch bytes 0..16 of
/// `send_buf` with the final VERSION header (cid = client seq if
/// any, data_type = 1 when seq present). The placeholder was seeded
/// at first append. Otherwise strip the placeholder by slicing off
/// the first 16 bytes — pre-V4.11 peers must not see the VERSION
/// header.
///
/// On `send_to` failure, log at warn level instead of silently
/// discarding (`caserverio.c:214-222` `errlogPrintf` parity).
async fn flush_send_buf(
    socket: &UdpSocket,
    src: SocketAddr,
    send_buf: &mut Vec<u8>,
    client_minor: Option<u16>,
    client_seq: Option<u32>,
    bind_ip: &Ipv4Addr,
) {
    if send_buf.is_empty() {
        return;
    }
    let payload: &[u8] = if client_minor.is_some_and(|m| m >= 11) {
        // Patch placeholder at bytes 0..16 with final seq/data_type.
        // The placeholder was seeded with cid=0, data_type=0.
        if send_buf.len() >= CaHeader::SIZE {
            let mut ver = CaHeader::new(CA_PROTO_VERSION);
            ver.count = CA_MINOR_VERSION;
            if let Some(seq) = client_seq {
                ver.cid = seq;
                ver.data_type = 1;
            }
            let bytes = ver.to_bytes();
            send_buf[..CaHeader::SIZE].copy_from_slice(&bytes);
        }
        &send_buf[..]
    } else {
        // Pre-V4.11 peer: strip the placeholder.
        if send_buf.len() >= CaHeader::SIZE {
            &send_buf[CaHeader::SIZE..]
        } else {
            // Defensive: nothing past the placeholder, nothing to send.
            send_buf.clear();
            return;
        }
    };
    if let Err(e) = socket.send_to(payload, src).await {
        tracing::warn!(
            target: "epics_ca_rs::server::udp",
            %bind_ip,
            dst = %src,
            payload_len = payload.len(),
            error = %e,
            "CA server UDP SEARCH-reply batch send failed"
        );
        metrics::counter!("ca_server_udp_search_reply_send_failures_total").increment(1);
    }
    send_buf.clear();
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
            // Age forward (`now - t`) rather than a `now - 5s` cutoff:
            // subtracting a Duration from an Instant panics on Windows
            // (QPC-since-boot) when machine uptime is shorter than 5s.
            counts.retain(|_, (t, _)| {
                now.saturating_duration_since(*t) <= std::time::Duration::from_secs(5)
            });
        }
        true
    }
}

#[cfg(test)]
mod mr_r8_responder_plan_tests {
    //! a wildcard interface configuration must produce exactly
    //! one responder socket — the single owner of ordinary
    //! unicast/broadcast SEARCH traffic — even when multicast groups
    //! are configured. No extra per-multicast-group `0.0.0.0:port`
    //! socket may be created.
    use super::plan_responder_specs;
    use std::net::Ipv4Addr;

    /// Pre-fix, a wildcard interface plus N multicast groups spawned
    /// 1 primary + N group-only responders, all bound `0.0.0.0:port`
    /// — duplicate-reply fanout. The plan must now be a single
    /// wildcard responder that itself carries every multicast group.
    #[test]
    fn mr_r8_wildcard_with_mcast_groups_yields_one_responder() {
        let groups = vec![Ipv4Addr::new(224, 0, 0, 100), Ipv4Addr::new(224, 0, 0, 101)];
        // Empty intf list → default wildcard interface.
        let specs = plan_responder_specs(Vec::new(), &groups);
        assert_eq!(
            specs.len(),
            1,
            "wildcard config must produce exactly ONE responder \
             socket, not one-per-multicast-group"
        );
        let (bind_ip, mcast) = &specs[0];
        assert_eq!(*bind_ip, Ipv4Addr::UNSPECIFIED);
        assert_eq!(
            mcast, &groups,
            "the single wildcard responder must own ALL multicast \
             group joins (C `conf->udp` parity)"
        );

        // An explicit `0.0.0.0` entry behaves identically.
        let specs2 = plan_responder_specs(vec![Ipv4Addr::UNSPECIFIED], &groups);
        assert_eq!(specs2.len(), 1);
        assert_eq!(specs2[0].1, groups);
    }

    /// A specific-interface configuration keeps one responder per
    /// interface entry, each carrying the full multicast group list.
    #[test]
    fn mr_r8_specific_intfs_each_own_all_mcast_groups() {
        let groups = vec![Ipv4Addr::new(224, 0, 0, 200)];
        let intfs = vec![Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 2)];
        let specs = plan_responder_specs(intfs.clone(), &groups);
        assert_eq!(specs.len(), 2, "one responder per interface entry");
        for (i, (bind_ip, mcast)) in specs.iter().enumerate() {
            assert_eq!(*bind_ip, intfs[i]);
            assert_eq!(
                mcast, &groups,
                "each interface responder joins every multicast group \
                 on its own socket (C `conf->udp` parity)"
            );
        }
    }

    /// No multicast groups: exactly one wildcard responder with an
    /// empty group list — no spurious extra sockets.
    #[test]
    fn mr_r8_no_mcast_groups_yields_single_plain_responder() {
        let specs = plan_responder_specs(Vec::new(), &[]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].0, Ipv4Addr::UNSPECIFIED);
        assert!(specs[0].1.is_empty());
    }
}

#[cfg(test)]
mod responder_bind_tests {
    use super::*;

    /// Boundary — port 0: an ephemeral responder must own its port
    /// exclusively. The reuse flags exist so a well-known CA port can be
    /// co-bound (caRepeater, a second IOC); on an ephemeral bind they let
    /// Linux hand out a port already held by a reuse-compatible socket,
    /// joining its SO_REUSEPORT group. The kernel then load-balances
    /// arriving datagrams across the group and SEARCHes for this server
    /// vanish into the other socket. A second bind of the same port must
    /// therefore be refused.
    #[test]
    fn ephemeral_responder_port_cannot_be_co_bound() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();

        let bound = bind_udp_responders(0, vec![Ipv4Addr::LOCALHOST], &[]).expect("bind ephemeral");
        assert_ne!(bound.port(), 0, "an ephemeral bind reports its real port");

        // A reuse-enabled datagram socket — exactly the kind the CA
        // client and caRepeater open — must NOT be able to join it.
        let intruder = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("socket");
        intruder.set_reuse_address(true).expect("reuse addr");
        #[cfg(unix)]
        intruder.set_reuse_port(true).expect("reuse port");
        let addr = std::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, bound.port());
        assert!(
            intruder.bind(&addr.into()).is_err(),
            "the responder's ephemeral port must be exclusively owned; a \
             second socket co-binding it would steal half its SEARCHes"
        );
    }

    /// Boundary — a fixed port keeps the datagram-fanout reuse flags, so
    /// the well-known CA port can still be co-bound (caRepeater parity).
    #[test]
    fn fixed_responder_port_still_allows_datagram_fanout() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();

        // Take an ephemeral port first, then re-bind that same number as
        // a *fixed* port so the test never guesses at a free one.
        let probe = bind_udp_responders(0, vec![Ipv4Addr::LOCALHOST], &[]).expect("probe bind");
        let port = probe.port();
        drop(probe);

        let bound = bind_udp_responders(port, vec![Ipv4Addr::LOCALHOST], &[]).expect("fixed bind");
        assert_eq!(bound.port(), port);

        let peer = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("socket");
        peer.set_reuse_address(true).expect("reuse addr");
        #[cfg(unix)]
        peer.set_reuse_port(true).expect("reuse port");
        let addr = std::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        assert!(
            peer.bind(&addr.into()).is_ok(),
            "a well-known CA port must stay co-bindable (caRepeater fanout)"
        );
    }
}
