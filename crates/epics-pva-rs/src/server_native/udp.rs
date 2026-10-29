//! UDP search responder + (very simple) beacon broadcaster.
//!
//! Listens on the configured UDP port for SEARCH requests and replies with
//! SEARCH_RESPONSE messages naming our TCP endpoint. Beacons are emitted
//! periodically to advertise our presence.

// RTEMS-EXEC-MODEL-ALLOW(12): this file does not build on the exec backend -
// server_native/mod.rs declares it under #[cfg(tokio_backend)] pub mod udp;.
use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(any(unix, windows)))]
use epics_base_rs::net::recv_from_with_drop_count_socket;
use epics_base_rs::net::{
    IfaceMap, PortOrigin, PortUse, bind_loopback_mcast, enable_so_rxq_ovfl_for_socket,
};
use tracing::debug;

use crate::config::Endpoint;
use crate::error::{PvaError, PvaResult};
use crate::proto::{
    ByteOrder, Command, PvaHeader, ReadExt, WriteExt, decode_size, encode_size_into, ip_from_bytes,
    ip_to_bytes,
};

use super::report::{Liveness, LivenessGuard};
use super::source::DynSource;
// The SEARCH protocol proper — frame parsing, name matching, response
// framing — lives in [`super::search`] so the TCP-circuit SEARCH handler can
// name it without inheriting this module's host-only gate. Imported for this
// module's own use, NOT re-exported: a `server_native::udp::` path to a
// protocol item resolves only on `tokio_backend`, so every caller must name
// `super::search` directly — see the cfg-out this re-export produced in
// `client_native::search_engine`'s tests.
use super::search::{SearchRequest, parse_search_request};
// The socket-free datagram half — chained-message drain, ORIGIN_TAG forward
// decision, reply-destination resolution, source filter, and the server GUID —
// moved to [`super::search_engine`] for the same reason and by the same move:
// the RTEMS blocking responder needs it and cannot inherit this module's
// host-only gate. Re-exported here so every `server_native::udp::` path keeps
// resolving, including this module's own tests.
pub use super::search_engine::random_guid;
pub(crate) use super::search_engine::{
    Origin, SearchListeners, SearchOutput, SearchReplier, ServerInterfaces, V6Listen,
    filter_inbound, local_v4_addrs, process_search_datagram,
};

/// pvxs `beaconSender4` (`server.cpp:389,399`): beacons leave from their own
/// unbound socket, so the interface list that limits the SEARCH listeners never
/// limits where a beacon may go.
fn bind_beacon_send_v4() -> std::io::Result<tokio::net::UdpSocket> {
    let sock = super::beacon::beacon_socket_v4()?;
    sock.set_nonblocking(true)?;
    tokio::net::UdpSocket::from_std(sock)
}

/// [`super::beacon::beacon_socket_v6`] on the reactor.
fn bind_beacon_send_v6() -> Option<Arc<tokio::net::UdpSocket>> {
    let sock = super::beacon::beacon_socket_v6()?;
    if let Err(e) = sock.set_nonblocking(true) {
        debug!("v6 beacon socket: set_nonblocking failed: {e}");
        return None;
    }
    match tokio::net::UdpSocket::from_std(sock) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            debug!("v6 beacon socket: tokio adoption failed: {e}");
            None
        }
    }
}

/// Bind the search sockets, returning them **and the port they actually
/// got**.
///
/// `port == 0` means "kernel, choose one", and the caller then has no other
/// way to learn the number — so it is read back from the bind here rather
/// than predicted. The socket that reports the port is the socket the
/// responder serves on, so nothing can take it in between. (The search
/// socket sets `SO_REUSEPORT` on every port, 0 included, as pvxs
/// `UDPCollector` does (`udp_collector.cpp:136`), so on a given number a
/// *collision does not fail* — a second server binds the same port silently
/// and the two answer searches at random. There is no error to detect after
/// the fact, which is why the number must never be guessed.)
///
/// The families follow `interfaces` as pvxs opens one collector per family
/// with an entry (`server.cpp:411-426`): the v4 wildcard collector for a v4
/// entry or the default, the `[::]` socket for a v6 one. pvxs
/// binds no per-interface socket: a listed interface is a listener on the
/// collector (`server.cpp:427-438`), which [`SearchListeners`] is.
///
/// The v6 socket binds on the v4 number, so a start either holds both
/// families or fails — pvxs binds its `[::]` listener on the number the v4
/// one read back and throws out of the `Server` constructor when it cannot
/// (`server.cpp:421-426`, `udp_collector.cpp:151`). v4 goes first: its
/// ephemeral pick must come from the kernel, since a guessed number under
/// the reuse flags would co-bind silently. The v6 socket then sets its
/// fanout flags before the bind, as pvxs does, since dual-stack without them
/// it would collide with this server's own v4 socket: a v6 holder of the
/// number without fanout is `EADDRINUSE` (`WSAEACCES` on Windows), one with
/// fanout co-binds as on any configured number. A held number, read as
/// [`epics_base_rs::net::port_held`] from either companion, drops the set
/// and asks the kernel again. pvxs does not do this: the retry is rs's own
/// deviation, and pvxs reading `SOCK_EACCES` in its TCP fallback
/// (`serverconn.cpp:437`) is only an analogy for the Windows case.
///
/// The v4 collector is also the ORIGIN_TAG socket (see [`Collector`]).
pub(crate) fn bind_udp(port: u16, interfaces: &ServerInterfaces) -> PvaResult<UdpBind> {
    const EPHEMERAL_TRIES: usize = 32;
    let origin = if port == 0 {
        PortOrigin::Chosen
    } else {
        PortOrigin::Configured
    };
    let v6_listen = interfaces.v6();
    if !interfaces.has_v4() {
        let listen =
            v6_listen.ok_or_else(|| PvaError::Protocol("no search interface to bind".into()))?;
        let sock = bind_search_v6(port, origin, interfaces)?;
        let port = sock.local_addr()?.port();
        return Ok(UdpBind {
            v4: None,
            v6: Some(V6Bind { sock, listen }),
            port,
        });
    }
    let map = IfaceMap::new()?;
    let mut tries = 1;
    let (port, collector, v6) = loop {
        let companions = || -> std::io::Result<_> {
            let collector = bind_loopback_mcast(port)?;
            let bound = collector.local_addr()?.port();
            let v6 = match &v6_listen {
                Some(listen) => {
                    #[cfg(test)]
                    tests::BEFORE_V6_BIND.with_borrow_mut(|hook| hook.as_mut().map(|h| h(bound)));
                    Some(V6Bind {
                        sock: bind_search_v6(bound, PortOrigin::Configured, interfaces)?,
                        listen: listen.clone(),
                    })
                }
                None => None,
            };
            Ok((collector, bound, v6))
        };
        match companions() {
            Ok((collector, bound, v6)) => break (bound, collector, v6),
            Err(e) if port == 0 && epics_base_rs::net::port_held(&e) && tries < EPHEMERAL_TRIES => {
                debug!("a v6 companion's UDP port is held; choosing another search port");
                tries += 1;
            }
            Err(e) => return Err(PvaError::Io(e)),
        }
    };
    // pvxs `UDPCollector::UDPCollector` (commit a064677e3625) enables
    // SO_RXQ_OVFL on the collector socket. No-op on non-Linux.
    if let Err(e) = enable_so_rxq_ovfl_for_socket(&collector) {
        debug!("loopback SO_RXQ_OVFL enable failed (non-fatal): {e}");
    }
    let collector = Collector::new(collector, interfaces, &map)?;
    Ok(UdpBind {
        v4: Some(V4Bind { collector }),
        v6,
        port,
    })
}

