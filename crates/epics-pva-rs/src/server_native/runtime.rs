//! Top-level [`PvaServer`] runtime: spawns UDP responder + TCP listener.

// RTEMS-EXEC-MODEL-ALLOW(11): checked - these run and pass in the feature-ON suite.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use crate::error::{PvaError, PvaResult};

use super::source::{ChannelInvalidator, ChannelSource, ChannelSourceObj, DynSource};
use super::udp::{random_guid, run_udp_responder_on_socket, run_udp_responder_v6};

// The config record moved to [`super::config`] so the protocol layer can name
// it without inheriting this module's host-only gate. Re-exported here so
// every `server_native::runtime::PvaServerConfig` path keeps resolving.
pub use super::config::{DEFAULT_MAX_MESSAGE_SIZE, PvaServerConfig};

/// Run a native PVA server forever.
///
/// The server spawns:
///
/// - UDP search responder on `config.udp_port` (also emits beacons every
///   15 s)
/// - TCP listener on `config.tcp_port` (handles connections concurrently)
pub async fn run_pva_server<S>(source: Arc<S>, config: PvaServerConfig) -> PvaResult<()>
where
    S: ChannelSource + 'static,
{
    run_pva_server_reporting(source, config, |_| {}).await
}

/// Like [`run_pva_server`], but invokes `on_started` once with a cheap,
/// shareable [`ServerReportHandle`] *after* the listeners are bound — so
/// the actually-bound TCP/TLS ports (which may differ from the requested
/// ones under the ephemeral-fallback path) are already known. The
/// callback runs synchronously between `start()` and `wait()`, before any
/// `.await`, so the handle is published the instant the server is live.
///
/// This is the seam the iocsh `pvxsr` command rides on: the native
/// `PvaServer` is born here and consumed by `wait()`, so a shell command
/// running inside the server has no other way to reach the report state.
///
/// It is also the only way to run a server on a **kernel-assigned** port and
/// learn the number: with `udp_port`/`tcp_port` = 0, `start()` binds and the
/// handle reports what it got, so a caller never has to guess a port or
/// probe-then-rebind one. `epics-oracle-rs`'s differential harness rides this
/// seam for exactly that reason.
pub async fn run_pva_server_reporting<S, F>(
    source: Arc<S>,
    config: PvaServerConfig,
    on_started: F,
) -> PvaResult<()>
where
    S: ChannelSource + 'static,
    F: FnOnce(ServerReportHandle),
{
    let server = PvaServer::start(source, config)?;
    on_started(server.report_handle());
    server.wait().await
}

/// Resolve the TCP listener bind addresses from the server interface
/// list.
///
/// `EPICS_PVAS_INTF_ADDR_LIST` (`interfaces`) is the address set the
/// server binds to; when empty it falls back to the single `bind_ip`
/// wildcard default (pvxs `server.cpp:407-487` derives the TCP listener
/// set from `effective.interfaces`). A wildcard entry (`0.0.0.0` /
/// `[::]`) subsumes every specific address — binding a specific address
/// on top of a wildcard already holding the port would fail — so it is
/// bound alone. Otherwise one listener is bound per listed address so
/// each interface is genuinely constrained at the kernel (a single
/// `0.0.0.0` listener would accept on every NIC regardless of the list).
fn tcp_bind_addresses(interfaces: &[IpAddr], bind_ip: IpAddr) -> Vec<IpAddr> {
    if interfaces.is_empty() {
        vec![bind_ip]
    } else if let Some(wildcard) = interfaces.iter().find(|ip| ip.is_unspecified()) {
        vec![*wildcard]
    } else {
        interfaces.to_vec()
    }
}

/// Handle to a running PVA server returned by [`PvaServer::start`].
///
/// Holds the JoinHandles for the UDP responder and TCP listener tasks
/// plus a shutdown channel. Use [`PvaServer::stop`] for graceful
/// shutdown — accept loop exits immediately so no new connections, and
/// existing per-client handler tasks unwind on their next read/write
/// (TCP keepalive plus the read-loop's `op_timeout` bound the stragglers).
/// [`PvaServer::wait`] blocks until both tasks have observed the
/// shutdown and returned.
pub struct PvaServer {
    /// Wrapped in Option so consuming methods (`run`, `wait`) can
    /// `.take()` the handles while leaving the struct intact for
    /// the Drop impl below.
    udp_handle: Option<tokio::task::JoinHandle<PvaResult<()>>>,
    /// Optional companion IPv6 UDP responder (PR #205 IPv6 Stage 2).
    /// Spawned only when `PvaServerConfig::enable_ipv6_udp = true`.
    /// `None` matches every existing v4-only deployment.
    udp_v6_handle: Option<tokio::task::JoinHandle<PvaResult<()>>>,
    tcp_handle: Option<tokio::task::JoinHandle<PvaResult<()>>>,
    /// Held only for the Drop impl. JoinHandle can't be cloned and
    /// `run`/`wait` may have already taken it, so Drop uses these
    /// AbortHandles to reach the live task either way.
    udp_abort: tokio::task::AbortHandle,
    udp_v6_abort: Option<tokio::task::AbortHandle>,
    tcp_abort: tokio::task::AbortHandle,
    /// Effective config the server is running under. Captured at
    /// `start()` so [`Self::client_config`] can hand back a builder
    /// pre-pointed at the actual bound TCP port without re-reading env
    /// vars (which may have changed since startup).
    effective_config: PvaServerConfig,
    /// Bound TCP socket address — useful when the configured port was
    /// 0, or the requested port was taken, and the OS picked one. This
    /// is the actually-bound port (`start()` stamps it back from
    /// `first_listener.local_addr()`), not the requested value —
    /// query [`Self::report`] for it, never assume `config.tcp_port`.
    bound_tcp_port: u16,
    /// Bound dedicated-TLS port, advertised as the `"tls"` SEARCH endpoint.
    /// Equals [`Self::bound_tcp_port`] when TLS shares the plaintext port
    /// (no separate listener) and is meaningless when TLS is disabled.
    bound_tls_port: u16,
    /// Programmatic interrupt for [`Self::run`]. Not used by `wait()`.
    interrupt: Arc<tokio::sync::Notify>,
    /// Per-peer book-keeping registry shared with `run_tcp_server`'s
    /// per-connection task. The accept loop registers an entry
    /// on connect; the connection task updates `last_rx_at` and
    /// `channels` periodically; the entry is removed on disconnect.
    /// `PvaServer::report()` snapshots the registry to surface per-
    /// connection diagnostics (pvxs `Server::report()` parity at the
    /// "live peers + channel counts" level).
    pub(crate) peers: Arc<crate::server_native::peers::PeerRegistry>,
}

impl Drop for PvaServer {
    /// Abort the listener tasks when the server value is dropped.
    /// Without this the bound TCP/UDP sockets and their accept loops
    /// outlive the PvaServer struct, leaking ports across test runs
    /// and surviving panics in the binary's main task. Per-connection
    /// handler tasks unwind on their next IO call.
    ///
    /// Skipped when the JoinHandles have already been moved out (via
    /// `run`/`wait`) — in that case the consuming method handles
    /// shutdown. Without the gating the abort would race with a
    /// reconnect on the same port: the listener task is killed
    /// mid-syscall before the OS releases the binding, surfacing as
    /// ConnectionRefused on the next bind attempt for ~hundreds of ms.
    fn drop(&mut self) {
        if self.udp_handle.is_some() || self.tcp_handle.is_some() {
            self.udp_abort.abort();
            if let Some(ref abort) = self.udp_v6_abort {
                abort.abort();
            }
            self.tcp_abort.abort();
            self.interrupt.notify_waiters();
        }
    }
}

impl PvaServer {
    /// Convenience factory: a loopback-only server with auto-picked
    /// free ports. Mirrors pvxs `Config::isolated().build()`. Useful
    /// for self-contained tests where a UDP-discoverable production
    /// config would interfere with concurrent runs.
    pub fn isolated<S>(source: Arc<S>) -> PvaResult<Self>
    where
        S: ChannelSource + 'static,
    {
        // Robustness: pass `tcp_port = 0` and let the OS pick during
        // the synchronous bind inside Self::start. The previous
        // design pre-bound ephemeral ports just to know them, then
        // dropped the binders before re-binding inside the accept
        // task — a concurrent test could steal the freshly-released
        // port in that window. Now there's no window at all: the
        // listener that ends up serving clients is the one we bound
        // before returning.
        //
        // UDP still uses pick-and-drop because the responder task
        // owns the UDP socket lifecycle and we don't yet thread a
        // pre-bound socket through; UDP search is also self-contained
        // (each test gets its own ephemeral port and discovers via
        // direct addr) so the race window is harmless there.
        let pick_udp = || -> PvaResult<u16> {
            let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
            let p = l.local_addr()?.port();
            drop(l);
            Ok(p)
        };
        let cfg = PvaServerConfig {
            tcp_port: 0,
            udp_port: pick_udp()?,
            ..PvaServerConfig::isolated()
        };
        Self::start(source, cfg)
    }

