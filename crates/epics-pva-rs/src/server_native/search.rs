//! SEARCH request parsing, name matching and SEARCH_RESPONSE framing.
//!
//! The protocol half of the discovery exchange: decode a SEARCH frame,
//! decide which CIDs this server answers for a given requester, and build
//! the reply frame. No socket, no async I/O — the bytes arrive from
//! `super::udp` on a datagram and from [`super::tcp`] on an established
//! circuit, and both hand them here.
//!
//! It lives beside the sources rather than inside `super::udp` because
//! the TCP-circuit SEARCH handler needs exactly these three entry points
//! (`parse_search_request`, `matched_cids_for_requester`,
//! `build_search_response_proto`) while `udp` itself is host-only —
//! `tokio::net::UdpSocket`, `socket2`, `if-addrs`. Keeping the protocol
//! here is what lets `tcp` compile for `armv7-rtems-eabihf`; `udp`
//! re-exports every item so its own call sites are unchanged.
//!
//! The transport-protocol gate is deliberately NOT here: a broadcast
//! SEARCH must be filtered by the requested protocol list, an established
//! circuit must not be (pvxs `serverchan.cpp:184-244`). That policy
//! differs per transport, so `udp::search_matched_cids` wraps
//! `matched_cids_for_requester` with it and `tcp` calls the core
//! directly.

use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::proto::{
    ByteOrder, Command, PvaHeader, ReadExt, WriteExt, decode_size, decode_string,
    encode_string_into, ip_from_bytes, ip_to_bytes,
};

use super::source::DynSource;

// exposed pub(crate) so the TCP-circuit SEARCH handler in
// tcp.rs can reuse this struct + the parser below. The fields are
// read-only after parse. The TCP-circuit handler consults `queries` (through
// `matched_cids_for_requester`), `must_reply`, `seq` and `byte_order`.
//
// It does NOT consult `protocols`, and that is upstream parity rather than an
// omission: pvxs `serverchan.cpp:185-192` decodes the TCP circuit's protocol
// list into a local `foundtcp` and then never reads it — `onSearch` is called
// unconditionally at `:216` and the reply always names "tcp" at `:246`. The
// protocol gate is a *broadcast* rule (see the module doc above), so wiring
// one in here would make us answer strictly fewer TCP-circuit SEARCHes than
// pvxs does. `tcp.rs` states the same conclusion at its call site.
//
// `protocols`, `reply_addr`, `reply_port`, `unicast` and `consumed` are read
// only on the UDP paths. The parser fills them for every caller because it is
// one parser, not two — which is also why an RTEMS build that has no UDP
// responder wired yet reports them as never read.
#[derive(Debug)]
pub(crate) struct SearchRequest {
    pub(crate) seq: u32,
    pub(crate) byte_order: ByteOrder,
    pub(crate) queries: Vec<(u32, String)>,
    /// Reply destination announced inside the SEARCH payload (the
    /// 16-byte address + 2-byte port fields). `None` means the address
    /// was the unspecified sentinel (`0.0.0.0` / `::`), in which case
    /// pvxs falls back to the UDP source address. The port is always
    /// populated, even when `reply_addr` is `None`.
    ///
    /// Stored as a full [`IpAddr`] (not IPv4-only) so the IPv6 responder
    /// can honour an advertised v6 reply address; pvxs decodes the field
    /// into a full `SockAddr` regardless of family
    /// (`udp_collector.cpp:367-370`).
    pub(crate) reply_addr: Option<IpAddr>,
    pub(crate) reply_port: u16,
    /// True when the SEARCH header had the Unicast flag (`0x80`,
    /// `pva_search_flags::Unicast`) set. pvxs uses this as a marker
    /// that the forwarder must clear before relaying via the loopback
    /// ORIGIN_TAG channel (`udp_collector.cpp:391`).
    pub(crate) unicast: bool,
    /// True when the SEARCH header had the `MustReply` flag (`0x01`,
    /// `pva_search_flags::MustReply`) set — pvlist-style discovery
    /// probes set this so every reachable server answers even with
    /// `nreply==0`. pvxs honours it at `server.cpp:730-732`
    /// (`if(nreply==0 && !msg.mustReply) return;`).
    pub(crate) must_reply: bool,
    /// the transport protocols the client requested in this
    /// SEARCH, stored as RAW BYTES. pvxs `udp_collector.cpp:411-421`
    /// reads each as a `std::string` without UTF-8 validation and only
    /// equality-checks it against "tcp"; `:424-441` queues matches only
    /// when "tcp" appeared. Keeping bytes (not `String`) mirrors that: an
    /// invalid-UTF-8 protocol is a non-"tcp" entry. An empty list
    /// (`nproto==0`) never raises pvxs `protoTCP`, so on the UDP
    /// responders `udp::search_matched_cids` matches nothing for it — it is
    /// not a wildcard.
    pub(crate) protocols: Vec<Vec<u8>>,
    /// Total bytes consumed from the input slice (header + payload),
    /// used by the multi-message drain loop to advance to the next
    /// chained message in the same datagram.
    pub(crate) consumed: usize,
}

