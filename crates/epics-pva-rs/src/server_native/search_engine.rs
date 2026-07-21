//! The socket-free half of the UDP SEARCH path.
//!
//! Where [`super::search`] owns one SEARCH *message* — parse, name match,
//! response framing — this module owns one UDP *datagram*: the chained-message
//! drain, the ORIGIN_TAG forward decision, the reply-destination resolution,
//! and the source filter that decides whether a datagram is looked at at all.
//! It decides *what* to send and returns it as a `SearchOutput` list; the
//! caller owns the sockets and performs the sends.
//!
//! **Why it is its own module.** `aa1af842` already made
//! `process_search_datagram` socket-free, but left it inside
//! [`super::udp`], which is gated out of RTEMS (`mod.rs`) because of the
//! `tokio::net` / `socket2` / `if-addrs` stack around it. A blocking RTEMS
//! driver could therefore not reach the decode even though the decode itself
//! needs no reactor. Lifting it here — un-gated, exactly as the SEARCH
//! protocol was lifted into [`super::search`] (`4da1e04a`) and the accept loop
//! into [`super::accept`] (`4c75e766`) — is a move, not a copy: a copy would
//! re-open the whole family of SEARCH-parity bugs on a second code path.
//! `super::udp` re-exports every name below, so its own call sites and tests
//! are unchanged and keep exercising this code.
//!
//! Nothing here names a socket type, a runtime, or an address enumerator.

use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};

use epics_base_rs::net::ORIGIN_TAG_MCAST_GROUP;
use tracing::debug;

use crate::codec::PvaCodec;
use crate::proto::{ByteOrder, PvaHeader, ip_to_bytes};

use super::search::{
    SearchRequest, build_search_response_proto, matched_cids_for_requester, parse_search_request,
};
use super::source::DynSource;
/// Generate a 12-byte server GUID.
///
/// Prefers `/dev/urandom` (Unix) so two servers started at the same
/// nanosecond on different machines get distinct GUIDs and clients
/// can't predict a server's GUID from start time + PID — pvxs
/// `Server::Pvt::randomGUID` parity (commit ca594f40 "server: randomize
/// UUID"). Falls back to the previous time + PID layout on platforms
/// without `/dev/urandom` so non-Unix builds still get a unique-enough
/// GUID per process.
pub fn random_guid() -> [u8; 12] {
    let mut buf = [0u8; 12];
    if !try_fill_secure(&mut buf) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        buf[..8].copy_from_slice(&now.to_le_bytes());
        let pid = std::process::id().to_le_bytes();
        buf[8..12].copy_from_slice(&pid);
    }
    buf
}

#[cfg(unix)]
fn try_fill_secure(buf: &mut [u8]) -> bool {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .is_ok()
}

#[cfg(not(unix))]
fn try_fill_secure(_buf: &mut [u8]) -> bool {
    false
}

/// Build a forward-ready copy of `frame` when its first SEARCH message
/// has the Unicast flag set. Mirrors pvxs `udp_collector.cpp:387-396`:
/// the recipient of the forwarded message has no access to the
/// original UDP source, so the forwarder rewrites the SEARCH's reply
/// address field with the resolved `reply_dest` (a fully concrete
/// destination) AND clears the Unicast flag before sending. Returns
/// `None` when the frame doesn't open with a unicast-flagged SEARCH —
/// no forward needed.
///
/// SEARCH payload layout (after the 8-byte PVA header):
///
/// | offset | size | field            |
/// |--------|------|------------------|
/// |   0    |  4   | sequence_id      |
/// |   4    |  1   | flags            |
/// |   5    |  3   | reserved         |
/// |   8    | 16   | response_addr    |
/// |  24    |  2   | response_port    |
pub(crate) fn try_build_forward_frame(frame: &[u8], reply_dest: SocketAddr) -> Option<Vec<u8>> {
    let req = parse_search_request(frame)?;
    if !req.unicast {
        return None;
    }
    if frame.len() < PvaHeader::SIZE + 26 {
        return None;
    }
    let mut out = frame.to_vec();
    let payload_off = PvaHeader::SIZE;
    // Clear Unicast bit so peers don't re-forward (pvxs uses the flag
    // as a "single-server-targeted" marker).
    out[payload_off + 4] &= !0x80;
    // Overwrite the 16-byte response_addr with the resolved reply
    // destination (`to_wire(replyDest)`, pvxs `udp_collector.cpp:394`):
    // a v4 address as the v4-mapped IPv6 form, a v6 address as its raw
    // 16 bytes. The reply-address field is family-independent even though
    // the ORIGIN_TAG forward transport is IPv4, so an explicit IPv6 reply
    // endpoint must survive the forward. The recipient uses this field as
    // the SEARCH_RESPONSE destination since the original UDP source is the
    // forwarder, not the requester.
    let addr_bytes = ip_to_bytes(reply_dest.ip());
    out[payload_off + 8..payload_off + 24].copy_from_slice(&addr_bytes);
    // Overwrite the 2-byte response_port in the SEARCH's byte order.
    let port_bytes = match req.byte_order {
        ByteOrder::Big => reply_dest.port().to_be_bytes(),
        ByteOrder::Little => reply_dest.port().to_le_bytes(),
    };
    out[payload_off + 24..payload_off + 26].copy_from_slice(&port_bytes);
    Some(out)
}