/// A server start's search sockets, all on [`Self::port`]; at least one
/// family is bound.
pub(crate) struct UdpBind {
    pub v4: Option<V4Bind>,
    pub v6: Option<V6Bind>,
    pub port: u16,
}

/// The v4 side: pvxs's one wildcard collector.
pub(crate) struct V4Bind {
    pub collector: Collector,
}

/// pvxs's v6 collector and its listeners.
pub(crate) struct V6Bind {
    pub sock: std::net::UdpSocket,
    pub listen: V6Listen,
}

/// The wildcard socket on the search port, pvxs's `UDPCollector`
/// (`udp_collector.cpp:124-167`). It is the ORIGIN_TAG
/// channel (`224.0.0.128` on loopback), it joins every beacon group on its
/// interface (`addListener` → `mcast_join`, `:186-196`), and it reads each
/// datagram's destination and arrival interface ([`epics_base_rs::net::udp_rx`], `:137`,
/// `:254-318`) for [`SearchListeners`] to route.
pub(crate) struct Collector {
    pub sock: Arc<tokio::net::UdpSocket>,
    listeners: SearchListeners,
}

impl Collector {
    fn new(
        sock: tokio::net::UdpSocket,
        interfaces: &ServerInterfaces,
        map: &IfaceMap,
    ) -> std::io::Result<Self> {
        join_beacon_groups(socket2::SockRef::from(&sock), &interfaces.groups);
        #[cfg(any(unix, windows))]
        epics_base_rs::net::udp_rx::enable(&sock, false)?;
        Ok(Self {
            sock: Arc::new(sock),
            listeners: SearchListeners::new(interfaces, &map.all()),
        })
    }
}

/// Join each `(group, interface)` on `sock`, pvxs `mcast_join`; a failed
/// join is logged and leaves that group unheard on that interface.
pub(crate) fn join_beacon_groups(sock: socket2::SockRef<'_>, joins: &[(Ipv4Addr, Ipv4Addr)]) {
    for &(group, iface) in joins {
        match sock.join_multicast_v4(&group, &iface) {
            Ok(()) => debug!("joined beacon multicast group {group} on interface {iface}"),
            Err(e) => debug!("join {group}@{iface} failed: {e}"),
        }
    }
}

/// One datagram off a collector: length, source, the `(destination,
/// interface index)` it was sent to, and the `SO_RXQ_OVFL` drop count.
#[cfg(any(unix, windows))]
async fn recv_collector(
    sock: &tokio::net::UdpSocket,
    buf: &mut [u8],
) -> std::io::Result<(usize, SocketAddr, Option<(IpAddr, u32)>, u32)> {
    loop {
        sock.readable().await?;
        match sock.try_io(tokio::io::Interest::READABLE, || {
            epics_base_rs::net::udp_rx::recv(sock, buf)
        }) {
            Ok(d) => return Ok((d.len, d.src, d.dest.map(|dest| (dest, d.ifindex)), d.drops)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(not(any(unix, windows)))]
async fn recv_collector(
    sock: &tokio::net::UdpSocket,
    buf: &mut [u8],
) -> std::io::Result<(usize, SocketAddr, Option<(IpAddr, u32)>, u32)> {
    let (n, src, drops) = recv_from_with_drop_count_socket(sock, buf).await?;
    Ok((n, src, None, drops))
}

/// pvxs's v6 collector on `port`: `[::]` with `IPV6_V6ONLY` at the OS
/// default, which pvxs never sets (`udp_collector.cpp:124-151`): dual-stack
/// on Linux and Darwin, v6-only on Windows. Fanout on every port and
/// `IPV6_RECVPKTINFO`, joined to the v6 groups of `interfaces`.
/// `origin` is [`PortOrigin::Chosen`] only for a v6 collector without a v4
/// one, which picks the number itself.
fn bind_search_v6(
    port: u16,
    origin: PortOrigin,
    interfaces: &ServerInterfaces,
) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_nonblocking(true)?;
    let bind_addr = SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), port);
    PortUse::Shared.bind(&sock, bind_addr, origin)?;
    for &(group, index) in &interfaces.groups_v6 {
        match sock.join_multicast_v6(&group, index) {
            Ok(()) => debug!("joined v6 multicast group {group} on interface {index}"),
            Err(e) => debug!("join {group}%{index} failed: {e}"),
        }
    }
    let sock: std::net::UdpSocket = sock.into();
    #[cfg(any(unix, windows))]
    epics_base_rs::net::udp_rx::enable(&sock, true)?;
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
        Vec::new(),
    )
    .await
}

/// Like [`run_udp_responder_proto`] but configurable: explicit beacon
/// period, explicit destinations, and an auto-NIC-broadcast flag. With
/// `auto_beacon`, beacons also go to the broadcasts of `interfaces`
/// (pvxs `expandAddrList`).
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
    // `EPICS_PVAS_INTF_ADDR_LIST`; empty = all-NIC default.
    interfaces: Vec<Endpoint>,
) -> PvaResult<()> {
    let interfaces = ServerInterfaces::resolve(&interfaces, &destinations, auto_beacon)?;
    let bound = bind_udp(udp_port, &interfaces)?;
    let beacons = super::beacon::beacon_destinations(
        destinations,
        auto_beacon,
        &interfaces.bcast_ifaces(),
        bound.port,
    );
    let (_, v4_alive) = Liveness::guarded();
    let (_, v6_alive) = Liveness::guarded();
    let (_, started) = tokio::sync::watch::channel(Some(SearchReplier {
        source,
        tcp_port,
        guid,
        protocol,
    }));
    serve_udp(
        bound,
        ignore_addrs,
        started,
        beacons,
        super::beacon::BeaconCycle::new(beacon_period, beacon_period_long, beacon_burst_count),
        v4_alive,
        v6_alive,
    )
    .await
}

/// A server's started state as its search responders read it: `None` from
/// the bind, the replier from `start` on.
pub(crate) type SearchStarted = tokio::sync::watch::Receiver<Option<SearchReplier>>;

/// Serve an already-bound [`UdpBind`] from the bind on: the responder of
/// each bound family, which answers only once `started` holds the replier,
/// and from then the beacons, until one of them ends — or, never started,
/// until `started`'s sender is dropped. Each responder holds its family's
/// guard, so a family never bound reads down at once.
///
/// Splitting the bind out is what lets [`crate::server_native::runtime`]
/// support `udp_port = 0`: the responders are spawned as a task, so a bind
/// performed inside it would publish the kernel-assigned port nowhere and
/// `report()`/SEARCH replies would advertise 0. The binder learns the port
/// and hands it here, so `bound.port` is always the real one — it is
/// embedded in beacons, the loopback-multicast retransmit, and the
/// origin-tag group, none of which tolerate a placeholder.
pub(crate) async fn serve_udp(
    bound: UdpBind,
    ignore_addrs: Vec<(IpAddr, u16)>,
    mut started: SearchStarted,
    beacons: Vec<Endpoint>,
    cycle: super::beacon::BeaconCycle,
    v4_alive: LivenessGuard,
    v6_alive: LivenessGuard,
) -> PvaResult<()> {
    let UdpBind { v4, v6, port } = bound;
    debug!(
        ?port,
        v4 = v4.is_some(),
        v6 = v6.is_some(),
        "UDP search responders reading"
    );
    let mut set: tokio::task::JoinSet<PvaResult<()>> = tokio::task::JoinSet::new();
    if let Some(v4) = v4 {
        let (ignore_addrs, started) = (ignore_addrs.clone(), started.clone());
        set.spawn(async move {
            let _alive = v4_alive;
            run_udp_responder_v4(v4, port, ignore_addrs, started).await
        });
    }
    if let Some(v6) = v6 {
        let started = started.clone();
        set.spawn(async move {
            let _alive = v6_alive;
            run_udp_responder_v6(v6, port, ignore_addrs, started).await
        });
    }
    // Dropping the set, on a return or an abort of this future, aborts
    // every responder and the beacons together.
    let joined = |res: Option<Result<PvaResult<()>, tokio::task::JoinError>>| match res {
        Some(Ok(res)) => res,
        Some(Err(e)) => Err(PvaError::Protocol(format!("udp task panic: {e}"))),
        None => Ok(()),
    };
    // pvxs arms the beacon timer in `Server::Pvt::start` (`server.cpp:590`).
    let replier = tokio::select! {
        replier = started.wait_for(Option::is_some) => replier.ok().and_then(|r| (*r).clone()),
        res = set.join_next() => return joined(res),
    };
    let Some(replier) = replier else {
        return Ok(());
    };
    set.spawn(run_beacons(replier, beacons, cycle));
    joined(set.join_next().await)
}

