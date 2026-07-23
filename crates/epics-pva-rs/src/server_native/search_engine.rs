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

use std::io::{self, Cursor};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};

use epics_base_rs::net::ORIGIN_TAG_MCAST_GROUP;
use tracing::debug;

use crate::codec::PvaCodec;
use crate::proto::{ByteOrder, PvaHeader, ip_to_bytes};

use super::search::{
    SearchRequest, build_search_response_proto, matched_cids_for_requester, parse_search_request,
};
use super::source::DynSource;
/// Generate a 12-byte server GUID from the platform's entropy source.
///
/// pvxs `Server::Pvt::randomGUID` parity (commit ca594f40 "server: randomize
/// UUID"): two servers started at the same nanosecond on different machines
/// must get distinct GUIDs, and a client must not be able to predict one from
/// start time + PID.
///
/// # There is no fallback, on purpose
///
/// This used to derive the GUID from `SystemTime::now()` + `process::id()`
/// when the entropy source was unavailable. That is worse than an error,
/// because **every consumer of a GUID collision degrades silently**:
///
/// * pvxs `client.cpp:938-943` logs "Duplicate PV name" only when the two
///   GUIDs *differ*, so two servers sharing one GUID and one PV name make the
///   warning disappear — exactly the case an operator most needs told.
/// * pvxs `client.cpp:807-824` treats a GUID change as a server restart. A
///   board that reboots into the same GUID is not seen to have restarted, so
///   no `Discovered::Timeout` is emitted and clients keep stale state.
/// * pvxs `client.cpp:454-460` + `:880-886` (`ignoreServerGUIDs`) is gateway
///   loop-avoidance. Two servers sharing a GUID means ignoring one silently
///   blackholes the other.
///
/// None of the three fails an acceptance test, a `pvget`, or a connection —
/// a fully green ladder passes with a colliding GUID live. So the failure has
/// to be raised at the only moment it is still visible: server construction.
///
/// The fallback was also *specifically* dangerous on RTEMS, where both of its
/// inputs are near-constant: EPICS base sets a fixed boot wall-clock
/// (`rtems_init.c:958-966`) and there is one process. Two boards, or one board
/// twice, could serve identical GUIDs.
pub fn random_guid() -> io::Result<[u8; 12]> {
    let mut buf = [0u8; 12];
    fill_entropy(&mut buf)?;
    Ok(buf)
}