/// Inbound source filter: drop UDP packets whose source IP is itself
/// a multicast group (forged — mcast is dest-only — replying would
/// amplify a DDoS) and any peer in the configured `ignore_addrs`
/// blocklist. Mirrors pvxs `udp_collector.cpp::handle_one` mcast-source
/// drop and `serverconn.cpp` ignoreAddrs check.
///
/// The multicast-source test is family-generic, matching pvxs
/// `SockAddr::isMCast()` which reports multicast for both `AF_INET` and
/// `AF_INET6` (`util.cpp:570-577`). `IpAddr::is_multicast()` dispatches to
/// the V4 or V6 predicate, so IPv6 multicast sources (e.g. `ff02::/16`)
/// are dropped by the same uniform rule rather than slipping past a
/// V4-only branch.
///
/// Returns `true` if the packet should be processed.
pub(crate) fn filter_inbound(peer: SocketAddr, ignore_addrs: &[(IpAddr, u16)]) -> bool {
    if peer.ip().is_multicast() {
        debug!("ignoring UDP with mcast source {peer}");
        return false;
    }
    let ignored = ignore_addrs
        .iter()
        .any(|(ip, port)| peer.ip() == *ip && (*port == 0 || peer.port() == *port));
    !ignored
}

/// One datagram the search decoder decided to emit, and the path it must
/// leave by. The decoder does not own a socket, so this is how it says
/// "send this" — see [`process_search_datagram`].
///
/// Two variants because the two outputs genuinely leave by different
/// sockets, not because one field means two things: a SEARCH_RESPONSE goes
/// out on the per-NIC bundle (and may be pinned to a NIC), while an
/// ORIGIN_TAG re-broadcast goes out on the loopback multicast socket (where
/// a NIC hint would be meaningless).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SearchOutput {
    /// SEARCH_RESPONSE for the per-NIC bundle.
    Reply {
        dest: SocketAddr,
        /// NIC to send from. `None` means "let the OS route" — the
        /// `reply_iface_ip = UNSPECIFIED` case, which arrives on
        /// FromOriginTag packets whose peeled origDest was the all-zeros
        /// `isAny()` and so carries no useful NIC pin.
        iface_hint: Option<Ipv4Addr>,
        bytes: Vec<u8>,
    },
    /// ORIGIN_TAG-prefixed re-broadcast for the loopback multicast socket.
    OriginTagForward { dest: SocketAddr, bytes: Vec<u8> },
}

