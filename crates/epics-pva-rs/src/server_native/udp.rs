//! UDP search responder + (very simple) beacon broadcaster.
//!
//! Listens on the configured UDP port for SEARCH requests and replies with
//! SEARCH_RESPONSE messages naming our TCP endpoint. Beacons are emitted
//! periodically to advertise our presence.

// RTEMS-EXEC-MODEL-ALLOW(6): checked - these run and pass in the feature-ON suite.
use std::collections::HashSet;
use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::net::{
    AsyncUdpV4, IfaceMap, bind_loopback_mcast, enable_so_rxq_ovfl_for_socket,
    recv_from_with_drop_count_socket,
};
use tracing::{debug, warn};

use crate::codec::PvaCodec;
use crate::config::Endpoint;
use crate::error::{PvaError, PvaResult};
use crate::proto::{
    ByteOrder, Command, PvaHeader, ReadExt, WriteExt, decode_size, encode_size_into,
    encode_string_into, ip_from_bytes, ip_to_bytes,
};

use super::source::DynSource;
// The SEARCH protocol proper — frame parsing, name matching, response
// framing — moved to [`super::search`] so the TCP-circuit SEARCH handler can
// name it without inheriting this module's host-only gate. Re-exported here
// so every `server_native::udp::` path keeps resolving, including the frame
// builders `client_native` uses in its tests.
pub(crate) use super::search::{SearchRequest, build_search_response_proto, parse_search_request};
// The socket-free datagram half — chained-message drain, ORIGIN_TAG forward
// decision, reply-destination resolution, source filter, and the server GUID —
// moved to [`super::search_engine`] for the same reason and by the same move:
// the RTEMS blocking responder needs it and cannot inherit this module's
// host-only gate. Re-exported here so every `server_native::udp::` path keeps
// resolving, including this module's own tests.
pub use super::search_engine::random_guid;
pub(crate) use super::search_engine::{
    Origin, SearchOutput, filter_inbound, process_search_datagram, search_matched_cids,
};

/// PR #205 IPv6 Stage 5: bind a v6-only ephemeral UDP socket used to
/// emit beacons to v6 multicast groups. Mirrors the client-side
/// `bind_ephemeral_udp_v6`. Returns `None` when the host lacks v6 —
/// the beacon emitter keeps running v4-only in that case.
fn bind_beacon_send_v6() -> Option<Arc<tokio::net::UdpSocket>> {
    use socket2::{Domain, Protocol, Socket, Type};

    let sock = match Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(s) => s,
        Err(e) => {
            debug!("v6 beacon socket: socket() failed: {e}; v6 beacon disabled");
            return None;
        }
    };
    if let Err(e) = sock.set_only_v6(true) {
        debug!("v6 beacon socket: set_only_v6 failed: {e}");
    }
    if let Err(e) = sock.set_nonblocking(true) {
        debug!("v6 beacon socket: set_nonblocking failed: {e}");
        return None;
    }
    if let Err(e) = sock.set_multicast_hops_v6(1) {
        debug!("v6 beacon socket: set_multicast_hops_v6 failed: {e}");
    }
    let bind = SocketAddr::V6(std::net::SocketAddrV6::new(
        std::net::Ipv6Addr::UNSPECIFIED,
        0,
        0,
        0,
    ));
    if let Err(e) = sock.bind(&bind.into()) {
        debug!("v6 beacon socket: bind {bind} failed: {e}; v6 beacon disabled");
        return None;
    }
    let std_sock: std::net::UdpSocket = sock.into();
    match tokio::net::UdpSocket::from_std(std_sock) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            debug!("v6 beacon socket: tokio adoption failed: {e}");
            None
        }
    }
}

/// Bind the search socket, returning the bundle **and the port it actually
/// got**.
///
/// `port == 0` means "kernel, choose one", and the caller then has no other
/// way to learn the number — so it is read back from the bind here rather
/// than predicted. Two properties this function owns:
///
/// * **One port across the bundle.** The responder embeds this number in
///   beacons, in the loopback-multicast retransmit, and in the multicast
///   joins below, so a bundle whose NICs each got a *different* ephemeral
///   port would make "the" UDP port meaningless. `bind_ephemeral_same_port*`
///   lets the first NIC pick and the rest reuse that one number.
/// * **Read back, never probed.** The socket that reports the port is the
///   socket the responder serves on, so nothing can take it in between.
///   (This matters more for PVA than for CA: the search socket sets
///   `SO_REUSEPORT`, so a *collision does not fail* — a second server binds
///   the same port silently and the two answer searches at random. There is
///   no error to detect after the fact, which is exactly why the number must
///   never be guessed.)
pub(crate) fn bind_udp(
    port: u16,
    interfaces: &[Ipv4Addr],
    beacon_dests: &[Endpoint],
) -> PvaResult<(AsyncUdpV4, u16)> {
    // `EPICS_PVAS_INTF_ADDR_LIST` constrains the responder to the listed
    // interfaces (pvxs `server.cpp:407-439` binds UDP search listeners
    // only on the effective interface set). Empty list = the historical
    // all-NIC default. A constrained bind also gets each listed NIC's
    // broadcast-RX socket from `bind_on_interfaces`, so subnet-directed
    // SEARCH to a listed interface still reaches us.
    let mut sock = if port == 0 {
        // Ephemeral: every NIC socket must land on the SAME kernel-assigned
        // port (see the doc comment) — `bind(0, ..)` per NIC would not.
        if interfaces.is_empty() {
            AsyncUdpV4::bind_ephemeral_same_port(true)
        } else {
            AsyncUdpV4::bind_ephemeral_same_port_on_interfaces(interfaces, true)
        }
    } else if interfaces.is_empty() {
        AsyncUdpV4::bind(port, true)
    } else {
        AsyncUdpV4::bind_on_interfaces(interfaces, port, true)
    }
    .map_err(PvaError::Io)?;

    // The bound port, read off the socket. pvxs does the same read-back
    // (`server.cpp:426`: `effective.udp_port = addr.addr.port()`), which is
    // what makes `EPICS_PVAS_BROADCAST_PORT=0` usable at all.
    let port = resolve_bound_udp_port(&sock, port)?;
    // pvxs `config.cpp:512-518` calls `addGroups(ifaces, beaconDestinations,
    // ifmap)` after resolving the server's beacon destinations, so every
    // multicast address the server beacons to also becomes a search-listener
    // interface (`config.cpp:310-335`); `server.cpp:411-423` then starts a
    // UDP listener — joining the group — for each. The join source is the
    // server's *effective beacon destinations*, which already encode the
    // `EPICS_PVAS_BEACON_ADDR_LIST` → `EPICS_PVA_ADDR_LIST` precedence and
    // any programmatic `PvaServerConfig::beacon_destinations`. (The
    // client-side `join_addr_list_multicast` reads `EPICS_PVA_ADDR_LIST`
    // directly because that is the client search-target list; for the
    // server, the raw client var is the wrong source — it ignores the
    // PVAS-specific list and programmatic destinations.)
    //
    // `addGroups` pushes the join targets onto the *listener* interface set
    // regardless of `EPICS_PVAS_INTF_ADDR_LIST`, so a modifier-less
    // multicast destination is joined on every external NIC — even NICs
    // outside the configured interface list — and `&mut sock` lets
    // `ensure_iface_socket` add those listener sockets to the bundle.
    let external_nics: Vec<Ipv4Addr> = IfaceMap::new()
        .up_non_loopback()
        .into_iter()
        .map(|i| i.ip)
        .collect();
    join_beacon_multicast_groups(&mut sock, beacon_dests, port, &external_nics);
    Ok((sock, port))
}

/// The single port a bound [`AsyncUdpV4`] bundle is serving on.
///
/// `requested != 0` is authoritative — that is the number every socket was
/// told to bind. For `requested == 0` the port is whatever the kernel handed
/// out, so it is read off the sockets; the bundle is required to agree,
/// because a split bundle has no single "the port" to advertise and
/// silently picking one socket's number would make beacons and SEARCH
/// replies point somewhere a peer cannot reach.
fn resolve_bound_udp_port(sock: &AsyncUdpV4, requested: u16) -> PvaResult<u16> {
    if requested != 0 {
        return Ok(requested);
    }
    let addrs = sock.local_addrs();
    let mut ports = addrs.iter().map(|a| a.port());
    let first = ports.next().ok_or_else(|| {
        PvaError::Protocol("UDP search socket bound no interfaces; no port to report".into())
    })?;
    if first == 0 {
        return Err(PvaError::Protocol(
            "UDP search socket reported port 0 after bind".into(),
        ));
    }
    if ports.any(|p| p != first) {
        return Err(PvaError::Protocol(format!(
            "UDP search sockets landed on different ephemeral ports ({addrs:?}); \
             the responder advertises one port and cannot honour a split bundle"
        )));
    }
    Ok(first)
}

/// Resolve the `(group, interface)` multicast join targets from the
/// server's effective beacon destinations, mirroring pvxs `addGroups`
/// (`config.cpp:310-335`).
///
/// Non-multicast and IPv6 destinations are dropped — the v4 socket cannot
/// join a v6 group, and broadcast/unicast destinations need no membership.
/// For each v4 multicast destination:
///
/// * an explicit `@iface` modifier (`Endpoint::iface`) resolves to that one
///   interface — the group is joined on that NIC alone (pvxs pushes the
///   endpoint as-is, `config.cpp:320-322`); an unresolvable interface spec
///   is skipped (logged), the same outcome it would have for beacon egress.
/// * a modifier-less destination expands over every external NIC
///   (`external_nics`, pvxs `ifmap.all_external()`, `config.cpp:324-333`),
///   yielding one join target per NIC.
///
/// `(group, iface)` pairs are deduplicated, first-appearance order
/// preserved (pvxs `removeDups`). Pure given `external_nics` — the only
/// non-pure step is `resolve_iface_v4` for *named* interfaces, which
/// IP-literal `@iface` forms bypass — so the plan is testable without real
/// NIC multicast.
fn multicast_join_plan(
    beacon_dests: &[Endpoint],
    external_nics: &[Ipv4Addr],
) -> Vec<(Ipv4Addr, Ipv4Addr)> {
    let mut plan: Vec<(Ipv4Addr, Ipv4Addr)> = Vec::new();
    let mut push_unique = |group: Ipv4Addr, iface: Ipv4Addr| {
        let entry = (group, iface);
        if !plan.contains(&entry) {
            plan.push(entry);
        }
    };
    for dest in beacon_dests {
        let group = match dest.addr {
            SocketAddr::V4(v4) if v4.ip().is_multicast() => *v4.ip(),
            _ => continue,
        };
        match dest.iface.as_deref() {
            Some(spec) => match crate::config::env::resolve_iface_v4(spec) {
                Ok(ip) => push_unique(group, ip),
                Err(e) => debug!(
                    "multicast beacon destination {group}@{spec}: \
                     interface resolve failed: {e}; skipping join"
                ),
            },
            None => {
                for &ip in external_nics {
                    push_unique(group, ip);
                }
            }
        }
    }
    plan
}