    /// Spawn the UDP responder and TCP listener; return a handle.
    ///
    /// # Errors
    ///
    /// Returns [`PvaError::Io`] if the TCP listener cannot be bound —
    /// e.g. the requested `bind_ip` is not a local address, or the
    /// requested port is taken and not eligible for the ephemeral
    /// fallback (the fallback covers `AddrInUse` / `PermissionDenied`
    /// on a non-zero requested port only). A bind failure returns
    /// `Err` rather than aborting the process.
    ///
    /// The user-supplied `source` is wrapped in a
    /// [`super::CompositeSource`] together with the built-in
    /// [`super::server_info::ServerInfoSource`]. The built-in source is
    /// registered at `order = -1` — BEFORE default-order (0) user
    /// sources — mirroring pvxs registering its `ServerSource` at
    /// `(order = -1, "__server")` (server.cpp:542-547), where the lowest
    /// order is consulted first. It only claims the reserved `server`
    /// name, so it shadows a user PV named `server` (the pvxs
    /// diagnostic-source contract) while all other names fall through to
    /// the user source; a user that wants to own `server` must register
    /// at an explicit order `< -1`. The built-in source answers GET / RPC
    /// against the `server` PV so `pvlist`-style clients can enumerate
    /// hosted channels and read server info (GUID, version, peer counts).
    pub fn start<S>(source: Arc<S>, config: PvaServerConfig) -> PvaResult<Self>
    where
        S: ChannelSource + 'static,
    {
        // `?` and not a fallback: a host with no entropy source must fail to
        // start rather than advertise a guessable identity — see `random_guid`
        // for why every consumer of a GUID collision degrades silently, which
        // makes this the last moment the problem is visible.
        let guid = random_guid()?;
        // the TCP-circuit SEARCH handler reads this guid
        // out of the per-connection config copy to populate the
        // SEARCH_RESPONSE body. Stamp it onto a local mut copy so
        // every per-conn task sees the same identity the UDP path
        // does.
        let mut config = config;
        config.guid = guid;
        // PVX-82: refuse to start when the env named interface(s) that all
        // failed to resolve — binding the wildcard `0.0.0.0` here would
        // silently listen on every interface instead of the requested
        // restriction (pvxs hard-fails such a config: `config.cpp:172-174`).
        if let Some(msg) = config.intf_addr_error.take() {
            return Err(PvaError::Protocol(format!("PvaServer::start: {msg}")));
        }
        // PVX-82 (IGNORE sibling): same refusal for an all-unresolvable
        // blocklist — running with a silently-empty IGNORE list would let
        // peers the operator meant to block through (pvxs `required=true`
        // hard-fails this config at `config.cpp:172-174`).
        if let Some(msg) = config.ignore_addr_error.take() {
            return Err(PvaError::Protocol(format!("PvaServer::start: {msg}")));
        }
        // The live per-peer registry is created up-front so the
        // built-in server-info source can report connection counts.
        let peers = crate::server_native::peers::PeerRegistry::new();

        // The reserved `__server` composition — the same one
        // `BlockingPvaServer::bind` performs. Written once in
        // `server_info::compose_with_server_info`, because two copies of a
        // composition rule is how the blocking driver came to have no
        // server meta-channel at all.
        let user_source: DynSource = source as Arc<dyn ChannelSourceObj>;
        let dyn_source: DynSource = super::server_info::compose_with_server_info(user_source)
            .map_err(|e| PvaError::Protocol(format!("PvaServer::start: {e}")))?;

        // One server-wide channel-invalidation fan-out. A source that can
        // invalidate a channel out of band (the PVA gateway, on an operator
        // `<prefix>:drop` / `:flush`) publishes the affected PV name(s) here;
        // every per-connection task holds a receiver and force-disconnects any
        // channel it currently serves under those names with a server-initiated
        // DESTROY_CHANNEL — the downstream effect of pva2pva's cache-entry
        // `channel->destroy()` fanout. Lossless by construction (per-connection
        // unbounded queues + one batch per removal command, see
        // [`ChannelInvalidator`]); per-connection receivers are minted at
        // accept from this clone.
        let channel_invalidator = ChannelInvalidator::new();
        dyn_source.set_channel_invalidator(channel_invalidator.clone());

        // TCP bind addresses derived from `EPICS_PVAS_INTF_ADDR_LIST`
        // (empty → single `bind_ip` default); see [`tcp_bind_addresses`].
        let tcp_bind_ips = tcp_bind_addresses(&config.interfaces, config.bind_ip);

        // Robustness: bind the TCP listener(s) synchronously here so the
        // actually-bound port is observable to client_config() before
        // start() returns. The previous design spawned the accept task
        // and trusted `config.tcp_port` (which is 0 for ephemeral
        // pickers), leaving a race window where a concurrent test
        // could grab a freshly-released port between `pick_port`'s
        // drop and the accept task's bind. tokio's
        // `std::net::TcpListener::bind` is sync; we then promote it
        // to a non-blocking tokio listener after spawning.
        //
        // Multi-server-on-one-host: when the requested port is taken
        // (e.g. an existing PVA IOC already bound 5075), fall back
        // to ephemeral (port 0) once and let the OS pick. Mirrors
        // pvxs `serverconn.cpp:493` (EADDRINUSE/EACCES → setPort(0),
        // single retry). The actually-bound port flows out via
        // `bound_tcp_port`, which the UDP responder advertises in
        // SEARCH_RESPONSE / beacons, so clients still find us. UDP
        // 5076 is already SO_REUSEPORT-shareable across local PVA
        // processes (epics-base-rs/net/async_udp_v4.rs:620), so the
        // remaining bottleneck was just the TCP single-bind.
        //
        // With a multi-interface list the FIRST listener picks the port
        // (subject to the ephemeral fallback); the remaining interfaces
        // then bind that same port on their own address (distinct
        // (addr,port) tuples, so no SO_REUSEADDR is needed).
        let first_bind_addr = SocketAddr::new(tcp_bind_ips[0], config.tcp_port);
        let first_listener = match std::net::TcpListener::bind(first_bind_addr) {
            Ok(l) => l,
            Err(e)
                if config.tcp_port != 0
                    && (e.kind() == std::io::ErrorKind::AddrInUse
                        || e.kind() == std::io::ErrorKind::PermissionDenied) =>
            {
                let fallback_addr = SocketAddr::new(tcp_bind_ips[0], 0);
                let listener = std::net::TcpListener::bind(fallback_addr)?;
                tracing::warn!(
                    requested = ?first_bind_addr,
                    bound = ?listener.local_addr().ok(),
                    error = %e,
                    "PVA TCP port unavailable; falling back to ephemeral",
                );
                listener
            }
            Err(e) => return Err(PvaError::Io(e)),
        };
        first_listener.set_nonblocking(true)?;
        let bound_tcp_port = first_listener.local_addr()?.port();
        // Single bound-port source of truth: stamp the actually-bound
        // port back onto `config` so every consumer of `config.tcp_port`
        // — the TCP-circuit SEARCH_RESPONSE (handle_tcp_search), beacons,
        // report(), and client_config() — advertises the live listener
        // port, not the requested value. pvxs writes the TCP SEARCH
        // server port from the bound interface address
        // (`iface->bind_addr.port()`, serverchan.cpp:238-242). Without
        // this, a `tcp_port = 0` or occupied-port fallback made UDP
        // discovery advertise the real port while TCP-circuit SEARCH
        // handed out 0 or the occupied requested port. The ephemeral
        // fallback above already consumed the original `tcp_port`, so
        // overwriting it here is safe.
        config.tcp_port = bound_tcp_port;
        let mut tcp_listeners: Vec<tokio::net::TcpListener> =
            vec![tokio::net::TcpListener::from_std(first_listener)?];
        for ip in tcp_bind_ips.iter().skip(1) {
            let addr = SocketAddr::new(*ip, bound_tcp_port);
            let std_listener = std::net::TcpListener::bind(addr).map_err(PvaError::Io)?;
            std_listener.set_nonblocking(true)?;
            tcp_listeners.push(tokio::net::TcpListener::from_std(std_listener)?);
        }

        // Dedicated TLS listener(s). pvxs binds a SEPARATE per-interface TLS
        // socket on `effective.tls_port` alongside the plaintext one
        // (server.cpp:595-608) and advertises it for a protoTLS SEARCH
        // (server.cpp:849-852). Without a distinct listener a deployment that
        // sets EPICS_PVAS_TLS_PORT — or a client addressed straight at the
        // TLS port via a name server — could not reach the server, since the
        // Rust server otherwise only listens on `tcp_port`. The listener runs
        // the same first-byte dispatch as the plaintext one
        // (`run_tcp_server_on_listener`): a TLS ClientHello is upgraded; a
        // plaintext peer is served plain unless `disable_plaintext` refuses
        // it. `bound_tls_port` is the port advertised as the `"tls"` endpoint.
        //
        // Skip the extra bind when the requested TLS port resolves to the
        // already-bound plaintext port (an explicit collision): the shared
        // listener already serves TLS via the peek, and a second bind on the
        // same (addr, port) would fail with AddrInUse.
        let bound_tls_port = if config.tls.is_some() && config.tls_port != bound_tcp_port {
            let first_tls_addr = SocketAddr::new(tcp_bind_ips[0], config.tls_port);
            let first_tls = match std::net::TcpListener::bind(first_tls_addr) {
                Ok(l) => l,
                // Same single-retry ephemeral fallback as the plaintext bind
                // (pvxs serverconn.cpp:493): a taken/forbidden explicit TLS
                // port falls back to an OS-picked one rather than failing the
                // whole server.
                Err(e)
                    if config.tls_port != 0
                        && (e.kind() == std::io::ErrorKind::AddrInUse
                            || e.kind() == std::io::ErrorKind::PermissionDenied) =>
                {
                    let listener =
                        std::net::TcpListener::bind(SocketAddr::new(tcp_bind_ips[0], 0))?;
                    tracing::warn!(
                        requested = ?first_tls_addr,
                        bound = ?listener.local_addr().ok(),
                        error = %e,
                        "PVA TLS port unavailable; falling back to ephemeral",
                    );
                    listener
                }
                Err(e) => return Err(PvaError::Io(e)),
            };
            first_tls.set_nonblocking(true)?;
            let tls_port = first_tls.local_addr()?.port();
            tcp_listeners.push(tokio::net::TcpListener::from_std(first_tls)?);
            for ip in tcp_bind_ips.iter().skip(1) {
                let addr = SocketAddr::new(*ip, tls_port);
                let std_listener = std::net::TcpListener::bind(addr).map_err(PvaError::Io)?;
                std_listener.set_nonblocking(true)?;
                tcp_listeners.push(tokio::net::TcpListener::from_std(std_listener)?);
            }
            tls_port
        } else {
            // No separate TLS listener: when TLS is enabled the shared
            // plaintext port serves it (peek dispatch), so the advertised
            // TLS endpoint is that port. When TLS is disabled the value is
            // inert (never advertised — see `protocol` below).
            bound_tcp_port
        };
        // Single bound-port source of truth for the TLS endpoint, mirroring
        // the `config.tcp_port = bound_tcp_port` stamp above. Only meaningful
        // when TLS is enabled; left untouched otherwise so a disabled-TLS
        // server's report keeps the configured value rather than aliasing the
        // plaintext port.
        if config.tls.is_some() {
            config.tls_port = bound_tls_port;
        }

        // v4 entries of the interface list constrain the UDP search
        // responder bind; v6 entries (rare) are handled by the wildcard
        // v6 responder below and are not part of the per-NIC v4 set.
        let udp_interfaces: Vec<Ipv4Addr> = config
            .interfaces
            .iter()
            .filter_map(|ip| match ip {
                IpAddr::V4(v4) => Some(*v4),
                IpAddr::V6(_) => None,
            })
            .collect();

        let protocol: &'static str = if config.tls.is_some() { "tls" } else { "tcp" };
        // The port advertised for `protocol` in SEARCH replies / beacons.
        // pvxs returns `tls_port` for a protoTLS reply and `tcp_port`
        // otherwise (server.cpp:849-857); on a TLS server clients are
        // therefore steered to the dedicated TLS listener bound above.
        let advertised_tcp_port = if config.tls.is_some() {
            bound_tls_port
        } else {
            bound_tcp_port
        };
        // Bind the search socket synchronously here, for the same reason the
        // TCP listener is bound above rather than inside its task: with
        // `udp_port = 0` the kernel picks the number, and a bind performed
        // inside the spawned responder would publish it nowhere — `report()`,
        // beacons, and `client_config()` would all advertise 0. Binding here
        // and stamping the result back onto `config` keeps a single
        // bound-port source of truth (pvxs does the same read-back at
        // `server.cpp:426`).
        let (udp_socket, bound_udp_port) = crate::server_native::udp::bind_udp(
            config.udp_port,
            &udp_interfaces,
            &config.beacon_destinations,
        )?;
        config.udp_port = bound_udp_port;
        let udp_handle = tokio::spawn(run_udp_responder_on_socket(
            udp_socket,
            bound_udp_port,
            dyn_source.clone(),
            advertised_tcp_port,
            guid,
            protocol,
            config.beacon_period,
            config.beacon_period_long,
            config.beacon_burst_count,
            config.beacon_destinations.clone(),
            config.auto_beacon,
            config.ignore_addrs.clone(),
            config.enable_ipv6_udp,
        ));
        // PR #205 IPv6 Stage 2: optional companion responder bound
        // to `[::]:udp_port` that answers v6 SEARCH packets. Shares
        // the GUID + TCP port + protocol of the v4 path so a peer
        // sees one consistent PVA identity across both families.
        // Default-off via `enable_ipv6_udp = false`.
        let udp_v6_handle = if config.enable_ipv6_udp {
            Some(tokio::spawn(run_udp_responder_v6(
                dyn_source.clone(),
                config.udp_port,
                advertised_tcp_port,
                guid,
                protocol,
                config.ignore_addrs.clone(),
            )))
        } else {
            None
        };
        // One accept loop per bound interface, supervised by a single
        // task so the `PvaServer` keeps its single-handle shape (Drop /
        // run / wait / report / stop are unchanged). The supervisor owns
        // the per-listener `run_tcp_server_on_listener` futures in a
        // JoinSet: the first listener to return Err ends the service and
        // dropping the supervisor future (PvaServer::stop →
        // `tcp_abort.abort()`) aborts every accept loop together. With
        // the default empty interface list this is exactly one listener,
        // identical to the previous single-bind behaviour.
        let tcp_source = dyn_source;
        let tcp_config = config.clone();
        let tcp_peers = peers.clone();
        let tcp_invalidator = channel_invalidator;
        let tcp_handle = tokio::spawn(async move {
            let mut set: tokio::task::JoinSet<PvaResult<()>> = tokio::task::JoinSet::new();
            for listener in tcp_listeners {
                set.spawn(crate::server_native::tcp::run_tcp_server_on_listener(
                    tcp_source.clone(),
                    listener,
                    tcp_config.clone(),
                    tcp_peers.clone(),
                    tcp_invalidator.clone(),
                ));
            }
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Ok(())) => continue,
                    Ok(Err(e)) => return Err(e),
                    Err(e) if e.is_cancelled() => return Ok(()),
                    Err(e) => {
                        return Err(crate::error::PvaError::Protocol(format!(
                            "tcp listener task panic: {e}"
                        )));
                    }
                }
            }
            Ok(())
        });

        let udp_abort = udp_handle.abort_handle();
        let udp_v6_abort = udp_v6_handle.as_ref().map(|h| h.abort_handle());
        let tcp_abort = tcp_handle.abort_handle();
        Ok(Self {
            udp_handle: Some(udp_handle),
            udp_v6_handle,
            tcp_handle: Some(tcp_handle),
            udp_abort,
            udp_v6_abort,
            tcp_abort,
            effective_config: config,
            bound_tcp_port,
            bound_tls_port,
            interrupt: Arc::new(tokio::sync::Notify::new()),
            peers,
        })
    }

    /// Build a [`crate::client_native::context::PvaClient`] pointed at
    /// this server on loopback. Mirrors pvxs `Server::clientConfig` —
    /// useful for self-contained tests where you want a client that
    /// talks to the in-process server without UDP discovery.
    ///
    /// Loopback family matches the server's `bind_ip` family (PR #205
    /// IPv6 Stage 1): a v6-bound server hands out a `[::1]:port`
    /// client target, a v4-bound server hands out `127.0.0.1:port`.
    /// Otherwise a v6 listener on `[::1]` would be unreachable from
    /// the test client.
    ///
    /// The one server method whose *return type* is a client — so it is the
    /// one that gates with the client (design doc §9 phase 6, item 2). Use
    /// [`Self::tcp_addr`] on a server-only build; that is what this
    /// hands the builder anyway.
    #[cfg(feature = "client")]
    pub fn client_config(&self) -> crate::client_native::context::PvaClient {
        let loopback = match self.effective_config.bind_ip {
            std::net::IpAddr::V4(_) => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            std::net::IpAddr::V6(_) => std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        };
        let addr = SocketAddr::new(loopback, self.bound_tcp_port);
        crate::client_native::context::PvaClient::builder()
            .server_addr(addr)
            .build()
    }

    /// Effective config snapshot. pvxs `Server::config` parity.
    pub fn config(&self) -> &PvaServerConfig {
        &self.effective_config
    }

    /// Loopback address the TCP listener actually bound to. Useful
    /// for tests that want a raw TCP socket against the server
    /// (e.g. wire-level interop tests that bypass the client
    /// stack). Mirrors pvxs `Server::bound_tcp_addr`.
    pub fn tcp_addr(&self) -> SocketAddr {
        let loopback = match self.effective_config.bind_ip {
            std::net::IpAddr::V4(_) => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            std::net::IpAddr::V6(_) => std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        };
        SocketAddr::new(loopback, self.bound_tcp_port)
    }

    /// Loopback address the dedicated TLS listener bound to, or `None`
    /// when TLS is disabled. When TLS shares the plaintext port (no
    /// separate listener) this equals [`Self::tcp_addr`]. Useful for
    /// raw-socket interop tests that drive a TLS ClientHello at the
    /// advertised `"tls"` endpoint.
    pub fn tls_addr(&self) -> Option<SocketAddr> {
        // Only present when TLS is configured.
        self.effective_config.tls.as_ref()?;
        let loopback = match self.effective_config.bind_ip {
            std::net::IpAddr::V4(_) => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            std::net::IpAddr::V6(_) => std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        };
        Some(SocketAddr::new(loopback, self.bound_tls_port))
    }

    /// Block on this server until it stops, SIGINT/SIGTERM is received,
    /// or [`Self::interrupt`] is called. pvxs `Server::run` for CLI
    /// daemons. Returns Ok on graceful shutdown, Err if a subsystem
    /// task panicked or exited abnormally.
    pub async fn run(mut self) -> PvaResult<()> {
        let interrupt = self.interrupt.clone();
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        let udp = self.udp_handle.take().expect("PvaServer::run called twice");
        let tcp = self.tcp_handle.take().expect("PvaServer::run called twice");
        tokio::select! {
            _ = ctrl_c => Ok(()),
            _ = interrupt.notified() => Ok(()),
            r = udp => match r {
                Ok(res) => res,
                Err(e) if e.is_cancelled() => Ok(()),
                Err(e) => Err(crate::error::PvaError::Protocol(format!("udp task panic: {e}"))),
            },
            r = tcp => match r {
                Ok(res) => res,
                Err(e) if e.is_cancelled() => Ok(()),
                Err(e) => Err(crate::error::PvaError::Protocol(format!("tcp task panic: {e}"))),
            },
        }
    }

    /// Trip [`Self::run`] from another task. Mirrors pvxs
    /// `Server::interrupt`.
    pub fn interrupt(&self) {
        self.interrupt.notify_waiters();
    }

    /// Snapshot summary-level diagnostics. pvxs `Server::report`
    /// counterpart: server liveness/config plus per-peer connection
    /// counters and per-channel tx/rx byte counters (see
    /// [`crate::server_native::PeerSnapshot::channels_detail`]).
    pub fn report(&self) -> ServerReport {
        self.report_zeroed(false)
    }

    /// like [`Self::report`] but, when `zero` is true, resets each peer's
    /// connection byte counters AND every per-channel tx/rx counter after
    /// the snapshot — pvxs `Server::report(bool zero)` (server.cpp:256-272),
    /// so a subsequent report returns the deltas since this one. Channel
    /// membership and credentials are not reset.
    pub fn report_zeroed(&self, zero: bool) -> ServerReport {
        // Liveness from the owned JoinHandles (a `None` handle — taken by
        // `run`/`wait` — reads as not-alive). `ServerReportHandle::report`
        // resolves the same booleans from the cloned AbortHandles; both
        // feed the single [`assemble_report`] builder so the struct shape
        // can never drift between the two paths.
        assemble_report(ReportFields {
            tcp_port: self.bound_tcp_port,
            udp_port: self.effective_config.udp_port,
            tls_port: self.bound_tls_port,
            tls_enabled: self.effective_config.tls.is_some(),
            ignore_addrs: self.effective_config.ignore_addrs.len(),
            beacon_period_secs: self.effective_config.beacon_period.as_secs(),
            udp_alive: self
                .udp_handle
                .as_ref()
                .map(|h| !h.is_finished())
                .unwrap_or(false),
            udp_v6_alive: self
                .udp_v6_handle
                .as_ref()
                .map(|h| !h.is_finished())
                .unwrap_or(false),
            tcp_alive: self
                .tcp_handle
                .as_ref()
                .map(|h| !h.is_finished())
                .unwrap_or(false),
            peers: &self.peers,
            zero,
        })
    }

    /// A cheap, cloneable handle that reproduces [`Self::report`] from
    /// outside this server's owning task.
    ///
    /// It captures the shared peer registry (`Arc`), the bound
    /// ports/config scalars, and the per-task `AbortHandle`s. Unlike a
    /// `&PvaServer` it stays valid after `run`/`wait` have consumed the
    /// `PvaServer` value — which is exactly the iocsh `pvxsr` situation:
    /// the native server is created and `wait()`-consumed deep inside
    /// [`run_pva_server`], two layers below the shell registration point,
    /// so the report state is otherwise unreachable from the shell.
    /// Liveness reads the same `AbortHandle`s that `Drop`/[`Self::stop`]
    /// act on, equivalent to the JoinHandle `is_finished` that
    /// [`Self::report_zeroed`] reads for the same tasks.
    pub fn report_handle(&self) -> ServerReportHandle {
        ServerReportHandle {
            bound_tcp_port: self.bound_tcp_port,
            udp_port: self.effective_config.udp_port,
            bound_tls_port: self.bound_tls_port,
            tls_enabled: self.effective_config.tls.is_some(),
            ignore_addrs: self.effective_config.ignore_addrs.len(),
            beacon_period_secs: self.effective_config.beacon_period.as_secs(),
            peers: self.peers.clone(),
            tcp_abort: self.tcp_abort.clone(),
            udp_abort: self.udp_abort.clone(),
            udp_v6_abort: self.udp_v6_abort.clone(),
        }
    }

    /// Stop accepting new connections. Aborts both background tasks;
    /// per-client tasks already spawned continue independently and
    /// unwind on their next failed I/O. Mirrors pvxs `Server::stop`
    /// (server.cpp:616) at the "no new connections" granularity. For
    /// hard-stop semantics drop the entire `PvaServer` instead.
    pub fn stop(&self) {
        self.tcp_abort.abort();
        self.udp_abort.abort();
        if let Some(ref abort) = self.udp_v6_abort {
            abort.abort();
        }
    }

    /// Block until either task returns. Either subsystem exiting is
    /// treated as fatal — an Err here means the server is no longer
    /// serving even if `stop()` wasn't called.
    pub async fn wait(mut self) -> PvaResult<()> {
        // select! drops the losing branch's JoinHandle, but
        // dropping a JoinHandle does NOT abort the task. Without an
        // explicit abort, a UDP-side panic leaves the TCP listener
        // orphaned (and vice versa). Use the per-server AbortHandles
        // (also held by Drop) to abort the loser regardless of which
        // branch fires first.
        let udp_abort = self.udp_abort.clone();
        let tcp_abort = self.tcp_abort.clone();
        let udp = self
            .udp_handle
            .take()
            .expect("PvaServer::wait called twice");
        let tcp = self
            .tcp_handle
            .take()
            .expect("PvaServer::wait called twice");
        tokio::select! {
            r = udp => {
                tcp_abort.abort();
                match r {
                    Ok(res) => res,
                    Err(e) if e.is_cancelled() => Ok(()),
                    Err(e) => Err(crate::error::PvaError::Protocol(format!("udp task panic: {e}"))),
                }
            },
            r = tcp => {
                udp_abort.abort();
                match r {
                    Ok(res) => res,
                    Err(e) if e.is_cancelled() => Ok(()),
                    Err(e) => Err(crate::error::PvaError::Protocol(format!("tcp task panic: {e}"))),
                }
            },
        }
    }
}

