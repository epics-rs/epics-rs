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
//! `super::udp`, which is gated out of RTEMS (`mod.rs`) because of the
//! `tokio::net` / `socket2` stack around it. A blocking RTEMS
//! driver could therefore not reach the decode even though the decode itself
//! needs no reactor. Lifting it here — un-gated, exactly as the SEARCH
//! protocol was lifted into [`super::search`] (`4da1e04a`) and the accept loop
//! into `super::accept` (`4c75e766`) — is a move, not a copy: a copy would
//! re-open the whole family of SEARCH-parity bugs on a second code path.
//! `super::udp` re-exports every name below, so its own call sites and tests
//! are unchanged and keep exercising this code.
//!
//! Nothing here names a socket type, a runtime, or an address enumerator.

use std::io::{self, Cursor};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};

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
/// * pvxs `src/client.cpp:938-943` logs "Duplicate PV name" only when the two
///   GUIDs *differ*, so two servers sharing one GUID and one PV name make the
///   warning disappear — exactly the case an operator most needs told.
/// * pvxs `src/client.cpp:807-824` treats a GUID change as a server restart. A
///   board that reboots into the same GUID is not seen to have restarted, so
///   no `Discovered::Timeout` is emitted and clients keep stale state.
/// * pvxs `src/client.cpp:454-462` + `:880-886` (`ignoreServerGUIDs`) is gateway
///   loop-avoidance. Two servers sharing a GUID means ignoring one silently
///   blackholes the other.
///
/// None of the three fails an acceptance test, a `pvget`, or a connection —
/// a fully green ladder passes with a colliding GUID live. So the failure has
/// to be raised at the only moment it is still visible: server construction.
///
/// The fallback was also *specifically* dangerous on RTEMS, where both of its
/// inputs are near-constant: EPICS base sets a fixed boot wall-clock
/// (`posix/rtems_init.c:958-966`) and there is one process. Two boards, or one board
/// twice, could serve identical GUIDs.
pub fn random_guid() -> io::Result<[u8; 12]> {
    let mut buf = [0u8; 12];
    fill_entropy(&mut buf)?;
    // pvxs XORs the host into the second word (`server.cpp:516-522`) beside
    // time and pid, which stay out (see the guard test below). The host word
    // reads `osiLocalAddr`, which is where softIocPVX on a loopback-only host
    // prints its fallback line.
    let host = epics_base_rs::net::iface_v4::broadcast_addrs()
        .into_iter()
        .fold(
            u32::from(epics_base_rs::net::iface_v4::osi_local_addr()),
            |h, b| h ^ u32::from(b),
        );
    let word = u32::from_ne_bytes(buf[4..8].try_into().unwrap()) ^ host;
    buf[4..8].copy_from_slice(&word.to_ne_bytes());
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

/// VxWorks: `randABytes`, bound by `libc` for this target even though
/// neither `getentropy` nor `arc4random_buf` is (measured: zero occurrences
/// of either symbol anywhere in the VxWorks SDK sysroot or kernel).
///
/// A dedicated arm rather than falling into the Unix one below, for the same
/// reason RTEMS has its own: `target_family = "unix"` holds for VxWorks too
/// and `/dev/urandom` is not something this target provides.
///
/// `randABytes`'s errno behavior on failure has not been measured on this
/// target, so a nonzero return reports a bare "entropy draw failed"
/// `io::Error` rather than `io::Error::last_os_error()`, which would assert
/// an errno value nobody has confirmed this call actually sets.
#[cfg(target_os = "vxworks")]
fn fill_entropy(buf: &mut [u8; 12]) -> io::Result<()> {
    // SAFETY: `buf` is a valid, exclusively-borrowed 12-byte region and the
    // length passed is its own.
    let rc = unsafe { libc::randABytes(buf.as_mut_ptr(), buf.len() as libc::c_int) };
    if rc != 0 {
        return Err(io::Error::other(
            "randABytes failed to draw entropy for the server GUID",
        ));
    }
    Ok(())
}

/// Every other Unix: `/dev/urandom`.
#[cfg(all(unix, not(any(target_os = "rtems", target_os = "vxworks"))))]
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

/// Build a forward-ready copy of `frame`, which opens with a SEARCH. Mirrors pvxs `udp_collector.cpp:387-396`:
/// the recipient of the forwarded message has no access to the
/// original UDP source, so the forwarder rewrites the SEARCH's reply
/// address field with the resolved `reply_dest` (a fully concrete
/// destination) AND clears the Unicast flag before sending. Returns
/// `None` when the frame doesn't open with a SEARCH. Whether to forward is the
/// caller's, by destination (`IfaceRoutes::classify`); the flag is only
/// cleared, as pvxs clears it (`udp_collector.cpp:390`).
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

/// The CMD_ORIGIN_TAG datagram that forwards `frame` to the loopback group,
/// when it opens with a SEARCH (pvxs `forwardM`,
/// `udp_collector.cpp:387-396,544-567`); `None` otherwise, so a datagram that
/// is already an ORIGIN_TAG forward is never forwarded again.
///
/// `orig_dest` is the tag's original destination, which a server's listener
/// predicate matches (`udp_collector.cpp:451,509-523`).
pub(crate) fn origin_tag_forward(
    frame: &[u8],
    udp_src: SocketAddr,
    orig_dest: Ipv4Addr,
) -> Option<Vec<u8>> {
    let req = parse_search_request(frame)?;
    // The forwarded SEARCH carries a concrete reply destination, since its
    // recipients never see the original UDP source (pvxs
    // `udp_collector.cpp:367-380`). An explicit reply address — v4 OR v6 —
    // is preserved (`replyDest = server`, :378); only the wildcard sentinel
    // falls back to the original UDP source (`replyDest = origSrc`, :372).
    // The 16-byte field is family-independent, so a requester that asks for
    // SEARCH_RESPONSE at its IPv6 endpoint keeps it over the v4 transport.
    let reply_dest = SocketAddr::new(req.reply_addr.unwrap_or(udp_src.ip()), req.reply_port);
    let forward = try_build_forward_frame(frame, reply_dest)?;
    let prefix = PvaCodec::build_origin_tag_prefix(orig_dest);
    let mut out = Vec::with_capacity(prefix.len() + forward.len());
    out.extend_from_slice(&prefix);
    out.extend_from_slice(&forward);
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

/// The source a [`SearchOutput::Reply`] leaves with: `iface_hint` when the
/// decode named one (`udp_collector.cpp:514-518` pins a tagged SEARCH's
/// reply to the tag's address). The one wildcard collector has no per-NIC
/// socket to pin with, so the source goes in an `IP_PKTINFO` control message
/// ([`send_from_v4`]).
///
/// Only Linux does this. Elsewhere the OS picks the source from the route to
/// `dest`, which differs from the hint only for a multi-homed host replying
/// to a peer that its routing table reaches through another NIC.
#[cfg(target_os = "linux")]
pub(crate) fn reply_pin(
    dest: SocketAddr,
    iface_hint: Option<Ipv4Addr>,
) -> Option<(SocketAddrV4, Ipv4Addr)> {
    match (iface_hint, dest) {
        (Some(src), SocketAddr::V4(dest)) => Some((dest, src)),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn send_from_v4<S: std::os::fd::AsFd>(
    socket: &S,
    bytes: &[u8],
    dest: SocketAddrV4,
    src: Ipv4Addr,
) -> io::Result<usize> {
    use std::mem::{size_of, zeroed};
    use std::os::fd::AsRawFd;

    // SAFETY: every pointer handed to `sendmsg` points at a local that
    // outlives the call, and `cmsg_space` is the `CMSG_SPACE` of the one
    // `in_pktinfo` this fills.
    unsafe {
        let mut addr: libc::sockaddr_in = zeroed();
        addr.sin_family = libc::AF_INET as libc::sa_family_t;
        addr.sin_port = dest.port().to_be();
        addr.sin_addr.s_addr = u32::from(*dest.ip()).to_be();
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut libc::c_void,
            iov_len: bytes.len(),
        };
        let cmsg_space = libc::CMSG_SPACE(size_of::<libc::in_pktinfo>() as u32) as usize;
        // u64 elements give the control buffer the alignment `cmsghdr` needs.
        let mut control = vec![0u64; cmsg_space.div_ceil(size_of::<u64>())];
        let mut msg: libc::msghdr = zeroed();
        msg.msg_name = &mut addr as *mut _ as *mut libc::c_void;
        msg.msg_namelen = size_of::<libc::sockaddr_in>() as libc::socklen_t;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_space as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::IPPROTO_IP;
        (*cmsg).cmsg_type = libc::IP_PKTINFO;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<libc::in_pktinfo>() as u32) as _;
        let info = libc::in_pktinfo {
            ipi_ifindex: 0,
            ipi_spec_dst: libc::in_addr {
                s_addr: u32::from(src).to_be(),
            },
            ipi_addr: libc::in_addr { s_addr: 0 },
        };
        std::ptr::write(libc::CMSG_DATA(cmsg) as *mut libc::in_pktinfo, info);
        let n = libc::sendmsg(socket.as_fd().as_raw_fd(), &msg, 0);
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
}

/// Process one fully-received UDP datagram: drain it for chained PVA
/// messages (pvxs `udp_collector.cpp::process_one` L329) and reply to
/// each SEARCH that matches a hosted PV. Replies route via the NIC
/// matched by `reply_iface_ip` (with OS fallback), or to the SEARCH
/// payload's announced reply addr when present.
///
/// **I/O-free.** It decides *what* to send and returns it; the caller owns
/// the sockets and performs the sends (`dispatch_search_outputs` is the
/// async responder's tail). That split exists so the blocking RTEMS driver
/// — one `std::net::UdpSocket`, no `AsyncUdpV4`, no runtime — can reuse
/// this decode verbatim and produce byte-identical replies, the same shape
/// CA took for its blocking front-end (`epics-ca-rs` `ad477153`:
/// `parse_search_datagram` pure, `send_reply_dg` the tail). RTEMS phase 6
/// item 7 stage B.
///
/// `forward` is pvxs `Forwarding`: the datagram was sent to an address of
/// the interface it arrived on (`IfaceRoutes::classify`) and the caller
/// can reach the ORIGIN_TAG group. Such a SEARCH is forwarded, flagged or
/// not, and answered only when the forward loops back
/// (`udp_collector.cpp:385-396`).
///
/// `origin` controls forwarding-related semantics:
/// - [`Origin::Direct`]: no special handling; reply to the UDP source
///   when the SEARCH announced no specific addr.
/// - `Origin::FromOriginTag`: the SEARCH came in via the loopback
///   ORIGIN_TAG channel. Drop SEARCHes that announced
///   `server.isAny()` since they would route the reply back to the
///   forwarder, not the original requester (pvxs warning at
///   `udp_collector.cpp:367-371`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_search_datagram(
    source: &DynSource,
    forward: bool,
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
    // After forwarding we return: the group loops the packet back, where it
    // re-enters as `FromOriginTag` and is answered (anti-loop: a peeled
    // packet is never forwarded again).
    if origin == Origin::Direct {
        if forward {
            if let Some(bytes) = origin_tag_forward(frame, udp_src, reply_iface_ip) {
                let dest = SocketAddr::V4(SocketAddrV4::new(ORIGIN_TAG_MCAST_GROUP, udp_port));
                out_dgs.push(SearchOutput::OriginTagForward { dest, bytes });
                return out_dgs;
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
                // msg.replyDest, src/server.cpp:674-704).
                let matched_cids = search_matched_cids(source, &req, protocol, reply_dest).await;
                // pvxs `src/server.cpp:715-717`: when `nreply==0` AND the
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
/// `src/server.cpp:715-717`). `searchable` (not `has_pv`) keeps non-advertised
/// built-in sources (e.g. the `server` PV) out of broadcast SEARCH
/// answers; a direct TCP connect still resolves them.
///
/// Returns an empty vec on protocol mismatch, so each responder's
/// `found`/`MustReply` decision is identical regardless of family.
///
/// `requester` is the SEARCH's resolved reply destination (UDP) or the
/// established peer (TCP circuit); it is forwarded to
/// [`ChannelSource::searchable_from`](crate::server_native::ChannelSource::searchable_from) so a source can scope
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
/// - `Origin::FromOriginTag`: arrived on the loopback mcast socket
///   wrapped in CMD_ORIGIN_TAG. Reply via the NIC matching the peeled
///   destination. Do NOT re-forward (anti-loop) and reject SEARCHes
///   with `server.isAny()` per pvxs `udp_collector.cpp:367-371`
///   ("Forwarded SEARCH with reply to sender never works").
/// - `Origin::Forwarded`: arrived on the loopback mcast socket
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
/// does, because that socket is the only thing that can deliver one. A host
/// build has it on both backends: `super::udp` joins `224.0.0.128` on
/// loopback with a socket of its own and classifies what arrives there, and
/// the blocking responder does the same on its one search socket
/// (`route_search_socket_datagram`). An embedded target has neither — its
/// responder serves one plain `std::net::UdpSocket`, passes
/// `origin_tag_forwarding = false`, and can only ever produce
/// [`Origin::Direct`] — because `socket2`, which sets the multicast options,
/// does not build there.
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
/// type, not a runtime branch").
///
/// A target build that ever grows a loopback multicast socket must bring these
/// back with it; the `cfg` is what makes that a compile error rather than a
/// silently missing anti-loop rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Direct,
    #[cfg(not(epics_embedded_target))]
    FromOriginTag,
    #[cfg(not(epics_embedded_target))]
    Forwarded,
}

/// Classification of a datagram arriving on the loopback `CMD_ORIGIN_TAG`
/// multicast socket, mirroring pvxs `udp_collector.cpp::process_one`,
/// which inspects the FIRST PVA header and routes by command *before*
/// any SEARCH matching:
///
/// - a malformed `CMD_ORIGIN_TAG` is dropped (`:501-506`);
/// - a valid tag whose origin is wildcard or a local interface address
///   is processed as `OriginTag` (`:508-525`);
/// - a tag carrying a non-local origin is warned and dropped (`:527-533`);
/// - a bare (unprefixed) `CMD_SEARCH` is tolerated and processed as
///   `Forwarded` (`:401-404`).
///
/// The prior
/// `try_peel_origin_tag(raw) == None` branch collapsed "malformed tag"
/// and "bare SEARCH" into a single `Origin::Forwarded` path. Because
/// `process_search_datagram` then drains the datagram header-by-header, a
/// `[malformed ORIGIN_TAG][valid SEARCH]` packet skipped the bad tag and
/// still reached the SEARCH matcher — pvxs stops at the malformed tag.
#[cfg(not(epics_embedded_target))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoopbackClass<'a> {
    /// First message is a valid `CMD_ORIGIN_TAG`; process `inner` as
    /// `Origin::FromOriginTag` with the reply pinned to `reply_iface_ip`
    /// (the `UNSPECIFIED` sentinel when the tag carried the wildcard
    /// origin, leaving the reply NIC to OS routing).
    OriginTag {
        reply_iface_ip: Ipv4Addr,
        inner: &'a [u8],
    },
    /// First message is a bare `CMD_SEARCH` with no tag; process the
    /// whole datagram as `Origin::Forwarded`.
    BareSearch,
    /// Malformed tag, non-local origin tag, or any non-SEARCH first
    /// header — dropped like pvxs.
    Drop,
}

/// Snapshot the host's IPv4 interface addresses for the loopback
/// origin-locality check (pvxs caches the same set as `ifinfo.byAddr`).
/// Taken once at responder startup like the beacon destinations — a NIC
/// added later needs a server restart to be recognised.
#[cfg(not(epics_embedded_target))]
pub(crate) fn local_v4_addrs() -> std::collections::HashSet<Ipv4Addr> {
    epics_base_rs::net::iface_v4::enumerate()
        .map(|ifaces| ifaces.into_iter().map(|i| i.ip).collect())
        .unwrap_or_default()
}

/// Classify a loopback `CMD_ORIGIN_TAG` socket datagram by its first PVA
/// header. See [`LoopbackClass`] for the pvxs parity rules this enforces.
#[cfg(not(epics_embedded_target))]
pub(crate) fn classify_loopback_datagram<'a>(
    raw: &'a [u8],
    local_v4: &std::collections::HashSet<Ipv4Addr>,
) -> LoopbackClass<'a> {
    // pvxs reads exactly one header before its command switch and drops
    // any control or segmented UDP datagram up front
    // (`udp_collector.cpp:332-340`).
    let Ok(header) = PvaHeader::decode(&mut Cursor::new(raw)) else {
        return LoopbackClass::Drop;
    };
    if header.flags.is_control() || !header.flags.unsegmented() {
        return LoopbackClass::Drop;
    }
    if header.command == crate::proto::Command::OriginTag.code() {
        match PvaCodec::try_peel_origin_tag(raw) {
            // Wildcard origin (0.0.0.0 / ::): valid forward, no per-NIC
            // info — accept and leave the reply NIC to OS routing.
            Some((None, inner)) => LoopbackClass::OriginTag {
                reply_iface_ip: Ipv4Addr::UNSPECIFIED,
                inner,
            },
            // Concrete origin: accept only when it names one of our own
            // interface addresses (pvxs `ifinfo.byAddr.find(originaddr)`).
            Some((Some(v4), inner)) if local_v4.contains(&v4) => LoopbackClass::OriginTag {
                reply_iface_ip: v4,
                inner,
            },
            // Concrete but non-local origin (spoofed/foreign tag): drop.
            Some((Some(_), _)) => LoopbackClass::Drop,
            // Malformed tag (too short, payload_length < 16, truncated):
            // drop — do NOT fall back to a bare forwarded SEARCH.
            None => LoopbackClass::Drop,
        }
    } else if header.command == crate::proto::Command::Search.code() {
        LoopbackClass::BareSearch
    } else {
        // Any other first header (BEACON, etc.) is not part of the
        // loopback SEARCH-forwarding contract — drop.
        LoopbackClass::Drop
    }
}

/// A datagram on the one search socket of a driver whose socket is also the
/// ORIGIN_TAG channel (pvxs `UDPCollector`): the frame to decode, the NIC the
/// reply is pinned to, and how it arrived. `None` is a datagram to drop.
///
/// `via_group` is whether `IP_PKTINFO` says the datagram arrived on loopback
/// addressed to the group, `None` without `IP_PKTINFO`. A first message of
/// `CMD_ORIGIN_TAG` is routed through [`classify_loopback_datagram`], and
/// honoured unless `IP_PKTINFO` says it did not come through the group
/// (`:278`, `:522`). Any other datagram through the group is pvxs `Forwarded`
/// (`:278-284`), from a peer that forwards without the tag; one that is not,
/// or is not known to be, is a direct datagram, whose first message the
/// decode itself sorts out.
#[cfg(not(epics_embedded_target))]
pub(crate) fn route_search_socket_datagram<'a>(
    raw: &'a [u8],
    local_v4: &std::collections::HashSet<Ipv4Addr>,
    via_group: Option<bool>,
) -> Option<(&'a [u8], Ipv4Addr, Origin)> {
    let tagged = PvaHeader::decode(&mut Cursor::new(raw))
        .is_ok_and(|h| h.command == crate::proto::Command::OriginTag.code());
    if !tagged {
        let origin = if via_group == Some(true) {
            Origin::Forwarded
        } else {
            Origin::Direct
        };
        return Some((raw, Ipv4Addr::UNSPECIFIED, origin));
    }
    if via_group == Some(false) {
        return None;
    }
    match classify_loopback_datagram(raw, local_v4) {
        LoopbackClass::OriginTag {
            reply_iface_ip,
            inner,
        } => Some((inner, reply_iface_ip, Origin::FromOriginTag)),
        LoopbackClass::BareSearch | LoopbackClass::Drop => None,
    }
}

/// pvxs's search listeners on the collector socket (`server.cpp:411-437`):
/// one per [`ServerInterfaces`] address, each listed address's broadcasts,
/// and each joined group. A wildcard entry, or none, is a listener that
/// takes everything.
#[cfg(not(epics_embedded_target))]
pub(crate) struct SearchListeners {
    any: bool,
    listen: std::collections::HashSet<Ipv4Addr>,
    routes: IfaceRoutes,
}

/// pvxs `ifinfo.byIndex` and its destination rule (`udp_collector.cpp:286-313`):
/// the one place that decides whether a datagram was sent to an address of
/// the interface it arrived on, which is what makes a SEARCH `Forwarding`.
/// The unicast flag plays no part in that, in pvxs or here.
#[cfg(not(epics_embedded_target))]
pub(crate) struct IfaceRoutes {
    by_index: std::collections::HashMap<u32, IfaceAddrs>,
}

/// Where [`IfaceRoutes::classify`] puts a datagram's destination.
#[cfg(not(epics_embedded_target))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DestClass {
    /// An address of the arrival interface, pvxs `Forwarding`.
    Unicast(Ipv4Addr),
    /// A broadcast or group, pvxs `Broadcast`: answered from this address.
    Broadcast(Ipv4Addr),
}