pub(crate) fn parse_search_request(frame: &[u8]) -> Option<SearchRequest> {
    if frame.len() < PvaHeader::SIZE {
        return None;
    }
    let mut cur = Cursor::new(frame);
    let header = PvaHeader::decode(&mut cur).ok()?;
    // UDP datagrams can carry neither control messages nor segmentation —
    // segment bits are a TCP reassembly feature only. pvxs drops such a
    // datagram before SEARCH matching (udp_collector.cpp:329-340).
    if header.command != Command::Search.code()
        || header.flags.is_control()
        || !header.flags.unsegmented()
    {
        return None;
    }
    let order = header.flags.byte_order();
    let payload_len = header.payload_length as usize;
    let avail = frame.len().saturating_sub(PvaHeader::SIZE);
    if avail < payload_len {
        return None;
    }
    let payload = &frame[PvaHeader::SIZE..PvaHeader::SIZE + payload_len];
    let mut p = Cursor::new(payload);
    let seq = p.get_u32(order).ok()?;
    let flags = p.get_u8().ok()?;
    let unicast = flags & 0x80 != 0;
    // `pva_search_flags::MustReply = 0x01`. pvxs `udp_collector.cpp:363`
    // mirrors the field into `SearchMsg::mustReply`; pvlist relies on
    // this to enumerate reachable servers regardless of name matches.
    let must_reply = flags & 0x01 != 0;
    let _ = p.get_bytes(3).ok()?;
    let addr_bytes = p.get_bytes(16).ok()?;
    let mut addr16 = [0u8; 16];
    addr16.copy_from_slice(&addr_bytes);
    // pvxs `udp_collector.cpp:351-360`: `server.isAny()` means "reply
    // to UDP source"; otherwise the SEARCH carries a specific reply
    // destination. Keep any concrete address (v4 or v6); fold only the
    // wildcard sentinel (`0.0.0.0` / `::`, in either raw or v4-mapped
    // form) to `None`. pvxs decodes a full `SockAddr` here
    // (`udp_collector.cpp:367-370`), so an IPv6 reply address must
    // survive rather than being dropped by an IPv4-only filter.
    let reply_addr = match ip_from_bytes(&addr16) {
        Some(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    };
    let reply_port = p.get_u16(order).ok()?;
    let n_proto = decode_size(&mut p, order).ok().flatten()? as usize;
    // collect the requested protocol list so the responder
    // can filter to PVs whose transport matches. pvxs
    // `udp_collector.cpp:408-421` records whether the SEARCH
    // included "tcp" and `:424-443` only queues matches when it
    // did. Pre-fix Rust read-and-discarded the list, then answered
    // every SEARCH regardless of the protocols the client asked
    // for.
    let mut protocols: Vec<Vec<u8>> = Vec::with_capacity(n_proto.min(8));
    for _ in 0..n_proto {
        // Read each protocol as a length-prefixed RAW byte string. pvxs
        // does not UTF-8-validate (`from_wire` into std::string), so an
        // invalid-UTF-8 protocol is preserved as a non-"tcp" entry rather
        // than being silently swallowed into an empty (wildcard) list.
        // A truncated length or body, by contrast, is a malformed frame:
        // the `?` aborts the whole parse (UDP drops it; the TCP circuit
        // treats the `None` as a decode fault and closes).
        let len = match decode_size(&mut p, order) {
            Ok(Some(n)) => n as usize,
            Ok(None) | Err(_) => return None,
        };
        protocols.push(p.get_bytes(len).ok()?);
    }
    let n = p.get_u16(order).ok()? as usize;
    // cap pre-alloc against attacker-announced
    // count. Each (cid u32, String) consumes >= 5 wire bytes; in
    // practice n is u16-bounded so the worst case is ~1.5MB but
    // capping at remaining-bytes keeps the small-datagram common
    // case tight.
    let remaining = p.get_ref().len().saturating_sub(p.position() as usize);
    let mut queries = Vec::with_capacity(n.min(remaining));
    for _ in 0..n {
        let cid = p.get_u32(order).ok()?;
        let name = decode_string(&mut p, order).ok().flatten()?;
        queries.push((cid, name));
    }
    Some(SearchRequest {
        seq,
        byte_order: order,
        queries,
        reply_addr,
        reply_port,
        unicast,
        must_reply,
        protocols,
        consumed: PvaHeader::SIZE + payload_len,
    })
}

/// Name-match core: the CIDs in `req` whose names this server will answer
/// for `requester`, WITHOUT the transport-protocol gate. This is the rule
/// pvxs applies on an established TCP circuit — `handle_SEARCH` parses the
/// protocol strings into `foundtcp` but never consults it before calling
/// every source's `onSearch` (serverchan.cpp:184-244). The transport was
/// already negotiated when the circuit opened, so a SEARCH payload's
/// protocol list does not re-gate matches on that circuit.
///
/// The UDP responders need the protocol gate (a broadcast SEARCH must not
/// pull a `found=1` from a server that does not speak the requested
/// transport), so `udp::search_matched_cids` wraps this with that gate. The
/// TCP-circuit handler calls this directly. Splitting the gate out keeps
/// the per-query `searchable_from` rule (endpoint-scoped advertisement,
/// pvxs `Search::source()`) shared across all three responders while the
/// protocol policy differs by transport.
pub(crate) async fn matched_cids_for_requester(
    source: &DynSource,
    req: &SearchRequest,
    requester: SocketAddr,
) -> Vec<u32> {
    let mut matched = Vec::with_capacity(req.queries.len());
    for (cid, name) in &req.queries {
        // Endpoint-scoped advertisement: a source may claim a name only
        // for some requesters (pvxs `Search::source()`). The `requester`
        // is the SEARCH's resolved reply destination for UDP and the
        // established peer for TCP-circuit search.
        if source.searchable_from(name, requester).await {
            matched.push(*cid);
        }
    }
    matched
}

/// Build a SEARCH_RESPONSE frame with explicit protocol name.
///
/// pvxs `server.cpp:743-746`: when `nreply==0`, the `found` byte is set to
/// `0` (clients see "this server has none of those names" rather than
/// "this server has empty matches"). When `cids` is empty the response is
/// still a valid frame — used as an answer to `MustReply`-flagged
/// SEARCHes (pvlist-style discovery probes) so the requester can build
/// its server list.
// exposed for tcp.rs so the TCP-circuit SEARCH handler
// reuses the same wire shape the UDP responder emits.
pub(crate) fn build_search_response_proto(
    guid: [u8; 12],
    seq: u32,
    tcp_port: u16,
    cids: &[u32],
    order: ByteOrder,
    protocol: &str,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&guid);
    payload.put_u32(seq, order);
    let addr = ip_to_bytes(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    payload.extend_from_slice(&addr);
    payload.put_u16(tcp_port, order);
    encode_string_into(protocol, order, &mut payload);
    payload.put_u8(if cids.is_empty() { 0 } else { 1 }); // found
    payload.put_u16(cids.len() as u16, order);
    for &cid in cids {
        payload.put_u32(cid, order);
    }
    let header = PvaHeader::application(
        true,
        order,
        Command::SearchResponse.code(),
        payload.len() as u32,
    );
    let mut out = Vec::new();
    header.write_into(&mut out);
    out.extend_from_slice(&payload);
    out
}