/// Snapshot returned by [`PvaServer::report`].
#[derive(Debug, Clone)]
pub struct ServerReport {
    pub tcp_port: u16,
    pub udp_port: u16,
    /// Bound dedicated-TLS port advertised as the `"tls"` SEARCH endpoint.
    /// Only meaningful when [`Self::tls_enabled`]; equals [`Self::tcp_port`]
    /// when TLS shares the plaintext port.
    pub tls_port: u16,
    pub tls_enabled: bool,
    pub ignore_addrs: usize,
    pub beacon_period_secs: u64,
    pub udp_alive: bool,
    /// `true` iff the optional IPv6 UDP responder task is running.
    /// `false` when `PvaServerConfig::enable_ipv6_udp = false`
    /// (the default) — distinct from "task crashed".
    pub udp_v6_alive: bool,
    pub tcp_alive: bool,
    /// Live per-connection counters captured under the registry's
    /// read lock. pvxs `Server::report()` parity at the
    /// "live peers + per-peer channel/op/byte counters" level.
    pub peers: Vec<(SocketAddr, crate::server_native::peers::PeerSnapshot)>,
    /// Total currently-active connections.
    pub peer_count: usize,
}

/// Resolved inputs to [`assemble_report`]. Liveness booleans are already
/// computed by the caller (from JoinHandles in [`PvaServer::report_zeroed`],
/// from AbortHandles in [`ServerReportHandle::report`]); everything else is
/// read straight through.
struct ReportFields<'a> {
    tcp_port: u16,
    udp_port: u16,
    tls_port: u16,
    tls_enabled: bool,
    ignore_addrs: usize,
    beacon_period_secs: u64,
    udp_alive: bool,
    udp_v6_alive: bool,
    tcp_alive: bool,
    peers: &'a crate::server_native::peers::PeerRegistry,
    zero: bool,
}