/// One interface of [`IfaceRoutes`], pvxs `IfaceMap::Iface`:
/// every address it carries, not one, since an aliased NIC or loopback has
/// several; and each broadcast with the address that owns it.
#[cfg(not(epics_embedded_target))]
#[derive(Default)]
struct IfaceAddrs {
    addrs: Vec<Ipv4Addr>,
    bcast: Vec<(Ipv4Addr, Ipv4Addr)>,
}

/// Where a datagram the collector socket received goes.
#[cfg(not(epics_embedded_target))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Arrival {
    /// The ORIGIN_TAG channel, or no `IP_PKTINFO` to tell: classify by the
    /// first header.
    OriginTag,
    /// Sent to this interface address, pvxs `Forwarding`: a SEARCH is
    /// forwarded to local peers, flagged or not.
    Unicast(Ipv4Addr),
    /// Sent to a broadcast or group, pvxs `Broadcast`: answered from this
    /// interface address, never forwarded.
    Broadcast(Ipv4Addr),
    /// No listener takes it.
    Drop,
}

#[cfg(not(epics_embedded_target))]
impl SearchListeners {
    pub(crate) fn new(server: &ServerInterfaces, ifaces: &[epics_base_rs::net::IfaceInfo]) -> Self {
        let interfaces = server.v4();
        let mut listen: std::collections::HashSet<Ipv4Addr> = interfaces.iter().copied().collect();
        // `server.cpp:433-437`: each listed interface's broadcasts.
        listen.extend(
            ifaces
                .iter()
                .filter(|i| interfaces.contains(&i.ip))
                .filter_map(|i| i.broadcast),
        );
        listen.extend(server.groups.iter().map(|&(group, _)| group));
        Self {
            any: server.any(),
            listen,
            routes: IfaceRoutes::new(ifaces),
        }
    }