/// Join every IPv4 multicast group in the server's effective beacon
/// destinations on the responder bundle, so SEARCH packets sent to those
/// groups reach us (pvxs `addGroups` / `server.cpp:411-423`).
///
/// Each plan entry is joined on its own interface via
/// [`AsyncUdpV4::join_multicast_v4_on`], not fanned out across every NIC:
/// an explicit `@iface` multicast destination must be received on that NIC
/// alone (pvxs joins the one chosen interface). When the target interface
/// lies outside `EPICS_PVAS_INTF_ADDR_LIST` (so the bundle has no socket on
/// it yet), [`AsyncUdpV4::ensure_iface_socket`] first binds and appends a
/// listener socket — pvxs adds that listener interface regardless of the
/// configured set. Idempotent across restarts; a single failed bind/join is
/// logged but does not disable the rest.
fn join_beacon_multicast_groups(
    sock: &mut AsyncUdpV4,
    beacon_dests: &[Endpoint],
    port: u16,
    external_nics: &[Ipv4Addr],
) {
    for (group, iface_ip) in multicast_join_plan(beacon_dests, external_nics) {
        match sock.ensure_iface_socket(iface_ip, port, true) {
            Ok(true) => debug!(
                "added multicast listener interface {iface_ip} \
                 (outside configured set) for group {group}"
            ),
            Ok(false) => {}
            Err(e) => {
                debug!(
                    "cannot bind multicast listener interface {iface_ip} \
                     for group {group}: {e}; skipping join"
                );
                continue;
            }
        }
        match sock.join_multicast_v4_on(group, iface_ip) {
            Ok(()) => debug!("joined beacon multicast group {group} on interface {iface_ip}"),
            Err(e) => debug!("join_multicast_v4_on {group}@{iface_ip} failed: {e}"),
        }
    }
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
        false,
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
    destinations: Vec<Endpoint>,
    auto_beacon: bool,
    ignore_addrs: Vec<(IpAddr, u16)>,
    // PR #205 IPv6 Stage 5: when true, also emit beacons to the
    // default v6 multicast group `[ff0e::400]:udp_port` via a
    // dedicated v6 send socket. The v6 SEARCH responder is spawned
    // separately (see [`run_udp_responder_v6`]); the v6 flag here only
    // controls beacon emission to the v6 group.
    enable_ipv6_udp: bool,
    // `EPICS_PVAS_INTF_ADDR_LIST` (v4 entries): when non-empty, the
    // search responder binds only these interfaces instead of every
    // discovered NIC. Empty = all-NIC default.
    interfaces: Vec<Ipv4Addr>,
) -> PvaResult<()> {
    let (socket, udp_port) = bind_udp(udp_port, &interfaces, &destinations)?;
    run_udp_responder_on_socket(
        socket,
        udp_port,
        source,
        tcp_port,
        guid,
        protocol,
        beacon_period,
        beacon_period_long,
        beacon_burst_count,
        destinations,
        auto_beacon,
        ignore_addrs,
        enable_ipv6_udp,
    )
    .await
}

/// Like [`run_udp_responder_with_config`] but serves an **already-bound**
/// socket whose port the caller has read back from the bind.
///
/// Splitting the bind out is what lets [`crate::server_native::runtime`]
/// support `udp_port = 0`: the responder is spawned as a task, so a bind
/// performed inside it would publish the kernel-assigned port nowhere and
/// `report()`/SEARCH replies would advertise 0. The binder learns the port
/// and hands it here, so `udp_port` below is always the real one — it is
/// embedded in beacons, the loopback-multicast retransmit, and the
/// origin-tag group, none of which tolerate a placeholder.
#[allow(clippy::too_many_arguments)]
pub async fn run_udp_responder_on_socket(
    socket: AsyncUdpV4,
    // The port `socket` is actually bound to — see [`bind_udp`].
    udp_port: u16,
    source: DynSource,
    tcp_port: u16,
    guid: [u8; 12],
    protocol: &'static str,
    beacon_period: Duration,
    beacon_period_long: Duration,
    beacon_burst_count: u8,
    destinations: Vec<Endpoint>,
    auto_beacon: bool,
    ignore_addrs: Vec<(IpAddr, u16)>,
    enable_ipv6_udp: bool,
) -> PvaResult<()> {
    let socket = Arc::new(socket);
    debug!(?udp_port, "UDP search responder started");

    // Resolve beacon destinations once at startup. pvxs re-resolves
    // on interface change but we keep it static for now; restart the
    // server to pick up new NICs.
    //
    // when `auto_beacon=false` AND `destinations` is empty,
    // the operator has explicitly opted out of broadcast beaconing
    // (matching pvxs `EPICS_PVAS_AUTO_BEACON_ADDR_LIST=NO` semantics).
    // Pre-fix this branch fell through to limited broadcast, leaking
    // beacon frames against site policy. Mirror the CA fix.
    let mut beacon_destinations: Vec<Endpoint> = if !destinations.is_empty() {
        destinations
    } else if auto_beacon {
        // Auto NIC-broadcast destinations carry no TTL/iface qualifier —
        // they are limited/subnet broadcasts, not multicast endpoints.
        crate::config::env::list_broadcast_addresses(udp_port)
            .into_iter()
            .map(Endpoint::from)
            .collect()
    } else {
        Vec::new()
    };
    // PR #205 IPv6 Stage 5: when v6 UDP is enabled AND auto_beacon is
    // on (i.e. the operator hasn't explicitly disabled beacon
    // emission), add the default v6 multicast group as a beacon
    // destination. Mirrors pvxs' v6 group `ff0e::400`.
    if enable_ipv6_udp && auto_beacon {
        let v6_mcast: Endpoint = SocketAddr::V6(std::net::SocketAddrV6::new(
            std::net::Ipv6Addr::new(0xff0e, 0, 0, 0, 0, 0, 0, 0x0400),
            udp_port,
            0,
            0,
        ))
        .into();
        if !beacon_destinations.iter().any(|e| e.addr == v6_mcast.addr) {
            beacon_destinations.push(v6_mcast);
        }
    }
    // Collapse duplicate beacon destinations by (addr, iface), keeping the
    // longest TTL and first-appearance order — pvxs `removeDups<SockEndpoint>`
    // applied to `beaconDestinations` in `Config::expand()` (config.cpp:523).
    // Runs AFTER auto-broadcast / v6-group expansion (matching pvxs, which
    // dedups after `expandAddrList`/`addGroups`) so synthesized destinations
    // are deduped too. Single owner for all sources: env, programmatic
    // `PvaServerConfig::beacon_destinations`, and the gateway CLI all flow
    // their beacon lists into this responder.
    beacon_destinations = crate::config::env::dedup_endpoints(beacon_destinations);
    // Send-only v6 socket used for beacon emission to v6 destinations.
    // The receive side (SEARCH on `[::]:udp_port`) is handled by the
    // companion `run_udp_responder_v6` task — keeping the beacon TX
    // socket separate avoids confusing the recv loop with our own
    // outgoing beacons echoed by the multicast group.
    let beacon_socket_v6: Option<Arc<tokio::net::UdpSocket>> = if enable_ipv6_udp {
        bind_beacon_send_v6()
    } else {
        None
    };
    debug!(
        ?beacon_destinations,
        ?beacon_period,
        v6 = beacon_socket_v6.is_some(),
        "beacon emitter config"
    );

    let beacon_socket = socket.clone();
    let beacon_socket_v6_for_task = beacon_socket_v6.clone();
    let beacon_guid = guid;
    let beacon_source = source.clone();
    // bind the JoinHandle to an AbortOnDrop guard scoped to this
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
        // Burst-then-slowdown cadence: emit `beacon_burst_count`
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
        // pvxs writes `beaconChange` (bumped on every source add/remove)
        // directly into the beacon (server.cpp:751-767). Track the
        // registry counter so a topology change advances `change_count`
        // even when the PV-set hash below is unchanged (source replaced
        // by another serving the same names, or a priority change).
        // Initialise to the value at task start so the startup source
        // registrations do not register as a runtime change on the first
        // beacon.
        let mut last_registry_change: u64 = beacon_source.beacon_change();
        let mut last_set_hash: u64 = 0;
        let mut emitted: u32 = 0;
        let mut beacon_send_errs: HashSet<SocketAddr> = HashSet::new();
        // pvxs cc5071cd22c4: fire the first beacon immediately on
        // server start (the original C code armed the libevent timer
        // with `&immediate = {0,0}`). Without this skip the first
        // sleep delays client discovery by `beacon_period` seconds.
        let mut first_beacon = true;
        loop {
            let cur_period = if emitted < beacon_burst_count as u32 {
                beacon_period
            } else {
                beacon_period_long
            };
            if first_beacon {
                first_beacon = false;
            } else {
                tokio::time::sleep(cur_period).await;
            }
            // Compute a stable hash of the current PV set so we don't
            // hold an allocated Vec across the await above.
            let pvs = beacon_source.list_pvs().await;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            use std::hash::{Hash, Hasher};
            let mut sorted = pvs;
            sorted.sort();
            sorted.hash(&mut h);
            let cur_hash = h.finish();
            // Primary, pvxs-faithful signal: a source registry mutation
            // bumped `beaconChange`. Advance `change_count` by the delta
            // so each add/remove is reflected even if the PV-name set is
            // identical before and after (source swap / priority change).
            let cur_registry_change = beacon_source.beacon_change();
            if cur_registry_change != last_registry_change {
                let delta = cur_registry_change.wrapping_sub(last_registry_change);
                change_count = change_count.wrapping_add(delta as u16);
                last_registry_change = cur_registry_change;
            }
            // Fallback signal: the enumerated PV set changed without a
            // registry mutation (e.g. a source mutated its own PV list).
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
                protocol,
            );
            for ep in &beacon_destinations {
                let dest = ep.addr;
                // Per-destination dispatch:
                // - V4 multicast → `send_multicast_v4`, which applies the
                //   endpoint TTL (`,ttl`) and selects the egress NIC by
                //   `@iface` (pvxs `mcast_prep_sendto`, evhelper.cpp:556-577).
                //   No `@iface` fans out every eligible NIC, matching the old
                //   `fanout_to` behaviour but now at the correct TTL.
                // - V4 limited broadcast → per-NIC `fanout_to` (TTL-agnostic).
                // - V4 subnet-broadcast / unicast → `send_to`'s source-bind
                //   NIC pick.
                // PR #205 IPv6 Stage 5: v6 destinations route through the
                // dedicated v6 send socket. The `enable_ipv6_udp` false path
                // never queues v6 destinations here, so the missing-v6-socket
                // branch is defensive.
                let result = match dest {
                    SocketAddr::V4(v4) if v4.ip().is_multicast() => {
                        // pvxs multicast TTL default is 1 (link-local); an
                        // absent `,ttl` qualifier stays link-local.
                        let ttl = ep.ttl.unwrap_or(1);
                        // Resolve the optional `@iface` to the egress NIC's
                        // IPv4 at send time (pvxs IfaceMap parity). A bad spec
                        // disables the NIC filter (fan out every eligible NIC)
                        // rather than dropping the beacon.
                        let iface_ip = ep
                            .iface
                            .as_deref()
                            .and_then(|spec| crate::config::env::resolve_iface_v4(spec).ok());
                        beacon_socket
                            .send_multicast_v4(&beacon, dest, ttl, iface_ip)
                            .await
                            .map(|_| ())
                    }
                    SocketAddr::V4(v4) if v4.ip().is_broadcast() => {
                        beacon_socket.fanout_to(&beacon, dest).await.map(|_| ())
                    }
                    SocketAddr::V4(_) => beacon_socket.send_to(&beacon, dest).await.map(|_| ()),
                    SocketAddr::V6(_) => match &beacon_socket_v6_for_task {
                        Some(s6) => s6.send_to(&beacon, dest).await.map(|_| ()),
                        None => Err(std::io::Error::new(
                            std::io::ErrorKind::AddrNotAvailable,
                            "v6 beacon destination configured without a v6 send socket",
                        )),
                    },
                };
                match result {
                    Ok(()) => {
                        beacon_send_errs.remove(&dest);
                    }
                    Err(e) => {
                        if beacon_send_errs.insert(dest) {
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
        Ok(s) => {
            // pvxs `udp_collector.cpp::UDPCollector::UDPCollector`
            // (commit a064677e3625) enables SO_RXQ_OVFL on the
            // collector socket so the kernel's per-socket dropped-
            // datagram counter is delivered as a cmsg on each
            // recvmsg. No-op on non-Linux.
            if let Err(e) = enable_so_rxq_ovfl_for_socket(&s) {
                debug!("loopback SO_RXQ_OVFL enable failed (non-fatal): {e}");
            }
            Some(Arc::new(s))
        }
        Err(e) => {
            debug!("loopback ORIGIN_TAG socket bind failed, running without: {e}");
            None
        }
    };
    // pvxs parity for the per-NIC SEARCH responder bundle. Each NIC
    // socket gets its own kernel counter; we track the previous
    // observed value per iface IP and log only on transitions.
    if let Err(e) = socket.enable_so_rxq_ovfl() {
        debug!("per-NIC SO_RXQ_OVFL enable failed (non-fatal): {e}");
    }
    let mut prev_drops_per_iface: std::collections::HashMap<Ipv4Addr, u32> =
        std::collections::HashMap::new();
    let mut prev_drops_lo: u32 = 0;

    // 64 KB receive buffer — IPv4 maximum. The previous 1500-byte
    // (Ethernet MTU) cap silently truncated large multi-PV searches:
    // pvxs clients pack many SEARCH messages into one datagram and a
    // gateway-restart storm can easily exceed 1500 bytes. 64 KB
    // matches the kernel ceiling without truncation. Heap-allocated
    // because 64 KB on the per-task stack is large; one allocation
    // amortized across the listener's lifetime.
    let mut buf = vec![0u8; 64 * 1024];
    let mut lo_buf = vec![0u8; 64 * 1024];
    // Local IPv4 interface addresses for the loopback ORIGIN_TAG
    // origin-locality check (pvxs `ifinfo.byAddr`). See
    // `classify_loopback_datagram`.
    let local_v4 = local_v4_addrs();
    loop {
        // Receive on whichever path is ready first. The per-NIC bundle
        // handles regular SEARCH/beacon traffic; the loopback mcast
        // socket (if bound) catches CMD_ORIGIN_TAG forwards from local
        // PVA peers. Both paths feed `process_search_datagram` with a
        // tagged origin so anti-loop and reply-routing rules apply.
        let recv_direct = socket.recv_with_meta_with_drops(&mut buf);
        let recv_lo = async {
            match lo_mcast.as_ref() {
                Some(s) => recv_from_with_drop_count_socket(s, &mut lo_buf)
                    .await
                    .map(Some),
                // No loopback socket: never resolve.
                None => {
                    std::future::pending::<std::io::Result<Option<(usize, SocketAddr, u32)>>>()
                        .await
                }
            }
        };
        // No `biased;` ordering — under SEARCH bursts on the wire the
        // per-NIC path can dominate the loopback path arbitrarily long
        // and we want fair round-robin between the two so a co-resident
        // PVA peer's ORIGIN_TAG forwards aren't starved of recv slots.
        tokio::select! {
            r = recv_direct => {
                let (meta, drops) = match r {
                    Ok(m) => m,
                    Err(e) => { debug!("udp recv error: {e}"); continue; }
                };
                // Surface per-NIC kernel drop transitions exactly
                // once per change — pvxs `udp_collector.cpp:55-67`
                // logs at debug on `prev != current && current != 0`.
                let prev = prev_drops_per_iface.insert(meta.iface_ip, drops).unwrap_or(0);
                if drops != 0 && drops != prev {
                    debug!(
                        iface_ip = %meta.iface_ip,
                        prev,
                        drops,
                        "PVA UDP collector socket buffer overflow on per-NIC socket"
                    );
                }
                let frame_len = meta.n;
                if !filter_inbound(meta.src, &ignore_addrs) {
                    continue;
                }
                let outputs = process_search_datagram(
                    &source,
                    lo_mcast.is_some(),
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
                dispatch_search_outputs(&socket, lo_mcast.as_ref(), outputs).await;
            }
            r = recv_lo => {
                let r = match r {
                    Ok(Some(v)) => v,
                    Ok(None) => continue,
                    Err(e) => { debug!("loopback udp recv error: {e}"); continue; }
                };
                let (n, src, drops) = r;
                if drops != 0 && drops != prev_drops_lo {
                    debug!(
                        prev = prev_drops_lo,
                        drops,
                        "PVA UDP collector socket buffer overflow on loopback ORIGIN_TAG socket"
                    );
                }
                prev_drops_lo = drops;
                if !filter_inbound(src, &ignore_addrs) {
                    continue;
                }
                let raw = &lo_buf[..n];
                // Classify the datagram by its first PVA header before any
                // SEARCH matching (pvxs `udp_collector.cpp::process_one`).
                // A valid CMD_ORIGIN_TAG with a wildcard/local origin is
                // processed as FromOriginTag; a bare CMD_SEARCH (some peers
                // forward to `224.0.0.128` without the tag,
                // `udp_collector.cpp:401-404`) is processed as Forwarded.
                // A malformed tag, a non-local origin tag, or any other
                // first header is dropped — NOT normalized into a forwarded
                // SEARCH. The
                // `Forwarded`/`FromOriginTag` rules downstream forbid
                // re-forwarding (anti-loop) and reject `isAny()` reply
                // addresses, so there is no amplification risk.
                let (inner, reply_iface_ip, origin) =
                    match classify_loopback_datagram(raw, &local_v4) {
                        LoopbackClass::OriginTag {
                            reply_iface_ip,
                            inner,
                        } => (inner, reply_iface_ip, Origin::FromOriginTag),
                        LoopbackClass::BareSearch => {
                            (raw, Ipv4Addr::UNSPECIFIED, Origin::Forwarded)
                        }
                        LoopbackClass::Drop => continue,
                    };
                let outputs = process_search_datagram(
                    &source,
                    lo_mcast.is_some(),
                    udp_port,
                    inner,
                    src,
                    reply_iface_ip,
                    origin,
                    tcp_port,
                    guid,
                    protocol,
                )
                .await;
                dispatch_search_outputs(&socket, lo_mcast.as_ref(), outputs).await;
            }
        }
    }

    // Beacon task is aborted via the AbortOnDrop guard (`_beacon_guard`)
    // when this function unwinds.
    #[allow(unreachable_code)]
    Ok(())
}

/// Send what [`process_search_datagram`] decided, over the sockets this
/// responder owns. The async responder's send tail, lifted out of the
/// decoder unchanged: same destinations, same NIC pinning, same
/// `AddrNotAvailable` fallback, same `debug!` text on failure.
///
/// An [`SearchOutput::OriginTagForward`] can only be produced when the
/// caller passed `origin_tag_forwarding = true`, which the responder does
/// exactly when `lo_mcast` is bound — so the `None` arm below is
/// unreachable in this caller and logged rather than silently dropped.
async fn dispatch_search_outputs(
    socket: &AsyncUdpV4,
    lo_mcast: Option<&Arc<tokio::net::UdpSocket>>,
    outputs: Vec<SearchOutput>,
) {
    for out in outputs {
        match out {
            SearchOutput::Reply {
                dest,
                iface_hint,
                bytes,
            } => {
                let send = match iface_hint {
                    None => socket.send_to(&bytes, dest).await,
                    Some(iface_ip) => match socket.send_via(&bytes, dest, iface_ip).await {
                        Ok(n) => Ok(n),
                        Err(e) if e.kind() == std::io::ErrorKind::AddrNotAvailable => {
                            socket.send_to(&bytes, dest).await
                        }
                        Err(e) => Err(e),
                    },
                };
                if let Err(e) = send {
                    // `via` keeps the pre-refactor text, including the
                    // `0.0.0.0` rendering of the no-NIC-pin case.
                    let via = iface_hint.unwrap_or(Ipv4Addr::UNSPECIFIED);
                    debug!("udp send to {dest} via {via}: {e}");
                }
            }
            SearchOutput::OriginTagForward { dest, bytes } => match lo_mcast {
                Some(lo) => {
                    if let Err(e) = lo.send_to(&bytes, dest).await {
                        debug!("ORIGIN_TAG forward to {dest}: {e}");
                    }
                }
                None => debug!("ORIGIN_TAG forward to {dest} dropped: no loopback socket"),
            },
        }
    }
}

/// IPv6 UDP responder. Binds `[::]:udp_port` (kernel default
/// `IPV6_V6ONLY` controls whether IPv4-mapped traffic also lands
/// here — Linux is dual-stack by default, BSD/macOS are v6-only)
/// and answers `Search` queries against `source` with a
/// `SearchResponse` containing this server's TCP port and GUID.
///
/// Companion to the existing IPv4 responder driven by
/// [`run_udp_responder_with_config`]. Both can run in parallel
/// (Stage 2 of the PR #205 IPv6 effort) and share a single GUID
/// so a client that sees both flavours of SEARCH_RESPONSE
/// resolves them to the same server identity.
///
/// Replies use the UDP source address as the destination,
/// ignoring any `reply_addr` announced inside the SEARCH payload —
/// today that field decodes IPv4-only via
/// `parse_search_request` and surfaces as `None` for v6 traffic.
/// Falling back to the source matches the pvxs
/// `udp_collector.cpp:540-548` "isAny() ⇒ reply to sender" path.
///
/// Beacon emission stays on the IPv4 path for this stage; v6
/// multicast beacons (FF0E::400) need per-NIC multicast scope
/// management that arrives in a follow-up commit.
pub async fn run_udp_responder_v6(
    source: DynSource,
    udp_port: u16,
    tcp_port: u16,
    guid: [u8; 12],
    protocol: &'static str,
    ignore_addrs: Vec<(IpAddr, u16)>,
) -> PvaResult<()> {
    use socket2::{Domain, Protocol, Socket, Type};
    // Build the v6 socket explicitly via socket2 so we can force
    // `IPV6_V6ONLY=1`. On Linux the kernel default is `0` (dual-stack):
    // a `[::]:port` socket would also claim v4 traffic and overlap
    // with the parallel v4 responder bundle, causing surprising
    // duplicate handling or EADDRINUSE on the per-NIC binds. Mirroring
    // `bind_beacon_send_v6` keeps the v4 and v6 responder lanes
    // strictly disjoint regardless of platform default.
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))
        .map_err(crate::error::PvaError::Io)?;
    if let Err(e) = sock.set_only_v6(true) {
        debug!("v6 responder: set_only_v6 failed: {e}");
    }
    // SO_REUSEADDR so a restart picks up the same port without TIME_WAIT.
    if let Err(e) = sock.set_reuse_address(true) {
        debug!("v6 responder: set_reuse_address failed: {e}");
    }
    sock.set_nonblocking(true)
        .map_err(crate::error::PvaError::Io)?;
    let bind_addr = SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), udp_port);
    sock.bind(&bind_addr.into())
        .map_err(crate::error::PvaError::Io)?;
    let std_sock: std::net::UdpSocket = sock.into();
    let socket = tokio::net::UdpSocket::from_std(std_sock).map_err(crate::error::PvaError::Io)?;
    if let Err(e) = enable_so_rxq_ovfl_for_socket(&socket) {
        debug!("v6 SO_RXQ_OVFL enable failed (non-fatal): {e}");
    }
    let socket = Arc::new(socket);
    debug!(?udp_port, "IPv6 UDP search responder started (V6ONLY=1)");

    let mut buf = vec![0u8; 64 * 1024];
    let mut prev_drops_v6: u32 = 0;
    loop {
        let (n, peer, drops) = match recv_from_with_drop_count_socket(&socket, &mut buf).await {
            Ok(v) => v,
            Err(e) => {
                debug!("v6 udp recv error: {e}");
                continue;
            }
        };
        if drops != 0 && drops != prev_drops_v6 {
            debug!(
                prev = prev_drops_v6,
                drops, "PVA UDP collector socket buffer overflow on v6 socket"
            );
        }
        prev_drops_v6 = drops;
        if !filter_inbound(peer, &ignore_addrs) {
            continue;
        }
        process_v6_search_datagram(&source, &socket, &buf[..n], peer, tcp_port, guid, protocol)
            .await;
    }
}