/// Process one fully-received UDP datagram: drain it for chained PVA
/// messages (pvxs `udp_collector.cpp::process_one` L329) and reply to
/// each SEARCH that matches a hosted PV. Replies route via the NIC
/// matched by `reply_iface_ip` (with OS fallback), or to the SEARCH
/// payload's announced reply addr when present.
///
/// **I/O-free.** It decides *what* to send and returns it; the caller owns
/// the sockets and performs the sends ([`dispatch_search_outputs`] is the
/// async responder's tail). That split exists so the blocking RTEMS driver
/// — one `std::net::UdpSocket`, no `AsyncUdpV4`, no runtime — can reuse
/// this decode verbatim and produce byte-identical replies, the same shape
/// CA took for its blocking front-end (`epics-ca-rs` `ad477153`:
/// `parse_search_datagram` pure, `send_reply_dg` the tail). RTEMS phase 6
/// item 7 stage B, `doc/pva-rtems-item7-design.md` §3.3.
///
/// `origin_tag_forwarding` stands in for "a loopback multicast socket is
/// bound": the forward path is skipped when it is `false`, exactly as it
/// was skipped when `lo_mcast` was `None`.
///
/// `origin` controls forwarding-related semantics:
/// - [`Origin::Direct`]: no special handling; reply to the UDP source
///   when the SEARCH announced no specific addr.
/// - [`Origin::FromOriginTag`]: the SEARCH came in via the loopback
///   ORIGIN_TAG channel. Drop SEARCHes that announced
///   `server.isAny()` since they would route the reply back to the
///   forwarder, not the original requester (pvxs warning at
///   `udp_collector.cpp:367-371`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_search_datagram(
    source: &DynSource,
    origin_tag_forwarding: bool,
    udp_port: u16,
    frame: &[u8],
    udp_src: SocketAddr,
    reply_iface_ip: Ipv4Addr,
    origin: Origin,
    tcp_port: u16,
    guid: [u8; 12],
    protocol: &'static str,
) -> Vec<SearchOutput> {
    let mut out_dgs: Vec<SearchOutput> = Vec::new();
    // Forward path (pvxs `udp_collector.cpp:387-396`): a unicast
    // SEARCH addressed at one of our NIC unicast IPs (origin=Direct +
    // unicast flag) is wrapped in CMD_ORIGIN_TAG and re-broadcast to
    // 224.0.0.128:port so other local PVA peers can answer too. Only
    // taken when origin=Direct (anti-loop: we never re-forward an
    // ORIGIN_TAG-peeled packet) and a loopback mcast socket is bound.
    // After forwarding we return — our own lo_mcast loops the packet
    // back via IP_MULTICAST_LOOP=1, where it re-enters this function
    // as origin=FromOriginTag and is processed locally.
    if origin == Origin::Direct {
        if origin_tag_forwarding {
            // Forwarding rule deviation from pvxs: pvxs classifies
            // by *destination address* (`udp_collector.cpp:286-318`)
            // because it has IP_PKTINFO cmsg telling it the original
            // dest. We only have the receiving socket's bound iface
            // IP (which equals the dest IP for unicast packets to the
            // unicast bind) and use the SEARCH header's Unicast flag
            // as the trigger instead. They coincide for the common
            // case (pvxs-style senders set Unicast for unicast
            // SEARCHes). A sender that targets us unicast without
            // setting Unicast won't be re-forwarded under our rule;
            // the gap is acceptable since the sender already reached
            // us directly. The prefix carries `reply_iface_ip` which
            // by construction equals origDest in this branch.
            //
            // Resolve the concrete reply destination for the forwarded
            // message: the SEARCH-payload addr if specified, else
            // (UDP source IP, announced port). Fold into a SocketAddrV4
            // for the forward-frame rewriter.
            if let Some(req) = parse_search_request(frame) {
                if req.unicast {
                    // Resolve the reply destination the forwarded SEARCH
                    // carries, transport-independent at the payload level
                    // (pvxs `udp_collector.cpp:367-380`). An explicit reply
                    // address — v4 OR v6 — is preserved (`replyDest =
                    // server`, :378); only the wildcard sentinel falls back
                    // to the original UDP source (`replyDest = origSrc`,
                    // :372). The ORIGIN_TAG forward transport is IPv4, but
                    // the 16-byte reply-address field is family-independent,
                    // so a requester that asks for SEARCH_RESPONSE at its
                    // IPv6 endpoint keeps it rather than being downgraded to
                    // the v4 sender or wildcard.
                    let reply_dest = match req.reply_addr {
                        Some(ip) => SocketAddr::new(ip, req.reply_port),
                        None => SocketAddr::new(udp_src.ip(), req.reply_port),
                    };
                    if let Some(forward) = try_build_forward_frame(frame, reply_dest) {
                        let prefix = PvaCodec::build_origin_tag_prefix(reply_iface_ip);
                        let mut out = Vec::with_capacity(prefix.len() + forward.len());
                        out.extend_from_slice(&prefix);
                        out.extend_from_slice(&forward);
                        let dest =
                            SocketAddr::V4(SocketAddrV4::new(ORIGIN_TAG_MCAST_GROUP, udp_port));
                        out_dgs.push(SearchOutput::OriginTagForward { dest, bytes: out });
                        return out_dgs;
                    }
                }
            }
        }
    }

    let mut pos = 0usize;
    while pos + PvaHeader::SIZE <= frame.len() {
        let chunk = &frame[pos..];
        let consumed = match parse_search_request(chunk) {
            Some(req) => {
                let consumed = req.consumed;
                // Resolve reply destination per pvxs
                // `udp_collector.cpp:351-371`: prefer the SEARCH
                // payload's announced (addr, port); fall back to the
                // UDP source when the address was the unspecified
                // sentinel; reject the SEARCH outright when the
                // sentinel arrives via ORIGIN_TAG (the forwarder is
                // not the original requester).
                let reply_dest = match req.reply_addr {
                    Some(ip) => SocketAddr::new(ip, req.reply_port),
                    None => {
                        // Any forwarded origin (tagged ORIGIN_TAG or
                        // untagged local forward) cannot reply to the
                        // sender: the UDP source is the forwarder, not the
                        // original requester. pvxs drops these
                        // (`udp_collector.cpp:367-371`,
                        // "Forwarded SEARCH with reply to sender never
                        // works"). Only `Origin::Direct` keeps the UDP
                        // source as the reply target.
                        if origin != Origin::Direct {
                            debug!(
                                "forwarded SEARCH announced isAny() reply addr; dropping per pvxs"
                            );
                            // Abandons the rest of the datagram, as the
                            // pre-refactor `return` did; replies already
                            // decided for earlier chained messages still go
                            // out, as they were already sent before.
                            return out_dgs;
                        }
                        // Direct origin: keep the UDP source's IP
                        // but use the announced reply port (pvxs
                        // `replyDest.setPort(port)`).
                        SocketAddr::new(udp_src.ip(), req.reply_port)
                    }
                };

                // Protocol-gated match set, shared with the v6 UDP and
                // TCP-circuit responders. See `search_matched_cids`.
                // The requester endpoint is the resolved reply
                // destination (pvxs fills Search::source from
                // msg.replyDest, server.cpp:674-704).
                let matched_cids = search_matched_cids(source, &req, protocol, reply_dest).await;
                // pvxs `server.cpp:730-732`: when `nreply==0` AND the
                // SEARCH header did not set `MustReply`, drop the
                // SEARCH silently. Honouring `MustReply` even with
                // `nreply==0` is what lets `pvlist` build its server
                // list — without this, our server stays invisible to
                // discovery probes.
                if !matched_cids.is_empty() || req.must_reply {
                    let resp = build_search_response_proto(
                        guid,
                        req.seq,
                        tcp_port,
                        &matched_cids,
                        req.byte_order,
                        protocol,
                    );
                    // `reply_iface_ip = UNSPECIFIED` is the sentinel
                    // we set on FromOriginTag packets whose peeled
                    // origDest was the all-zeros isAny() — there's no
                    // useful NIC pin so go straight to OS routing
                    // instead of paying the AddrNotAvailable round-trip.
                    // Resolved to `None` here so the field the caller reads
                    // means one thing: a NIC to send from.
                    let iface_hint = if reply_iface_ip.is_unspecified() {
                        None
                    } else {
                        Some(reply_iface_ip)
                    };
                    out_dgs.push(SearchOutput::Reply {
                        dest: reply_dest,
                        iface_hint,
                        bytes: resp,
                    });
                }
                consumed
            }
            None => match PvaHeader::decode(&mut Cursor::new(chunk)) {
                Ok(h) => PvaHeader::SIZE + h.payload_length as usize,
                Err(_) => break,
            },
        };
        if consumed == 0 {
            break;
        }
        pos = pos.saturating_add(consumed);
    }
    out_dgs
}

