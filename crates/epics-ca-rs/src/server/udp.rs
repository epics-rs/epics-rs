// The bound-socket UDP responder stack (`socket2` shared-port setup, the
// `tokio::net::UdpSocket` recv loops, the `epics_base_rs::net` RX-overflow
// helpers) is the async host-only front-end. The embedded build (RTEMS or
// VxWorks) answers SEARCH through `server::blocking`'s `std::net` responder,
// reusing only the shared decode/shape logic (`SearchReplyBatch`,
// `parse_search_datagram`, `shape_search_reply_dg`) below. Those imports are
// gated out for `epics_embedded_target`.
// RTEMS-EXEC-MODEL-ALLOW(2): both sites hand-build a tokio runtime to drive the
// tokio::net UDP name-server socket. These run and pass in the
// feature-ON suite on the tokio driver.
#[cfg(not(epics_embedded_target))]
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
// `Ipv4Addr`, `Arc`, and `CaResult` are used only by the bound-socket responder
// stack, which is host-only; the shared decode path uses `SocketAddr` only.
#[cfg(not(epics_embedded_target))]
use std::net::Ipv4Addr;
#[cfg(not(epics_embedded_target))]
use std::sync::Arc;
#[cfg(not(epics_embedded_target))]
use tokio::net::UdpSocket;

use crate::protocol::*;
#[cfg(not(epics_embedded_target))]
use epics_base_rs::error::CaResult;
#[cfg(not(epics_embedded_target))]
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
#[cfg(not(epics_embedded_target))]
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
#[cfg(not(epics_embedded_target))]
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
#[cfg(not(epics_embedded_target))]
#[derive(Clone)]
pub struct BoundUdp {
    responders: Vec<BoundResponder>,
    port: u16,
}

#[cfg(not(epics_embedded_target))]
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
#[cfg(not(epics_embedded_target))]
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
#[cfg(not(epics_embedded_target))]
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