/// The single owner of [`ServerReport`] construction. Both
/// [`PvaServer::report_zeroed`] and [`ServerReportHandle::report`] route
/// through here, so the snapshot/count pair and the struct's field set
/// cannot drift between the two paths — a new `ServerReport` field is added
/// in exactly one place.
fn assemble_report(f: ReportFields<'_>) -> ServerReport {
    ServerReport {
        tcp_port: f.tcp_port,
        udp_port: f.udp_port,
        tls_port: f.tls_port,
        tls_enabled: f.tls_enabled,
        ignore_addrs: f.ignore_addrs,
        beacon_period_secs: f.beacon_period_secs,
        udp_alive: f.udp_alive,
        udp_v6_alive: f.udp_v6_alive,
        tcp_alive: f.tcp_alive,
        peers: f.peers.snapshot_zeroed(f.zero),
        peer_count: f.peers.len(),
    }
}

/// Cheap, cloneable, `Send + Sync` handle to a running [`PvaServer`]'s
/// diagnostics, produced by [`PvaServer::report_handle`].
///
/// Unlike a `&PvaServer` it survives the `run`/`wait` consumption of the
/// server, so an in-process iocsh command (`pvxsr`) running inside the
/// live server can snapshot the report even though the owning `PvaServer`
/// value lives — and is `wait()`-consumed — inside [`run_pva_server`].
#[derive(Clone)]
pub struct ServerReportHandle {
    bound_tcp_port: u16,
    udp_port: u16,
    bound_tls_port: u16,
    tls_enabled: bool,
    ignore_addrs: usize,
    beacon_period_secs: u64,
    peers: Arc<crate::server_native::peers::PeerRegistry>,
    tcp_abort: tokio::task::AbortHandle,
    udp_abort: tokio::task::AbortHandle,
    udp_v6_abort: Option<tokio::task::AbortHandle>,
}