/// RTEMS: `getentropy`, declared for this target by `libc`
/// (`libc-0.2.186/src/unix/newlib/rtems/mod.rs:139`).
///
/// A dedicated arm rather than the Unix one below, because `target_family =
/// "unix"` holds for RTEMS and would otherwise select a `/dev/urandom` open
/// that a BSP need not provide — the silent selection of a Linux-shaped path
/// that made this defect possible.
///
/// `getentropy` reports failure through `errno`, which is why it is used
/// rather than the `arc4random_buf` beside it in the same header: that one
/// returns `void` and so cannot tell us it had nothing to draw on, which is
/// the same silence being removed here.
#[cfg(target_os = "rtems")]
fn fill_entropy(buf: &mut [u8; 12]) -> io::Result<()> {
    // SAFETY: `buf` is a valid, exclusively-borrowed 12-byte region and the
    // length passed is its own; 12 is far below `getentropy`'s 256-byte cap,
    // so a short read is not representable.
    let rc = unsafe { libc::getentropy(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Every other Unix: `/dev/urandom`.
#[cfg(all(unix, not(target_os = "rtems")))]
fn fill_entropy(buf: &mut [u8; 12]) -> io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(buf))
}

/// Windows: `ProcessPrng` — the per-process user-mode PRNG, the same call
/// `getrandom` makes on this platform, and the one Microsoft documents as the
/// replacement for `RtlGenRandom`. It draws on the kernel pool and needs no
/// algorithm handle to open or close.
#[cfg(windows)]
fn fill_entropy(buf: &mut [u8; 12]) -> io::Result<()> {
    // SAFETY: `buf` is a valid, exclusively-borrowed 12-byte region and the
    // length passed is its own.
    let ok = unsafe {
        windows_sys::Win32::Security::Cryptography::ProcessPrng(buf.as_mut_ptr(), buf.len())
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Anything else: refuse rather than invent. Reaching this arm is a porting
/// task, and it must read as one at the call site instead of as a server that
/// started and quietly advertised a guessable identity.
#[cfg(not(any(unix, windows)))]
fn fill_entropy(_buf: &mut [u8; 12]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no entropy source is wired for this target; a PVA server cannot be given a GUID",
    ))
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
///
/// # Why two of the three are `cfg`-gated
///
/// Both forwarded origins exist only where a **loopback multicast socket**
/// does, because that socket is the only thing that can deliver one:
/// [`super::udp`] joins `224.0.0.128` on loopback and classifies what arrives
/// there ([`super::udp::classify_loopback_datagram`]). That module is
/// `#[cfg(not(target_os = "rtems"))]`, and the driver that replaces it on the
/// target — [`super::blocking`]'s UDP responder — serves one plain
/// `std::net::UdpSocket`, passes `origin_tag_forwarding = false`, and can
/// therefore only ever produce [`Origin::Direct`].
///
/// So on the target these are not merely unused, they are unreachable, and
/// carrying them there made the RTEMS gate print `variants FromOriginTag and
/// Forwarded are never constructed` on every run. The fix is neither
/// `#[allow(dead_code)]` nor deletion: pvxs upstream does distinguish these
/// (`udp_collector.cpp:63-68` `origin_t`, acted on at `:373-374`, `:385-389`,
/// `:508`, `:524`), so the hosted server needs them for parity. Gating instead
/// states the target's actual property in the type — "this server has exactly
/// one search origin" — the same shape `epics-ca-rs` took with
/// `SearchTransport::NameServersOnly` ("no UDP socket is a fact about the
/// type, not a runtime branch", `doc/calink-rtems-design.md` §10.1).
///
/// A target build that ever grows a loopback multicast socket must bring these
/// back with it; the `cfg` is what makes that a compile error rather than a
/// silently missing anti-loop rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Direct,
    #[cfg(not(target_os = "rtems"))]
    FromOriginTag,
    #[cfg(not(target_os = "rtems"))]
    Forwarded,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Production scope of a source file: everything before the first
    /// column-0 `#[cfg(test)]`. Same helper the driver modules use.
    fn production_scope(src: &str) -> &str {
        match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        }
    }

    /// The GUID is drawn, not defaulted. `[0u8; 12]` is what
    /// `PvaServerConfig::default()` carries, so a `random_guid` that silently
    /// produced it would look like a server that simply had not been stamped.
    #[test]
    fn a_drawn_guid_is_not_the_default_zeros() {
        let guid = random_guid().expect("this host has an entropy source");
        assert_ne!(guid, [0u8; 12], "a drawn GUID must not be the zero default");
        assert!(
            guid.iter().any(|&b| b != guid[0]),
            "12 identical bytes is not a draw: {guid:?}"
        );
    }

    /// Two draws in one process differ — and that is the assertion the old
    /// time+PID fallback would have failed on RTEMS specifically.
    ///
    /// The fallback packed `SystemTime::now()` nanos into bytes 0..8 and the
    /// PID into 8..12. One process has one PID, so two draws differed *only*
    /// by the clock; under EPICS base's fixed RTEMS boot wall-clock
    /// (`rtems_init.c:958-966`) plus a coarse tick, two draws in the same tick
    /// were byte-identical. Drawing many times in a tight loop is what makes
    /// that mechanism visible on any host: a clock-derived GUID collides here,
    /// an entropy-derived one does not.
    #[test]
    fn draws_in_one_process_do_not_collide() {
        const DRAWS: usize = 256;
        let mut seen = HashSet::new();
        for _ in 0..DRAWS {
            let guid = random_guid().expect("this host has an entropy source");
            assert!(
                seen.insert(guid),
                "two GUIDs collided within one process: {guid:?}"
            );
        }
        assert_eq!(seen.len(), DRAWS);
    }

    /// **Proven by inspection, pinned by source guard.** The RTEMS arm cannot
    /// be executed on this host, so what is testable here is that it *exists*
    /// and that it is selected by the target rather than by the family.
    ///
    /// That distinction is the whole defect: `target_family = "unix"` holds
    /// for RTEMS, so a bare `#[cfg(unix)]` arm silently routed RTEMS into a
    /// `/dev/urandom` open a BSP need not provide. A future "simplification"
    /// that merges the two arms back together must fail here rather than on a
    /// board.
    #[test]
    fn rtems_selects_entropy_by_target_not_by_family() {
        let prod = production_scope(include_str!("search_engine.rs"));
        assert_eq!(
            prod.matches(r#"#[cfg(target_os = "rtems")]"#).count(),
            1,
            "RTEMS must have an entropy arm of its own"
        );
        assert!(
            prod.contains("libc::getentropy("),
            "the RTEMS arm must call getentropy, which reports failure, \
             and not arc4random_buf, which returns void"
        );
        assert_eq!(
            prod.matches(r#"#[cfg(all(unix, not(target_os = "rtems")))]"#)
                .count(),
            1,
            "the /dev/urandom arm must exclude RTEMS explicitly"
        );
        assert_eq!(
            prod.matches(r#"#[cfg(unix)]"#).count(),
            0,
            "a bare cfg(unix) arm would capture RTEMS again — that is the defect"
        );
    }

    /// **Proven by inspection, pinned by source guard.** The two forwarded
    /// origins are `cfg`-gated off the RTEMS target, and this pins the fact
    /// that makes that sound: the driver that serves UDP there constructs
    /// only [`Origin::Direct`].
    ///
    /// The gate is not cosmetic. `Forwarded` and `FromOriginTag` carry pvxs's
    /// anti-loop rule and its refusal of an `isAny()` reply address
    /// (`udp_collector.cpp:367-371`), and both are reachable only through the
    /// loopback multicast socket `super::udp` binds — a module the target does
    /// not compile. `super::blocking`'s responder has one plain
    /// `std::net::UdpSocket` and passes `origin_tag_forwarding = false`, so an
    /// origin-tagged datagram cannot reach it.
    ///
    /// What would break silently without this guard: a future blocking
    /// responder that starts peeling CMD_ORIGIN_TAG, or joins the multicast
    /// group, while the variants encoding "do not re-forward this" remain
    /// `cfg`-ed away on that target. It fails here, on a host, rather than as
    /// a forwarding loop on a board.
    #[test]
    fn the_blocking_udp_responder_produces_only_a_direct_origin() {
        let prod = production_scope(include_str!("blocking.rs"));
        assert_eq!(
            prod.matches("Origin::Direct").count(),
            1,
            "the blocking responder passes exactly one origin, and it is Direct"
        );
        for gated in ["Origin::FromOriginTag", "Origin::Forwarded"] {
            assert_eq!(
                prod.matches(gated).count(),
                0,
                "`{gated}` is `#[cfg(not(target_os = \"rtems\"))]`; naming it \
                 from the target's own UDP responder cannot compile there, and \
                 doing so means the loopback-multicast path arrived without \
                 the anti-loop rules that go with it"
            );
        }
        // The other half of the same claim, from the other side: the responder
        // must keep declining to forward. `origin_tag_forwarding` is what
        // stands in for "a loopback multicast socket is bound".
        assert!(
            prod.contains("Origin::Direct,\n            tcp_port,"),
            "the origin argument must still sit where process_search_datagram \
             takes it; this guard reads position, so a reordered call must \
             re-state the claim rather than pass by accident"
        );
    }

    /// No arm may substitute a derived value for entropy it could not get.
    /// The removed fallback built the GUID from `SystemTime::now()` and
    /// `process::id()`; neither may reappear anywhere in this module, on any
    /// platform, because a near-deterministic GUID fails silently at every
    /// consumer (see [`random_guid`]) while an error does not.
    #[test]
    fn no_arm_derives_a_guid_instead_of_drawing_one() {
        // Comment lines are stripped, because the doc on `random_guid` names
        // the removed fallback in order to explain why it is gone — the guard
        // is about what the code does, not about what it documents.
        let prod: String = production_scope(include_str!("search_engine.rs"))
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in ["SystemTime::now", "process::id", "UNIX_EPOCH"] {
            assert_eq!(
                prod.matches(banned).count(),
                0,
                "`{banned}` is a GUID fallback returning: entropy failure must \
                 propagate as an error, not become a guessable identity"
            );
        }
    }
}