/// The CIDs in `req` whose names this server will answer, gated by the
/// advertised transport protocol — the match rule shared by the v4 UDP
/// and v6 UDP SEARCH responders (the TCP-circuit responder calls the
/// ungated [`matched_cids_for_requester`] directly, mirroring pvxs
/// `serverchan.cpp` which does not re-gate on an established circuit).
///
/// pvxs `udp_collector.cpp:411-441` starts `protoTCP=false`, sets it true
/// only when a parsed protocol string equals "tcp", and queues a SEARCH's
/// PV names exclusively `if(protoTCP && …)`. A request advertising a
/// protocol the server does not speak — including an *empty* protocol list
/// (`nproto==0`, which never sets `protoTCP`) — therefore matches nothing.
/// This is what stops a TLS-only client from getting `found=1` off a
/// tcp-only server, and what makes a zero-protocol UDP SEARCH produce no
/// claims (only a `MustReply` header then forces a `found=0` reply, pvxs
/// `server.cpp:715-716`). `searchable` (not `has_pv`) keeps non-advertised
/// built-in sources (e.g. the `server` PV) out of broadcast SEARCH
/// answers; a direct TCP connect still resolves them.
///
/// Returns an empty vec on protocol mismatch, so each responder's
/// `found`/`MustReply` decision is identical regardless of family.
///
/// `requester` is the SEARCH's resolved reply destination (UDP) or the
/// established peer (TCP circuit); it is forwarded to
/// [`ChannelSource::searchable_from`] so a source can scope
/// advertisement by client endpoint (pvxs `Search::source()`).
pub(crate) async fn search_matched_cids(
    source: &DynSource,
    req: &SearchRequest,
    protocol: &str,
    requester: SocketAddr,
) -> Vec<u32> {
    // Byte-exact protocol gate (pvxs udp_collector.cpp:414-418 compares
    // the raw wire string to "tcp"). pvxs `protoTCP` starts false and is
    // only raised by a matching entry, so an *empty* list (`nproto==0`)
    // matches nothing — it is NOT a wildcard. A non-empty list with no
    // entry equal to our protocol likewise does not match, including a
    // present-but-undecodable (e.g. invalid-UTF-8) protocol, which must
    // not collapse to a wildcard.
    let protocol_ok = req
        .protocols
        .iter()
        .any(|p| p.as_slice() == protocol.as_bytes());
    if !protocol_ok {
        return Vec::new();
    }
    matched_cids_for_requester(source, req, requester).await
}