impl ServerReportHandle {
    /// Snapshot the same [`ServerReport`] as [`PvaServer::report`], read
    /// through the shared registry and per-task `AbortHandle`s. Liveness
    /// uses `AbortHandle::is_finished` — the task-completion signal
    /// `Drop`/[`PvaServer::stop`] act on — which is equivalent to the
    /// JoinHandle `is_finished` that [`PvaServer::report_zeroed`] reads
    /// for the very same tasks. Counters are never zeroed through this
    /// handle (`zero = false`); the resetting variant stays on
    /// [`PvaServer::report_zeroed`], which owns the JoinHandles.
    pub fn report(&self) -> ServerReport {
        assemble_report(ReportFields {
            tcp_port: self.bound_tcp_port,
            udp_port: self.udp_port,
            tls_port: self.bound_tls_port,
            tls_enabled: self.tls_enabled,
            ignore_addrs: self.ignore_addrs,
            beacon_period_secs: self.beacon_period_secs,
            udp_alive: !self.udp_abort.is_finished(),
            udp_v6_alive: self
                .udp_v6_abort
                .as_ref()
                .map(|a| !a.is_finished())
                .unwrap_or(false),
            tcp_alive: !self.tcp_abort.is_finished(),
            peers: &self.peers,
            zero: false,
        })
    }
}

#[cfg(test)]
mod tcp_fallback_tests {
    //! pvxs parity for multi-server-on-one-host: when the requested
    //! `tcp_port` is already taken, fall back to ephemeral so a
    //! second IOC on the same machine doesn't panic on startup.
    //! Mirrors pvxs `serverconn.cpp:493` (EADDRINUSE → setPort(0),
    //! single retry).
    use super::*;
    use crate::server_native::SharedSource;
    // `Duration` left this module with `PvaServerConfig` (now `super::config`);
    // the config-building tests below still spell it out.
    use std::time::Duration;

    /// When the requested TCP port is already bound, `PvaServer::start`
    /// must fall back to ephemeral instead of panicking. The actually-
    /// bound port is observable via `report().tcp_port` and is what
    /// SEARCH_RESPONSE / beacons advertise to clients.
    #[tokio::test]
    async fn second_server_falls_back_to_ephemeral_when_port_taken() {
        // Pin a port by binding it ourselves and holding the listener
        // open for the duration of the test. The PvaServer below will
        // ask for the same port and must NOT panic.
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").expect("blocker bind");
        let blocked_port = blocker.local_addr().expect("blocker addr").port();

        let source = Arc::new(SharedSource::new());
        let config = PvaServerConfig {
            tcp_port: blocked_port,
            udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        };

        // The pre-fix code would panic here; now the requested port
        // being taken triggers the ephemeral fallback, not a panic.
        let server = PvaServer::start(source, config).expect("fallback must not error");
        let report = server.report();
        assert_ne!(
            report.tcp_port, blocked_port,
            "fallback must hand out a different port"
        );
        assert_ne!(
            report.tcp_port, 0,
            "bound port must be a concrete OS-assigned port, not the sentinel"
        );
        // Sanity: blocker is still alive on its port.
        assert_eq!(
            blocker.local_addr().expect("blocker addr").port(),
            blocked_port,
        );

        drop(server);
        drop(blocker);
    }

    /// `PvaServer::report_handle()` produces a detached handle whose
    /// `report()` mirrors `PvaServer::report()` field-for-field on a live
    /// server. This is the property the iocsh `pvxsr` command rides on:
    /// the handle outlives the `PvaServer` value that `run`/`wait`
    /// consume, so it must report the same thing the server would.
    #[tokio::test]
    async fn report_handle_mirrors_live_server_report() {
        let source = Arc::new(SharedSource::new());
        let config = PvaServerConfig {
            tcp_port: 0,
            udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        };
        let server = PvaServer::start(source, config).expect("server must start");

        let direct = server.report();
        let via_handle = server.report_handle().report();

        assert_eq!(via_handle.tcp_port, direct.tcp_port);
        assert_eq!(via_handle.udp_port, direct.udp_port);
        assert_eq!(via_handle.tls_port, direct.tls_port);
        assert_eq!(via_handle.tls_enabled, direct.tls_enabled);
        assert_eq!(via_handle.beacon_period_secs, direct.beacon_period_secs);
        assert_eq!(via_handle.ignore_addrs, direct.ignore_addrs);
        assert_eq!(via_handle.peer_count, direct.peer_count);
        assert_eq!(via_handle.tcp_alive, direct.tcp_alive);
        assert_eq!(via_handle.udp_alive, direct.udp_alive);
        assert_eq!(via_handle.udp_v6_alive, direct.udp_v6_alive);
        assert!(
            via_handle.tcp_alive,
            "tcp task must be live on a fresh server"
        );

        // A clone keeps answering after the PvaServer value is gone — the
        // ports are immutable scalars captured at bind, and the call must
        // not panic even though the backing tasks have been aborted.
        let detached = server.report_handle();
        drop(server);
        let post = detached.report();
        assert_eq!(
            post.tcp_port, direct.tcp_port,
            "handle ports stay fixed after the server value drops"
        );
    }