#[cfg(not(epics_embedded_target))]
fn bind_single_responder(
    bind_ip: Ipv4Addr,
    port: u16,
    mcast_groups: &[Ipv4Addr],
) -> CaResult<BoundResponder> {
    let socket = bind_responder_socket(bind_ip, port)?;
    // The port the primary actually took — identical to `port`, except
    // for an ephemeral (0) request, where the kernel chose it. The only
    // consumer of the resolved port is the broadcast secondary responder
    // below, which exists on non-Windows only (Windows has no secondary
    // socket), so resolve it under the same gate — otherwise the binding
    // is unused on Windows.
    #[cfg(not(any(windows, target_os = "windows")))]
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

#[cfg(not(epics_embedded_target))]
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
#[cfg(not(epics_embedded_target))]
fn bind_responder_socket(bind_ip: Ipv4Addr, port: u16) -> CaResult<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // This is a *datagram fanout* socket, so it mirrors libcom's
    // `epicsSocketEnableAddressUseForDatagramFanout`
    // (osdSockAddrReuse.cpp), which the C CA server applies to exactly
    // these sockets — the UDP name receiver and its broadcast companion
    // (caservertask.c:628, :698). That helper sets SO_REUSEPORT (where
    // it exists) followed by SO_REUSEADDR on *every* platform: it has no
    // `#ifdef _WIN32`. Windows must set SO_REUSEADDR too, or a second IOC
    // on the host cannot bind the CA port and never receives a broadcast
    // SEARCH.
    //
    // Do not confuse this with `epicsSocketEnableAddressReuseDuringTimeWaitState`
    // (the TCP time-wait helper), which *is* a Windows no-op because
    // WINSOCK's SO_REUSEADDR has port-hijack semantics there. That rule
    // governs the TCP listener and caRepeater's exclusive bind, not this
    // socket.
    //
    // Only for a *well-known* port, which is the case the fanout exists
    // for — C likewise leaves its PORT_ANY sockets bare (udpiiu.cpp:248
    // binds the client search socket to PORT_ANY and enables no reuse).
    // On an ephemeral bind the flags are actively harmful: Linux may hand
    // bind(0) a port that already belongs to a reuse-compatible socket,
    // silently joining its SO_REUSEPORT group — the kernel then
    // load-balances arriving datagrams across the group, so SEARCHes for
    // this server land on an unrelated socket and go unanswered. Leaving
    // the flags off makes the kernel pick a port this socket exclusively
    // owns.
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

/// Cross-datagram accumulator for the UDP SEARCH responder: the reply
/// batch under construction plus the per-datagram VERSION-echo state
/// ([`shape_search_reply_dg`] consults both at flush time). Persisting it
/// across [`parse_search_datagram`] calls lets a caller coalesce several
/// same-source datagrams into one reply — C `cast_server.c:266-281` drains
/// the recv queue (FIONREAD) into a single `cas_send_dg_msg`.
#[derive(Default)]
pub(crate) struct SearchReplyBatch {
    /// One outbound datagram in progress. Byte 0 holds the seeded VERSION
    /// placeholder; SEARCH replies are appended after it.
    send_buf: Vec<u8>,
    /// Client sequence captured from a leading VERSION with
    /// `m_dataType == sequenceNoIsValid`; echoed in the reply VERSION.
    client_seq: Option<u32>,
    /// Largest VERSION minor seen this datagram; drives the CA_V411
    /// keep/strip decision at flush time.
    client_minor: Option<u16>,
}

impl SearchReplyBatch {
    /// Shape the trailing (post-last-flush) batch into its on-wire bytes,
    /// or `None` when empty. The blocking responder
    /// (`crate::server::blocking`) calls this to emit the final datagram
    /// after [`parse_search_datagram`] returns; the async responder does the
    /// equivalent through `flush_send_buf`. Encapsulates the private batch
    /// fields so the shaping stays in one place.
    pub(crate) fn shape_trailing(&self) -> Option<Vec<u8>> {
        shape_search_reply_dg(&self.send_buf, self.client_minor, self.client_seq)
    }

    /// Shape the current batch into its on-wire datagram (or `None` when
    /// empty), then reset the batch to empty. The blocking responder
    /// (`crate::server::blocking`) calls this to flush at a coalescing-group
    /// boundary — a recv-queue drain (FIONREAD == 0), a peer change, or
    /// shutdown — which is byte-equivalent to the async responder's
    /// `flush_send_buf` (shape + `send_buf.clear()`) followed by the
    /// peer-change `client_seq`/`client_minor` reset (`recv_loop`,
    /// udp.rs:846-849). Resetting the whole batch keeps the invariant "a
    /// non-empty batch's `client_*` echo state belongs to its own replies"
    /// true by construction for the next group.
    pub(crate) fn take_reply(&mut self) -> Option<Vec<u8>> {
        let dg = self.shape_trailing();
        *self = SearchReplyBatch::default();
        dg
    }
}

/// Parse one inbound UDP datagram's CA messages (VERSION + SEARCH),
/// appending SEARCH-reply bytes to `batch`. This is the shared
/// decode/respond core of the CA UDP name-search responder: the async
/// reactor front-end (`recv_loop`) and the blocking thread-per-client
/// front-end (`crate::server::blocking`, the embedded-target server) both
/// call it, so a search reply is byte-identical on either path.
///
/// `batch` persists across calls so the caller can coalesce several
/// same-source datagrams into one reply datagram (C FIONREAD drain). When
/// the accumulated batch would exceed the ~1 KB UDP flush threshold, the
/// current batch is shaped ([`shape_search_reply_dg`]) and pushed to
/// `ready`, and a fresh batch is begun — mirroring `cas_copy_in_header`'s
/// mid-batch flush (`caserverio.c:280-294`). The caller sends each `ready`
/// datagram; the trailing `batch.send_buf` is shaped and sent when the
/// caller's recv queue drains or the peer changes.
///
/// `lookup_src` is the datagram source threaded into `has_name_from` for
/// host-scoped gateway `.pvlist` admission (C `pvExistTest`).
///
/// Contains no socket I/O: the only await is the database name lookup,
/// which resolves on the first poll for a local record, so it is safe to
/// drive under `park_on` on a runtime-less embedded-target thread.
///
/// C: `search_reply_udp` / `udp_version_action` (`rsrv/camessage.c`).
pub(crate) async fn parse_search_datagram(
    input: &[u8],
    db: &PvDatabase,
    tcp_port: u16,
    lookup_src: SocketAddr,
    batch: &mut SearchReplyBatch,
    ready: &mut Vec<Vec<u8>>,
) {
    // match C's `MAX_UDP_SEND = 1024` (`caProto.h:66`).
    // `cas_copy_in_header` (`caserverio.c:280-294`) flushes when the next
    // message would push `stk > maxstk`, so C never builds a UDP reply
    // datagram larger than ~1024 bytes. Third-party CA implementations
    // (Java CAJ, asyncio-ca, embedded ports) may assume that contract and
    // truncate the tail of larger replies. libca peers pre-allocate
    // `recvBuf[MAX_UDP_RECV]` so they tolerate larger, but the wire-byte
    // parity argument favors matching the C constant.
    const UDP_FLUSH_THRESHOLD: usize = 1024;
    let SearchReplyBatch {
        send_buf,
        client_seq,
        client_minor,
    } = batch;
    let mut offset = 0;
    while offset + CaHeader::SIZE <= input.len() {
        let hdr = match CaHeader::from_bytes(&input[offset..]) {
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

        if offset + msg_len > input.len() {
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
            *client_minor = Some(client_minor.unwrap_or(0).max(hdr.count));
            if hdr.data_type == 1 {
                *client_seq = Some(hdr.cid);
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
            let payload = &input[payload_start..payload_end];

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
                if db.has_name_from(pv_name, Some(lookup_src)).await {
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
                    // flush re-seed (handled below) mirrors C's
                    // per-flush re-seed.
                    const SEARCH_REPLY_LEN: usize = CaHeader::SIZE + 8;
                    if !send_buf.is_empty()
                        && send_buf.len() + SEARCH_REPLY_LEN > UDP_FLUSH_THRESHOLD
                    {
                        // Over the ~1 KB MTU: shape the current batch, hand it
                        // to the caller to send, and begin a fresh batch. C
                        // `cas_copy_in_header` flushes mid-build the same way
                        // (`caserverio.c:280-294`).
                        if let Some(dg) =
                            shape_search_reply_dg(&send_buf[..], *client_minor, *client_seq)
                        {
                            ready.push(dg);
                        }
                        send_buf.clear();
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
}

#[cfg(not(epics_embedded_target))]
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
        // Reply batch + per-datagram VERSION-echo state, persisted across
        // this inbound and any same-source datagrams drained below, so a
        // search storm of N same-peer datagrams yields ONE outbound — C
        // `cast_server.c:266-281` accumulates into one `cas_send_dg_msg`.
        let mut batch = SearchReplyBatch::default();
        'parse: loop {
            let mut ready: Vec<Vec<u8>> = Vec::new();
            parse_search_datagram(&current_buf, &db, tcp_port, src, &mut batch, &mut ready).await;
            // Send this datagram's over-threshold batches to its peer. C
            // `cas_send_dg_msg` flushes each full batch as it is built
            // (`caserverio.c:280-294`); the trailing partial batch is flushed
            // by the peer-change / queue-drain logic below.
            for dg in ready.drain(..) {
                send_reply_dg(&socket, current_src, &dg, &bind_ip).await;
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
                        flush_send_buf(&socket, current_src, &mut batch, &bind_ip).await;
                        current_src = peek_src;
                        batch.client_seq = None;
                        batch.client_minor = None;
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
        // flush the accumulated SEARCH replies as a single outbound
        // datagram. `cas_send_dg_msg` does the same after each batch is
        // fully parsed.
        if !batch.send_buf.is_empty() {
            flush_send_buf(&socket, current_src, &mut batch, &bind_ip).await;
        }
    }
}

/// Shape the final on-wire bytes of one accumulated SEARCH-reply batch —
/// the pure, I/O-free core of `cas_send_dg_msg` (`caserverio.c:185-201`).
/// Shared by the async responder (`flush_send_buf`) and the blocking
/// thread-per-client responder (`crate::server::blocking`), so a reply is
/// byte-identical on either front-end.
///
/// If `client_minor >= 11` (CA_V411 peer), the seeded VERSION placeholder
/// at bytes 0..16 is patched with the final header (cid = client seq if
/// any, data_type = 1 when seq present). Otherwise the 16-byte placeholder
/// is stripped — pre-V4.11 peers must not see the VERSION header. Returns
/// `None` when there is nothing to send. Does not mutate `send_buf`; the
/// caller clears it after a flush.
pub(crate) fn shape_search_reply_dg(
    send_buf: &[u8],
    client_minor: Option<u16>,
    client_seq: Option<u32>,
) -> Option<Vec<u8>> {
    if send_buf.is_empty() {
        return None;
    }
    if client_minor.is_some_and(|m| m >= 11) {
        // Patch placeholder at bytes 0..16 with final seq/data_type.
        // The placeholder was seeded with cid=0, data_type=0.
        let mut out = send_buf.to_vec();
        if out.len() >= CaHeader::SIZE {
            let mut ver = CaHeader::new(CA_PROTO_VERSION);
            ver.count = CA_MINOR_VERSION;
            if let Some(seq) = client_seq {
                ver.cid = seq;
                ver.data_type = 1;
            }
            out[..CaHeader::SIZE].copy_from_slice(&ver.to_bytes());
        }
        Some(out)
    } else if send_buf.len() >= CaHeader::SIZE {
        // Pre-V4.11 peer: strip the placeholder.
        Some(send_buf[CaHeader::SIZE..].to_vec())
    } else {
        // Defensive: nothing past the placeholder, nothing to send.
        None
    }
}

/// Send one already-shaped reply datagram to `src`. On failure, log at warn
/// level instead of silently discarding (`caserverio.c:214-222`
/// `errlogPrintf` parity).
#[cfg(not(epics_embedded_target))]
async fn send_reply_dg(socket: &UdpSocket, src: SocketAddr, payload: &[u8], bind_ip: &Ipv4Addr) {
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
}

/// Shape the batch's trailing `send_buf` and send it as one datagram, then
/// clear the batch buffer. The async responder's flush path.
#[cfg(not(epics_embedded_target))]
async fn flush_send_buf(
    socket: &UdpSocket,
    src: SocketAddr,
    batch: &mut SearchReplyBatch,
    bind_ip: &Ipv4Addr,
) {
    if let Some(payload) =
        shape_search_reply_dg(&batch.send_buf, batch.client_minor, batch.client_seq)
    {
        send_reply_dg(socket, src, &payload, bind_ip).await;
    }
    batch.send_buf.clear();
}

/// Per-source-IP token bucket on the UDP search responder. Mitigates
/// amplification (a tiny SEARCH eliciting a much larger SEARCH_REPLY
/// across many records) and absurd loops from misconfigured clients.
///
/// Disabled when neither env var is set; the cost is one IP-equality
/// comparison per packet otherwise. The implementation is a fixed
/// 1-second sliding window — coarse but cheap; replace with
/// per-IP token buckets if a finer policy is ever needed.
#[cfg(not(epics_embedded_target))]
struct UdpRateLimiter {
    enabled: bool,
    cap_per_sec: u32,
    counts:
        std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, (std::time::Instant, u32)>>,
}

#[cfg(not(epics_embedded_target))]
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
    /// the well-known CA port can still be co-bound (multiple IOCs on one
    /// host must all receive a broadcast SEARCH).
    ///
    /// Runs on Windows too: libcom's fanout helper sets SO_REUSEADDR on
    /// every platform, so the co-bind must work there as well. This test
    /// is what pins that — it failed on Windows while
    /// `bind_responder_socket` was (wrongly) applying the TCP time-wait
    /// helper's Windows carve-out.
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