    /// Whether a SEARCH with this `IP_PKTINFO` is pvxs `Forwarding`: sent to
    /// an address of the interface it arrived on. One through the group is
    /// `Forwarded` and never forwarded again; without `IP_PKTINFO` nothing
    /// says it was sent to this host.
    pub(crate) fn forwards(&self, pktinfo: Option<(Ipv4Addr, u32)>) -> bool {
        pktinfo.is_some_and(|(dest, ifindex)| {
            dest != ORIGIN_TAG_MCAST_GROUP
                && self.routes.classify(dest, ifindex) == DestClass::Unicast(dest)
        })
    }

    /// pvxs's listener predicate for a destination
    /// (`L->dest.addr.isAny() || L->dest.addr==origDest`,
    /// `udp_collector.cpp:451`): a forwarded datagram's is its tag's
    /// address, or the wildcard when it carries none.
    pub(crate) fn listens_for(&self, dest: Ipv4Addr) -> bool {
        self.any || self.listen.contains(&dest)
    }

    /// Route a datagram by the `(destination, interface index)` its
    /// `IP_PKTINFO` named; `None` where there is none.
    pub(crate) fn arrival(&self, pktinfo: Option<(Ipv4Addr, u32)>) -> Arrival {
        let Some((dest, ifindex)) = pktinfo else {
            return Arrival::OriginTag;
        };
        if dest == ORIGIN_TAG_MCAST_GROUP {
            return if self.routes.is_loopback(ifindex) {
                Arrival::OriginTag
            } else {
                Arrival::Drop
            };
        }
        if !self.listens_for(dest) {
            return Arrival::Drop;
        }
        match self.routes.classify(dest, ifindex) {
            DestClass::Unicast(ip) => Arrival::Unicast(ip),
            DestClass::Broadcast(ip) => Arrival::Broadcast(ip),
        }
    }