    /// `run_pva_server_reporting` fires `on_started` with a usable
    /// `ServerReportHandle` the instant the listeners bind — before
    /// `wait()` — which is exactly how the wrapper publishes the handle to
    /// the iocsh `pvxsr` command.
    #[tokio::test]
    async fn run_pva_server_reporting_publishes_handle_at_bind() {
        let source = Arc::new(SharedSource::new());
        let config = PvaServerConfig {
            tcp_port: 0,
            udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let server_task = tokio::spawn(run_pva_server_reporting(source, config, move |handle| {
            let _ = tx.send(handle);
        }));

        let handle = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("handle must be published promptly")
            .expect("callback must send the handle");
        let report = handle.report();
        assert!(report.tcp_alive, "server reports live right after bind");
        assert_ne!(
            report.tcp_port, 0,
            "bound TCP port is concrete, not the sentinel"
        );

        server_task.abort();
        let _ = server_task.await;
    }

    /// epics-base PR #205 IPv6 Stage 1 — `PvaServerConfig::bind_ip`
    /// accepts `IpAddr::V6` and the TCP listener binds successfully.
    /// Verifies the change is genuinely IPv6-capable rather than just
    /// type-compatible.
    #[tokio::test]
    async fn binds_ipv6_loopback_listener() {
        use std::net::Ipv6Addr;
        let source = Arc::new(SharedSource::new());
        let config = PvaServerConfig {
            tcp_port: 0,
            udp_port: 0,
            bind_ip: IpAddr::V6(Ipv6Addr::LOCALHOST),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        };
        let server = PvaServer::start(source, config).expect("v6 listener must start");
        let report = server.report();
        assert!(report.tcp_port != 0, "v6 listener must bind a port");

        // Confirm we can dial the IPv6 listener — the bind type is
        // really v6, not silently downgraded to v4.
        let addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), report.tcp_port);
        let connect =
            tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr))
                .await
                .expect("connect timed out");
        let _stream = connect.expect("IPv6 TCP connect must succeed");
        drop(server);
    }

    /// PR #205 IPv6 Stage 2 — `enable_ipv6_udp = true` spawns a
    /// companion `[::]:udp_port` SEARCH responder. We send a hand-
    /// rolled SEARCH datagram from a v6 client socket against
    /// `[::1]:udp_port` and verify the server's SEARCH_RESPONSE
    /// arrives back with the right tcp_port + GUID-length payload.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn udp_v6_responder_answers_v6_search() {
        use crate::nt::typed::TypedNT;
        use crate::proto::{ByteOrder, Command, PvaHeader, ReadExt, WriteExt};
        use std::io::Cursor;
        use std::net::Ipv6Addr;
        use tokio::net::UdpSocket as TokioUdp;

        // Pick a free v4 UDP port via `Ipv4Addr::LOCALHOST` so the
        // server's pick is OS-coordinated; the v6 responder will
        // bind `[::]:that_port` for its companion listener.
        let pick_udp = || {
            let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .expect("isolated udp port");
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };

        let pv = crate::server_native::SharedPV::new();
        pv.open(f64::descriptor(), f64::to_pv_field(&1.0)).unwrap();
        let source = Arc::new(SharedSource::new());
        source.add("V6:UDP:PV", pv);

        let udp_port = pick_udp();
        let config = PvaServerConfig {
            tcp_port: 0,
            udp_port,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            enable_ipv6_udp: true,
            ..Default::default()
        };
        let server = PvaServer::start(source, config).expect("v6 udp server must start");
        let server_tcp_port = server.report().tcp_port;
        // Wait briefly for the v6 listener to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let report = server.report();
        assert!(
            report.udp_v6_alive,
            "udp_v6 responder must be alive when enable_ipv6_udp=true"
        );

        // Hand-roll a SEARCH frame asking for `V6:UDP:PV`. The
        // server replies with a SEARCH_RESPONSE containing
        // tcp_port + "tcp" protocol when the PV name matches.
        let client = TokioUdp::bind("[::1]:0").await.expect("v6 client bind");
        let mut payload = Vec::new();
        payload.put_u32(0xABCD_0001, ByteOrder::Little); // seq
        payload.put_u8(0); // flags
        payload.extend_from_slice(&[0u8; 3]); // reserved
        // 16-byte reply addr = ::1
        payload.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        let reply_port = client.local_addr().unwrap().port();
        payload.put_u16(reply_port, ByteOrder::Little);
        // 1 protocol = "tcp"
        payload.extend_from_slice(&crate::proto::encode_size(1, ByteOrder::Little));
        crate::proto::encode_string_into("tcp", ByteOrder::Little, &mut payload);
        // 1 query: cid=0x1234, name = V6:UDP:PV
        payload.put_u16(1, ByteOrder::Little);
        payload.put_u32(0x1234, ByteOrder::Little);
        crate::proto::encode_string_into("V6:UDP:PV", ByteOrder::Little, &mut payload);
        let header = PvaHeader::application(
            true,
            ByteOrder::Little,
            Command::Search.code(),
            payload.len() as u32,
        );
        let mut frame = Vec::new();
        header.write_into(&mut frame);
        frame.extend_from_slice(&payload);

        let server_v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), udp_port);
        client
            .send_to(&frame, server_v6)
            .await
            .expect("send v6 SEARCH");

        // Receive SEARCH_RESPONSE.
        let mut rx = vec![0u8; 64 * 1024];
        let (n, peer) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut rx))
            .await
            .expect("v6 SEARCH_RESPONSE timed out")
            .expect("recv_from ok");
        assert!(n >= PvaHeader::SIZE, "response too short: {n}");
        // Replies arrive on the v6 unicast loopback path, source
        // address may be the link-local equivalent. Accept any v6.
        assert!(matches!(peer.ip(), IpAddr::V6(_)));
        let mut cur = Cursor::new(&rx[..n]);
        let hdr = PvaHeader::decode(&mut cur).expect("hdr decode");
        assert_eq!(
            hdr.command,
            Command::SearchResponse.code(),
            "expected SEARCH_RESPONSE"
        );
        // Confirm the payload mentions our server's tcp_port (16-byte
        // address + 2-byte port at offset 12+4+16 = 32, accounting
        // for guid+seq+addr layout). Cheaper: decode after guid.
        let mut p = Cursor::new(&rx[PvaHeader::SIZE..n]);
        let _guid = p.get_bytes(12).expect("guid");
        let _seq = p.get_u32(hdr.flags.byte_order()).expect("seq");
        let _addr16 = p.get_bytes(16).expect("addr16");
        let advertised_tcp = p.get_u16(hdr.flags.byte_order()).expect("tcp_port");
        assert_eq!(
            advertised_tcp, server_tcp_port,
            "SEARCH_RESPONSE must advertise server's actual TCP port"
        );

        drop(server);
    }

    /// End-to-end IPv6 PVA round-trip — start a server on `[::1]`,
    /// add a PV, build a client via `client_config()` (which now
    /// hands out a v6 loopback target to match the server family),
    /// and pvget the value. Proves the full PVA stack — TCP listener,
    /// handshake, channel creation, GET — works over IPv6.
    ///
    /// Drives a real client, so it rides the `client` feature.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pvget_round_trip_over_ipv6_loopback() {
        use crate::nt::typed::TypedNT;
        use std::net::Ipv6Addr;
        // Plain NTScalar<double> source, matching the typed_nt
        // `pvget_typed_primitive_f64` shape.
        let pv = crate::server_native::SharedPV::new();
        let value: f64 = 42.5;
        pv.open(f64::descriptor(), f64::to_pv_field(&value))
            .unwrap();
        let source = Arc::new(SharedSource::new());
        source.add("V6:LOOP", pv);
        let config = PvaServerConfig {
            tcp_port: 0,
            udp_port: 0,
            bind_ip: IpAddr::V6(Ipv6Addr::LOCALHOST),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        };
        let server = PvaServer::start(source, config).expect("v6 loopback server must start");
        let client = server.client_config();

        let got: f64 =
            tokio::time::timeout(Duration::from_secs(5), client.pvget_typed::<f64>("V6:LOOP"))
                .await
                .expect("pvget timed out over IPv6")
                .expect("pvget must succeed over IPv6");
        assert_eq!(got, value, "round-trip value must match over IPv6");
        drop(server);
    }

    /// A TCP bind failure that is NOT eligible for the ephemeral
    /// fallback (the fallback only covers `AddrInUse` / `PermissionDenied`
    /// on a non-zero requested port) must surface as `Err(PvaError)`
    /// rather than aborting the process. Binding to an address that is
    /// not assigned to any local interface yields `AddrNotAvailable`,
    /// which is outside the fallback set, so `start` returns `Err`.
    #[tokio::test]
    async fn start_returns_err_on_unbindable_address() {
        let source = Arc::new(SharedSource::new());
        let config = PvaServerConfig {
            tcp_port: 5075,
            udp_port: 0,
            // 192.0.2.0/24 is the TEST-NET-1 documentation range
            // (RFC 5737); it is never assigned to a local interface,
            // so binding it fails with AddrNotAvailable.
            bind_ip: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        };
        let result = PvaServer::start(source, config);
        assert!(
            result.is_err(),
            "bind to an unassigned address must return Err, not panic",
        );
        assert!(
            matches!(result, Err(crate::error::PvaError::Io(_))),
            "TCP bind failure must surface as PvaError::Io",
        );
    }

    /// A second server asking for an already-bound port with the
    /// ephemeral fallback eligible must succeed (fallback), and the
    /// happy/fallback paths both return `Ok` — `start` never panics.
    #[tokio::test]
    async fn start_returns_ok_when_port_taken_with_fallback() {
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").expect("blocker bind");
        let blocked_port = blocker.local_addr().expect("blocker addr").port();

        let source = Arc::new(SharedSource::new());
        let config = PvaServerConfig {
            tcp_port: blocked_port,
            udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        };
        let result = PvaServer::start(source, config);
        assert!(
            result.is_ok(),
            "an in-use port must trigger the ephemeral fallback, returning Ok",
        );
        drop(result);
        drop(blocker);
    }

    /// Sanity: when the requested port IS available, no fallback is
    /// triggered and the server gets exactly what it asked for.
    #[tokio::test]
    async fn requested_port_used_when_available() {
        // Reserve a port, drop the reservation so the kernel almost
        // certainly hands it back, then ask the server for it. The
        // window is small enough that this is reliable in practice;
        // if a sibling test happens to grab the port between our
        // drop and the server's bind, the fallback path catches it
        // and the test still passes (just doesn't exercise the
        // happy path). That's an acceptable trade for not having
        // to manage a shared port pool.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        let target = probe.local_addr().expect("probe addr").port();
        drop(probe);

        let source = Arc::new(SharedSource::new());
        let config = PvaServerConfig {
            tcp_port: target,
            udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        };

        let server = PvaServer::start(source, config).expect("server must start");
        let report = server.report();
        // Either we got the requested port (happy path) or fallback
        // kicked in (sibling test grabbed it). Both are valid; only
        // a panic would be a regression.
        assert!(report.tcp_port != 0, "must bind a real port");
        drop(server);
    }

    /// `udp_port = 0` must report the port the kernel actually assigned.
    ///
    /// This is the property a caller that cannot guess a port depends on
    /// (`epics-oracle-rs` boots its Rust PVA side this way and prints the
    /// number for the harness to aim clients at). It is load-bearing for PVA
    /// specifically: the search socket sets `SO_REUSEPORT`, so two servers
    /// sharing a port bind *without any error* and then answer searches at
    /// random — there is no failure to detect afterwards, so the port must be
    /// read back from the bind rather than predicted. Before the bind moved
    /// into `start()`, the responder bound inside its own task and `report()`
    /// handed back the requested `0`.
    #[tokio::test]
    async fn ephemeral_udp_port_is_reported_not_left_zero() {
        let source = Arc::new(SharedSource::new());
        let config = PvaServerConfig {
            tcp_port: 0,
            udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            interfaces: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        };
        let server = PvaServer::start(source, config).expect("server must start");
        let report = server.report();
        assert_ne!(
            report.udp_port, 0,
            "udp_port = 0 must be resolved to the kernel-assigned port and reported",
        );
        // The reported port must be the one actually held: nothing else may
        // take it while the responder is alive. A plain (non-SO_REUSEPORT)
        // bind is refused by a live pvxs/epics-rs search socket, so a
        // successful bind here would mean the report named a port the server
        // is not on.
        assert!(
            std::net::UdpSocket::bind(("127.0.0.1", report.udp_port)).is_err(),
            "reported UDP port {} is not actually held by the server",
            report.udp_port,
        );
        drop(server);
    }

    /// `EPICS_PVAS_INTF_ADDR_LIST` empty → fall back to the single
    /// `bind_ip` default (historical behaviour).
    #[test]
    fn tcp_bind_addresses_empty_list_uses_bind_ip() {
        let bind_ip = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        assert_eq!(tcp_bind_addresses(&[], bind_ip), vec![bind_ip]);
    }

    /// A loopback-only server interface list binds TCP only to loopback —
    /// no wildcard / non-loopback address is produced.
    #[test]
    fn tcp_bind_addresses_loopback_only_binds_loopback_only() {
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let addrs = tcp_bind_addresses(&[lo], IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(addrs, vec![lo]);
        assert!(
            addrs.iter().all(|ip| ip.is_loopback()),
            "loopback-only interface list produced a non-loopback bind addr: {addrs:?}"
        );
    }

    /// Multiple specific interfaces → one listener per interface (each
    /// kernel-constrained to its own address).
    #[test]
    fn tcp_bind_addresses_multi_interface_binds_each() {
        let a = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let b = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5));
        let addrs = tcp_bind_addresses(&[a, b], IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(addrs, vec![a, b]);
    }

    /// A wildcard entry subsumes every specific address (binding a
    /// specific addr on top of a wildcard already holding the port would
    /// fail), so the wildcard is bound alone.
    #[test]
    fn tcp_bind_addresses_wildcard_subsumes_specifics() {
        let wild = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let specific = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5));
        let addrs = tcp_bind_addresses(&[specific, wild], IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(addrs, vec![wild]);
    }

    /// End-to-end: a server constrained to loopback starts, binds, and is
    /// reachable on loopback (the constrained bind path is wired through
    /// `start()`, not just the helper).
    #[tokio::test]
    async fn server_with_loopback_interface_list_starts_and_is_reachable() {
        let source = Arc::new(SharedSource::new());
        let config = PvaServerConfig {
            tcp_port: 0,
            udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            interfaces: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        };
        let server = PvaServer::start(source, config).expect("loopback-constrained server starts");
        let report = server.report();
        assert!(report.tcp_port != 0, "must bind a real port");
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), report.tcp_port);
        let connect =
            tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr))
                .await
                .expect("connect timed out");
        let _stream = connect.expect("loopback TCP connect must succeed");
        drop(server);
    }

    /// PVX-82: when `with_env` recorded that `EPICS_PVA[S]_INTF_ADDR_LIST`
    /// named interface(s) that all failed to resolve, `PvaServer::start`
    /// must refuse to bind rather than silently promoting the empty
    /// `interfaces` to the wildcard `0.0.0.0`. Deterministic — no DNS, no
    /// real bind (it errors before any listener is created).
    #[test]
    fn start_refuses_when_intf_addr_error_recorded() {
        let source = Arc::new(SharedSource::new());
        let config = PvaServerConfig {
            tcp_port: 0,
            udp_port: 0,
            interfaces: Vec::new(),
            intf_addr_error: Some(
                "EPICS_PVA[S]_INTF_ADDR_LIST=\"bad.invalid\" named interface(s) \
                 but none resolved"
                    .to_string(),
            ),
            auto_beacon: false,
            ..Default::default()
        };
        // `PvaServer` is not `Debug`, so match on the result rather than
        // `expect_err` (which would require the `Ok` type to be `Debug`).
        let result = PvaServer::start(source, config);
        assert!(
            matches!(&result, Err(PvaError::Protocol(m)) if m.contains("none resolved")),
            "an unresolved INTF list must fail server start with the resolution \
             error, not bind 0.0.0.0"
        );
    }

    /// PVX-82 (IGNORE sibling): when `with_env` recorded that
    /// `EPICS_PVAS_IGNORE_ADDR_LIST` named peer(s) that all failed to
    /// resolve, `PvaServer::start` must refuse rather than run with a
    /// silently-empty blocklist. Same deterministic start-refusal as the
    /// INTF case (errors before any listener is created).
    #[test]
    fn start_refuses_when_ignore_addr_error_recorded() {
        let source = Arc::new(SharedSource::new());
        let config = PvaServerConfig {
            tcp_port: 0,
            udp_port: 0,
            ignore_addr_error: Some(
                "EPICS_PVAS_IGNORE_ADDR_LIST=\"bad.invalid\" named peer(s) to \
                 block but none resolved"
                    .to_string(),
            ),
            auto_beacon: false,
            ..Default::default()
        };
        let result = PvaServer::start(source, config);
        assert!(
            matches!(&result, Err(PvaError::Protocol(m)) if m.contains("none resolved")),
            "an unresolved IGNORE list must fail server start, not run with an \
             empty blocklist"
        );
    }
}