/// How a SEARCH packet reached us. Mirrors pvxs `udp_collector.cpp`'s
/// `origin_t` (Broadcast / Forwarding / Forwarded / OriginTag), but we
/// only distinguish the cases that change processing rules:
///
/// - [`Origin::Direct`]: arrived on a per-NIC socket. Reply via the
///   same NIC the packet came in on. Treat unicast-flagged SEARCHes
///   as candidates for re-forwarding (sub-phase d).
/// - [`Origin::FromOriginTag`]: arrived on the loopback mcast socket
///   wrapped in CMD_ORIGIN_TAG. Reply via the NIC matching the peeled
///   destination. Do NOT re-forward (anti-loop) and reject SEARCHes
///   with `server.isAny()` per pvxs `udp_collector.cpp:367-371`
///   ("Forwarded SEARCH with reply to sender never works").
/// - [`Origin::Forwarded`]: arrived on the loopback mcast socket
///   WITHOUT a CMD_ORIGIN_TAG prefix. pvxs tolerates these because some
///   PVA implementations forward unicast SEARCHes to `224.0.0.128`
///   without adding the tag (`udp_collector.cpp:401-404`), and still
///   parses/matches the SEARCH (`udp_collector.cpp:385-407`). Same
///   processing rules as `FromOriginTag`: do NOT re-forward (anti-loop)
///   and reject `server.isAny()` reply addresses. There is no peeled
///   destination, so the reply NIC is left to OS routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Direct,
    FromOriginTag,
    Forwarded,
}