    /// A datagram on a driver's one search socket, routed by [`Self::arrival`]
    /// and [`route_search_socket_datagram`]: the frame to decode, the NIC the
    /// reply is pinned to, how it arrived, and whether it is forwarded
    /// ([`Self::forwards`]). `None` is a datagram to drop.
    ///
    /// Through the group, the tag's address — or the wildcard, untagged — is
    /// the destination a listener must match (`udp_collector.cpp:514-524`).
    /// Without `IP_PKTINFO` only a tag says a datagram was forwarded.
    pub(crate) fn route<'a>(
        &self,
        raw: &'a [u8],
        local_v4: &std::collections::HashSet<Ipv4Addr>,
        pktinfo: Option<(Ipv4Addr, u32)>,
    ) -> Option<(&'a [u8], Ipv4Addr, Origin, bool)> {
        // A direct datagram is answered, and a forward tagged, from the
        // address pvxs takes as `replySrc` (`:286-318`).
        let (via_group, reply_from) = match self.arrival(pktinfo) {
            Arrival::Drop => return None,
            Arrival::OriginTag => (true, None),
            Arrival::Unicast(ip) | Arrival::Broadcast(ip) => (false, Some(ip)),
        };
        let forward = self.forwards(pktinfo);
        let via_group = pktinfo.map(|_| via_group);
        let (frame, dest, origin) = route_search_socket_datagram(raw, local_v4, via_group)?;
        (origin == Origin::Direct || self.listens_for(dest)).then_some((
            frame,
            reply_from.unwrap_or(dest),
            origin,
            forward,
        ))
    }
}

#[cfg(not(epics_embedded_target))]
impl IfaceRoutes {
    /// Each interface's addresses in pvxs's `std::map` order
    /// ([`crate::config::iface::pvxs_v4_order`], `evhelper.h:340`), and a
    /// broadcast two addresses share owned by the later one, as
    /// `iface.bcast[bcast] = addr` leaves it (`evhelper.cpp:814`).
    pub(crate) fn new(ifaces: &[epics_base_rs::net::IfaceInfo]) -> Self {
        let mut ifaces: Vec<_> = ifaces.iter().filter(|i| i.index != 0).collect();
        ifaces.sort_by_key(|i| crate::config::iface::pvxs_v4_order(&i.ip));
        // Index 0 is "none reported", on an interface and on a datagram alike.
        let by_index = ifaces
            .into_iter()
            .fold(std::collections::HashMap::new(), |mut map, i| {
                let entry: &mut IfaceAddrs = map.entry(i.index).or_default();
                if !entry.addrs.contains(&i.ip) {
                    entry.addrs.push(i.ip);
                }
                if let Some(b) = i.broadcast {
                    entry.bcast.retain(|&(have, _)| have != b);
                    entry.bcast.push((b, i.ip));
                }
                map
            });
        Self { by_index }
    }

    fn is_loopback(&self, ifindex: u32) -> bool {
        self.by_index
            .get(&ifindex)
            .is_some_and(|i| i.addrs.iter().any(Ipv4Addr::is_loopback))
    }

    /// `dest` as it arrived on `ifindex` (0: not reported): its broadcast
    /// answers from the address owning it, its own address from itself, a
    /// group from its first address.
    pub(crate) fn classify(&self, dest: Ipv4Addr, ifindex: u32) -> DestClass {
        let Some(iface) = self.by_index.get(&ifindex) else {
            // An index the start-time map does not know (pvxs re-reads its
            // map and drops these, `:267-271`): any local address is unicast.
            return if self.by_index.values().any(|i| i.addrs.contains(&dest)) {
                DestClass::Unicast(dest)
            } else {
                DestClass::Broadcast(Ipv4Addr::UNSPECIFIED)
            };
        };
        if let Some(&(_, ip)) = iface.bcast.iter().find(|&&(b, _)| b == dest) {
            DestClass::Broadcast(ip)
        } else if iface.addrs.contains(&dest) {
            DestClass::Unicast(dest)
        } else {
            DestClass::Broadcast(
                iface
                    .addrs
                    .first()
                    .copied()
                    .unwrap_or(Ipv4Addr::UNSPECIFIED),
            )
        }
    }
}

/// The multicast memberships a UDP collector takes, server and client alike:
/// pvxs `addGroups` (`config.cpp:310-335`) resolved by
/// `SockEndpoint::resolve` (`:88-120`).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct McastJoins {
    /// `(group, interface address)`; `0.0.0.0` where pvxs joins on
    /// `INADDR_ANY`.
    pub(crate) v4: Vec<(Ipv4Addr, Ipv4Addr)>,
    /// `(group, interface index)`; 0 where pvxs's `index_of` finds none
    /// (`evhelper.cpp:852-858`).
    pub(crate) v6: Vec<(Ipv6Addr, u32)>,
}

impl McastJoins {
    /// The multicast entries of `listed` as given, then under `expand` those
    /// of `dests`, one per `all_external` interface when an entry names none
    /// — the server's interfaces and `auto_beacon` beacon destinations, or
    /// the client's interfaces and `autoAddrList` address list (`:517`,
    /// `:642`). An `@iface` joins on what `ifmap` resolves it to, the
    /// wildcard when nothing.
    pub(crate) fn new(
        listed: &[crate::config::Endpoint],
        dests: &[crate::config::Endpoint],
        expand: bool,
        ifmap: &crate::config::IfaceMap,
    ) -> Self {
        let mut out = Self::default();
        let external = ifmap.external();
        let mut push = |ep: &crate::config::Endpoint, wildcard: &[(Ipv4Addr, u32)]| match (
            ep.addr,
            ep.iface.is_some(),
        ) {
            (SocketAddr::V4(g), true) => out.v4.push((*g.ip(), ep.mcast_iface_v4(ifmap))),
            (SocketAddr::V4(g), false) => {
                out.v4.extend(wildcard.iter().map(|&(ip, _)| (*g.ip(), ip)))
            }
            (SocketAddr::V6(g), true) => out.v6.push((*g.ip(), ep.mcast_iface_v6(ifmap))),
            (SocketAddr::V6(g), false) => out
                .v6
                .extend(wildcard.iter().map(|&(_, index)| (*g.ip(), index))),
        };
        // A listed entry without `@iface` is one `INADDR_ANY` / index-0 join.
        let any = [(Ipv4Addr::UNSPECIFIED, 0)];
        for ep in listed.iter().filter(|ep| ep.addr.ip().is_multicast()) {
            push(ep, &any);
        }
        if expand {
            for ep in dests.iter().filter(|ep| ep.addr.ip().is_multicast()) {
                push(ep, &external);
            }
        }
        // pvxs keys collector memberships by value (`mcast_grps`).
        let mut seen = std::collections::HashSet::new();
        out.v4.retain(|g| seen.insert(*g));
        let mut seen = std::collections::HashSet::new();
        out.v6.retain(|g| seen.insert(*g));
        out
    }

    /// [`Self::new`] against this host's interfaces, for the client's
    /// collector.
    #[cfg(feature = "client")]
    pub(crate) fn resolve(
        listed: &[crate::config::Endpoint],
        dests: &[crate::config::Endpoint],
        expand: bool,
    ) -> Self {
        Self::new(listed, dests, expand, &crate::config::IfaceMap::current())
    }
}

/// The server's interface list after pvxs `server::Config::expand`
/// (`config.cpp:488-523`), the one source every bind, listener, group join
/// and beacon fan-out reads.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ServerInterfaces {
    /// The non-multicast entries: the listed ones, then one per multicast
    /// entry — its `@iface` address, else the wildcard of its family
    /// (`:497-509`). Empty is the unconfigured default, pvxs's `0.0.0.0`.
    pub(crate) addrs: Vec<SocketAddr>,
    /// `(group, interface)` to join: the listed multicast entries, then
    /// under `auto_beacon` the multicast beacon destinations (`addGroups`,
    /// `:310-335`). The interface is `0.0.0.0` where pvxs joins on
    /// `INADDR_ANY` (`SockEndpoint::resolve`, `:88-120`).
    pub(crate) groups: Vec<(Ipv4Addr, Ipv4Addr)>,
    /// The v6 groups the same way, joined on an interface index, 0 where
    /// pvxs's `index_of` finds none (`evhelper.cpp:852-858`).
    pub(crate) groups_v6: Vec<(Ipv6Addr, u32)>,
}