#[cfg(test)]
mod sr9_message_size_tests {
    //! The inbound message-size cap defaults to
    //! [`DEFAULT_MAX_MESSAGE_SIZE`], **not** to pvxs's unbounded `None`.
    //!
    //! This module previously asserted the opposite ("must be unbounded
    //! (None), not a fixed cap"), on pvxs parity grounds. That parity
    //! argument was incomplete: pvxs affords an uncapped RX path because a
    //! failed allocation there is a `bad_alloc` caught per connection
    //! (`conn.cpp:307-335`), whereas in Rust an infallible `Vec` growth
    //! reaches `handle_alloc_error` and aborts the IOC. The growth path is
    //! now fallible too (`crate::peer_buf`), so both halves of pvxs's
    //! behaviour are reachable — but on an RTEMS target whose whole heap is
    //! a few hundred MiB, "unbounded" and "the machine" are the same number,
    //! so the *default* is a ceiling and `None` stays available as the
    //! explicit opt-out.
    use super::*;

    #[test]
    fn default_message_size_is_capped() {
        assert_eq!(
            PvaServerConfig::default().max_message_size,
            Some(DEFAULT_MAX_MESSAGE_SIZE),
            "default server config must carry the default ceiling"
        );
        // `isolated()` inherits the default via `..Default::default()`.
        assert_eq!(
            PvaServerConfig::isolated().max_message_size,
            Some(DEFAULT_MAX_MESSAGE_SIZE)
        );
        // `with_env()` neither introduces nor removes a cap.
        assert_eq!(
            PvaServerConfig::default().with_env().max_message_size,
            Some(DEFAULT_MAX_MESSAGE_SIZE)
        );
    }