/// Resolve a SEARCH_RESPONSE destination from the announced reply
/// address/port and the UDP source, for the IPv6 responder.
///
/// Mirrors pvxs `udp_collector.cpp:367-380`: the 16-byte reply address
/// decodes into `replyDest`, then `replyDest.setPort(port)` runs
/// unconditionally — on both the explicit-address branch and the
/// wildcard `server.isAny()` branch. So:
///
/// - Explicit (non-wildcard) reply address: reply to the advertised
///   address AND port.
/// - Wildcard reply address: reply to the sender's source IP but with
///   the advertised port (pvxs `:380 setPort`), not the UDP source port.
///   This is correct because the Rust v6 SEARCH client now advertises
///   the actual v6 receive-socket port (`client_native/search_engine.rs`
///   `bind_ephemeral_udp_v6` binds an independent ephemeral port whose
///   number is stamped into the v6 SEARCH frame's `response_port`), so a
///   pvxs-compatible split-socket client that sends from one port and
///   listens on the advertised port is answered on the port it listens
///   on.
fn resolve_reply_dest(
    reply_addr: Option<IpAddr>,
    reply_port: u16,
    udp_src: SocketAddr,
) -> SocketAddr {
    match reply_addr {
        Some(ip) => SocketAddr::new(ip, reply_port),
        None => SocketAddr::new(udp_src.ip(), reply_port),
    }
}

