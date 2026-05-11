//! UDP search responder + (very simple) beacon broadcaster.
//!
//! Listens on the configured UDP port for SEARCH requests and replies with
//! SEARCH_RESPONSE messages naming our TCP endpoint. Beacons are emitted
//! periodically to advertise our presence.

use std::collections::HashSet;
use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::net::{AsyncUdpV4, ORIGIN_TAG_MCAST_GROUP, bind_loopback_mcast};
use std::net::SocketAddrV4;
use tracing::{debug, warn};

use crate::codec::PvaCodec;
use crate::error::{PvaError, PvaResult};
use crate::proto::{
    ByteOrder, Command, PvaHeader, ReadExt, WriteExt, decode_size, decode_string,
    encode_string_into, ip_from_bytes, ip_to_bytes,
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

fn bind_udp(port: u16) -> PvaResult<AsyncUdpV4> {
    let sock = AsyncUdpV4::bind(port, true).map_err(PvaError::Io)?;
    // pvxs server.cpp joins multicast groups listed in
    // EPICS_PVAS_INTF_ADDR_LIST / EPICS_PVA_ADDR_LIST so SEARCH packets
    // sent to those groups reach the responder. We do the same here —
    // the call is idempotent on each restart and silently skips
    // non-multicast entries.
    crate::client_native::search_engine::join_addr_list_multicast(&sock);
    Ok(sock)
}

/// Run the UDP search responder + beacon emitter until the runtime is dropped.
///
/// `tcp_port` is advertised in SEARCH_RESPONSE so clients know where to
/// open the virtual circuit. `protocol` is normally `"tcp"`; set to
/// `"tls"` when the TCP listener requires TLS so pvxs clients with TLS
/// configured will connect over `pvas://`.
pub async fn run_udp_responder_proto(
    source: DynSource,
    udp_port: u16,
    tcp_port: u16,
    guid: [u8; 12],
    protocol: &'static str,
) -> PvaResult<()> {
    run_udp_responder_with_config(
        source,
        udp_port,
        tcp_port,
        guid,
        protocol,
        Duration::from_secs(15),
        Duration::from_secs(180),
        10,
        Vec::new(),
        true,
        Vec::new(),
    )
    .await
}

/// Like [`run_udp_responder_proto`] but configurable: explicit beacon
/// period, explicit destinations, and an auto-NIC-broadcast flag. When
/// `destinations` is empty AND `auto_beacon` is true, beacons fan out
/// to per-NIC broadcasts (via [`crate::config::env::list_broadcast_addresses`]).
/// When `destinations` is non-empty, exactly those addresses are used.
#[allow(clippy::too_many_arguments)]
pub async fn run_udp_responder_with_config(
    source: DynSource,
    udp_port: u16,
    tcp_port: u16,
    guid: [u8; 12],
    protocol: &'static str,
    beacon_period: Duration,
    beacon_period_long: Duration,
    beacon_burst_count: u8,
    destinations: Vec<SocketAddr>,
    auto_beacon: bool,
    ignore_addrs: Vec<(IpAddr, u16)>,
) -> PvaResult<()> {
    let socket = bind_udp(udp_port)?;
    let socket = Arc::new(socket);
    debug!(?udp_port, "UDP search responder started");

    // Resolve beacon destinations once at startup. pvxs re-resolves
    // on interface change but we keep it static for now; restart the
    // server to pick up new NICs.
    //
    // Round 26: when `auto_beacon=false` AND `destinations` is empty,
    // the operator has explicitly opted out of broadcast beaconing
    // (matching pvxs `EPICS_PVAS_AUTO_BEACON_ADDR_LIST=NO` semantics).
    // Pre-fix this branch fell through to limited broadcast, leaking
    // beacon frames against site policy. Mirror round-25 CA fix.
    let beacon_destinations: Vec<SocketAddr> = if !destinations.is_empty() {
        destinations
    } else if auto_beacon {
        crate::config::env::list_broadcast_addresses(udp_port)
    } else {
        Vec::new()
    };
    debug!(
        ?beacon_destinations,
        ?beacon_period,
        "beacon emitter config"
    );

    let beacon_socket = socket.clone();
    let beacon_guid = guid;
    let beacon_source = source.clone();
    // F2: bind the JoinHandle to an AbortOnDrop guard scoped to this
    // function's stack so the beacon task is aborted when the parent
    // UDP responder unwinds (PvaServer Drop, listener task panic).
    // Without this the bound socket-cloning beacon task lingered
    // until runtime shutdown across server restart cycles.
    struct AbortOnDrop(tokio::task::AbortHandle);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let beacon_join = tokio::spawn(async move {
        // Burst-then-slowdown cadence (P-G17): emit `beacon_burst_count`
        // beacons at `beacon_period` (default 15s × 10), then drop to
        // `beacon_period_long` (default 180s) for steady state. Mirrors
        // pvxs `server.cpp:826-832`: after the burst every receiver in
        // earshot has had multiple chances to notice the new server, so
        // 12× more steady-state beacons just burn UDP without
        // information gain. Per-emitter monotonically advancing
        // beacon_seq + change_count let clients detect missed beacons
        // and topology changes regardless of cadence.
        let mut beacon_seq: u8 = 0;
        let mut change_count: u16 = 0;
        let mut last_set_hash: u64 = 0;
        let mut emitted: u32 = 0;
        let mut beacon_send_errs: HashSet<SocketAddr> = HashSet::new();
        loop {
            let cur_period = if emitted < beacon_burst_count as u32 {
                beacon_period
            } else {
                beacon_period_long
            };
            tokio::time::sleep(cur_period).await;
            // Compute a stable hash of the current PV set so we don't
            // hold an allocated Vec across the await above.
            let pvs = beacon_source.list_pvs().await;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            use std::hash::{Hash, Hasher};
            let mut sorted = pvs;
            sorted.sort();
            sorted.hash(&mut h);
            let cur_hash = h.finish();
            let topology_changed = cur_hash != last_set_hash && last_set_hash != 0;
            if topology_changed {
                change_count = change_count.wrapping_add(1);
                // pvxs doesn't reset to short-burst on topology change,
                // but we also don't lose anything by re-burst on real
                // PV-set churn (rare event); we leave the counter
                // alone to keep parity with pvxs.
            }
            last_set_hash = cur_hash;

            let beacon = build_beacon(
                beacon_guid,
                tcp_port,
                ByteOrder::Little,
                beacon_seq,
                change_count,
            );
            for dest in &beacon_destinations {
                // Limited broadcast / multicast destinations need
                // explicit per-NIC fanout. Per-subnet broadcast and
                // unicast route via AsyncUdpV4::send_to's NIC pick.
                let needs_fanout = match dest {
                    SocketAddr::V4(v4) => v4.ip().is_broadcast() || v4.ip().is_multicast(),
                    SocketAddr::V6(_) => false,
                };
                let result = if needs_fanout {
                    beacon_socket.fanout_to(&beacon, *dest).await.map(|_| ())
                } else {
                    beacon_socket.send_to(&beacon, *dest).await.map(|_| ())
                };
                match result {
                    Ok(()) => {
                        beacon_send_errs.remove(dest);
                    }
                    Err(e) => {
                        if beacon_send_errs.insert(*dest) {
                            warn!("beacon TX to {dest} failed: {e}");
                        } else {
                            debug!("beacon TX to {dest} failed: {e}");
                        }
                    }
                }
            }
            beacon_seq = beacon_seq.wrapping_add(1);
            emitted = emitted.saturating_add(1);
        }
    });
    let _beacon_guard = AbortOnDrop(beacon_join.abort_handle());

    // pvxs `udp_collector.cpp:127, :140-167`: a dedicated socket
    // bound wildcard, joined to 224.0.0.128 via 127.0.0.1, sits
    // alongside the per-NIC SEARCH responder so other PVA peers on
    // this host can forward SEARCHes (CMD_ORIGIN_TAG-prefixed) into
    // every local listener. Optional — if the bind fails (most
    // commonly a sandboxed test environment that prohibits multicast
    // joins, or a kernel without IP_ADD_MEMBERSHIP for the
    // requested group), we log at debug and run without ORIGIN_TAG
    // delivery rather than aborting startup.
    let lo_mcast = match bind_loopback_mcast(udp_port) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            debug!("loopback ORIGIN_TAG socket bind failed, running without: {e}");
            None
        }
    };

    // 64 KB receive buffer — IPv4 maximum. The previous 1500-byte
    // (Ethernet MTU) cap silently truncated large multi-PV searches:
    // pvxs clients pack many SEARCH messages into one datagram and a
    // gateway-restart storm can easily exceed 1500 bytes. 64 KB
    // matches the kernel ceiling without truncation. Heap-allocated
    // because 64 KB on the per-task stack is large; one allocation
    // amortized across the listener's lifetime.
    let mut buf = vec![0u8; 64 * 1024];
    let mut lo_buf = vec![0u8; 64 * 1024];
    loop {
        // Receive on whichever path is ready first. The per-NIC bundle
        // handles regular SEARCH/beacon traffic; the loopback mcast
        // socket (if bound) catches CMD_ORIGIN_TAG forwards from local
        // PVA peers. Both paths feed `process_search_datagram` with a
        // tagged origin so anti-loop and reply-routing rules apply.
        let recv_direct = socket.recv_with_meta(&mut buf);
        let recv_lo = async {
            match lo_mcast.as_ref() {
                Some(s) => s.recv_from(&mut lo_buf).await.map(Some),
                // No loopback socket: never resolve.
                None => {
                    std::future::pending::<std::io::Result<Option<(usize, SocketAddr)>>>().await
                }
            }
        };
        // No `biased;` ordering — under SEARCH bursts on the wire the
        // per-NIC path can dominate the loopback path arbitrarily long
        // and we want fair round-robin between the two so a co-resident
        // PVA peer's ORIGIN_TAG forwards aren't starved of recv slots.
        tokio::select! {
            r = recv_direct => {
                let meta = match r {
                    Ok(m) => m,
                    Err(e) => { debug!("udp recv error: {e}"); continue; }
                };
                let frame_len = meta.n;
                if !filter_inbound(meta.src, &ignore_addrs) {
                    continue;
                }
                process_search_datagram(
                    &source,
                    &socket,
                    lo_mcast.as_ref(),
                    udp_port,
                    &buf[..frame_len],
                    meta.src,
                    meta.iface_ip,
                    Origin::Direct,
                    tcp_port,
                    guid,
                    protocol,
                )
                .await;
            }
            r = recv_lo => {
                let r = match r {
                    Ok(Some(v)) => v,
                    Ok(None) => continue,
                    Err(e) => { debug!("loopback udp recv error: {e}"); continue; }
                };
                let (n, src) = r;
                if !filter_inbound(src, &ignore_addrs) {
                    continue;
                }
                let raw = &lo_buf[..n];
                // Peel the CMD_ORIGIN_TAG prefix; if it isn't one,
                // pvxs `udp_collector.cpp:402-405` allows processing
                // an unprefixed forward from peers that don't
                // implement ORIGIN_TAG. We're stricter for now: drop
                // the packet rather than risk reply amplification on
                // unprefixed mcast.
                let Some((peeled_dest, inner)) = PvaCodec::try_peel_origin_tag(raw) else {
                    debug!("loopback mcast missing/invalid ORIGIN_TAG prefix; dropping");
                    continue;
                };
                // peeled_dest = None means the forwarder set 0.0.0.0
                // (no NIC info). Use UNSPECIFIED as a sentinel — the
                // reply path checks for it and skips the per-NIC pin,
                // letting OS routing pick a source NIC.
                let reply_iface_ip = peeled_dest.unwrap_or(Ipv4Addr::UNSPECIFIED);
                process_search_datagram(
                    &source,
                    &socket,
                    lo_mcast.as_ref(),
                    udp_port,
                    inner,
                    src,
                    reply_iface_ip,
                    Origin::FromOriginTag,
                    tcp_port,
                    guid,
                    protocol,
                )
                .await;
            }
        }
    }

    // Beacon task is aborted via the AbortOnDrop guard (`_beacon_guard`)
    // when this function unwinds.
    #[allow(unreachable_code)]
    Ok(())
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
fn try_build_forward_frame(frame: &[u8], reply_dest: SocketAddrV4) -> Option<Vec<u8>> {
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
    // Overwrite the 16-byte response_addr with the resolved IPv4
    // (v4-mapped IPv6 form). This is what the recipient must use as
    // the reply destination since the original UDP source is the
    // forwarder, not the requester.
    let addr_bytes = ip_to_bytes(IpAddr::V4(*reply_dest.ip()));
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
/// Returns `true` if the packet should be processed.
fn filter_inbound(peer: SocketAddr, ignore_addrs: &[(IpAddr, u16)]) -> bool {
    if let std::net::IpAddr::V4(v4) = peer.ip() {
        if v4.is_multicast() {
            debug!("ignoring UDP with mcast source {peer}");
            return false;
        }
    }
    let ignored = ignore_addrs
        .iter()
        .any(|(ip, port)| peer.ip() == *ip && (*port == 0 || peer.port() == *port));
    !ignored
}

/// Process one fully-received UDP datagram: drain it for chained PVA
/// messages (pvxs `udp_collector.cpp::process_one` L329) and reply to
/// each SEARCH that matches a hosted PV. Replies route via the NIC
/// matched by `reply_iface_ip` (with OS fallback), or to the SEARCH
/// payload's announced reply addr when present.
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
async fn process_search_datagram(
    source: &DynSource,
    socket: &AsyncUdpV4,
    lo_mcast: Option<&Arc<tokio::net::UdpSocket>>,
    udp_port: u16,
    frame: &[u8],
    udp_src: SocketAddr,
    reply_iface_ip: Ipv4Addr,
    origin: Origin,
    tcp_port: u16,
    guid: [u8; 12],
    protocol: &'static str,
) {
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
        if let Some(lo) = lo_mcast {
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
                    let reply_ip = req.reply_addr.unwrap_or_else(|| match udp_src.ip() {
                        IpAddr::V4(v4) => v4,
                        IpAddr::V6(_) => Ipv4Addr::UNSPECIFIED,
                    });
                    let reply_dest = SocketAddrV4::new(reply_ip, req.reply_port);
                    if let Some(forward) = try_build_forward_frame(frame, reply_dest) {
                        let prefix = PvaCodec::build_origin_tag_prefix(reply_iface_ip);
                        let mut out = Vec::with_capacity(prefix.len() + forward.len());
                        out.extend_from_slice(&prefix);
                        out.extend_from_slice(&forward);
                        let dest =
                            SocketAddr::V4(SocketAddrV4::new(ORIGIN_TAG_MCAST_GROUP, udp_port));
                        if let Err(e) = lo.send_to(&out, dest).await {
                            debug!("ORIGIN_TAG forward to {dest}: {e}");
                        }
                        return;
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
                    Some(ip) => SocketAddr::V4(std::net::SocketAddrV4::new(ip, req.reply_port)),
                    None => {
                        if origin == Origin::FromOriginTag {
                            debug!(
                                "ORIGIN_TAG SEARCH announced isAny() reply addr; dropping per pvxs"
                            );
                            return;
                        }
                        // Direct origin: keep the UDP source's IP
                        // but use the announced reply port (pvxs
                        // `replyDest.setPort(port)`).
                        SocketAddr::new(udp_src.ip(), req.reply_port)
                    }
                };

                let mut matched_cids: Vec<u32> = Vec::with_capacity(req.queries.len());
                for (cid, name) in &req.queries {
                    if source.has_pv(name).await {
                        matched_cids.push(*cid);
                    }
                }
                if !matched_cids.is_empty() {
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
                    let send = if reply_iface_ip.is_unspecified() {
                        socket.send_to(&resp, reply_dest).await
                    } else {
                        match socket.send_via(&resp, reply_dest, reply_iface_ip).await {
                            Ok(n) => Ok(n),
                            Err(e) if e.kind() == std::io::ErrorKind::AddrNotAvailable => {
                                socket.send_to(&resp, reply_dest).await
                            }
                            Err(e) => Err(e),
                        }
                    };
                    if let Err(e) = send {
                        debug!("udp send to {reply_dest} via {reply_iface_ip}: {e}");
                    }
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
}

/// Build a (one-PV) SEARCH_RESPONSE frame with explicit protocol name.
fn build_search_response_proto(
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
    payload.put_u8(1); // found
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

/// Backwards-compat wrapper: protocol = "tcp".
pub async fn run_udp_responder(
    source: DynSource,
    udp_port: u16,
    tcp_port: u16,
    guid: [u8; 12],
) -> PvaResult<()> {
    run_udp_responder_proto(source, udp_port, tcp_port, guid, "tcp").await
}

fn build_beacon(
    guid: [u8; 12],
    tcp_port: u16,
    order: ByteOrder,
    sequence: u8,
    change_count: u16,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&guid);
    // pvxs server.cpp::doBeacons: flags(u8) + seq(u8) + change(u16) = 4 bytes
    payload.put_u8(0); // flags / QoS (undefined, 0)
    payload.put_u8(sequence);
    payload.put_u16(change_count, order);
    let addr = ip_to_bytes(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    payload.extend_from_slice(&addr);
    payload.put_u16(tcp_port, order);
    encode_string_into("tcp", order, &mut payload);
    payload.put_u8(0xFF); // null serverStatus marker (matches pvxs)
    let header = PvaHeader::application(true, order, Command::Beacon.code(), payload.len() as u32);
    let mut out = Vec::new();
    header.write_into(&mut out);
    out.extend_from_slice(&payload);
    out
}

#[derive(Debug)]
struct SearchRequest {
    seq: u32,
    byte_order: ByteOrder,
    queries: Vec<(u32, String)>,
    /// Reply destination announced inside the SEARCH payload (the
    /// 16-byte address + 2-byte port fields). `None` means the address
    /// was the unspecified sentinel (`0.0.0.0` / `::`), in which case
    /// pvxs falls back to the UDP source address. The port is always
    /// populated, even when `reply_addr` is `None`.
    reply_addr: Option<Ipv4Addr>,
    reply_port: u16,
    /// True when the SEARCH header had the Unicast flag (`0x80`,
    /// `pva_search_flags::Unicast`) set. pvxs uses this as a marker
    /// that the forwarder must clear before relaying via the loopback
    /// ORIGIN_TAG channel (`udp_collector.cpp:391`).
    unicast: bool,
    /// Total bytes consumed from the input slice (header + payload),
    /// used by the multi-message drain loop to advance to the next
    /// chained message in the same datagram.
    consumed: usize,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Direct,
    FromOriginTag,
}

fn parse_search_request(frame: &[u8]) -> Option<SearchRequest> {
    if frame.len() < PvaHeader::SIZE {
        return None;
    }
    let mut cur = Cursor::new(frame);
    let header = PvaHeader::decode(&mut cur).ok()?;
    if header.command != Command::Search.code() || header.flags.is_control() {
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
    let _ = p.get_bytes(3).ok()?;
    let addr_bytes = p.get_bytes(16).ok()?;
    let mut addr16 = [0u8; 16];
    addr16.copy_from_slice(&addr_bytes);
    // pvxs `udp_collector.cpp:351-360`: `server.isAny()` means "reply
    // to UDP source"; otherwise the SEARCH carries a specific reply
    // destination. Filter to IPv4 since this stack is IPv4-only.
    let reply_addr = match ip_from_bytes(&addr16) {
        Some(IpAddr::V4(v4)) if !v4.is_unspecified() => Some(v4),
        _ => None,
    };
    let reply_port = p.get_u16(order).ok()?;
    let n_proto = decode_size(&mut p, order).ok().flatten()? as usize;
    for _ in 0..n_proto {
        let _ = decode_string(&mut p, order).ok()?;
    }
    let n = p.get_u16(order).ok()? as usize;
    // P-G22 follow-up: cap pre-alloc against attacker-announced
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
        consumed: PvaHeader::SIZE + payload_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end forward path: `process_search_datagram` invoked
    /// with `Origin::Direct` and a unicast-flagged SEARCH MUST emit
    /// a CMD_ORIGIN_TAG-prefixed packet on `224.0.0.128:port` (the
    /// loopback ORIGIN_TAG channel). A second observer socket joined
    /// to the same group via `bind_loopback_mcast` should receive it,
    /// and peeling the prefix should yield the inner SEARCH with the
    /// Unicast flag cleared and the reply addr rewritten to the
    /// resolved destination — pvxs `udp_collector.cpp:387-396` end-
    /// to-end parity.
    #[tokio::test]
    async fn forward_path_emits_origin_tag_on_unicast_search() {
        use crate::pvdata::{FieldDesc, PvField};
        use crate::server_native::source::ChannelSource;
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::mpsc;

        // Minimal source: has_pv returns false for every name. The
        // forward path triggers BEFORE local processing, so the
        // source is only consulted on the FromOriginTag (loop-back)
        // path which is out of scope for this test.
        struct EmptySource;
        #[allow(clippy::manual_async_fn)]
        impl ChannelSource for EmptySource {
            fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
                async { Vec::new() }
            }
            fn has_pv(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn get_introspection(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { None }
            }
            fn get_value(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _name: &str,
                _value: PvField,
            ) -> impl std::future::Future<Output = Result<(), String>> + Send {
                async { Err("read-only test source".into()) }
            }
            fn is_writable(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(EmptySource);
        let socket = Arc::new(AsyncUdpV4::bind(0, false).expect("bind per-NIC"));

        // Bind the lo_mcast send socket on an ephemeral port AND a
        // second observer socket on the same port (SO_REUSEPORT).
        // Both are joined to 224.0.0.128 — the observer will receive
        // the forwarded packet via IP_MULTICAST_LOOP=1.
        let lo_mcast = Arc::new(bind_loopback_mcast(0).expect("lo_mcast bind"));
        let port = lo_mcast.local_addr().unwrap().port();
        let observer = bind_loopback_mcast(port).expect("observer bind");

        // Build a unicast SEARCH for "MY:PV" with reply 127.0.0.1:9999.
        let codec = PvaCodec { big_endian: false };
        let frame = codec.build_search(7, 42, "MY:PV", [127, 0, 0, 1], 9999, true);

        let udp_src = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 99, 5), 30000));
        // Simulated NIC unicast IP — embedded into the ORIGIN_TAG
        // prefix as the orig destination.
        let reply_iface_ip = Ipv4Addr::new(192, 168, 99, 10);

        process_search_datagram(
            &source,
            &socket,
            Some(&lo_mcast),
            port,
            &frame,
            udp_src,
            reply_iface_ip,
            Origin::Direct,
            5076,
            [0u8; 12],
            "tcp",
        )
        .await;

        let mut buf = [0u8; 4096];
        let (n, _src) = tokio::time::timeout(Duration::from_secs(2), observer.recv_from(&mut buf))
            .await
            .expect("observer recv timeout — forward not emitted")
            .expect("observer recv ok");
        let raw = &buf[..n];

        let (peeled, inner) = PvaCodec::try_peel_origin_tag(raw).expect("peel ok");
        assert_eq!(
            peeled,
            Some(reply_iface_ip),
            "ORIGIN_TAG prefix must carry the iface IP"
        );

        let req = parse_search_request(inner).expect("inner SEARCH parses");
        assert!(
            !req.unicast,
            "forwarded SEARCH must have Unicast flag cleared"
        );
        assert_eq!(
            req.reply_addr,
            Some(Ipv4Addr::new(127, 0, 0, 1)),
            "reply addr preserved from original SEARCH"
        );
        assert_eq!(req.reply_port, 9999);
        assert_eq!(req.queries.len(), 1);
        assert_eq!(req.queries[0].1, "MY:PV");
    }

    /// `process_search_datagram` with `Origin::FromOriginTag` MUST
    /// NOT re-forward — anti-loop guard (pvxs `udp_collector.cpp`
    /// only enters the Forwarding branch when origin is the
    /// non-loopback per-NIC path). Verify by sending a unicast SEARCH
    /// with `FromOriginTag` origin and asserting the observer never
    /// sees a forwarded packet.
    #[tokio::test]
    async fn forward_path_skipped_when_origin_is_from_origin_tag() {
        use crate::pvdata::{FieldDesc, PvField};
        use crate::server_native::source::ChannelSource;
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::mpsc;

        // Same EmptySource as above; duplicated to keep the tests
        // independent of test-ordering.
        struct EmptySource;
        #[allow(clippy::manual_async_fn)]
        impl ChannelSource for EmptySource {
            fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
                async { Vec::new() }
            }
            fn has_pv(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn get_introspection(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { None }
            }
            fn get_value(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _name: &str,
                _value: PvField,
            ) -> impl std::future::Future<Output = Result<(), String>> + Send {
                async { Err("read-only test source".into()) }
            }
            fn is_writable(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(EmptySource);
        let socket = Arc::new(AsyncUdpV4::bind(0, false).expect("bind per-NIC"));
        let lo_mcast = Arc::new(bind_loopback_mcast(0).expect("lo_mcast bind"));
        let port = lo_mcast.local_addr().unwrap().port();
        let observer = bind_loopback_mcast(port).expect("observer bind");

        let codec = PvaCodec { big_endian: false };
        let frame = codec.build_search(7, 42, "MY:PV", [127, 0, 0, 1], 9999, true);

        process_search_datagram(
            &source,
            &socket,
            Some(&lo_mcast),
            port,
            &frame,
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30000)),
            Ipv4Addr::LOCALHOST,
            Origin::FromOriginTag,
            5076,
            [0u8; 12],
            "tcp",
        )
        .await;

        // Observer must NOT receive anything — short timeout proves
        // the absence of forward emission.
        let mut buf = [0u8; 4096];
        let r =
            tokio::time::timeout(Duration::from_millis(150), observer.recv_from(&mut buf)).await;
        assert!(
            r.is_err(),
            "FromOriginTag origin must NOT trigger re-forward, but observer got {r:?}"
        );
    }

    /// `FromOriginTag` origin + isAny() reply addr in the SEARCH
    /// payload must drop the SEARCH without sending a response —
    /// pvxs `udp_collector.cpp:367-371` warning ("Forwarded SEARCH
    /// with reply to sender never works"). Verify by hosting a PV
    /// the SEARCH names and asserting the per-NIC socket emits no
    /// reply within the timeout window.
    #[tokio::test]
    async fn from_origin_tag_search_with_isany_reply_addr_is_dropped() {
        use crate::pvdata::{FieldDesc, PvField, ScalarType};
        use crate::server_native::source::ChannelSource;
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::mpsc;

        // Source that DOES claim to host "MY:PV" — proves the drop is
        // because of the isAny() rule, not because the PV was unknown.
        struct PresentSource;
        #[allow(clippy::manual_async_fn)]
        impl ChannelSource for PresentSource {
            fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
                async { vec!["MY:PV".into()] }
            }
            fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
                let m = name == "MY:PV";
                async move { m }
            }
            fn get_introspection(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { Some(FieldDesc::Scalar(ScalarType::Double)) }
            }
            fn get_value(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _name: &str,
                _value: PvField,
            ) -> impl std::future::Future<Output = Result<(), String>> + Send {
                async { Err("read-only".into()) }
            }
            fn is_writable(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(PresentSource);
        let socket = Arc::new(AsyncUdpV4::bind(0, false).expect("bind per-NIC"));
        // Bind a sniffer socket to the loopback NIC's own port so it
        // would catch any reply the responder tries to send back.
        // We use the NIC bundle's own addr as the simulated requester.
        let sniffer = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("sniffer bind");
        let sniffer_addr = sniffer.local_addr().unwrap();

        // SEARCH for "MY:PV" with reply addr 0.0.0.0 (isAny).
        let codec = PvaCodec { big_endian: false };
        let frame = codec.build_search(7, 42, "MY:PV", [0, 0, 0, 0], sniffer_addr.port(), false);

        process_search_datagram(
            &source,
            &socket,
            None, // no lo_mcast — irrelevant for this code path
            5076,
            &frame,
            sniffer_addr, // simulated requester
            Ipv4Addr::LOCALHOST,
            Origin::FromOriginTag,
            5076,
            [0u8; 12],
            "tcp",
        )
        .await;

        // Sniffer must NOT receive any reply within a short window —
        // proves the isAny() drop fires.
        let mut buf = [0u8; 4096];
        let r = tokio::time::timeout(Duration::from_millis(150), sniffer.recv_from(&mut buf)).await;
        assert!(
            r.is_err(),
            "isAny() reply addr on FromOriginTag must drop SEARCH; got {r:?}"
        );
    }

    /// `try_build_forward_frame` rewrites the first SEARCH's reply
    /// addr + port with the resolved destination AND clears the
    /// Unicast flag (pvxs `udp_collector.cpp:387-396`). Returns
    /// `None` when the frame's first message isn't a unicast-flagged
    /// SEARCH — the caller then skips forwarding entirely.
    #[test]
    fn try_build_forward_frame_clears_unicast_and_overwrites_reply_dest() {
        let codec = PvaCodec { big_endian: false };
        // Original SEARCH: unicast=true, reply 0.0.0.0:5076.
        let original = codec.build_search(7, 42, "MY:PV", [0, 0, 0, 0], 5076, true);
        // Forwarder resolves reply_dest = 192.168.1.5:54321 (the UDP
        // source IP + announced port).
        let dest = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 5), 54321);
        let out = try_build_forward_frame(&original, dest).expect("unicast → Some");

        // Re-parse the forwarded frame: unicast must be cleared, reply
        // must reflect the resolved dest.
        let req = parse_search_request(&out).expect("re-parse ok");
        assert!(!req.unicast, "unicast flag must be cleared");
        assert_eq!(req.reply_addr, Some(Ipv4Addr::new(192, 168, 1, 5)));
        assert_eq!(req.reply_port, 54321);

        // Non-unicast SEARCH: forward returns None (no rewrite).
        let bcast = codec.build_search(7, 42, "MY:PV", [10, 0, 0, 1], 5076, false);
        assert!(try_build_forward_frame(&bcast, dest).is_none());
    }

    /// `parse_search_request` extracts the reply addr + port from the
    /// SEARCH payload's 16-byte address field. Specific IPv4 → `Some`,
    /// `0.0.0.0` (and IPv6/zeros) → `None` (caller falls back to UDP
    /// source per pvxs `udp_collector.cpp:351-360`).
    #[test]
    fn parse_search_request_extracts_reply_addr() {
        let codec = PvaCodec { big_endian: false };
        // Specific reply addr 192.168.5.10:9999.
        let frame = codec.build_search(7, 42, "MY:PV", [192, 168, 5, 10], 9999, false);
        let req = parse_search_request(&frame).expect("parse ok");
        assert_eq!(req.reply_addr, Some(Ipv4Addr::new(192, 168, 5, 10)));
        assert_eq!(req.reply_port, 9999);

        // Unspecified addr → None (sentinel for "use UDP source").
        let frame_any = codec.build_search(7, 42, "MY:PV", [0, 0, 0, 0], 5076, false);
        let req_any = parse_search_request(&frame_any).expect("parse ok");
        assert_eq!(req_any.reply_addr, None);
        assert_eq!(req_any.reply_port, 5076);
    }

    /// `filter_inbound`: mcast-source packets and blocklisted peers
    /// are dropped; everything else passes. Mirrors pvxs anti-amp +
    /// `serverconn.cpp` ignoreAddrs semantics.
    #[test]
    fn filter_inbound_drops_mcast_source_and_blocklist() {
        let blocklist = vec![(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 99)), 5076)];
        // Plain unicast peer passes.
        let ok = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5076);
        assert!(filter_inbound(ok, &blocklist));
        // Multicast source dropped.
        let mcast = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 128)), 5076);
        assert!(!filter_inbound(mcast, &blocklist));
        // Blocklisted peer dropped on matching port.
        let blocked = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 99)), 5076);
        assert!(!filter_inbound(blocked, &blocklist));
        // Same IP on a different port still passes (blocklist port-scoped).
        let blocked_other_port = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 99)), 6000);
        assert!(filter_inbound(blocked_other_port, &blocklist));
        // Wildcard-port (0) entry blocks all ports for the IP.
        let any_port = vec![(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 99)), 0u16)];
        assert!(!filter_inbound(blocked_other_port, &any_port));
    }

    /// build_beacon writes the supplied sequence + change_count into
    /// the payload at the documented offsets (after the 12-byte GUID +
    /// flags byte). Locks in the field order so a refactor cannot swap
    /// them silently.
    #[test]
    fn beacon_payload_carries_sequence_and_change_count() {
        let guid = [0x11; 12];
        let bytes = build_beacon(guid, 5075, ByteOrder::Little, 42, 0xBEEF);
        // 8-byte PVA header + 12-byte GUID = 20 bytes prefix.
        let payload = &bytes[8..];
        assert_eq!(&payload[0..12], &guid);
        assert_eq!(payload[12], 0); // flags byte
        assert_eq!(payload[13], 42, "beacon sequence at offset 13");
        assert_eq!(
            u16::from_le_bytes([payload[14], payload[15]]),
            0xBEEF,
            "change_count at offset 14"
        );
    }
}