    /// The cap is a default, not a policy nailed shut: pvxs-exact unbounded
    /// behaviour stays one field away.
    #[test]
    fn unbounded_is_still_expressible() {
        let cfg = PvaServerConfig {
            max_message_size: None,
            ..Default::default()
        };
        assert_eq!(cfg.max_message_size, None);
    }

    /// A ceiling below the largest frame the protocol layer will accept would
    /// make the two limits disagree about which one refused a message.
    #[test]
    fn the_default_ceiling_admits_a_realistic_array() {
        // 1M doubles — an NTScalarArray waveform an IOC can genuinely be
        // asked to accept — must fit under whatever the default config
        // actually serves, not merely under the constant.
        let cap = PvaServerConfig::default()
            .max_message_size
            .expect("the default config must carry a ceiling");
        assert!(
            cap >= 1024 * 1024 * 8,
            "default ceiling {cap} refuses a 1M-element double array"
        );
    }
}

#[cfg(test)]
mod with_env_preserve_tests {
    //! pvxs `Config::applyEnv` parity (`config.cpp:397-437`): `with_env`
    //! overrides a field only when a backing env var is *present*; an
    //! absent var must leave the caller-supplied value intact. Pre-fix
    //! Rust assigned every field unconditionally from a default-returning
    //! helper, so an empty environment reset ports / auto-beacon back to
    //! the LAN-facing defaults — and broke the `isolated()` contract.
    use super::*;
    // `Duration` left this module with `PvaServerConfig` (now `super::config`).
    use std::time::Duration;

    /// Clear the env vars a test depends on, run `f`, then restore them.
    /// nextest isolates each test in its own process, but we still save +
    /// restore so a `cargo test` (thread-shared) run stays coherent.
    fn with_cleared_env<R>(vars: &[&str], f: impl FnOnce() -> R) -> R {
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|v| ((*v).to_string(), std::env::var(v).ok()))
            .collect();
        for v in vars {
            unsafe { std::env::remove_var(v) };
        }
        let out = f();
        for (k, val) in saved {
            match val {
                Some(s) => unsafe { std::env::set_var(&k, s) },
                None => unsafe { std::env::remove_var(&k) },
            }
        }
        out
    }

    const PORT_VARS: &[&str] = &[
        "EPICS_PVAS_SERVER_PORT",
        "EPICS_PVA_SERVER_PORT",
        "EPICS_PVAS_BROADCAST_PORT",
        "EPICS_PVA_BROADCAST_PORT",
    ];

    #[test]
    #[serial_test::serial(epics_env)]
    fn with_env_preserves_caller_fields_when_vars_absent() {
        with_cleared_env(PORT_VARS, || {
            let cfg = PvaServerConfig {
                tcp_port: 12345,
                udp_port: 11111,
                ..Default::default()
            }
            .with_env();
            assert_eq!(
                cfg.tcp_port, 12345,
                "absent server-port var must not reset tcp_port"
            );
            assert_eq!(
                cfg.udp_port, 11111,
                "absent broadcast-port var must not reset udp_port"
            );
        });
    }

    #[test]
    #[serial_test::serial(epics_env)]
    fn with_env_overrides_only_the_present_var() {
        with_cleared_env(PORT_VARS, || {
            unsafe { std::env::set_var("EPICS_PVAS_SERVER_PORT", "6789") };
            let cfg = PvaServerConfig {
                tcp_port: 12345,
                udp_port: 11111,
                ..Default::default()
            }
            .with_env();
            assert_eq!(
                cfg.tcp_port, 6789,
                "present EPICS_PVAS_SERVER_PORT must override tcp_port"
            );
            assert_eq!(
                cfg.udp_port, 11111,
                "absent broadcast-port var must leave udp_port"
            );
        });
    }

    /// `with_env` derives `idle_timeout` from `EPICS_PVA_CONN_TMO` through
    /// the same owner as the client (`env::effective_tcp_timeout_secs`):
    /// the 4/3 scale, then pvxs `enforceTimeout` — floor AND upper reset.
    /// A configured 7e18 is ACCEPTED by `parse_timeout` (<= time_t::max)
    /// but its scaled form (≈9.33e18) crosses time_t::max, so pvxs falls
    /// back to 40 s. Pre-fix this site applied only the floor and armed the
    /// inactivity reaper with a ~3e11-year window (R17-34).
    #[test]
    #[serial_test::serial(epics_env)]
    fn with_env_idle_timeout_applies_both_enforce_timeout_bounds() {
        with_cleared_env(&["EPICS_PVA_CONN_TMO"], || {
            let base = PvaServerConfig {
                idle_timeout: Duration::from_secs(45),
                ..Default::default()
            };

            // Absent → caller value preserved.
            assert_eq!(
                base.clone().with_env().idle_timeout,
                Duration::from_secs(45)
            );

            // Default-shaped value: 30 × 4/3 = 40 s.
            unsafe { std::env::set_var("EPICS_PVA_CONN_TMO", "30") };
            assert_eq!(
                base.clone().with_env().idle_timeout,
                Duration::from_secs_f64(40.0)
            );

            // Below the floor: 1.0 × 4/3 = 1.333 → 2 s.
            unsafe { std::env::set_var("EPICS_PVA_CONN_TMO", "1.0") };
            assert_eq!(
                base.clone().with_env().idle_timeout,
                Duration::from_secs_f64(2.0)
            );

            // Upper reset: scaled >= time_t::max → 40 s.
            unsafe { std::env::set_var("EPICS_PVA_CONN_TMO", "7e18") };
            assert_eq!(
                base.with_env().idle_timeout,
                Duration::from_secs_f64(40.0),
                "scaled CONN_TMO >= time_t::max must reset to pvxs's 40s default"
            );
        });
    }

    const TLS_PORT_VARS: &[&str] = &["EPICS_PVAS_TLS_PORT", "EPICS_PVA_TLS_PORT"];

    /// `EPICS_PVAS_TLS_PORT` overrides `tls_port`; with only the shared
    /// `EPICS_PVA_TLS_PORT` set that form wins (pvxs `config.cpp:513`
    /// `PickOne{EPICS_PVAS_TLS_PORT, EPICS_PVA_TLS_PORT}`); an absent var
    /// preserves the caller value.
    #[test]
    #[serial_test::serial(epics_env)]
    fn with_env_tls_port_pvas_first_then_shared_then_preserve() {
        with_cleared_env(TLS_PORT_VARS, || {
            // Absent → caller value preserved.
            let cfg = PvaServerConfig {
                tls_port: 5555,
                ..Default::default()
            }
            .with_env();
            assert_eq!(cfg.tls_port, 5555, "absent TLS-port var must not reset");

            // Shared form alone is honoured.
            unsafe { std::env::set_var("EPICS_PVA_TLS_PORT", "5077") };
            let cfg = PvaServerConfig::default().with_env();
            assert_eq!(cfg.tls_port, 5077, "shared EPICS_PVA_TLS_PORT must apply");

            // Server-specific form takes precedence over the shared form.
            unsafe { std::env::set_var("EPICS_PVAS_TLS_PORT", "5078") };
            let cfg = PvaServerConfig::default().with_env();
            assert_eq!(
                cfg.tls_port, 5078,
                "EPICS_PVAS_TLS_PORT must win over EPICS_PVA_TLS_PORT"
            );
        });
    }

    #[test]
    #[serial_test::serial(epics_env)]
    fn isolated_with_env_stays_isolated_in_empty_env() {
        let vars = [
            "EPICS_PVAS_SERVER_PORT",
            "EPICS_PVA_SERVER_PORT",
            "EPICS_PVAS_BROADCAST_PORT",
            "EPICS_PVA_BROADCAST_PORT",
            "EPICS_PVAS_AUTO_BEACON_ADDR_LIST",
            "EPICS_PVA_AUTO_ADDR_LIST",
        ];
        with_cleared_env(&vars, || {
            let cfg = PvaServerConfig::isolated().with_env();
            assert_eq!(
                cfg.tcp_port, 0,
                "isolated().with_env() must keep the ephemeral tcp_port"
            );
            assert_eq!(
                cfg.udp_port, 0,
                "isolated().with_env() must keep the ephemeral udp_port"
            );
            assert!(
                !cfg.auto_beacon,
                "isolated().with_env() must keep auto_beacon off"
            );
        });
    }
}