/// Stripped-down `process_search_datagram` for the v6 responder.
/// Skips the IPv4-only ORIGIN_TAG forwarding path entirely (v6 has
/// its own group conventions that aren't wired yet) but honours the
/// in-payload reply address/port: pvxs decodes that field into a full
/// `SockAddr` and uses an advertised non-wildcard destination
/// (`udp_collector.cpp:367-380`), so a v6 SEARCH that points the reply
/// at a different v6 address or port than the UDP source tuple is
/// answered there. Wildcard falls back to the UDP source IP with the
/// advertised port.
async fn process_v6_search_datagram(
    source: &DynSource,
    socket: &tokio::net::UdpSocket,
    frame: &[u8],
    udp_src: SocketAddr,
    tcp_port: u16,
    guid: [u8; 12],
    protocol: &'static str,
) {
    let mut pos = 0usize;
    while pos + PvaHeader::SIZE <= frame.len() {
        let chunk = &frame[pos..];
        let consumed = match parse_search_request(chunk) {
            Some(req) => {
                let consumed = req.consumed;
                // Honour the SEARCH's announced reply destination: an
                // advertised non-wildcard v6 address/port wins (pvxs
                // `udp_collector.cpp:377-380`); a wildcard falls back to
                // the UDP source tuple. Pre-fix the v6 responder always
                // replied to the UDP source and ignored an explicit v6
                // reply endpoint. See `resolve_reply_dest` for the
                // wildcard-port deviation and its cross-request.
                let reply_dest = resolve_reply_dest(req.reply_addr, req.reply_port, udp_src);
                // The v6 responder must apply the same protocol gate as
                // v4/TCP — without it a TLS-only client gets `found=1`
                // from a tcp-only server. Shared helper. Requester
                // endpoint is the resolved reply destination.
                let matched_cids = search_matched_cids(source, &req, protocol, reply_dest).await;
                // pvxs `server.cpp:730-732` (also reached for v6
                // SEARCH via the same handler): honour `MustReply`
                // with an empty (`found=0`, `nreply=0`) response so
                // pvlist-style discovery sees the server.
                if !matched_cids.is_empty() || req.must_reply {
                    let resp = build_search_response_proto(
                        guid,
                        req.seq,
                        tcp_port,
                        &matched_cids,
                        req.byte_order,
                        protocol,
                    );
                    if let Err(e) = socket.send_to(&resp, reply_dest).await {
                        debug!("v6 udp send to {reply_dest}: {e}");
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
    protocol: &str,
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
    // beacons advertise the runtime transport. Pre-fix
    // Rust hard-coded "tcp" even when SEARCH_RESPONSE said "tls",
    // making discovery inconsistent across the two announcement
    // channels. pvxs `server.cpp:738-745, 773-781` keeps both
    // consistent.
    encode_string_into(protocol, order, &mut payload);
    payload.put_u8(0xFF); // null serverStatus marker (matches pvxs)
    let header = PvaHeader::application(true, order, Command::Beacon.code(), payload.len() as u32);
    let mut out = Vec::new();
    header.write_into(&mut out);
    out.extend_from_slice(&payload);
    out
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
#[derive(Debug, PartialEq, Eq)]
enum LoopbackClass<'a> {
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
fn local_v4_addrs() -> HashSet<Ipv4Addr> {
    let mut set = HashSet::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if let if_addrs::IfAddr::V4(v4) = iface.addr {
                set.insert(v4.ip);
            }
        }
    }
    set
}

/// Classify a loopback `CMD_ORIGIN_TAG` socket datagram by its first PVA
/// header. See [`LoopbackClass`] for the pvxs parity rules this enforces.
fn classify_loopback_datagram<'a>(
    raw: &'a [u8],
    local_v4: &HashSet<Ipv4Addr>,
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
    if header.command == Command::OriginTag.code() {
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
    } else if header.command == Command::Search.code() {
        LoopbackClass::BareSearch
    } else {
        // Any other first header (BEACON, etc.) is not part of the
        // loopback SEARCH-forwarding contract — drop.
        LoopbackClass::Drop
    }
}

/// A decoded BEACON, holding only the fields the multicast shim
/// re-emits. pvxs forwards a beacon by writing guid, a zeroed
/// flags/seq/change word, the (resolved) server address + TCP port, the
/// protocol, and a null serverStatus (`tools/mshim.cpp:176-200`), so the
/// transient flags/seq/change are decoded-and-discarded here.
struct BeaconForward {
    order: ByteOrder,
    guid: [u8; 12],
    /// Server address announced in the beacon. `None` when it is the
    /// unspecified sentinel (`0.0.0.0` / `::`), in which case the shim
    /// resolves it to the datagram source — matching pvxs `UDPManager`,
    /// which fills `Beacon.server` from the source on `isAny`.
    server_addr: Option<IpAddr>,
    server_port: u16,
    /// Advertised protocol, raw bytes (no UTF-8 validation — pvxs treats
    /// it as an opaque `std::string`).
    protocol: Vec<u8>,
    consumed: usize,
}

/// Decode a single BEACON message at the start of `frame`. `None` for a
/// non-beacon / malformed frame (mirrors [`parse_search_request`]).
fn parse_beacon(frame: &[u8]) -> Option<BeaconForward> {
    if frame.len() < PvaHeader::SIZE {
        return None;
    }
    let mut cur = Cursor::new(frame);
    let header = PvaHeader::decode(&mut cur).ok()?;
    // UDP datagrams can carry neither control messages nor segmentation
    // (segment bits are TCP-only). pvxs drops a segmented UDP datagram
    // before decode (udp_collector.cpp:329-340).
    if header.command != Command::Beacon.code()
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
    let guid_bytes = p.get_bytes(12).ok()?;
    let mut guid = [0u8; 12];
    guid.copy_from_slice(&guid_bytes);
    // flags(u8) + seq(u8) + change(u16): transient, decoded-and-discarded
    // (pvxs writes a zero word in their place when forwarding).
    let _ = p.get_u8().ok()?;
    let _ = p.get_u8().ok()?;
    let _ = p.get_u16(order).ok()?;
    let addr_bytes = p.get_bytes(16).ok()?;
    let mut addr16 = [0u8; 16];
    addr16.copy_from_slice(&addr_bytes);
    let server_addr = match ip_from_bytes(&addr16) {
        Some(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    };
    let server_port = p.get_u16(order).ok()?;
    let plen = decode_size(&mut p, order).ok().flatten()? as usize;
    let protocol = p.get_bytes(plen).ok()?;
    // A serverStatus field follows (a null-type `0xFF` marker or a full
    // Field/Value). The shim always re-emits a null serverStatus, so its
    // exact bytes are irrelevant — requiring the fixed prefix above to
    // parse cleanly is enough to recognize the frame.
    Some(BeaconForward {
        order,
        guid,
        server_addr,
        server_port,
        protocol,
        consumed: PvaHeader::SIZE + payload_len,
    })
}

/// A PVA UDP datagram the multicast shim (`mshim-rs`) recognizes and
/// rewrites before forwarding.
///
/// pvxs `tools/mshim.cpp` does NOT relay the original datagram bytes: it
/// decodes each packet as a `UDPManager::Search` / `UDPManager::Beacon`,
/// drops anything it cannot decode, and reconstructs a fresh wire body
/// per forward destination (mshim.cpp:95-200). This type captures that
/// decode so the shim can forward only recognized SEARCH/BEACON and, for
/// SEARCH, set the `Unicast` flag (`0x80`) per destination.
///
/// The inner representation is private so the public API stays the two
/// methods below; the shim never needs the decoded fields directly.
pub struct ForwardableDatagram(ForwardKind);

enum ForwardKind {
    Search(SearchRequest),
    Beacon(BeaconForward),
}

impl ForwardableDatagram {
    /// Decode only the FIRST SEARCH/BEACON message in a UDP datagram and
    /// ignore any trailing bytes — the pvxs `UDPCollector::process_one`
    /// contract (`udp_collector.cpp:329-352`: read one PVA header, switch
    /// once on the command, dispatch the listener once, then break; for a
    /// BEACON the remaining bytes are an explicitly-ignored serverStatus
    /// blob, `:480`). Returns `None` when the first message is not a
    /// recognized SEARCH/BEACON, so the shim drops the whole datagram.
    ///
    /// This is the default `mshim-rs` forward boundary: pvxs `mshim`
    /// forwards only the first decoded message, never a chained tail.
    /// [`decode_all`](Self::decode_all) is a Rust-only extension that
    /// additionally drains chained messages; it is not the pvxs-faithful
    /// default.
    pub fn decode_first(datagram: &[u8]) -> Option<Self> {
        if let Some(req) = parse_search_request(datagram) {
            Some(Self(ForwardKind::Search(req)))
        } else {
            parse_beacon(datagram).map(|b| Self(ForwardKind::Beacon(b)))
        }
    }

    /// Decode every chained SEARCH/BEACON message in one UDP datagram, in
    /// wire order. A datagram normally carries exactly one message, and
    /// pvxs dispatches only that first message (see
    /// [`decode_first`](Self::decode_first)); this multi-message drain is
    /// a Rust-only extension, NOT pvxs-faithful — `mshim-rs` must not use
    /// it for default forwarding or it amplifies one received datagram
    /// into several forwarded ones. Stops at the first undecodable /
    /// non-SEARCH-or-BEACON message: the returned vec holds the
    /// recognized prefix. An empty vec means "drop the datagram".
    pub fn decode_all(datagram: &[u8]) -> Vec<Self> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < datagram.len() {
            let rest = &datagram[pos..];
            if let Some(req) = parse_search_request(rest) {
                let advance = req.consumed;
                out.push(Self(ForwardKind::Search(req)));
                if advance == 0 {
                    break;
                }
                pos += advance;
            } else if let Some(b) = parse_beacon(rest) {
                let advance = b.consumed;
                out.push(Self(ForwardKind::Beacon(b)));
                if advance == 0 {
                    break;
                }
                pos += advance;
            } else {
                break;
            }
        }
        out
    }

    /// True for a SEARCH — only SEARCH carries the destination-dependent
    /// `Unicast` flag. A BEACON forwards identically to every
    /// destination, so the shim can log/handle the two cases distinctly.
    pub fn is_search(&self) -> bool {
        matches!(self.0, ForwardKind::Search(_))
    }

    /// Rebuild the wire frame to forward to one destination.
    ///
    /// * SEARCH — re-encodes preserving the search ID, `MustReply`,
    ///   reply destination, protocol list, and channel queries, setting
    ///   the `Unicast` flag iff `dest_unicast` (pvxs mshim.cpp:140-146:
    ///   clear for multicast/broadcast, set otherwise). An `isAny` reply
    ///   address is resolved to `source`, so the forwarded SEARCH names
    ///   the ORIGINAL requester as the reply target rather than the shim.
    /// * BEACON — re-encodes guid, a zeroed flags/seq/change word, the
    ///   server address (resolved to `source` when the beacon announced
    ///   `isAny`), TCP port, protocol, and a null serverStatus. The
    ///   header command is `CMD_BEACON`; `dest_unicast` is unused.
    pub fn rebuild_for(&self, dest_unicast: bool, source: SocketAddr) -> Vec<u8> {
        match &self.0 {
            ForwardKind::Search(req) => rebuild_search_forward(req, dest_unicast, source),
            ForwardKind::Beacon(b) => rebuild_beacon_forward(b, source),
        }
    }
}

fn rebuild_search_forward(req: &SearchRequest, dest_unicast: bool, source: SocketAddr) -> Vec<u8> {
    let order = req.byte_order;
    // isAny reply addr means "reply to the UDP source"; the forwarded
    // SEARCH must carry the resolved requester address or the downstream
    // server would reply to the shim. Mirrors `resolve_reply_dest` and
    // pvxs UDPManager filling `Search.replyDest` from the source.
    let reply_ip = req.reply_addr.unwrap_or_else(|| source.ip());
    let mut flags: u8 = 0;
    if dest_unicast {
        flags |= 0x80; // pva_search_flags::Unicast
    }
    if req.must_reply {
        flags |= 0x01; // pva_search_flags::MustReply, preserved verbatim
    }

    let mut p = Vec::new();
    p.put_u32(req.seq, order);
    p.put_u8(flags);
    p.extend_from_slice(&[0u8; 3]); // reserved
    p.extend_from_slice(&ip_to_bytes(reply_ip));
    p.put_u16(req.reply_port, order);
    // protocol list, preserved verbatim (raw bytes — "tcp"/"tls"/legacy).
    encode_size_into(req.protocols.len() as u32, order, &mut p);
    for proto in &req.protocols {
        encode_size_into(proto.len() as u32, order, &mut p);
        p.extend_from_slice(proto);
    }
    // channel queries, preserved verbatim.
    p.put_u16(req.queries.len() as u16, order);
    for (cid, name) in &req.queries {
        p.put_u32(*cid, order);
        encode_string_into(name, order, &mut p);
    }
    let header = PvaHeader::application(false, order, Command::Search.code(), p.len() as u32);
    let mut out = Vec::new();
    header.write_into(&mut out);
    out.extend_from_slice(&p);
    out
}

fn rebuild_beacon_forward(b: &BeaconForward, source: SocketAddr) -> Vec<u8> {
    let order = b.order;
    let server_ip = b.server_addr.unwrap_or_else(|| source.ip());
    let mut payload = Vec::new();
    payload.extend_from_slice(&b.guid);
    // flags + seq + change: pvxs writes a single zero word here
    // (mshim.cpp:182 "skip flags, seq, and change count. unused").
    payload.put_u8(0);
    payload.put_u8(0);
    payload.put_u16(0, order);
    payload.extend_from_slice(&ip_to_bytes(server_ip));
    payload.put_u16(b.server_port, order);
    encode_size_into(b.protocol.len() as u32, order, &mut payload);
    payload.extend_from_slice(&b.protocol);
    payload.put_u8(0xFF); // null serverStatus (pvxs mshim.cpp:188)
    // Header command is CMD_BEACON. pvxs mshim.cpp:199 writes CMD_SEARCH
    // here (a copy-paste slip from onSearch — the adjacent log string on
    // :191 is also the onSearch message); forwarding a beacon mislabeled
    // as a search would be malformed, so we emit the correct command.
    let header = PvaHeader::application(true, order, Command::Beacon.code(), payload.len() as u32);
    let mut out = Vec::new();
    header.write_into(&mut out);
    out.extend_from_slice(&payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_native::MonitorStream;
    // Named from its new home rather than re-exported through the module
    // under test: production `udp` has no forward-frame call of its own
    // any more, so a `pub(crate) use` at module scope would be an unused
    // import in a non-test build.
    use crate::server_native::search_engine::try_build_forward_frame;
    use crate::server_native::source::OpError;
    use std::net::SocketAddrV4;

    /// Decode + dispatch in one call, with the argument list
    /// `process_search_datagram` had before stage B split the sends out of
    /// it. These cases are end-to-end by design — they assert on datagrams
    /// a real sniffer socket receives, which is what makes them wire-golden
    /// — so they keep driving both halves rather than inspecting the
    /// returned [`SearchOutput`] list.
    #[allow(clippy::too_many_arguments)]
    async fn process_and_dispatch(
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
        let outputs = process_search_datagram(
            source,
            lo_mcast.is_some(),
            udp_port,
            frame,
            udp_src,
            reply_iface_ip,
            origin,
            tcp_port,
            guid,
            protocol,
        )
        .await;
        dispatch_search_outputs(socket, lo_mcast, outputs).await;
    }

    /// Stage B's point, stated as a test: the decode half runs with **no
    /// socket at all**. No `AsyncUdpV4`, no `tokio::net::UdpSocket`, and so
    /// no `epics_base_rs::net` — none of which exist on the RTEMS target
    /// (`doc/pva-rtems-item7-design.md` §1), which is why the blocking
    /// driver could not reuse this decode before the split.
    ///
    /// The reply bytes must be exactly what the wire builder produces, so
    /// a driver that sends them over `std::net::UdpSocket` is byte-identical
    /// to this one.
    #[epics_macros_rs::epics_test]
    async fn the_search_decoder_produces_replies_without_a_socket() {
        use crate::server_native::source::ChannelSource;
        use std::sync::Arc;

        struct OneSource;
        #[allow(clippy::manual_async_fn)]
        impl ChannelSource for OneSource {
            fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
                async { vec!["MY:PV".into()] }
            }
            fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
                let m = name == "MY:PV";
                async move { m }
            }
            fn get_introspection(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<crate::pvdata::FieldDesc>> + Send
            {
                async { None }
            }
            fn get_value(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<crate::pvdata::PvField>> + Send
            {
                async { None }
            }
            fn put_value(
                &self,
                _: &str,
                _: crate::pvdata::PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only".into()) }
            }
            fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<crate::pvdata::PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(OneSource);
        let codec = PvaCodec { big_endian: false };
        // seq = 7, cid = 42, reply announced at 127.0.0.1:9999.
        let frame = codec.build_search(7, 42, "MY:PV", [127, 0, 0, 1], 9999, false);
        let udp_src = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 9, 5), 30000));
        let guid = [3u8; 12];

        let outputs = process_search_datagram(
            &source,
            false, // no ORIGIN_TAG forwarding
            5076,
            &frame,
            udp_src,
            Ipv4Addr::new(192, 168, 9, 10), // NIC the SEARCH arrived on
            Origin::Direct,
            5075,
            guid,
            "tcp",
        )
        .await;

        let golden = build_search_response_proto(guid, 7, 5075, &[42], ByteOrder::Little, "tcp");
        assert_eq!(
            outputs,
            vec![SearchOutput::Reply {
                dest: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999)),
                iface_hint: Some(Ipv4Addr::new(192, 168, 9, 10)),
                bytes: golden,
            }],
            "the decoder must return the announced reply dest, the arrival NIC, \
             and byte-identical SEARCH_RESPONSE bytes"
        );

        // The `reply_iface_ip = UNSPECIFIED` sentinel — set on ORIGIN_TAG
        // packets whose peeled origDest was `isAny()` — must resolve to
        // "no NIC pin" rather than travel on as an address.
        let outputs = process_search_datagram(
            &source,
            false,
            5076,
            &frame,
            udp_src,
            Ipv4Addr::UNSPECIFIED,
            Origin::Direct,
            5075,
            guid,
            "tcp",
        )
        .await;
        assert!(
            matches!(
                outputs.as_slice(),
                [SearchOutput::Reply {
                    iface_hint: None,
                    ..
                }]
            ),
            "the UNSPECIFIED sentinel must resolve to `iface_hint: None`, got {outputs:?}"
        );
    }

    /// End-to-end forward path: `process_search_datagram` invoked
    /// with `Origin::Direct` and a unicast-flagged SEARCH MUST emit
    /// a CMD_ORIGIN_TAG-prefixed packet on `224.0.0.128:port` (the
    /// loopback ORIGIN_TAG channel). The sending `lo_mcast` socket is
    /// itself joined to the group with IP_MULTICAST_LOOP=1, so it
    /// receives its own emission; peeling the prefix should yield the
    /// inner SEARCH with the Unicast flag cleared and the reply addr
    /// rewritten to the resolved destination — pvxs
    /// `udp_collector.cpp:387-396` end-to-end parity.
    #[tokio::test]
    async fn forward_path_emits_origin_tag_on_unicast_search() {
        use crate::pvdata::{FieldDesc, PvField};
        use crate::server_native::source::ChannelSource;
        use std::sync::Arc;
        use std::time::Duration;

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
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only test source".into()) }
            }
            fn is_writable(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(EmptySource);
        let socket = Arc::new(AsyncUdpV4::bind(0, false).expect("bind per-NIC"));

        // Bind the lo_mcast send/receive socket on an ephemeral port. It
        // joins 224.0.0.128 with IP_MULTICAST_LOOP=1, so it receives its
        // own forwarded packet — no second co-bound observer needed (a
        // co-bind would require SO_REUSEPORT, which Windows lacks).
        let lo_mcast = Arc::new(bind_loopback_mcast(0).expect("lo_mcast bind"));
        let port = lo_mcast.local_addr().unwrap().port();

        // Build a unicast SEARCH for "MY:PV" with reply 127.0.0.1:9999.
        let codec = PvaCodec { big_endian: false };
        let frame = codec.build_search(7, 42, "MY:PV", [127, 0, 0, 1], 9999, true);

        let udp_src = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 99, 5), 30000));
        // Simulated NIC unicast IP — embedded into the ORIGIN_TAG
        // prefix as the orig destination.
        let reply_iface_ip = Ipv4Addr::new(192, 168, 99, 10);

        process_and_dispatch(
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
        let (n, _src) = tokio::time::timeout(Duration::from_secs(2), lo_mcast.recv_from(&mut buf))
            .await
            .expect("lo_mcast recv timeout — forward not emitted")
            .expect("lo_mcast recv ok");
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
            Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
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
    /// with `FromOriginTag` origin and asserting the group-joined
    /// `lo_mcast` socket never sees a forwarded packet.
    #[tokio::test]
    async fn forward_path_skipped_when_origin_is_from_origin_tag() {
        use crate::pvdata::{FieldDesc, PvField};
        use crate::server_native::source::ChannelSource;
        use std::sync::Arc;
        use std::time::Duration;

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
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only test source".into()) }
            }
            fn is_writable(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(EmptySource);
        let socket = Arc::new(AsyncUdpV4::bind(0, false).expect("bind per-NIC"));
        // lo_mcast is joined to the forward group with IP_MULTICAST_LOOP=1,
        // so it would see any re-forward itself — no co-bound observer (and
        // no Windows-absent SO_REUSEPORT) required.
        let lo_mcast = Arc::new(bind_loopback_mcast(0).expect("lo_mcast bind"));
        let port = lo_mcast.local_addr().unwrap().port();

        let codec = PvaCodec { big_endian: false };
        let frame = codec.build_search(7, 42, "MY:PV", [127, 0, 0, 1], 9999, true);

        process_and_dispatch(
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

        // lo_mcast must NOT receive anything — short timeout proves
        // the absence of forward emission.
        let mut buf = [0u8; 4096];
        let r =
            tokio::time::timeout(Duration::from_millis(150), lo_mcast.recv_from(&mut buf)).await;
        assert!(
            r.is_err(),
            "FromOriginTag origin must NOT trigger re-forward, but lo_mcast got {r:?}"
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
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only".into()) }
            }
            fn is_writable(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
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

        process_and_dispatch(
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

    /// An UNPREFIXED forwarded SEARCH on the loopback mcast path
    /// (`Origin::Forwarded`) MUST still be processed — pvxs tolerates
    /// peers that forward without CMD_ORIGIN_TAG
    /// (`udp_collector.cpp:401-404`) and parses/matches the SEARCH
    /// (`:385-407`). With a concrete (non-isAny) reply address a hosted
    /// PV gets a SEARCH_RESPONSE, and the packet is NOT re-forwarded
    /// (anti-loop: only `Origin::Direct` re-forwards).
    #[tokio::test]
    async fn forwarded_origin_unprefixed_search_is_answered_without_reforward() {
        use crate::pvdata::{FieldDesc, PvField, ScalarType};
        use crate::server_native::source::ChannelSource;
        use std::sync::Arc;
        use std::time::Duration;

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
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only".into()) }
            }
            fn is_writable(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(PresentSource);
        let socket = Arc::new(AsyncUdpV4::bind(0, false).expect("bind per-NIC"));
        // lo_mcast joins the forward group with IP_MULTICAST_LOOP=1, so it
        // catches any re-forward itself — no co-bound observer (and no
        // Windows-absent SO_REUSEPORT) required.
        let lo_mcast = Arc::new(bind_loopback_mcast(0).expect("lo_mcast bind"));
        let port = lo_mcast.local_addr().unwrap().port();
        // Requester socket — the concrete reply destination.
        let requester = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("requester bind");
        let requester_addr = requester.local_addr().unwrap();

        // Unprefixed SEARCH for "MY:PV" with a concrete reply addr/port.
        let codec = PvaCodec { big_endian: false };
        let frame =
            codec.build_search(7, 42, "MY:PV", [127, 0, 0, 1], requester_addr.port(), false);

        process_and_dispatch(
            &source,
            &socket,
            Some(&lo_mcast),
            port,
            &frame,
            // simulated forwarder as the UDP source (not the requester)
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30000)),
            Ipv4Addr::UNSPECIFIED,
            Origin::Forwarded,
            5076,
            [0u8; 12],
            "tcp",
        )
        .await;

        // The requester MUST receive a SEARCH_RESPONSE.
        let mut buf = [0u8; 4096];
        let got = tokio::time::timeout(Duration::from_millis(500), requester.recv_from(&mut buf))
            .await
            .expect("requester must receive a SEARCH_RESPONSE for the forwarded SEARCH")
            .expect("recv ok");
        let resp = PvaHeader::decode(&mut Cursor::new(&buf[..got.0])).expect("decode header");
        assert_eq!(
            resp.command,
            Command::SearchResponse.code(),
            "reply to a forwarded SEARCH must be a SEARCH_RESPONSE"
        );

        // lo_mcast MUST NOT see a re-forward (anti-loop).
        let mut obuf = [0u8; 4096];
        let reforward =
            tokio::time::timeout(Duration::from_millis(150), lo_mcast.recv_from(&mut obuf)).await;
        assert!(
            reforward.is_err(),
            "Forwarded origin must NOT re-forward, but lo_mcast got {reforward:?}"
        );
    }

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

    /// a UDP SEARCH for the built-in `server` PV MUST NOT be
    /// answered — pvxs `ServerSource::onSearch` is empty so `server`
    /// resolves only by direct TCP connect, never by broadcast
    /// discovery. The built-in `ServerInfoSource::searchable` returns
    /// `false`, so `process_search_datagram` adds no matched CID and
    /// (with `must_reply=false`) emits nothing. The same source's
    /// `get_value("server")` — the direct-connect GET path — still
    /// returns the server-info structure.
    #[tokio::test]
    async fn server_pv_not_answered_to_udp_search_but_direct_connect_works() {
        use crate::server_native::server_info::{SERVER_PV_NAME, ServerInfoSource};
        use crate::server_native::source::ChannelSource;
        use std::sync::Arc;
        use std::time::Duration;

        let server_src = ServerInfoSource::new(|| async { Vec::<String>::new() });

        // Direct-connect path still resolves `server`.
        assert!(
            server_src.has_pv(SERVER_PV_NAME).await,
            "has_pv must still resolve `server` for direct TCP connect"
        );
        assert!(
            !server_src.searchable(SERVER_PV_NAME).await,
            "`server` must NOT be UDP-search-advertised"
        );
        // pvxs `server` has no GET surface (onRPC only); the direct
        // path reaches it via RPC, and a GET returns no prototype/value.
        assert!(
            server_src.get_value(SERVER_PV_NAME).await.is_none(),
            "`server` has no GET surface (pvxs onRPC-only)"
        );

        // UDP search path: a broadcast SEARCH naming `server` must
        // produce no reply on the sniffer socket.
        let source: DynSource = Arc::new(server_src);
        let socket = Arc::new(AsyncUdpV4::bind(0, false).expect("bind per-NIC"));
        let sniffer = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("sniffer bind");
        let sniffer_addr = sniffer.local_addr().unwrap();

        // Non-unicast SEARCH (no MustReply) for the literal name
        // `server`, reply addr = the sniffer socket.
        let codec = PvaCodec { big_endian: false };
        let frame = codec.build_search(
            7,
            42,
            SERVER_PV_NAME,
            [127, 0, 0, 1],
            sniffer_addr.port(),
            false,
        );

        process_and_dispatch(
            &source,
            &socket,
            None,
            5076,
            &frame,
            sniffer_addr,
            Ipv4Addr::LOCALHOST,
            Origin::Direct,
            5076,
            [0u8; 12],
            "tcp",
        )
        .await;

        let mut buf = [0u8; 4096];
        let r = tokio::time::timeout(Duration::from_millis(200), sniffer.recv_from(&mut buf)).await;
        assert!(
            r.is_err(),
            "UDP SEARCH for `server` must NOT be answered; sniffer got {r:?}"
        );
    }

    /// The shared SEARCH match rule forwards the requester endpoint to
    /// `ChannelSource::searchable_from`, so a source can claim a PV only
    /// for some requesters (pvxs `Search::source()`). Because the v4
    /// UDP, v6 UDP, and TCP-circuit responders all call this one rule,
    /// the endpoint scope governs every search path.
    #[epics_macros_rs::epics_test]
    async fn search_matched_cids_honors_requester_endpoint() {
        use crate::pvdata::{FieldDesc, PvField};
        use crate::server_native::source::ChannelSource;
        use std::sync::Arc;

        // Claims "MY:PV" only for a requester at 10.0.0.5.
        struct ScopedSource;
        #[allow(clippy::manual_async_fn)]
        impl ChannelSource for ScopedSource {
            fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
                async { vec!["MY:PV".into()] }
            }
            fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
                let m = name == "MY:PV";
                async move { m }
            }
            fn searchable_from(
                &self,
                name: &str,
                requester: SocketAddr,
            ) -> impl std::future::Future<Output = bool> + Send {
                let ok =
                    name == "MY:PV" && requester.ip() == IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
                async move { ok }
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
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only".into()) }
            }
            fn is_writable(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(ScopedSource);
        let req = SearchRequest {
            seq: 1,
            byte_order: ByteOrder::Little,
            queries: vec![(7, "MY:PV".into())],
            reply_addr: None,
            reply_port: 0,
            unicast: false,
            must_reply: false,
            // A matching protocol so the gate passes; this test exercises
            // endpoint scoping (`searchable_from`), not the protocol gate.
            protocols: vec![b"tcp".to_vec()],
            consumed: 0,
        };
        let allowed: SocketAddr = "10.0.0.5:5076".parse().unwrap();
        let denied: SocketAddr = "192.168.1.1:5076".parse().unwrap();

        assert_eq!(
            search_matched_cids(&source, &req, "tcp", allowed).await,
            vec![7],
            "the scoped source must claim the PV for the allowed requester"
        );
        assert!(
            search_matched_cids(&source, &req, "tcp", denied)
                .await
                .is_empty(),
            "the scoped source must NOT claim the PV for a different requester"
        );
    }

    /// The shared SEARCH match rule gates on the advertised protocol:
    /// a matching protocol (or an empty/legacy list) yields the searchable
    /// CIDs, a mismatched one yields nothing. This is the single rule the
    /// v4 UDP, v6 UDP, and TCP-circuit responders all call — the v6 path
    /// previously skipped it and would answer a TLS-only client off a
    /// tcp-only server.
    #[epics_macros_rs::epics_test]
    async fn search_matched_cids_gates_on_protocol() {
        use crate::pvdata::{FieldDesc, PvField, ScalarType};
        use crate::server_native::source::ChannelSource;
        use std::sync::Arc;

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
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only".into()) }
            }
            fn is_writable(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &str,
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(PresentSource);
        let requester: SocketAddr = "127.0.0.1:5076".parse().unwrap();
        let req = |protocols: Vec<Vec<u8>>| SearchRequest {
            seq: 1,
            byte_order: ByteOrder::Little,
            queries: vec![(7, "MY:PV".into())],
            reply_addr: None,
            reply_port: 0,
            unicast: false,
            must_reply: false,
            protocols,
            consumed: 0,
        };

        // Server speaks "tcp". Matching protocol → searchable CID returned.
        assert_eq!(
            search_matched_cids(&source, &req(vec![b"tcp".to_vec()]), "tcp", requester).await,
            vec![7]
        );
        // Client asks for "tls" only → no match off a tcp server.
        assert!(
            search_matched_cids(&source, &req(vec![b"tls".to_vec()]), "tcp", requester)
                .await
                .is_empty()
        );
        // Empty protocol list (`nproto==0`) → never raises pvxs
        // `protoTCP`, so no names are queued and nothing matches. The
        // caller still answers a `MustReply` probe with `found=0`, but
        // the match set itself is empty. Regression for the bug where an
        // empty list was treated as a "tcp" wildcard, letting malformed
        // zero-protocol UDP SEARCHes pull a `found=1` off the server.
        assert!(
            search_matched_cids(&source, &req(vec![]), "tcp", requester)
                .await
                .is_empty(),
            "empty protocol list must not be treated as a wildcard match"
        );
        // A protocol entry that is not valid UTF-8 must compare as raw
        // bytes (≠ "tcp") and therefore NOT match — pvxs preserves the
        // wire bytes (serverconn.cpp SearchOp) rather than collapsing a
        // failed-decode entry into a wildcard. Regression for the bug
        // where a non-UTF-8 protocol downgraded to an empty list and
        // matched every tcp server.
        assert!(
            search_matched_cids(&source, &req(vec![vec![0xff, 0xfe]]), "tcp", requester)
                .await
                .is_empty(),
            "non-UTF-8 protocol must not be treated as a wildcard match"
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
        let dest = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 54321);
        let out = try_build_forward_frame(&original, dest).expect("unicast → Some");

        // Re-parse the forwarded frame: unicast must be cleared, reply
        // must reflect the resolved dest.
        let req = parse_search_request(&out).expect("re-parse ok");
        assert!(!req.unicast, "unicast flag must be cleared");
        assert_eq!(
            req.reply_addr,
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)))
        );
        assert_eq!(req.reply_port, 54321);

        // Non-unicast SEARCH: forward returns None (no rewrite).
        let bcast = codec.build_search(7, 42, "MY:PV", [10, 0, 0, 1], 5076, false);
        assert!(try_build_forward_frame(&bcast, dest).is_none());
    }

    /// An explicit
    /// IPv6 reply endpoint inside a forwarded SEARCH must be preserved as
    /// its raw 16-byte address, not downgraded to the IPv4 sender or
    /// wildcard. pvxs forwarding is transport-independent at the payload
    /// level (`to_wire(replyDest)`, `udp_collector.cpp:394`), so a
    /// requester asking for SEARCH_RESPONSE at `[2001:db8::1]:9876` keeps
    /// that endpoint even though the ORIGIN_TAG forward channel is IPv4.
    #[test]
    fn try_build_forward_frame_preserves_ipv6_reply_endpoint() {
        use std::net::Ipv6Addr;
        let codec = PvaCodec { big_endian: false };
        // Unicast SEARCH whose payload reply address is IPv6.
        let mut original = codec.build_search(7, 42, "MY:PV", [0, 0, 0, 0], 5076, true);
        let v6 = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let off = PvaHeader::SIZE + 8;
        original[off..off + 16].copy_from_slice(&ip_to_bytes(IpAddr::V6(v6)));

        // Forwarder resolved the same v6 endpoint (caller keeps
        // `req.reply_addr` verbatim) with the advertised port.
        let dest = SocketAddr::new(IpAddr::V6(v6), 9876);
        let out = try_build_forward_frame(&original, dest).expect("unicast → Some");

        let req = parse_search_request(&out).expect("re-parse ok");
        assert!(!req.unicast, "unicast flag must be cleared");
        assert_eq!(
            req.reply_addr,
            Some(IpAddr::V6(v6)),
            "forwarded SEARCH must keep the IPv6 reply address, not downgrade it"
        );
        assert_eq!(req.reply_port, 9876);
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
        assert_eq!(
            req.reply_addr,
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 5, 10)))
        );
        assert_eq!(req.reply_port, 9999);

        // Unspecified addr → None (sentinel for "use UDP source").
        let frame_any = codec.build_search(7, 42, "MY:PV", [0, 0, 0, 0], 5076, false);
        let req_any = parse_search_request(&frame_any).expect("parse ok");
        assert_eq!(req_any.reply_addr, None);
        assert_eq!(req_any.reply_port, 5076);
    }

    /// `parse_search_request` must keep a concrete IPv6 reply address
    /// instead of folding it to `None`. pvxs decodes the 16-byte address
    /// field into a full `SockAddr` (`udp_collector.cpp:367-370`); the
    /// pre-fix IPv4-only filter dropped every v6 address.
    #[test]
    fn parse_search_request_keeps_ipv6_reply_addr() {
        use std::net::Ipv6Addr;
        let codec = PvaCodec { big_endian: false };
        // Build a v4 frame, then overwrite the 16-byte response_addr
        // field (payload offset 8) with a real IPv6 address.
        let mut frame = codec.build_search(7, 42, "MY:PV", [0, 0, 0, 0], 9999, false);
        let v6 = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let off = PvaHeader::SIZE + 8;
        frame[off..off + 16].copy_from_slice(&ip_to_bytes(IpAddr::V6(v6)));
        let req = parse_search_request(&frame).expect("parse ok");
        assert_eq!(req.reply_addr, Some(IpAddr::V6(v6)));
        assert_eq!(req.reply_port, 9999);

        // Raw-zero `::` (v6 wildcard) still folds to None.
        let mut frame_any = codec.build_search(7, 42, "MY:PV", [0, 0, 0, 0], 5076, false);
        frame_any[off..off + 16].copy_from_slice(&[0u8; 16]);
        let req_any = parse_search_request(&frame_any).expect("parse ok");
        assert_eq!(req_any.reply_addr, None);
        assert_eq!(req_any.reply_port, 5076);
    }

    /// Segment bits are a TCP reassembly feature; a UDP datagram must
    /// never carry them. pvxs drops a segmented UDP datagram before
    /// SEARCH matching (`udp_collector.cpp:329-340`). Each segment-bit
    /// combination on an otherwise-valid SEARCH must yield `None`, while
    /// the same frame un-segmented parses — so the reject is the segment
    /// bits, not a malformed payload.
    #[test]
    fn parse_search_request_rejects_segmented_datagram() {
        use crate::proto::header::HeaderFlags;
        let codec = PvaCodec { big_endian: false };
        // Sanity: the un-segmented frame parses.
        let base = codec.build_search(7, 42, "MY:PV", [192, 168, 5, 10], 9999, false);
        assert!(parse_search_request(&base).is_some());
        // Flags byte lives at frame offset 2.
        for seg in [
            HeaderFlags::SEGMENT_FIRST,
            HeaderFlags::SEGMENT_LAST,
            HeaderFlags::SEGMENT_MASK,
        ] {
            let mut frame = base.clone();
            frame[2] |= seg;
            assert!(
                parse_search_request(&frame).is_none(),
                "segmented SEARCH (bits {seg:#04x}) must be dropped"
            );
        }
    }

    /// As above for BEACON: a segmented UDP beacon datagram must be
    /// dropped (`udp_collector.cpp:329-340`), never decoded or
    /// re-forwarded.
    #[test]
    fn parse_beacon_rejects_segmented_datagram() {
        use crate::proto::header::HeaderFlags;
        let guid = [0x11; 12];
        let base = build_beacon(guid, 5075, ByteOrder::Little, 42, 0xBEEF, "tcp");
        assert!(parse_beacon(&base).is_some());
        for seg in [
            HeaderFlags::SEGMENT_FIRST,
            HeaderFlags::SEGMENT_LAST,
            HeaderFlags::SEGMENT_MASK,
        ] {
            let mut frame = base.clone();
            frame[2] |= seg;
            assert!(
                parse_beacon(&frame).is_none(),
                "segmented BEACON (bits {seg:#04x}) must be dropped"
            );
        }
    }

    /// `resolve_reply_dest` invariant boundaries: an explicit advertised
    /// address is honoured with its advertised port (pvxs
    /// `udp_collector.cpp:377-380`), including a port that differs from
    /// the UDP source port; a wildcard (`None`) replies to the sender's
    /// source IP but with the advertised port (pvxs's unconditional
    /// `:380 setPort`).
    #[test]
    fn resolve_reply_dest_boundaries() {
        use std::net::Ipv6Addr;
        let src: SocketAddr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 5000);

        // Wildcard reply addr → sender's source IP with the *advertised*
        // port (9876), not the UDP source port (5000): pvxs `setPort`.
        assert_eq!(
            resolve_reply_dest(None, 9876, src),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 9876),
            "wildcard replies to the sender IP on the advertised port"
        );

        // Explicit v6 reply addr → that addr, advertised port (differs
        // from the UDP source port 5000).
        let v6 = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2);
        assert_eq!(
            resolve_reply_dest(Some(IpAddr::V6(v6)), 9876, src),
            SocketAddr::new(IpAddr::V6(v6), 9876),
            "explicit v6 reply addr/port is honoured over the UDP source"
        );

        // Explicit v4 reply addr with a differing port.
        let v4 = Ipv4Addr::new(192, 168, 9, 9);
        assert_eq!(
            resolve_reply_dest(Some(IpAddr::V4(v4)), 1234, src),
            SocketAddr::new(IpAddr::V4(v4), 1234),
        );
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
        // Multicast source dropped (IPv4).
        let mcast = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 128)), 5076);
        assert!(!filter_inbound(mcast, &blocklist));
        // Multicast source dropped (IPv6) — pvxs `SockAddr::isMCast()` is
        // family-generic (`util.cpp:570-577`); `ff02::1` is the v6
        // all-nodes link-local multicast group.
        let mcast_v6 = SocketAddr::new(
            IpAddr::V6(std::net::Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1)),
            5076,
        );
        assert!(!filter_inbound(mcast_v6, &blocklist));
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
        let bytes = build_beacon(guid, 5075, ByteOrder::Little, 42, 0xBEEF, "tcp");
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

    /// PR #205 IPv6 Stage 5: when `enable_ipv6_udp=true` and an
    /// explicit v6 beacon destination is configured, the beacon
    /// emitter routes it through a v6 send socket. Regression guard
    /// against the v6 send path being dropped from the per-destination
    /// dispatch in the beacon loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn beacon_emit_reaches_explicit_ipv6_destination() {
        use crate::server_native::SharedSource;
        use std::net::{Ipv6Addr, SocketAddrV6};

        // Sniffer bound to `[::1]:0`. The server will send beacons
        // unicast here.
        let sniffer = tokio::net::UdpSocket::bind("[::1]:0")
            .await
            .expect("v6 sniffer bind");
        let sniffer_port = sniffer.local_addr().unwrap().port();
        let v6_dest = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, sniffer_port, 0, 0));

        // Pick a free UDP port for the responder itself (not used by
        // this test directly — we only care about beacon TX).
        let pick_udp = || {
            let l =
                std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("probe bind");
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        let udp_port = pick_udp();

        // Short beacon period so the first burst fires quickly.
        let task = tokio::spawn(run_udp_responder_with_config(
            Arc::new(SharedSource::new()) as DynSource,
            udp_port,
            5075, // advertised TCP port (any non-zero value)
            [0xAB; 12],
            "tcp",
            std::time::Duration::from_millis(50),
            std::time::Duration::from_secs(10),
            3,
            vec![Endpoint::from(v6_dest)],
            false, // auto_beacon=false: only the explicit v6 dest
            Vec::new(),
            true,       // enable_ipv6_udp
            Vec::new(), // interfaces: all-NIC default
        ));

        let mut buf = vec![0u8; 4096];
        let recv = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            sniffer.recv_from(&mut buf),
        )
        .await
        .expect("beacon must arrive within 3s")
        .expect("recv_from ok");
        let n = recv.0;
        // Verify it's actually a beacon (CMD_BEACON header).
        assert!(n >= PvaHeader::SIZE, "beacon too short: {n}");
        let hdr = PvaHeader::decode(&mut Cursor::new(&buf[..n])).expect("hdr decode");
        assert_eq!(
            hdr.command,
            Command::Beacon.code(),
            "expected CMD_BEACON on v6 unicast destination"
        );
        // Confirm GUID in payload matches what we passed in.
        let payload = &buf[PvaHeader::SIZE..n];
        assert_eq!(
            &payload[0..12],
            &[0xAB; 12],
            "GUID in v6 beacon payload must match server's guid"
        );

        task.abort();
    }

    /// pvxs `udp_collector.cpp:363`: `mustReply = flags & pva_search_flags::
    /// MustReply`. The `MustReply` bit (`0x01`) and the `Unicast` bit
    /// (`0x80`) live in the same flags byte and are extracted by the
    /// parser; previously only `Unicast` survived the round trip.
    #[test]
    fn parse_search_extracts_must_reply_flag() {
        let codec = PvaCodec { big_endian: false };
        // build_discover_search sets flags = 0x01 (MustReply only).
        let frame = codec.build_discover_search(7, 9999);
        let req = parse_search_request(&frame).expect("discover SEARCH parses");
        assert!(
            req.must_reply,
            "MustReply flag (0x01) must be extracted into SearchRequest"
        );
        assert!(!req.unicast, "Unicast flag (0x80) should be clear");

        // Plain non-must-reply unicast SEARCH (flags = 0x80) keeps both
        // bits independent.
        let frame2 = codec.build_search(1, 7, "MY:PV", [0, 0, 0, 0], 5076, true);
        let req2 = parse_search_request(&frame2).expect("plain SEARCH parses");
        assert!(req2.unicast);
        assert!(!req2.must_reply);
    }

    /// pvxs `server.cpp:743-744`: when `nreply==0` the `found` byte is
    /// `0`, not `1`. Building a response with an empty CID slice must
    /// emit `found=0` so a MustReply discovery probe sees the correct
    /// shape and counts our server as reachable-but-no-match.
    #[test]
    fn search_response_empty_cids_emits_found_zero() {
        let guid = [0x55u8; 12];
        let bytes = build_search_response_proto(guid, 42, 5075, &[], ByteOrder::Little, "tcp");
        // Header (8) + GUID (12) + seq (4) + addr16 (16) + port (2) +
        // size(1)+"tcp"(3) = offset 46 for the `found` byte.
        let found_off = PvaHeader::SIZE + 12 + 4 + 16 + 2 + 1 + 3;
        assert_eq!(
            bytes[found_off], 0,
            "found byte must be 0 when no CIDs claimed (pvxs nreply==0 path)"
        );
        // nreply u16 immediately follows; must also be 0.
        assert_eq!(
            u16::from_le_bytes([bytes[found_off + 1], bytes[found_off + 2]]),
            0,
            "nreply must be 0 alongside found=0"
        );

        // Sanity check: non-empty CIDs still emit found=1.
        let bytes2 = build_search_response_proto(guid, 42, 5075, &[7u32], ByteOrder::Little, "tcp");
        assert_eq!(bytes2[found_off], 1, "found must be 1 when CIDs present");
    }

    // ── mshim forward rewrite (PVA SEARCH/BEACON per destination) ───────────

    fn req_with(unicast: bool, must_reply: bool, reply: Option<IpAddr>) -> SearchRequest {
        SearchRequest {
            seq: 42,
            byte_order: ByteOrder::Little,
            queries: vec![(7, "PV:A".to_string()), (8, "PV:B".to_string())],
            reply_addr: reply,
            reply_port: 5566,
            unicast,
            must_reply,
            protocols: vec![b"tcp".to_vec()],
            consumed: 0,
        }
    }

    /// A unicast-flagged SEARCH forwarded to a MULTICAST destination must
    /// have the Unicast flag CLEARED; MustReply, reply addr/port, and
    /// channel queries are preserved (pvxs mshim.cpp:140-146).
    #[test]
    fn mshim_search_unicast_to_multicast_clears_unicast() {
        let req = req_with(true, true, Some(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        let source = "192.168.9.9:1111".parse().unwrap();
        let frame = rebuild_search_forward(&req, /*dest_unicast=*/ false, source);
        let out = parse_search_request(&frame).expect("rebuilt SEARCH parses");
        assert!(!out.unicast, "Unicast must be cleared for a multicast dest");
        assert!(out.must_reply, "MustReply must be preserved");
        assert_eq!(out.reply_addr, Some(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert_eq!(out.reply_port, 5566);
        assert_eq!(
            out.queries,
            vec![(7, "PV:A".to_string()), (8, "PV:B".to_string())]
        );
    }

    /// A non-unicast SEARCH forwarded to a UNICAST destination must have
    /// the Unicast flag SET (pvxs mshim.cpp:144-145).
    #[test]
    fn mshim_search_multicast_to_unicast_sets_unicast() {
        let req = req_with(false, false, Some(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        let source = "192.168.9.9:1111".parse().unwrap();
        let frame = rebuild_search_forward(&req, /*dest_unicast=*/ true, source);
        let out = parse_search_request(&frame).expect("rebuilt SEARCH parses");
        assert!(out.unicast, "Unicast must be set for a unicast dest");
        assert!(!out.must_reply);
    }

    /// An `isAny` reply address (unspecified) must be resolved to the
    /// datagram source so the forwarded SEARCH names the original
    /// requester as the reply target, not the shim. The announced reply
    /// PORT is preserved.
    #[test]
    fn mshim_search_isany_reply_resolves_to_source() {
        let req = req_with(true, false, None);
        let source: SocketAddr = "192.168.5.9:1111".parse().unwrap();
        let frame = rebuild_search_forward(&req, true, source);
        let out = parse_search_request(&frame).expect("rebuilt SEARCH parses");
        assert_eq!(
            out.reply_addr,
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 5, 9))),
            "isAny reply addr must resolve to the source IP"
        );
        assert_eq!(out.reply_port, 5566, "announced reply port is kept");
    }

    /// The full protocol list is preserved verbatim, including non-`tcp`
    /// entries — a `tls` SEARCH must still advertise `tls` downstream.
    #[test]
    fn mshim_search_preserves_protocol_list() {
        let mut req = req_with(false, false, Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        req.protocols = vec![b"tls".to_vec(), b"tcp".to_vec()];
        let source = "192.168.9.9:1111".parse().unwrap();
        let frame = rebuild_search_forward(&req, true, source);
        let out = parse_search_request(&frame).expect("rebuilt SEARCH parses");
        assert_eq!(out.protocols, vec![b"tls".to_vec(), b"tcp".to_vec()]);
    }

    /// End-to-end decode: a real SEARCH datagram built by the codec is
    /// recognized by `decode_all` and rebuilt for a multicast dest with
    /// the Unicast bit cleared.
    #[test]
    fn mshim_decode_all_recognizes_real_search() {
        let codec = PvaCodec { big_endian: false };
        let frame = codec.build_search(1, 7, "MY:PV", [10, 1, 2, 3], 5076, true);
        let msgs = ForwardableDatagram::decode_all(&frame);
        assert_eq!(msgs.len(), 1, "one SEARCH message decoded");
        assert!(msgs[0].is_search());
        let source = "192.168.9.9:1111".parse().unwrap();
        let rebuilt = msgs[0].rebuild_for(/*dest_unicast=*/ false, source);
        let out = parse_search_request(&rebuilt).expect("rebuilt parses");
        assert!(!out.unicast);
        assert_eq!(out.queries, vec![(7, "MY:PV".to_string())]);
    }

    /// A BEACON is rebuilt as a valid CMD_BEACON (NOT mislabeled as
    /// SEARCH the way pvxs mshim.cpp:199 does), preserving the GUID,
    /// protocol, and TCP port, with the server address resolved to the
    /// source when the beacon announced `isAny`.
    #[test]
    fn mshim_beacon_rebuild_is_valid_beacon() {
        let guid = [0xABu8; 12];
        // build_beacon writes an UNSPECIFIED server address (servers send
        // 0.0.0.0; receivers use the UDP source), so this exercises the
        // isAny→source resolution.
        let frame = build_beacon(guid, 5075, ByteOrder::Little, 3, 9, "tcp");
        let msgs = ForwardableDatagram::decode_all(&frame);
        assert_eq!(msgs.len(), 1, "one BEACON message decoded");
        assert!(!msgs[0].is_search(), "a beacon is not a search");
        let source: SocketAddr = "192.168.7.7:9".parse().unwrap();
        let rebuilt = msgs[0].rebuild_for(false, source);
        // Re-parse as a beacon — this also asserts the header command is
        // CMD_BEACON (parse_beacon rejects any other command).
        let b = parse_beacon(&rebuilt).expect("rebuilt BEACON parses as a beacon");
        assert_eq!(b.guid, guid);
        assert_eq!(b.server_port, 5075);
        assert_eq!(b.protocol, b"tcp".to_vec());
        assert_eq!(
            b.server_addr,
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 7, 7))),
            "isAny beacon server addr must resolve to the source"
        );
        // The rebuilt frame must NOT decode as a SEARCH (no CMD_SEARCH
        // mislabel).
        assert!(parse_search_request(&rebuilt).is_none());
    }

    /// Unrecognized datagrams (garbage, or a valid-but-non-SEARCH/BEACON
    /// command) are dropped — `decode_all` returns empty so the shim
    /// forwards nothing rather than relaying raw bytes.
    #[test]
    fn mshim_unrecognized_datagram_is_dropped() {
        assert!(ForwardableDatagram::decode_all(&[]).is_empty());
        assert!(ForwardableDatagram::decode_all(b"not a pva frame at all").is_empty());
        // A well-formed but non-forwardable command (CREATE_CHANNEL).
        let codec = PvaCodec { big_endian: false };
        let cc = codec.build_create_channel(1, "PV:X");
        assert!(
            ForwardableDatagram::decode_all(&cc).is_empty(),
            "only SEARCH/BEACON are forwardable"
        );
    }

    /// pvxs `UDPCollector::process_one` decodes only the FIRST PVA header
    /// in a datagram and ignores trailing bytes (udp_collector.cpp:329-352),
    /// so `mshim` forwards exactly one message. A crafted
    /// `SEARCH(A) || SEARCH(B)` must yield only A under `decode_first` (the
    /// default mshim boundary), while `decode_all` (the Rust-only
    /// extension) still drains both — the contrast that proves the fix.
    #[test]
    fn mshim_decode_first_ignores_chained_search() {
        let codec = PvaCodec { big_endian: false };
        let a = codec.build_search(1, 7, "PV:A", [10, 0, 0, 1], 5076, true);
        let b = codec.build_search(2, 8, "PV:B", [10, 0, 0, 2], 5076, true);
        let mut chained = a.clone();
        chained.extend_from_slice(&b);

        // decode_first: exactly the first message (SEARCH A), tail ignored.
        let first = ForwardableDatagram::decode_first(&chained).expect("first SEARCH decodes");
        assert!(first.is_search());
        let source: SocketAddr = "192.168.9.9:1111".parse().unwrap();
        let out = parse_search_request(&first.rebuild_for(false, source)).expect("rebuilt parses");
        assert_eq!(
            out.queries,
            vec![(7, "PV:A".to_string())],
            "only the first SEARCH is forwarded, not the chained PV:B"
        );

        // decode_all (Rust-only extension) still drains both.
        assert_eq!(
            ForwardableDatagram::decode_all(&chained).len(),
            2,
            "decode_all drains every chained message"
        );
    }

    /// `SEARCH || BEACON`.
    ///
    /// A `SEARCH` followed by a `BEACON` in one datagram is forwarded as
    /// the single first message (the SEARCH); the trailing BEACON is
    /// ignored, matching pvxs `process_one`.
    #[test]
    fn mshim_decode_first_search_then_beacon_takes_search() {
        let codec = PvaCodec { big_endian: false };
        let search = codec.build_search(1, 7, "PV:A", [10, 0, 0, 1], 5076, true);
        let beacon = build_beacon([0xCDu8; 12], 5075, ByteOrder::Little, 3, 9, "tcp");
        let mut chained = search.clone();
        chained.extend_from_slice(&beacon);

        let first = ForwardableDatagram::decode_first(&chained).expect("first message decodes");
        assert!(
            first.is_search(),
            "the first message (SEARCH) is taken; the trailing BEACON is ignored"
        );
        assert_eq!(
            ForwardableDatagram::decode_all(&chained).len(),
            2,
            "decode_all still sees both the SEARCH and the trailing BEACON"
        );
    }

    /// The server's multicast SEARCH-listener joins are derived from its
    /// effective beacon destinations (pvxs `addGroups`): only IPv4
    /// multicast destinations become group joins. Broadcast, unicast, and
    /// IPv6 destinations are dropped. A modifier-less destination is joined
    /// on the (single) external NIC supplied here.
    #[test]
    fn multicast_join_plan_selects_only_v4_multicast_beacon_destinations() {
        use std::net::{Ipv6Addr, SocketAddrV6};
        let nics = [Ipv4Addr::new(10, 0, 0, 1)];
        let dests: Vec<Endpoint> = vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 1, 1, 1)), 5076).into(), // mcast
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 255)), 5076).into(), // broadcast
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), 5076).into(),  // unicast
            SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::new(0xff0e, 0, 0, 0, 0, 0, 0, 0x400),
                5076,
                0,
                0,
            ))
            .into(), // v6 mcast — not joinable on the v4 socket
        ];
        assert_eq!(
            multicast_join_plan(&dests, &nics),
            vec![(Ipv4Addr::new(224, 1, 1, 1), Ipv4Addr::new(10, 0, 0, 1))]
        );
        assert!(multicast_join_plan(&[], &nics).is_empty());
    }

    /// PVA-33 regression: a server configured with
    /// `EPICS_PVAS_BEACON_ADDR_LIST` and NO `EPICS_PVA_ADDR_LIST` must
    /// still join the configured multicast group for inbound SEARCH. The
    /// old join read `EPICS_PVA_ADDR_LIST` directly, so the PVAS-specific
    /// beacon list never produced a join. Drive the real config path
    /// (`server_beacon_addr_list_opt`) and assert the group is selected.
    #[test]
    #[serial_test::serial(epics_env)]
    fn pvas_beacon_addr_list_without_pva_addr_list_joins_multicast() {
        // SAFETY: std::env::{set,remove}_var are unsafe in the 2024
        // edition; the `epics_env` serial guard makes the mutation
        // race-free under `cargo test`, which runs all lib tests as
        // threads in one process.
        unsafe {
            std::env::remove_var("EPICS_PVA_ADDR_LIST");
            std::env::set_var("EPICS_PVAS_BEACON_ADDR_LIST", "224.0.7.7");
        }
        let dests =
            crate::config::env::server_beacon_endpoints_opt().expect("PVAS beacon list present");
        unsafe {
            std::env::remove_var("EPICS_PVAS_BEACON_ADDR_LIST");
        }
        let nics = [Ipv4Addr::new(10, 0, 0, 1)];
        assert_eq!(
            multicast_join_plan(&dests, &nics),
            vec![(Ipv4Addr::new(224, 0, 7, 7), Ipv4Addr::new(10, 0, 0, 1))],
            "PVAS_BEACON_ADDR_LIST multicast group must be joined even with PVA_ADDR_LIST unset"
        );
    }

    /// PVA-33 regression: a programmatic `PvaServerConfig::beacon_destinations`
    /// (no env at all) must also produce the multicast join — the old
    /// env-only join ignored programmatic destinations entirely.
    #[test]
    fn programmatic_beacon_destinations_join_multicast() {
        let dests: Vec<Endpoint> =
            vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 1, 2, 3)), 5076).into()];
        let nics = [Ipv4Addr::new(10, 0, 0, 1)];
        assert_eq!(
            multicast_join_plan(&dests, &nics),
            vec![(Ipv4Addr::new(239, 1, 2, 3), Ipv4Addr::new(10, 0, 0, 1))]
        );
    }

    /// The join plan must honour
    /// the `@iface` modifier exactly like pvxs `addGroups`
    /// (`config.cpp:310-335`):
    ///
    /// * an explicit `@iface` multicast destination is joined on *that one*
    ///   interface only — NOT fanned out across every NIC (the pre-fix
    ///   `multicast_v4_groups` dropped the `@iface` and `join_multicast_v4`
    ///   joined every NIC), and NOT expanded over the external-NIC set.
    /// * a modifier-less multicast destination expands over *every* external
    ///   NIC — one join target per NIC — even NICs outside
    ///   `EPICS_PVAS_INTF_ADDR_LIST` (the `external_nics` argument is the
    ///   full external set, not the bound interface set).
    ///
    /// `(group, iface)` pairs are deduplicated, first-appearance order kept.
    /// IP-literal `@iface` forms are used so `resolve_iface_v4` is a pure
    /// passthrough and the assertion is deterministic.
    #[test]
    fn multicast_join_plan_respects_iface_modifier_and_expands_modifierless() {
        let external = [Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 1, 1)];

        // Explicit @iface (IP form): joined on that NIC alone, even though
        // it is NOT in `external` — pvxs pushes the endpoint as-is.
        let explicit = Endpoint {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 2, 9)), 5076),
            ttl: None,
            iface: Some("192.168.5.5".to_string()),
        };
        assert_eq!(
            multicast_join_plan(std::slice::from_ref(&explicit), &external),
            vec![(Ipv4Addr::new(224, 0, 2, 9), Ipv4Addr::new(192, 168, 5, 5))],
            "explicit @iface joins exactly that interface, not every NIC"
        );

        // Modifier-less: expands over every external NIC.
        let wildcard: Endpoint =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 3, 3)), 5076).into();
        assert_eq!(
            multicast_join_plan(std::slice::from_ref(&wildcard), &external),
            vec![
                (Ipv4Addr::new(224, 0, 3, 3), Ipv4Addr::new(10, 0, 0, 1)),
                (Ipv4Addr::new(224, 0, 3, 3), Ipv4Addr::new(10, 0, 1, 1)),
            ],
            "modifier-less multicast joins every external NIC"
        );

        // Both together, plus a duplicate wildcard entry: dedup keeps each
        // (group, iface) pair once, in first-appearance order.
        let plan = multicast_join_plan(&[explicit.clone(), wildcard.clone(), wildcard], &external);
        assert_eq!(
            plan,
            vec![
                (Ipv4Addr::new(224, 0, 2, 9), Ipv4Addr::new(192, 168, 5, 5)),
                (Ipv4Addr::new(224, 0, 3, 3), Ipv4Addr::new(10, 0, 0, 1)),
                (Ipv4Addr::new(224, 0, 3, 3), Ipv4Addr::new(10, 0, 1, 1)),
            ],
        );
    }
}