/// The beacon stream, first beacon at once (pvxs cc5071cd22c4:
/// `server.cpp:590-592` arms the timer `immediate`). pvxs `beaconSender4`
/// and `beaconSender6`; the v6 one is opened only for a v6 destination.
async fn run_beacons(
    serve: SearchReplier,
    dests: Vec<Endpoint>,
    mut cycle: super::beacon::BeaconCycle,
) -> PvaResult<()> {
    let v4 = bind_beacon_send_v4()?;
    let v6 = if dests.iter().any(|e| e.addr.is_ipv6()) {
        bind_beacon_send_v6()
    } else {
        None
    };
    debug!(?dests, v6 = v6.is_some(), "beacon emitter config");
    loop {
        let beacon = cycle.frame(
            serve.source.beacon_change(),
            serve.guid,
            serve.tcp_port,
            serve.protocol,
        );
        let ifmap = crate::config::IfaceMap::for_endpoints(dests.iter());
        for ep in &dests {
            let result = match ep.addr {
                SocketAddr::V4(_) => {
                    super::beacon::mcast_prep(&v4, ep, ep.addr, &ifmap);
                    v4.send_to(&beacon, ep.addr).await.map(|_| ())
                }
                SocketAddr::V6(_) => match &v6 {
                    Some(s6) => {
                        super::beacon::mcast_prep(&**s6, ep, ep.addr, &ifmap);
                        s6.send_to(&beacon, ep.addr).await.map(|_| ())
                    }
                    None => Err(super::beacon::no_v6_socket()),
                },
            };
            cycle.sent(ep.addr, result);
        }
        tokio::time::sleep(cycle.period()).await;
    }
}

/// The v4 responder on the collector, routed by [`SearchListeners::route`]
/// as the blocking responder's collector is.
async fn run_udp_responder_v4(
    bind: V4Bind,
    udp_port: u16,
    ignore_addrs: Vec<(IpAddr, u16)>,
    started: SearchStarted,
) -> PvaResult<()> {
    let V4Bind { collector } = bind;
    let mut prev_drops: u32 = 0;
    // 64 KB receive buffer — IPv4 maximum: pvxs clients pack many SEARCH
    // messages into one datagram, and a 1500-byte buffer truncated them.
    let mut buf = vec![0u8; 64 * 1024];
    // Local IPv4 interface addresses for the ORIGIN_TAG origin-locality
    // check (pvxs `ifinfo.byAddr`).
    let local_v4 = local_v4_addrs();
    loop {
        let (n, src, pktinfo, drops) = match recv_collector(&collector.sock, &mut buf).await {
            Ok(v) => v,
            Err(e) => {
                debug!("udp recv error: {e}");
                continue;
            }
        };
        // pvxs `udp_collector.cpp:55-67` logs on `prev != current && current != 0`.
        if drops != 0 && drops != prev_drops {
            debug!(
                prev = prev_drops,
                drops, "PVA UDP collector socket buffer overflow"
            );
        }
        prev_drops = drops;
        if !filter_inbound(src, &ignore_addrs) {
            continue;
        }
        let pktinfo = pktinfo.and_then(|(dest, index)| match dest {
            IpAddr::V4(dest) => Some((dest, index)),
            IpAddr::V6(_) => None,
        });
        let Some((frame, reply_iface_ip, origin, forward)) =
            collector.listeners.route(&buf[..n], &local_v4, pktinfo)
        else {
            continue;
        };
        let replier = started.borrow().clone();
        let outputs = process_search_datagram(
            replier.as_ref(),
            forward,
            udp_port,
            frame,
            src,
            reply_iface_ip,
            origin,
        )
        .await;
        dispatch_search_outputs(&collector.sock, outputs).await;
    }
}

/// Send what [`process_search_datagram`] decided from the collector: a
/// reply pinned as [`super::search_engine::reply_pin`] says, a forward to
/// the ORIGIN_TAG group the collector is a member of.
async fn dispatch_search_outputs(sock: &tokio::net::UdpSocket, outputs: Vec<SearchOutput>) {
    for out in outputs {
        match out {
            SearchOutput::Reply {
                dest,
                iface_hint,
                bytes,
            } => {
                #[cfg(target_os = "linux")]
                let send = match super::search_engine::reply_pin(dest, iface_hint) {
                    Some((dest, src)) => {
                        sock.async_io(tokio::io::Interest::WRITABLE, || {
                            super::search_engine::send_from_v4(sock, &bytes, dest, src)
                        })
                        .await
                    }
                    None => sock.send_to(&bytes, dest).await,
                };
                #[cfg(not(target_os = "linux"))]
                let send = sock.send_to(&bytes, dest).await;
                if let Err(e) = send {
                    let via = iface_hint.unwrap_or(Ipv4Addr::UNSPECIFIED);
                    debug!("udp send to {dest} via {via}: {e}");
                }
            }
            SearchOutput::OriginTagForward { dest, bytes } => {
                if let Err(e) = sock.send_to(&bytes, dest).await {
                    debug!("ORIGIN_TAG forward to {dest}: {e}");
                }
            }
        }
    }
}