impl ServerInterfaces {
    /// [`Self::new`] against this host's interfaces.
    pub(crate) fn resolve(
        interfaces: &[crate::config::Endpoint],
        beacon_dests: &[crate::config::Endpoint],
        auto_beacon: bool,
    ) -> io::Result<Self> {
        Self::new(
            interfaces,
            beacon_dests,
            auto_beacon,
            &crate::config::IfaceMap::current(),
        )
    }

    /// A multicast entry's `@iface` adds `ifmap`'s `address_of` it, v4 or
    /// v6. One that owns no address fails the start as pvxs's does: its
    /// `AF_UNSPEC` address prints as `<>` into the expanded list, which the
    /// server then cannot parse back (`config.cpp:505`, `util.cpp:705`).
    pub(crate) fn new(
        interfaces: &[crate::config::Endpoint],
        beacon_dests: &[crate::config::Endpoint],
        auto_beacon: bool,
        ifmap: &crate::config::IfaceMap,
    ) -> io::Result<Self> {
        let McastJoins { v4, v6 } = McastJoins::new(interfaces, beacon_dests, auto_beacon, ifmap);
        let mut out = Self {
            groups: v4,
            groups_v6: v6,
            ..Self::default()
        };
        let mut derived = Vec::new();
        for ep in interfaces {
            if !ep.addr.ip().is_multicast() {
                out.addrs.push(ep.addr);
                continue;
            }
            let addr = match ep.iface.as_deref() {
                Some(name) => ifmap.address_of(name).ok_or_else(|| {
                    // libevent's `evutil_gai_strerror(EAI_NONAME)` text.
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Error resolving \"<>\" : nodename nor servname provided, or not known",
                    )
                })?,
                None => match ep.addr {
                    SocketAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
                    SocketAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
                },
            };
            derived.push(SocketAddr::new(addr, 0));
        }
        out.addrs.extend(derived);
        // pvxs `removeDups`: first appearance kept.
        let mut seen = std::collections::HashSet::new();
        out.addrs.retain(|a| seen.insert(*a));
        Ok(out)
    }

    /// The v4 entries, the wildcard included.
    pub(crate) fn v4(&self) -> Vec<Ipv4Addr> {
        self.addrs
            .iter()
            .filter_map(|a| match a.ip() {
                IpAddr::V4(v4) => Some(v4),
                IpAddr::V6(_) => None,
            })
            .collect()
    }

    /// Whether pvxs opens its v4 collector at all: some v4 listener, the
    /// unconfigured default's `0.0.0.0` or a v4 group
    /// (`UDPManager::onSearch` per entry, `server.cpp:411-437`).
    pub(crate) fn has_v4(&self) -> bool {
        self.addrs.is_empty() || !self.v4().is_empty() || !self.groups.is_empty()
    }

    /// pvxs `isAny` (`server.cpp:413-418`): the default or a v4 wildcard
    /// entry, so the v4 search listener takes every destination.
    #[cfg(not(epics_embedded_target))]
    pub(crate) fn any(&self) -> bool {
        self.addrs.is_empty() || self.v4().contains(&Ipv4Addr::UNSPECIFIED)
    }

    /// The v6 collector's listeners, `None` when no entry is v6 and pvxs
    /// opens none.
    pub(crate) fn v6(&self) -> Option<V6Listen> {
        let v6: Vec<Ipv6Addr> = self
            .addrs
            .iter()
            .filter_map(|a| match a.ip() {
                IpAddr::V6(v6) => Some(v6),
                IpAddr::V4(_) => None,
            })
            .collect();
        if v6.is_empty() && self.groups_v6.is_empty() {
            return None;
        }
        Some(V6Listen {
            any: v6.contains(&Ipv6Addr::UNSPECIFIED),
            dests: v6
                .into_iter()
                .chain(self.groups_v6.iter().map(|&(g, _)| g))
                .collect(),
        })
    }

    /// The interfaces whose broadcasts `auto_beacon` adds, pvxs
    /// `expandAddrList` over the expanded list (`config.cpp:284-308`): the
    /// default's `0.0.0.0`, the v4 entries, and on Linux `[::]` as
    /// `0.0.0.0`.
    pub(crate) fn bcast_ifaces(&self) -> Vec<Ipv4Addr> {
        if self.addrs.is_empty() {
            return vec![Ipv4Addr::UNSPECIFIED];
        }
        self.addrs
            .iter()
            .filter_map(|a| match a.ip() {
                IpAddr::V4(v4) => Some(v4),
                IpAddr::V6(v6) if cfg!(target_os = "linux") && v6.is_unspecified() => {
                    Some(Ipv4Addr::UNSPECIFIED)
                }
                IpAddr::V6(_) => None,
            })
            .collect()
    }
}

/// pvxs's listeners on its `[::]` collector, dual-stack where the OS
/// defaults `IPV6_V6ONLY` off: pvxs never sets it (`udp_collector.cpp:124-151`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct V6Listen {
    any: bool,
    dests: Vec<Ipv6Addr>,
}