/// The v6 responder on pvxs's `[::]` collector: what its
/// listeners take ([`V6Listen::takes`]) is answered from the socket, and
/// nothing is forwarded — pvxs forwards only a v4 destination
/// (`udp_collector.cpp:385`).
async fn run_udp_responder_v6(
    bind: V6Bind,
    udp_port: u16,
    ignore_addrs: Vec<(IpAddr, u16)>,
    started: SearchStarted,
) -> PvaResult<()> {
    let V6Bind { sock, listen } = bind;
    let socket = tokio::net::UdpSocket::from_std(sock).map_err(PvaError::Io)?;
    if let Err(e) = enable_so_rxq_ovfl_for_socket(&socket) {
        debug!("v6 SO_RXQ_OVFL enable failed (non-fatal): {e}");
    }
    let mut buf = vec![0u8; 64 * 1024];
    let mut prev_drops_v6: u32 = 0;
    loop {
        let (n, src, pktinfo, drops) = match recv_collector(&socket, &mut buf).await {
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
        if !filter_inbound(src, &ignore_addrs) || !listen.takes(src, pktinfo.map(|(dest, _)| dest))
        {
            continue;
        }
        let replier = started.borrow().clone();
        let outputs = process_search_datagram(
            replier.as_ref(),
            false,
            udp_port,
            &buf[..n],
            src,
            Ipv4Addr::UNSPECIFIED,
            Origin::Direct,
        )
        .await;
        for out in outputs {
            if let SearchOutput::Reply { dest, bytes, .. } = out
                && let Err(e) = socket.send_to(&bytes, dest).await
            {
                debug!("v6 udp send to {dest}: {e}");
            }
        }
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
    // server would reply to the shim. Mirrors `process_search_datagram` and
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
        crate::proto::encode_string_bytes_into(name, order, &mut p);
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
    use crate::codec::PvaCodec;
    use crate::server_native::beacon::build_beacon;
    thread_local! {
        /// Called with each number `bind_udp` is about to bind on v6.
        pub(super) static BEFORE_V6_BIND: std::cell::RefCell<Option<Box<dyn FnMut(u16)>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Loopback only.
    fn lo_mcast_server() -> crate::server_native::PvaServerConfig {
        crate::server_native::PvaServerConfig {
            interfaces: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into()],
            tcp_port: 0,
            udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        }
    }

    /// The ORIGIN_TAG socket is bound and joined before `start` returns, as
    /// pvxs's `UDPCollector` is in the `Server` constructor: a SEARCH sent to
    /// the group before the responder task first runs is still answered.
    #[tokio::test]
    async fn the_origin_tag_socket_is_joined_when_start_returns() {
        use crate::nt::typed::TypedNT;
        use crate::server_native::{PvaServer, SharedPV, SharedSource};
        let pv = SharedPV::new();
        pv.open(f64::descriptor(), f64::to_pv_field(&1.0)).unwrap();
        let source = Arc::new(SharedSource::new());
        source.add("LO:MCAST:PV", pv);
        let server = PvaServer::start(source, lo_mcast_server()).expect("start");
        let port = server.report().udp_port;

        // This runtime is current-thread, so the responder has not run yet.
        #[cfg(target_os = "linux")]
        {
            let wildcard = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
            let ours: std::collections::HashSet<String> = std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
                .map(|l| l.to_string_lossy().into_owned())
                .collect();
            let held = named_port::port_holders::udp_holders(port)
                .unwrap()
                .iter()
                .any(|h| h.local == wildcard && ours.contains(&h.fd_link()));
            assert!(held, "no wildcard socket of ours on {port} at start");
        }

        let client = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket2::SockRef::from(&client)
            .set_multicast_if_v4(&Ipv4Addr::LOCALHOST)
            .unwrap();
        let reply_port = client.local_addr().unwrap().port();
        // Tagged with the listed interface: a bare forward has the wildcard
        // destination, which no listener on 127.0.0.1 takes.
        let mut frame = PvaCodec::build_origin_tag_prefix(Ipv4Addr::LOCALHOST).to_vec();
        frame.extend(PvaCodec { big_endian: false }.build_search(
            1,
            0x1234,
            "LO:MCAST:PV",
            [127, 0, 0, 1],
            reply_port,
            false,
        ));
        client
            .send_to(&frame, (epics_base_rs::net::ORIGIN_TAG_MCAST_GROUP, port))
            .unwrap();
        client.set_nonblocking(true).unwrap();
        let client = tokio::net::UdpSocket::from_std(client).unwrap();
        let mut buf = [0u8; 1024];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
            .await
            .expect("no SEARCH_RESPONSE: the group SEARCH sent at start was lost")
            .unwrap();
        let hdr = PvaHeader::decode(&mut Cursor::new(&buf[..n])).unwrap();
        assert_eq!(hdr.command, Command::SearchResponse.code());
    }

    /// pvxs's collector reads from its bind (`udp_collector.cpp:169`) and
    /// forwards a local unicast SEARCH at once (`forwardM`, `:385-397`), while
    /// its search listeners start only in `Server::Pvt::start`
    /// (`server.cpp:555-599`): a SEARCH that reaches the bound sockets before
    /// `start` is never answered, and a unicast one is forwarded then.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_search_before_start_is_dropped_and_a_unicast_one_forwarded() {
        use crate::nt::typed::TypedNT;
        use crate::server_native::blocking::tests::origin_tag_member;
        use crate::server_native::{PvaServer, SharedPV, SharedSource};
        /// Whether a group datagram carrying `needle` arrives, each within 2 s.
        async fn saw(member: &tokio::net::UdpSocket, needle: &[u8]) -> bool {
            let mut buf = [0u8; 1500];
            while let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_secs(2), member.recv(&mut buf)).await
            {
                if buf[..n].windows(needle.len()).any(|w| w == needle) {
                    return true;
                }
            }
            false
        }
        let bound = PvaServer::bind(lo_mcast_server()).expect("bind");
        let port = bound.report_handle().report().udp_port;
        let group = SocketAddr::from((epics_base_rs::net::ORIGIN_TAG_MCAST_GROUP, port));
        let member = origin_tag_member(port);
        member.set_nonblocking(true).unwrap();
        let member = tokio::net::UdpSocket::from_std(member).unwrap();
        let client = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket2::SockRef::from(&client)
            .set_multicast_if_v4(&Ipv4Addr::LOCALHOST)
            .unwrap();
        let reply_port = client.local_addr().unwrap().port();
        client.set_nonblocking(true).unwrap();
        let client = tokio::net::UdpSocket::from_std(client).unwrap();
        let codec = PvaCodec { big_endian: false };
        let tagged = |seq| {
            let mut frame = PvaCodec::build_origin_tag_prefix(Ipv4Addr::LOCALHOST).to_vec();
            frame.extend(codec.build_search(
                seq,
                0x1234,
                "LO:MCAST:PV",
                [127, 0, 0, 1],
                reply_port,
                false,
            ));
            frame
        };
        // Answered once started; seen by the member when the collector has it.
        client.send_to(&tagged(1), group).await.unwrap();
        assert!(
            saw(&member, b"LO:MCAST:PV").await,
            "the group SEARCH reached the group"
        );
        // Queued behind it on the collector, so its forward is proof the
        // collector read the group SEARCH before `start`.
        let sink = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let unicast = codec.build_search(
            2,
            0x1235,
            "pre.start.forward",
            [127, 0, 0, 1],
            sink.local_addr().unwrap().port(),
            true,
        );
        client
            .send_to(&unicast, (Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let forwarded = saw(&member, b"pre.start.forward").await;

        let pv = SharedPV::new();
        pv.open(f64::descriptor(), f64::to_pv_field(&1.0)).unwrap();
        let source = Arc::new(SharedSource::new());
        source.add("LO:MCAST:PV", pv);
        let _server = bound.start(source).expect("start");
        let mut buf = [0u8; 1500];
        let answered = tokio::time::timeout(Duration::from_secs(1), client.recv(&mut buf))
            .await
            .is_ok();
        assert_eq!(
            (forwarded, answered),
            (true, false),
            "(unicast forwarded before start, group SEARCH answered after)"
        );
        client.send_to(&tagged(3), group).await.unwrap();
        let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf))
            .await
            .expect("a SEARCH_RESPONSE once started")
            .unwrap();
        let hdr = PvaHeader::decode(&mut Cursor::new(&buf[..n])).unwrap();
        assert_eq!(hdr.command, Command::SearchResponse.code());
    }

    /// pvxs throws out of the `Server` constructor when the collector cannot
    /// bind (`udp_collector.cpp:151`, `evhelper.cpp:498-505`). A stranger on
    /// 127.0.0.2:P without reuse flags leaves the 127.0.0.1:P search bind free
    /// but refuses the wildcard ORIGIN_TAG bind on P.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_failed_origin_tag_bind_fails_the_start() {
        use crate::server_native::{PvaServer, SharedSource};
        let holder = std::net::UdpSocket::bind((Ipv4Addr::new(127, 0, 0, 2), 0)).unwrap();
        let config = crate::server_native::PvaServerConfig {
            udp_port: holder.local_addr().unwrap().port(),
            ..lo_mcast_server()
        };
        let err = PvaServer::start(Arc::new(SharedSource::new()), config).err();
        assert!(
            matches!(&err, Some(PvaError::Io(e)) if epics_base_rs::net::port_held(e)),
            "{:?}",
            err.map(|e| e.to_string())
        );
    }

    /// Start a server on `config` and send one SEARCH for its PV to `dest` on
    /// its search port, out of the loopback interface; whether it answered.
    /// The SEARCH is unicast-flagged whatever `dest` is: pvxs decides by the
    /// destination whether to forward or answer.
    #[cfg(target_os = "linux")]
    async fn answers_search_to(
        config: crate::server_native::PvaServerConfig,
        dest: Ipv4Addr,
    ) -> bool {
        use crate::nt::typed::TypedNT;
        use crate::server_native::{PvaServer, SharedPV, SharedSource};
        let pv = SharedPV::new();
        pv.open(f64::descriptor(), f64::to_pv_field(&1.0)).unwrap();
        let source = Arc::new(SharedSource::new());
        source.add("COLLECTOR:PV", pv);
        let server = PvaServer::start(source, config).expect("start");
        let port = server.report().udp_port;
        let client = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket2::SockRef::from(&client)
            .set_multicast_if_v4(&Ipv4Addr::LOCALHOST)
            .unwrap();
        let reply_port = client.local_addr().unwrap().port();
        let frame = PvaCodec { big_endian: false }.build_search(
            1,
            0x1234,
            "COLLECTOR:PV",
            [127, 0, 0, 1],
            reply_port,
            true,
        );
        client.send_to(&frame, (dest, port)).unwrap();
        client.set_nonblocking(true).unwrap();
        let client = tokio::net::UdpSocket::from_std(client).unwrap();
        let mut buf = [0u8; 1024];
        loop {
            let Ok(got) =
                tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut buf)).await
            else {
                return false;
            };
            let (n, _) = got.unwrap();
            let hdr = PvaHeader::decode(&mut Cursor::new(&buf[..n])).unwrap();
            if hdr.command == Command::SearchResponse.code() {
                return true;
            }
        }
    }

    /// pvxs joins a beacon group on the collector and listens on it only
    /// under `auto_beacon` (`config.cpp:512-519`), so a SEARCH sent to the
    /// group is answered then and not otherwise.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_group_search_is_answered_only_with_auto_beacon() {
        let group = Ipv4Addr::new(224, 0, 9, 17);
        let config = |auto_beacon| crate::server_native::PvaServerConfig {
            auto_beacon,
            beacon_destinations: vec![
                crate::config::env::Endpoint::parse("224.0.9.17@127.0.0.1", 5076).unwrap(),
            ],
            ..lo_mcast_server()
        };
        assert!(
            answers_search_to(config(true), group).await,
            "AUTO_BEACON=YES"
        );
        assert!(
            !answers_search_to(config(false), group).await,
            "AUTO_BEACON=NO"
        );
        // Answered from the arrival interface even when it is not listed: a
        // group is pvxs `Broadcast`, which is never forwarded to a peer that
        // would then find no listener for the interface.
        let unlisted = crate::server_native::PvaServerConfig {
            interfaces: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 0).into()],
            ..config(true)
        };
        assert!(
            answers_search_to(unlisted, group).await,
            "interface not listed"
        );
    }

    /// A unicast SEARCH to a local address outside `EPICS_PVAS_INTF_ADDR_LIST`
    /// reaches only the collector, where no pvxs listener takes it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_search_to_an_unlisted_address_is_not_answered() {
        assert!(answers_search_to(lo_mcast_server(), Ipv4Addr::LOCALHOST).await);
        assert!(!answers_search_to(lo_mcast_server(), Ipv4Addr::new(127, 0, 0, 2)).await);
    }

    /// Beacons leave from pvxs's unbound `beaconSender4`, so a group or a
    /// broadcast on an interface outside `EPICS_PVAS_INTF_ADDR_LIST` still
    /// gets them.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn beacons_reach_destinations_outside_the_interface_list() {
        use crate::server_native::{PvaServer, SharedSource};
        let group = Ipv4Addr::new(224, 0, 9, 18);
        let sniffer = || std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let in_group = sniffer();
        in_group
            .join_multicast_v4(&group, &Ipv4Addr::LOCALHOST)
            .unwrap();
        let on_bcast = sniffer();
        let dests = [
            format!(
                "{group}:{}@127.0.0.1",
                in_group.local_addr().unwrap().port()
            ),
            format!("127.255.255.255:{}", on_bcast.local_addr().unwrap().port()),
        ];
        let config = crate::server_native::PvaServerConfig {
            interfaces: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 0).into()],
            beacon_destinations: dests
                .iter()
                .map(|d| Endpoint::parse(d, 5076).unwrap())
                .collect(),
            ..lo_mcast_server()
        };
        let _server = PvaServer::start(Arc::new(SharedSource::new()), config).expect("start");
        for (dest, sock) in dests.iter().zip([in_group, on_bcast]) {
            sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let got = tokio::task::spawn_blocking(move || {
                let mut buf = [0u8; 1024];
                loop {
                    let n = sock.recv(&mut buf).ok()?;
                    let hdr = PvaHeader::decode(&mut Cursor::new(&buf[..n])).ok()?;
                    if hdr.command == Command::Beacon.code() {
                        return Some(());
                    }
                }
            })
            .await
            .unwrap();
            assert!(got.is_some(), "no beacon to {dest}");
        }
    }

    /// Holds each number `bind_udp` tries on v6, as a stranger with no reuse
    /// flag would, for the first `n` tries; returns the tries seen. A number
    /// some other process already holds there counts as held.
    fn hold_v6_for(n: usize) -> std::rc::Rc<std::cell::RefCell<Vec<u16>>> {
        let tried = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut held = Vec::new();
        let seen = tried.clone();
        BEFORE_V6_BIND.set(Some(Box::new(move |port| {
            seen.borrow_mut().push(port);
            if seen.borrow().len() <= n {
                held.extend(try_v6_holder(port));
            }
        })));
        tried
    }

    fn try_v6_holder(port: u16) -> Option<socket2::Socket> {
        use socket2::{Domain, Protocol, Socket, Type};
        let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        s.set_only_v6(true).unwrap();
        let addr = SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), port);
        match s.bind(&addr.into()) {
            Ok(()) => Some(s),
            Err(e) if epics_base_rs::net::port_held(&e) => None,
            Err(e) => panic!("hold {port} on v6: {e}"),
        }
    }

    /// Loopback v4 and the v6 wildcard, so both families bind.
    fn localhost_and_v6() -> ServerInterfaces {
        ServerInterfaces {
            addrs: vec![
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)),
            ],
            ..Default::default()
        }
    }

    #[epics_macros_rs::epics_test]
    async fn an_ephemeral_number_held_on_v6_is_chosen_again() {
        let tried = hold_v6_for(3);
        let bound = super::bind_udp(0, &localhost_and_v6()).expect("bind");
        BEFORE_V6_BIND.take();
        let tried = tried.borrow();
        // More than four when a stranger already held a later pick.
        assert!(tried.len() >= 4, "{tried:?}");
        assert_eq!(*tried.last().unwrap(), bound.port);
        assert!(!tried[..tried.len() - 1].contains(&bound.port), "{tried:?}");
        let v6 = bound.v6.expect("the v6 socket");
        assert_eq!(v6.sock.local_addr().unwrap().port(), bound.port);
    }

    /// The re-choice is bounded: a number held on every try fails the start.
    #[epics_macros_rs::epics_test]
    async fn an_ephemeral_number_held_on_every_try_fails_the_bind() {
        let tried = hold_v6_for(usize::MAX);
        let err = super::bind_udp(0, &localhost_and_v6()).err();
        BEFORE_V6_BIND.take();
        assert!(
            matches!(&err, Some(PvaError::Io(e)) if epics_base_rs::net::port_held(e)),
            "{err:?}"
        );
        assert_eq!(tried.borrow().len(), 32);
    }

    /// A configured number is never swapped for another: held on v6, the
    /// start fails, as softIocPVX's does.
    #[epics_macros_rs::epics_test]
    async fn a_configured_number_held_on_v6_fails_the_bind() {
        // The number is free on v6 only; a stranger holding it on v4 fails the
        // v4 bind before the v6 one is tried, so the test takes another.
        let mut tries = 0;
        let (port, tried, err) = loop {
            tries += 1;
            let holder = try_v6_holder(0).unwrap();
            let port = holder.local_addr().unwrap().as_socket().unwrap().port();
            let tried = hold_v6_for(0);
            let err = super::bind_udp(port, &localhost_and_v6()).err();
            BEFORE_V6_BIND.take();
            if !tried.borrow().is_empty() {
                break (port, tried, err);
            }
            assert!(tries < 5, "v4 bind of {port} failed {tries} times: {err:?}");
        };
        assert!(
            matches!(&err, Some(PvaError::Io(e)) if epics_base_rs::net::port_held(e)),
            "{err:?}"
        );
        assert_eq!(*tried.borrow(), [port]);
    }

    /// A pvxs client given this server's ephemeral number as
    /// `EPICS_PVA_BROADCAST_PORT` binds its beacon receiver on the wildcard
    /// there with fanout (`client.cpp:641-642`, `udp_collector.cpp:136`); an
    /// exclusive responder makes that bind `EADDRINUSE`.
    #[epics_macros_rs::epics_test]
    async fn an_ephemeral_responder_lets_a_fanout_listener_co_bind() {
        use socket2::{Domain, Protocol, Socket, Type};
        let interfaces = ServerInterfaces {
            addrs: vec![
                SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
                SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)),
            ],
            ..Default::default()
        };
        let bound = super::bind_udp(0, &interfaces).expect("bind");
        assert!(bound.v6.is_some());
        let wildcard = [
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), bound.port),
            SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), bound.port),
        ];
        // A specific address too, as a client bound to one NIC would.
        let lo = SocketAddr::from((Ipv4Addr::LOCALHOST, bound.port));
        // Held to the end, so each bind joins the ones before it.
        let mut held = Vec::new();
        for addr in wildcard.into_iter().chain([lo]) {
            let s =
                Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP)).unwrap();
            if addr.is_ipv6() {
                s.set_only_v6(true).unwrap();
            }
            PortUse::Shared
                .bind(&s, addr, PortOrigin::Configured)
                .unwrap_or_else(|e| panic!("fanout listener on {addr}: {e}"));
            held.push(s);
        }
    }

    use super::*;
    use crate::server_native::MonitorStream;
    use crate::server_native::search::build_search_response_proto;
    use crate::server_native::search_engine::search_matched_cids;
    // Named from its new home rather than re-exported through the module
    // under test: production `udp` has no forward-frame call of its own
    // any more, so a `pub(crate) use` at module scope would be an unused
    // import in a non-test build.
    use crate::server_native::search_engine::try_build_forward_frame;
    use crate::server_native::source::OpError;
    use epics_base_rs::types::RecordName;
    use std::net::SocketAddrV4;

    /// The flags byte of a SEARCH frame, after the header and sequence id.
    fn search_flags(frame: &[u8]) -> u8 {
        frame[PvaHeader::SIZE + 4]
    }

    /// Decode + dispatch in one call, with the argument list
    /// `process_search_datagram` had before stage B split the sends out of
    /// it. These cases are end-to-end by design — they assert on datagrams
    /// a real sniffer socket receives, which is what makes them wire-golden
    /// — so they keep driving both halves rather than inspecting the
    /// returned [`SearchOutput`] list.
    #[allow(clippy::too_many_arguments)]
    async fn process_and_dispatch(
        source: &DynSource,
        socket: &tokio::net::UdpSocket,
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
        let replier = SearchReplier {
            source: source.clone(),
            tcp_port,
            guid,
            protocol,
        };
        let outputs = process_search_datagram(
            Some(&replier),
            lo_mcast.is_some(),
            udp_port,
            frame,
            udp_src,
            reply_iface_ip,
            origin,
        )
        .await;
        dispatch_search_outputs(lo_mcast.map_or(socket, |lo| &**lo), outputs).await;
    }

    /// A reply carrying an `iface_hint` leaves the wildcard collector with
    /// that source address, one without it with the route's. 127.0.0.2 is
    /// not what the route to 127.0.0.1 picks.
    #[cfg(all(tokio_backend, target_os = "linux"))]
    #[epics_macros_rs::epics_test]
    async fn a_reply_with_an_iface_hint_leaves_with_that_source() {
        let pinned = Ipv4Addr::new(127, 0, 0, 2);
        let sock = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .expect("collector bind");
        let requester = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("requester bind");
        let dest = requester.local_addr().unwrap();
        let reply = |iface_hint| SearchOutput::Reply {
            dest,
            iface_hint,
            bytes: vec![1, 2, 3],
        };
        let mut buf = [0u8; 8];

        dispatch_search_outputs(&sock, vec![reply(Some(pinned))]).await;
        let (_, from) = requester.recv_from(&mut buf).await.unwrap();
        assert_eq!(from.ip(), IpAddr::V4(pinned));

        dispatch_search_outputs(&sock, vec![reply(None)]).await;
        let (_, from) = requester.recv_from(&mut buf).await.unwrap();
        assert_eq!(from.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    /// Stage B's point, stated as a test: the decode half runs with **no
    /// socket at all**. No `AsyncUdpV4`, no `tokio::net::UdpSocket`, and so
    /// no `epics_base_rs::net` — none of which exist on the RTEMS target,
    /// which is why the blocking driver could not reuse this decode before
    /// the split.
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
            fn list_pvs(&self) -> Vec<epics_base_rs::types::RecordName> {
                vec!["MY:PV".into()]
            }
            fn has_pv(&self, name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                let m = name == b"MY:PV";
                async move { m }
            }
            fn get_introspection(
                &self,
                _: &[u8],
            ) -> impl std::future::Future<Output = Option<crate::pvdata::FieldDesc>> + Send
            {
                async { None }
            }
            fn get_value(
                &self,
                _: &[u8],
            ) -> impl std::future::Future<Output = Option<crate::pvdata::PvField>> + Send
            {
                async { None }
            }
            fn put_value(
                &self,
                _: &[u8],
                _: crate::pvdata::PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only".into()) }
            }
            fn is_writable(&self, _: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _: &[u8],
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
        let replier = SearchReplier {
            source,
            tcp_port: 5075,
            guid,
            protocol: "tcp",
        };

        let outputs = process_search_datagram(
            Some(&replier),
            false, // no ORIGIN_TAG forwarding
            5076,
            &frame,
            udp_src,
            Ipv4Addr::new(192, 168, 9, 10), // NIC the SEARCH arrived on
            Origin::Direct,
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
            Some(&replier),
            false,
            5076,
            &frame,
            udp_src,
            Ipv4Addr::UNSPECIFIED,
            Origin::Direct,
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
            fn list_pvs(&self) -> Vec<epics_base_rs::types::RecordName> {
                Vec::new()
            }
            fn has_pv(&self, _name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn get_introspection(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { None }
            }
            fn get_value(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _name: &[u8],
                _value: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only test source".into()) }
            }
            fn is_writable(&self, _name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(EmptySource);
        let socket = Arc::new(
            tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                .await
                .expect("bind"),
        );

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
        assert_eq!(
            search_flags(inner) & 0x80,
            0,
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
    /// non-loopback direct path). Verify by sending a unicast SEARCH
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
            fn list_pvs(&self) -> Vec<epics_base_rs::types::RecordName> {
                Vec::new()
            }
            fn has_pv(&self, _name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn get_introspection(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { None }
            }
            fn get_value(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _name: &[u8],
                _value: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only test source".into()) }
            }
            fn is_writable(&self, _name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(EmptySource);
        let socket = Arc::new(
            tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                .await
                .expect("bind"),
        );
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
    /// the SEARCH names and asserting the responder emits no
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
            fn list_pvs(&self) -> Vec<epics_base_rs::types::RecordName> {
                vec!["MY:PV".into()]
            }
            fn has_pv(&self, name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                let m = name == b"MY:PV";
                async move { m }
            }
            fn get_introspection(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { Some(FieldDesc::Scalar(ScalarType::Double)) }
            }
            fn get_value(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _name: &[u8],
                _value: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only".into()) }
            }
            fn is_writable(&self, _name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(PresentSource);
        let socket = Arc::new(
            tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                .await
                .expect("bind"),
        );
        // Bind a sniffer socket so it would catch any reply the responder
        // tries to send back.
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
            fn list_pvs(&self) -> Vec<epics_base_rs::types::RecordName> {
                vec!["MY:PV".into()]
            }
            fn has_pv(&self, name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                let m = name == b"MY:PV";
                async move { m }
            }
            fn get_introspection(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { Some(FieldDesc::Scalar(ScalarType::Double)) }
            }
            fn get_value(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _name: &[u8],
                _value: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only".into()) }
            }
            fn is_writable(&self, _name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send
            {
                async { None }
            }
        }

        let source: DynSource = Arc::new(PresentSource);
        let socket = Arc::new(
            tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                .await
                .expect("bind"),
        );
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

        let server_src = ServerInfoSource::new(Vec::<epics_base_rs::types::RecordName>::new);

        // Direct-connect path still resolves `server`.
        assert!(
            server_src.has_pv(SERVER_PV_NAME.as_bytes()).await,
            "has_pv must still resolve `server` for direct TCP connect"
        );
        assert!(
            !server_src.searchable(SERVER_PV_NAME.as_bytes()).await,
            "`server` must NOT be UDP-search-advertised"
        );
        // pvxs `server` has no GET surface (onRPC only); the direct
        // path reaches it via RPC, and a GET returns no prototype/value.
        assert!(
            server_src
                .get_value(SERVER_PV_NAME.as_bytes())
                .await
                .is_none(),
            "`server` has no GET surface (pvxs onRPC-only)"
        );

        // UDP search path: a broadcast SEARCH naming `server` must
        // produce no reply on the sniffer socket.
        let source: DynSource = Arc::new(server_src);
        let socket = Arc::new(
            tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                .await
                .expect("bind"),
        );
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
            fn list_pvs(&self) -> Vec<epics_base_rs::types::RecordName> {
                vec!["MY:PV".into()]
            }
            fn has_pv(&self, name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                let m = name == b"MY:PV";
                async move { m }
            }
            fn searchable_from(
                &self,
                name: &[u8],
                requester: SocketAddr,
            ) -> impl std::future::Future<Output = bool> + Send {
                let ok =
                    name == b"MY:PV" && requester.ip() == IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
                async move { ok }
            }
            fn get_introspection(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { None }
            }
            fn get_value(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _name: &[u8],
                _value: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only".into()) }
            }
            fn is_writable(&self, _name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &[u8],
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
            fn list_pvs(&self) -> Vec<epics_base_rs::types::RecordName> {
                vec!["MY:PV".into()]
            }
            fn has_pv(&self, name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                let m = name == b"MY:PV";
                async move { m }
            }
            fn get_introspection(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
                async { Some(FieldDesc::Scalar(ScalarType::Double)) }
            }
            fn get_value(
                &self,
                _name: &[u8],
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                async { None }
            }
            fn put_value(
                &self,
                _name: &[u8],
                _value: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                async { Err("read-only".into()) }
            }
            fn is_writable(&self, _name: &[u8]) -> impl std::future::Future<Output = bool> + Send {
                async { false }
            }
            fn subscribe(
                &self,
                _name: &[u8],
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
    /// Unicast flag (pvxs `udp_collector.cpp:387-396`), and builds a
    /// flagless SEARCH the same way: the destination, not the flag,
    /// decides whether to forward.
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
        assert_eq!(search_flags(&out) & 0x80, 0, "unicast flag must be cleared");
        assert_eq!(
            req.reply_addr,
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)))
        );
        assert_eq!(req.reply_port, 54321);

        let flagless = codec.build_search(7, 42, "MY:PV", [10, 0, 0, 1], 5076, false);
        let out = try_build_forward_frame(&flagless, dest).expect("flagless → Some");
        let req = parse_search_request(&out).expect("re-parse ok");
        assert_eq!(search_flags(&out) & 0x80, 0);
        assert_eq!(req.reply_port, 54321);
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
        assert_eq!(search_flags(&out) & 0x80, 0, "unicast flag must be cleared");
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
        let base = build_beacon(guid, 5075, 42, 0xBEEF, "tcp");
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
        let bytes = build_beacon(guid, 5075, 42, 0xBEEF, "tcp");
        // 8-byte PVA header + 12-byte GUID = 20 bytes prefix.
        let payload = &bytes[8..];
        assert_eq!(&payload[0..12], &guid);
        assert_eq!(payload[12], 0); // flags byte
        assert_eq!(payload[13], 42, "beacon sequence at offset 13");
        assert_eq!(
            u16::from_be_bytes([payload[14], payload[15]]),
            0xBEEF,
            "change_count at offset 14"
        );
    }

    /// An explicit v6 beacon destination opens the v6 send socket and is
    /// sent to, as pvxs's `beaconSender6` (`server.cpp:779`). Regression
    /// guard against the v6 send path being dropped from the
    /// per-destination dispatch in the beacon loop.
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

        // The responder's own search port is ephemeral: this test reads only
        // the beacon it transmits, so nothing here needs the number and
        // nothing should be handed a port it has stopped holding.
        // Short beacon period so the first burst fires quickly.
        let task = tokio::spawn(run_udp_responder_with_config(
            Arc::new(SharedSource::new()) as DynSource,
            0,    // ephemeral search port; `bind_udp` picks and owns it
            5075, // advertised TCP port (any non-zero value)
            [0xAB; 12],
            "tcp",
            std::time::Duration::from_millis(50),
            std::time::Duration::from_secs(10),
            3,
            vec![Endpoint::from(v6_dest)],
            false, // auto_beacon=false: only the explicit v6 dest
            Vec::new(),
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

        // A unicast-flagged SEARCH (flags = 0x80) is not MustReply.
        let frame2 = codec.build_search(1, 7, "MY:PV", [0, 0, 0, 0], 5076, true);
        let req2 = parse_search_request(&frame2).expect("plain SEARCH parses");
        assert!(!req2.must_reply);
    }

    /// pvxs `src/server.cpp:728-729`: when `nreply==0` the `found` byte is
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

    fn req_with(must_reply: bool, reply: Option<IpAddr>) -> SearchRequest {
        SearchRequest {
            seq: 42,
            byte_order: ByteOrder::Little,
            queries: vec![(7, RecordName::from("PV:A")), (8, RecordName::from("PV:B"))],
            reply_addr: reply,
            reply_port: 5566,
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
        let req = req_with(true, Some(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        let source = "192.168.9.9:1111".parse().unwrap();
        let frame = rebuild_search_forward(&req, /*dest_unicast=*/ false, source);
        let out = parse_search_request(&frame).expect("rebuilt SEARCH parses");
        assert_eq!(
            search_flags(&frame) & 0x80,
            0,
            "Unicast must be cleared for a multicast dest"
        );
        assert!(out.must_reply, "MustReply must be preserved");
        assert_eq!(out.reply_addr, Some(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert_eq!(out.reply_port, 5566);
        assert_eq!(
            out.queries,
            vec![(7, RecordName::from("PV:A")), (8, RecordName::from("PV:B"))]
        );
    }

    /// A non-unicast SEARCH forwarded to a UNICAST destination must have
    /// the Unicast flag SET (pvxs mshim.cpp:144-145).
    #[test]
    fn mshim_search_multicast_to_unicast_sets_unicast() {
        let req = req_with(false, Some(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        let source = "192.168.9.9:1111".parse().unwrap();
        let frame = rebuild_search_forward(&req, /*dest_unicast=*/ true, source);
        let out = parse_search_request(&frame).expect("rebuilt SEARCH parses");
        assert_ne!(
            search_flags(&frame) & 0x80,
            0,
            "Unicast must be set for a unicast dest"
        );
        assert!(!out.must_reply);
    }

    /// An `isAny` reply address (unspecified) must be resolved to the
    /// datagram source so the forwarded SEARCH names the original
    /// requester as the reply target, not the shim. The announced reply
    /// PORT is preserved.
    #[test]
    fn mshim_search_isany_reply_resolves_to_source() {
        let req = req_with(false, None);
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
        let mut req = req_with(false, Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        req.protocols = vec![b"tls".to_vec(), b"tcp".to_vec()];
        let source = "192.168.9.9:1111".parse().unwrap();
        let frame = rebuild_search_forward(&req, true, source);
        let out = parse_search_request(&frame).expect("rebuilt SEARCH parses");
        assert_eq!(out.protocols, vec![b"tls".to_vec(), b"tcp".to_vec()]);
    }

    /// A SEARCH name is wire bytes in pvxs: a Latin-1 query survives the
    /// parse and the forward rebuild unchanged.
    #[test]
    fn mshim_search_keeps_a_latin1_query_byte_for_byte() {
        let mut req = req_with(false, Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        req.queries = vec![(9, RecordName::from(&b"TEMP:\xb0C"[..]))];
        let source = "192.168.9.9:1111".parse().unwrap();
        let frame = rebuild_search_forward(&req, true, source);
        let out = parse_search_request(&frame).expect("rebuilt SEARCH parses");
        assert_eq!(out.queries, vec![(9, RecordName::from(&b"TEMP:\xb0C"[..]))]);
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
        assert_eq!(search_flags(&rebuilt) & 0x80, 0);
        assert_eq!(out.queries, vec![(7, RecordName::from("MY:PV"))]);
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
        let frame = build_beacon(guid, 5075, 3, 9, "tcp");
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
            vec![(7, RecordName::from("PV:A"))],
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
        let beacon = build_beacon([0xCDu8; 12], 5075, 3, 9, "tcp");
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

    /// The groups [`ServerInterfaces`] joins for `dests` alone.
    fn beacon_groups(dests: &[Endpoint], nics: &[Ipv4Addr]) -> Vec<(Ipv4Addr, Ipv4Addr)> {
        let nics: Vec<_> = nics.iter().map(|&ip| (ip, 0)).collect();
        ServerInterfaces::new(&[], dests, true, &crate::config::IfaceMap::of_v4(&nics))
            .unwrap()
            .groups
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
            beacon_groups(&dests, &nics),
            vec![(Ipv4Addr::new(224, 1, 1, 1), Ipv4Addr::new(10, 0, 0, 1))]
        );
        assert!(beacon_groups(&[], &nics).is_empty());
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
            beacon_groups(&dests, &nics),
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
            beacon_groups(&dests, &nics),
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
    ///   joined every NIC), and NOT expanded over the external-NIC set;
    ///   one the map does not know joins on `INADDR_ANY`.
    /// * a modifier-less multicast destination expands over *every* external
    ///   NIC — one join target per NIC — even NICs outside
    ///   `EPICS_PVAS_INTF_ADDR_LIST` (the `external_nics` argument is the
    ///   full external set, not the bound interface set).
    ///
    /// `(group, iface)` pairs are deduplicated, first-appearance order kept.
    #[test]
    fn multicast_join_plan_respects_iface_modifier_and_expands_modifierless() {
        let external = [Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 1, 1)];

        // Explicit @iface: joined on that NIC alone.
        let explicit = Endpoint {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 2, 9)), 5076),
            ttl: -1,
            iface: Some("n1".to_string()),
        };
        assert_eq!(
            beacon_groups(std::slice::from_ref(&explicit), &external),
            vec![(Ipv4Addr::new(224, 0, 2, 9), Ipv4Addr::new(10, 0, 1, 1))],
            "explicit @iface joins exactly that interface, not every NIC"
        );
        let unknown = Endpoint {
            iface: Some("nosuch".to_string()),
            ..explicit.clone()
        };
        assert_eq!(
            beacon_groups(std::slice::from_ref(&unknown), &external),
            vec![(Ipv4Addr::new(224, 0, 2, 9), Ipv4Addr::UNSPECIFIED)],
        );

        // Modifier-less: expands over every external NIC.
        let wildcard: Endpoint =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 3, 3)), 5076).into();
        assert_eq!(
            beacon_groups(std::slice::from_ref(&wildcard), &external),
            vec![
                (Ipv4Addr::new(224, 0, 3, 3), Ipv4Addr::new(10, 0, 0, 1)),
                (Ipv4Addr::new(224, 0, 3, 3), Ipv4Addr::new(10, 0, 1, 1)),
            ],
            "modifier-less multicast joins every external NIC"
        );

        // Both together, plus a duplicate wildcard entry: dedup keeps each
        // (group, iface) pair once, in first-appearance order.
        let plan = beacon_groups(&[explicit.clone(), wildcard.clone(), wildcard], &external);
        assert_eq!(
            plan,
            vec![
                (Ipv4Addr::new(224, 0, 2, 9), Ipv4Addr::new(10, 0, 1, 1)),
                (Ipv4Addr::new(224, 0, 3, 3), Ipv4Addr::new(10, 0, 0, 1)),
                (Ipv4Addr::new(224, 0, 3, 3), Ipv4Addr::new(10, 0, 1, 1)),
            ],
        );
    }
}