impl V6Listen {
    /// Whether a listener takes a datagram from `src` to `dest`
    /// (`L->dest.addr.isAny() || L->dest.addr==origDest`,
    /// `udp_collector.cpp:451`). A v4 datagram the dual socket also receives
    /// is never answered: pvxs's reply goes from a v6 address of the
    /// arrival interface to the v4-mapped sender and the send fails
    /// (`:299-313`), as softIocPVX's does under `EPICS_PVAS_INTF_ADDR_LIST=[::]`.
    /// An embedded build has no destination address to route on.
    #[cfg(not(epics_embedded_target))]
    pub(crate) fn takes(&self, src: SocketAddr, dest: Option<IpAddr>) -> bool {
        let SocketAddr::V6(src) = src else {
            return false;
        };
        if src.ip().to_ipv4_mapped().is_some() {
            return false;
        }
        self.any
            || dest.is_some_and(|d| match d {
                IpAddr::V6(d) => self.dests.contains(&d),
                IpAddr::V4(_) => false,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IfaceMap;
    use source_guard::{Comments, production};
    use std::collections::HashSet;

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
    /// (`posix/rtems_init.c:958-966`) plus a coarse tick, two draws in the same tick
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

    /// **Proven by inspection, pinned by source guard.** Neither the RTEMS
    /// nor the VxWorks arm can be executed on this host, so what is testable
    /// here is that each *exists* and is selected by the target rather than
    /// by the family.
    ///
    /// That distinction is the whole defect: `target_family = "unix"` holds
    /// for both, so a bare `#[cfg(unix)]` arm would silently route either
    /// into a `/dev/urandom` open neither provides. A future
    /// "simplification" that merges the arms back together must fail here
    /// rather than on a board.
    #[test]
    fn rtems_selects_entropy_by_target_not_by_family() {
        let prod = production(include_str!("search_engine.rs"), Comments::Strip);
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
            prod.matches(r#"#[cfg(target_os = "vxworks")]"#).count(),
            1,
            "VxWorks must have an entropy arm of its own"
        );
        assert!(
            prod.contains("libc::randABytes("),
            "the VxWorks arm must call randABytes — neither getentropy nor \
             arc4random_buf exists in the VxWorks SDK sysroot or kernel"
        );
        assert_eq!(
            prod.matches(
                r#"#[cfg(all(unix, not(any(target_os = "rtems", target_os = "vxworks"))))]"#
            )
            .count(),
            1,
            "the /dev/urandom arm must exclude RTEMS and VxWorks explicitly"
        );
        assert_eq!(
            prod.matches(r#"#[cfg(unix)]"#).count(),
            0,
            "a bare cfg(unix) arm would capture RTEMS/VxWorks again — that is the defect"
        );
    }

    /// `FromOriginTag` is `cfg`-gated off every embedded target, and this pins
    /// the fact that makes that sound: the blocking responder reaches an
    /// origin other than [`Origin::Direct`] only through
    /// [`route_search_socket_datagram`], and only under
    /// `cfg(not(epics_embedded_target))`.
    ///
    /// `FromOriginTag` carries pvxs's anti-loop rule and its refusal of an
    /// `isAny()` reply address (`udp_collector.cpp:367-371`). On the target the
    /// responder has one plain `std::net::UdpSocket` that never joined the
    /// group and passes `origin_tag_forwarding = false`, so no tagged datagram
    /// reaches it. A call to the router that lost its gate would not compile
    /// there; a router call or a named variant that moved outside the gate by
    /// hand fails here, on a host, rather than as a forwarding loop on a board.
    #[test]
    fn the_blocking_udp_responder_reaches_a_tagged_origin_only_through_the_gated_router() {
        let prod = production(include_str!("blocking.rs"), Comments::Strip);
        for named in ["Origin::FromOriginTag", "Origin::Forwarded"] {
            assert_eq!(
                prod.matches(named).count(),
                0,
                "`{named}` belongs to the router in search_engine; the responder \
                 must not pick an origin itself"
            );
        }
        assert_eq!(
            prod.matches("listeners.route(").count(),
            1,
            "the responder routes through one call"
        );
        assert!(
            prod.contains("#[cfg(not(epics_embedded_target))]\n        let routed = match &role {"),
            "the router call must sit under cfg(not(epics_embedded_target))"
        );
        assert!(
            prod.contains(
                "#[cfg(epics_embedded_target)]\n        let routed = Some((&buf[..len], Ipv4Addr::UNSPECIFIED, Origin::Direct, false));"
            ),
            "the embedded arm must route everything Direct"
        );
    }

    /// No arm may substitute a derived value for entropy it could not get.
    /// The removed fallback built the GUID from `SystemTime::now()` and
    /// `process::id()`; neither may reappear anywhere in this module, on any
    /// platform, because a near-deterministic GUID fails silently at every
    /// consumer (see [`random_guid`]) while an error does not.
    #[test]
    fn no_arm_derives_a_guid_instead_of_drawing_one() {
        // `Strip`, because the doc on `random_guid` names the removed
        // fallback in order to explain why it is gone — the guard is about
        // what the code does, not about what it documents.
        let prod = production(include_str!("search_engine.rs"), Comments::Strip);
        for banned in ["SystemTime::now", "process::id", "UNIX_EPOCH"] {
            assert_eq!(
                prod.matches(banned).count(),
                0,
                "`{banned}` is a GUID fallback returning: entropy failure must \
                 propagate as an error, not become a guessable identity"
            );
        }
    }

    #[cfg(not(epics_embedded_target))]
    mod loopback_class_tests {
        use super::*;
        use crate::proto::Command;

        // `classify_loopback_datagram` must route a loopback datagram by its
        // first PVA header exactly like pvxs `udp_collector.cpp::process_one`
        // — a malformed/non-local CMD_ORIGIN_TAG is dropped, a valid tag is
        // peeled, and only a genuinely untagged CMD_SEARCH enters the
        // `Forwarded` path. The prior code collapsed every
        // `try_peel_origin_tag == None` reason (including a malformed tag) into
        // `Forwarded`, so a `[malformed ORIGIN_TAG][valid SEARCH]` packet
        // skipped the bad tag and still reached the SEARCH matcher.

        /// Build a `CMD_ORIGIN_TAG` header (little-endian application frame)
        /// declaring `payload_len` payload bytes. Used to forge malformed
        /// tags whose declared length differs from the bytes that follow.
        fn origin_tag_header(payload_len: u32) -> [u8; PvaHeader::SIZE] {
            PvaHeader::application(
                false,
                ByteOrder::Little,
                Command::OriginTag.code(),
                payload_len,
            )
            .encode()
        }

        fn valid_search_bytes() -> Vec<u8> {
            PvaCodec { big_endian: false }.build_search(7, 42, "MY:PV", [127, 0, 0, 1], 9999, false)
        }

        #[test]
        fn classify_bare_unprefixed_search_is_forwarded() {
            let local = HashSet::new();
            let search = valid_search_bytes();
            assert_eq!(
                classify_loopback_datagram(&search, &local),
                LoopbackClass::BareSearch,
                "an untagged CMD_SEARCH on the loopback socket must be Forwarded"
            );
        }

        #[test]
        fn classify_valid_wildcard_origin_tag_peels_to_unspecified() {
            let local = HashSet::new();
            let mut dg = PvaCodec::build_origin_tag_prefix(Ipv4Addr::UNSPECIFIED).to_vec();
            let search = valid_search_bytes();
            dg.extend_from_slice(&search);
            assert_eq!(
                classify_loopback_datagram(&dg, &local),
                LoopbackClass::OriginTag {
                    reply_iface_ip: Ipv4Addr::UNSPECIFIED,
                    inner: &search,
                },
                "wildcard-origin tag must peel with the UNSPECIFIED reply sentinel"
            );
        }

        #[test]
        fn classify_valid_local_origin_tag_peels_to_iface_ip() {
            let iface = Ipv4Addr::new(192, 168, 99, 10);
            let local: HashSet<Ipv4Addr> = [iface].into_iter().collect();
            let mut dg = PvaCodec::build_origin_tag_prefix(iface).to_vec();
            let search = valid_search_bytes();
            dg.extend_from_slice(&search);
            assert_eq!(
                classify_loopback_datagram(&dg, &local),
                LoopbackClass::OriginTag {
                    reply_iface_ip: iface,
                    inner: &search,
                },
                "a tag whose origin is one of our interfaces must peel and pin that NIC"
            );
        }

        #[test]
        fn classify_non_local_origin_tag_is_dropped() {
            // Origin is a concrete address that is NOT in our interface set.
            let local: HashSet<Ipv4Addr> = [Ipv4Addr::new(192, 168, 99, 10)].into_iter().collect();
            let foreign = Ipv4Addr::new(203, 0, 113, 7);
            let mut dg = PvaCodec::build_origin_tag_prefix(foreign).to_vec();
            dg.extend_from_slice(&valid_search_bytes());
            assert_eq!(
                classify_loopback_datagram(&dg, &local),
                LoopbackClass::Drop,
                "a tag whose origin is not a local interface must be dropped (pvxs warns + ignores)"
            );
        }

        #[test]
        fn classify_malformed_origin_tag_with_trailing_search_is_dropped() {
            // First header claims CMD_ORIGIN_TAG with payload_length=8 (< the
            // 16-byte minimum), then 8 junk bytes, then a fully valid SEARCH.
            // The old code took the `None => Forwarded` branch and the
            // header-by-header scanner then skipped the bad tag and answered
            // the SEARCH. pvxs stops at the malformed tag.
            let mut dg = origin_tag_header(8).to_vec();
            dg.extend_from_slice(&[0u8; 8]);
            dg.extend_from_slice(&valid_search_bytes());
            let local = HashSet::new();
            assert_eq!(
                classify_loopback_datagram(&dg, &local),
                LoopbackClass::Drop,
                "a malformed ORIGIN_TAG must drop the whole datagram, not expose the inner SEARCH"
            );
        }

        #[test]
        fn classify_truncated_origin_tag_is_dropped() {
            // Header declares a 16-byte origin payload but only 10 bytes
            // follow, so the datagram is shorter than the declared tag.
            let mut dg = origin_tag_header(16).to_vec();
            dg.extend_from_slice(&[0u8; 10]);
            let local = HashSet::new();
            assert_eq!(
                classify_loopback_datagram(&dg, &local),
                LoopbackClass::Drop,
                "a truncated ORIGIN_TAG must be dropped"
            );
        }

        #[test]
        fn classify_short_and_non_search_first_header_are_dropped() {
            let local = HashSet::new();
            // Too short to even decode a header.
            assert_eq!(
                classify_loopback_datagram(&[], &local),
                LoopbackClass::Drop,
                "empty datagram must be dropped"
            );
            assert_eq!(
                classify_loopback_datagram(&[0xCA, 0x02, 0x00], &local),
                LoopbackClass::Drop,
                "sub-header datagram must be dropped"
            );
            // A first header that is neither SEARCH nor ORIGIN_TAG (here
            // SEARCH_RESPONSE) is not part of the forwarding contract.
            let other =
                PvaHeader::application(true, ByteOrder::Little, Command::SearchResponse.code(), 0)
                    .encode();
            assert_eq!(
                classify_loopback_datagram(&other, &local),
                LoopbackClass::Drop,
                "a non-SEARCH, non-ORIGIN_TAG first header must be dropped"
            );
        }
    }

    /// A multicast entry whose `@iface` owns no address fails the start
    /// with pvxs's text, which `pvxsBaseRegistrar` prints.
    #[test]
    fn a_multicast_interface_on_an_addressless_iface_fails() {
        let intf = [crate::config::Endpoint::parse("239.3.3.6@nosuch", 0).unwrap()];
        let err = ServerInterfaces::new(
            &intf,
            &[],
            false,
            &IfaceMap::of_v4(&[(Ipv4Addr::new(10, 9, 0, 1), 2)]),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Error resolving \"<>\" : nodename nor servname provided, or not known"
        );
    }

    /// pvxs `Config::expand` (`config.cpp:497-509`) for
    /// `EPICS_PVAS_INTF_ADDR_LIST=224.0.9.17`: the group adds the wildcard,
    /// so every destination is listened for and joined on `INADDR_ANY`.
    #[cfg(not(epics_embedded_target))]
    #[test]
    fn a_multicast_interface_without_iface_adds_the_wildcard() {
        let group = Ipv4Addr::new(224, 0, 9, 17);
        let intf = [crate::config::Endpoint::parse("224.0.9.17", 0).unwrap()];
        let got = ServerInterfaces::new(
            &intf,
            &[],
            true,
            &IfaceMap::of_v4(&[(Ipv4Addr::new(10, 9, 0, 1), 2)]),
        )
        .unwrap();
        assert_eq!(
            got,
            ServerInterfaces {
                addrs: vec![SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))],
                groups: vec![(group, Ipv4Addr::UNSPECIFIED)],
                groups_v6: Vec::new(),
            }
        );
        assert!(got.any());
    }

    /// `127.0.0.1 224.0.9.17@<iface>`: the group adds its interface's
    /// address, whose broadcast is then listened for, and is joined there.
    #[cfg(not(epics_embedded_target))]
    #[test]
    fn a_multicast_interface_with_iface_adds_that_address() {
        let (group, nic) = (Ipv4Addr::new(224, 0, 9, 17), Ipv4Addr::new(10, 9, 0, 1));
        let intf = intf(&["127.0.0.1", "224.0.9.17@n0"]);
        let got = ServerInterfaces::new(&intf, &[], false, &IfaceMap::of_v4(&[(nic, 2)])).unwrap();
        assert_eq!(
            got,
            ServerInterfaces {
                addrs: vec![
                    SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                    SocketAddr::from((nic, 0)),
                ],
                groups: vec![(group, nic)],
                groups_v6: Vec::new(),
            }
        );
        assert!(!got.any());
        let bcast = Ipv4Addr::new(10, 9, 0, 255);
        let d0 = epics_base_rs::net::IfaceInfo {
            index: 2,
            name: "d0".into(),
            ip: nic,
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            broadcast: Some(bcast),
            up_non_loopback: true,
        };
        let listeners = SearchListeners::new(&got, &[d0]);
        for dest in [Ipv4Addr::LOCALHOST, nic, bcast, group] {
            assert!(listeners.listens_for(dest), "{dest}");
        }
        assert!(!listeners.listens_for(Ipv4Addr::new(10, 8, 0, 1)));
    }

    /// `addGroups` (`config.cpp:310-335`) runs only under `auto_beacon`, and
    /// a modifier-less beacon group is joined on every external NIC.
    #[cfg(not(epics_embedded_target))]
    #[test]
    fn beacon_groups_join_only_under_auto_beacon() {
        let nics = [
            (Ipv4Addr::new(10, 9, 0, 1), 2),
            (Ipv4Addr::new(10, 8, 0, 1), 3),
        ];
        let map = IfaceMap::of_v4(&nics);
        let dests = [crate::config::Endpoint::parse("224.0.7.7", 5076).unwrap()];
        let group = Ipv4Addr::new(224, 0, 7, 7);
        assert!(
            ServerInterfaces::new(&[], &dests, false, &map)
                .unwrap()
                .groups
                .is_empty()
        );
        let got = ServerInterfaces::new(&[], &dests, true, &map).unwrap();
        assert_eq!(got.groups, vec![(group, nics[0].0), (group, nics[1].0)]);
        assert!(got.addrs.is_empty() && got.any());
    }

    /// The client's joins, pvxs `Config::expand` (`config.cpp:640-643`):
    /// listed interface groups always, address-list groups only under
    /// `autoAddrList`, a modifier-less one on every external interface in
    /// both families, an `@iface` one there alone; unicast joins nothing.
    #[cfg(not(epics_embedded_target))]
    #[test]
    fn address_list_groups_join_only_under_auto_addr_list() {
        let nics = [
            (Ipv4Addr::LOCALHOST, 1),
            (Ipv4Addr::new(10, 9, 0, 1), 2),
            (Ipv4Addr::UNSPECIFIED, 3),
        ];
        let map = IfaceMap::of_v4(&nics);
        let addr_list = intf(&["224.0.2.3", "239.1.1.1@n0", "ff02::42", "10.0.0.7"]);
        let listed = intf(&["224.0.9.9", "10.9.0.1"]);
        let (g, e, l) = (
            Ipv4Addr::new(224, 0, 2, 3),
            Ipv4Addr::new(239, 1, 1, 1),
            Ipv4Addr::new(224, 0, 9, 9),
        );
        let v6: Ipv6Addr = "ff02::42".parse().unwrap();
        let off = McastJoins::new(&listed, &addr_list, false, &map);
        assert_eq!(
            off,
            McastJoins {
                v4: vec![(l, Ipv4Addr::UNSPECIFIED)],
                v6: Vec::new(),
            }
        );
        let on = McastJoins::new(&listed, &addr_list, true, &map);
        assert_eq!(
            on,
            McastJoins {
                v4: vec![
                    (l, Ipv4Addr::UNSPECIFIED),
                    (g, nics[0].0),
                    (g, nics[1].0),
                    (g, nics[2].0),
                    (e, Ipv4Addr::LOCALHOST),
                ],
                v6: vec![(v6, 1), (v6, 2), (v6, 3)],
            }
        );
    }

    fn intf(list: &[&str]) -> Vec<crate::config::Endpoint> {
        list.iter()
            .map(|t| crate::config::Endpoint::parse(t, 0).unwrap())
            .collect()
    }

    /// Which collectors each `EPICS_PVAS_INTF_ADDR_LIST` opens, and whose
    /// broadcasts `auto_beacon` adds, per pvxs `onSearch` per entry
    /// (`server.cpp:411-437`) and `expandAddrList` (`config.cpp:284-308`).
    #[cfg(not(epics_embedded_target))]
    #[test]
    fn each_interface_list_opens_the_families_pvxs_opens() {
        let any = Ipv4Addr::UNSPECIFIED;
        let linux_any: Vec<Ipv4Addr> = if cfg!(target_os = "linux") {
            vec![any]
        } else {
            Vec::new()
        };
        let v6 = |any: bool, dests: &[Ipv6Addr]| {
            Some(V6Listen {
                any,
                dests: dests.to_vec(),
            })
        };
        let cases: [(&[&str], bool, bool, Option<V6Listen>, Vec<Ipv4Addr>); 5] = [
            (&[], true, true, None, vec![any]),
            (&["127.0.0.1"], true, false, None, vec![Ipv4Addr::LOCALHOST]),
            (
                &["[::]"],
                false,
                false,
                v6(true, &[Ipv6Addr::UNSPECIFIED]),
                linux_any.clone(),
            ),
            (
                &["::1"],
                false,
                false,
                v6(false, &[Ipv6Addr::LOCALHOST]),
                Vec::new(),
            ),
            (
                &["0.0.0.0", "[::]"],
                true,
                true,
                v6(true, &[Ipv6Addr::UNSPECIFIED]),
                [vec![any], linux_any].concat(),
            ),
        ];
        for (list, has_v4, wildcard, listen, bcast) in cases {
            let got = ServerInterfaces::new(&intf(list), &[], true, &IfaceMap::default()).unwrap();
            assert_eq!(got.has_v4(), has_v4, "{list:?}");
            assert_eq!(got.has_v4() && got.any(), wildcard, "{list:?}");
            assert_eq!(got.v6(), listen, "{list:?}");
            assert_eq!(got.bcast_ifaces(), bcast, "{list:?}");
        }
    }

    /// A v6 group entry joins on its `@iface` index, else 0, and adds the
    /// iface's address or the v6 wildcard (`config.cpp:497-509`); a v6
    /// beacon group joins on every external NIC under `auto_beacon`
    /// (`addGroups`).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_v6_group_joins_on_its_interface_index() {
        let group: Ipv6Addr = "ff02::42:1".parse().unwrap();
        let host = IfaceMap::current();
        let got = ServerInterfaces::new(&intf(&["[ff02::42:1]@lo"]), &[], false, &host).unwrap();
        assert_eq!(got.addrs, vec![SocketAddr::from((Ipv4Addr::LOCALHOST, 0))]);
        assert_eq!(got.groups_v6, vec![(group, host.index_of("lo"))]);
        assert_ne!(host.index_of("lo"), 0);
        let got = ServerInterfaces::new(&intf(&["[ff02::42:1]"]), &[], false, &host).unwrap();
        assert_eq!(
            got.addrs,
            vec![SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))]
        );
        assert_eq!(got.groups_v6, vec![(group, 0)]);
        assert!(!got.has_v4());
        let dests = intf(&["[ff02::42:1]"]);
        let nics = [
            (Ipv4Addr::new(10, 9, 0, 1), 2),
            (Ipv4Addr::new(10, 8, 0, 1), 3),
        ];
        let got = ServerInterfaces::new(&[], &dests, true, &IfaceMap::of_v4(&nics)).unwrap();
        assert_eq!(got.groups_v6, vec![(group, 2), (group, 3)]);
        assert!(got.groups.is_empty() && got.has_v4());
    }

    /// The dual-stack collector's listener rule, per boundary: a v4 or
    /// v4-mapped sender is never taken; a wildcard takes any v6 one; a
    /// listed destination takes only its own.
    #[test]
    fn the_v6_listener_takes_only_its_v6_destinations() {
        let v6_src = SocketAddr::from((Ipv6Addr::LOCALHOST, 9));
        let mapped = SocketAddr::from((Ipv4Addr::LOCALHOST.to_ipv6_mapped(), 9));
        let v4_src = SocketAddr::from((Ipv4Addr::LOCALHOST, 9));
        let lo6 = Some(IpAddr::V6(Ipv6Addr::LOCALHOST));
        let wildcard = V6Listen {
            any: true,
            dests: vec![Ipv6Addr::UNSPECIFIED],
        };
        assert!(wildcard.takes(v6_src, None));
        assert!(!wildcard.takes(mapped, lo6));
        assert!(!wildcard.takes(v4_src, lo6));
        let listed = V6Listen {
            any: false,
            dests: vec![Ipv6Addr::LOCALHOST],
        };
        assert!(listed.takes(v6_src, lo6));
        assert!(!listed.takes(
            v6_src,
            Some(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)))
        ));
        assert!(!listed.takes(v6_src, None));
    }

    /// pvxs classifies against every address of the arrival interface
    /// (`udp_collector.cpp:286-313`). An aliased interface carries several; a
    /// map holding one of them read a SEARCH to another as broadcast, so it
    /// was never forwarded to the servers sharing the port.
    #[cfg(not(epics_embedded_target))]
    #[test]
    fn arrival_reads_every_address_of_the_arrival_interface() {
        let nic = |index, ip: [u8; 4], broadcast: Option<[u8; 4]>| epics_base_rs::net::IfaceInfo {
            index,
            name: format!("if{index}"),
            ip: ip.into(),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            broadcast: broadcast.map(Into::into),
            up_non_loopback: true,
        };
        let ifaces = [
            nic(2, [10, 0, 0, 1], Some([10, 0, 0, 255])),
            nic(2, [10, 0, 1, 1], Some([10, 0, 1, 255])),
            nic(3, [10, 9, 0, 1], None),
        ];
        let listeners = SearchListeners::new(&ServerInterfaces::default(), &ifaces);
        let on = |dest: [u8; 4], index| listeners.arrival(Some((dest.into(), index)));
        let ip = |o: [u8; 4]| Ipv4Addr::from(o);
        assert_eq!(on([10, 0, 0, 1], 2), Arrival::Unicast(ip([10, 0, 0, 1])));
        assert_eq!(on([10, 0, 1, 1], 2), Arrival::Unicast(ip([10, 0, 1, 1])));
        assert_eq!(
            on([10, 0, 1, 255], 2),
            Arrival::Broadcast(ip([10, 0, 1, 1]))
        );
        assert_eq!(
            on([10, 0, 0, 255], 2),
            Arrival::Broadcast(ip([10, 0, 0, 1]))
        );
        // Another interface's address arriving here is no unicast to it.
        assert_eq!(on([10, 9, 0, 1], 2), Arrival::Broadcast(ip([10, 0, 0, 1])));
        assert_eq!(on([10, 9, 0, 1], 3), Arrival::Unicast(ip([10, 9, 0, 1])));
        // An index the map does not know: any local address is unicast.
        assert_eq!(on([10, 0, 1, 1], 9), Arrival::Unicast(ip([10, 0, 1, 1])));
        assert_eq!(
            on([224, 0, 7, 7], 9),
            Arrival::Broadcast(Ipv4Addr::UNSPECIFIED)
        );
    }
    /// A NIC's addresses in pvxs's map order, `s_addr` as a native integer,
    /// whatever order the host lists them in: a group is answered from the
    /// first, a shared broadcast from the last.
    #[cfg(not(epics_embedded_target))]
    #[test]
    fn a_nics_addresses_take_pvxs_map_order() {
        let nic = |ip: [u8; 4]| epics_base_rs::net::IfaceInfo {
            index: 2,
            name: "if2".into(),
            ip: ip.into(),
            netmask: Ipv4Addr::new(255, 255, 0, 0),
            broadcast: Some(Ipv4Addr::new(10, 0, 255, 255)),
            up_non_loopback: true,
        };
        let (a, b) = ([10, 0, 0, 2], [10, 0, 1, 1]);
        let (first, last) = if cfg!(target_endian = "little") {
            (b, a)
        } else {
            (a, b)
        };
        for ifaces in [[nic(a), nic(b)], [nic(b), nic(a)]] {
            let listeners = SearchListeners::new(&ServerInterfaces::default(), &ifaces);
            let on = |dest: [u8; 4]| listeners.arrival(Some((dest.into(), 2)));
            assert_eq!(on([224, 0, 2, 3]), Arrival::Broadcast(first.into()));
            assert_eq!(on([10, 0, 255, 255]), Arrival::Broadcast(last.into()));
        }
    }

    /// pvxs forwards a SEARCH by its destination alone
    /// (`udp_collector.cpp:286-306`, `:385-396`): an address of the arrival
    /// interface is `Forwarding`; its broadcast, a group, or the ORIGIN_TAG
    /// group itself (`Forwarded`) is not, and neither is a datagram with no
    /// `IP_PKTINFO` to say where it was sent.
    #[cfg(not(epics_embedded_target))]
    #[test]
    fn forwards_reads_the_destination_not_the_flag() {
        let nic = |index, ip: [u8; 4], broadcast: Option<[u8; 4]>| epics_base_rs::net::IfaceInfo {
            index,
            name: format!("if{index}"),
            ip: ip.into(),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            broadcast: broadcast.map(Into::into),
            up_non_loopback: index != 1,
        };
        let ifaces = [
            nic(1, [127, 0, 0, 1], None),
            nic(2, [10, 0, 0, 1], Some([10, 0, 0, 255])),
        ];
        let listeners = SearchListeners::new(&ServerInterfaces::default(), &ifaces);
        let on = |dest: [u8; 4], index| listeners.forwards(Some((dest.into(), index)));
        assert!(on([10, 0, 0, 1], 2));
        assert!(on([127, 0, 0, 1], 1));
        assert!(!on([10, 0, 0, 255], 2));
        assert!(!on([224, 0, 2, 3], 2));
        assert!(!on(ORIGIN_TAG_MCAST_GROUP.octets(), 1));
        assert!(!listeners.forwards(None));
    }
}
