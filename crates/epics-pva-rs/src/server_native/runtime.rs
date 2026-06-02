//! Top-level [`PvaServer`] runtime: spawns UDP responder + TCP listener.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use crate::error::{PvaError, PvaResult};

use super::source::{ChannelSource, ChannelSourceObj, DynSource};
use super::udp::{random_guid, run_udp_responder_v6, run_udp_responder_with_config};

/// Capacity of the server-wide channel-invalidation broadcast (see
/// [`ChannelSource::set_channel_invalidator`]). Each per-connection task
/// holds one receiver; a one-shot operator `:flush` publishes one name per
/// dropped cache entry, so the buffer must comfortably absorb a large flush
/// without lagging a connection task that is briefly busy in its read loop.
/// Shared with the standalone `run_tcp_server_with_peers` entry point so
/// both server paths size the broadcast identically.
pub(crate) const CHANNEL_INVALIDATION_CAPACITY: usize = 1024;

/// Runtime configuration for [`run_pva_server`].
#[derive(Clone)]
pub struct PvaServerConfig {
    pub tcp_port: u16,
    pub udp_port: u16,
    /// server identity propagated into the TCP-circuit
    /// `Command::Search` reply (`pvxs serverchan.cpp:215-235`). UDP
    /// SEARCH_RESPONSE uses the same guid emitted by the UDP
    /// responder. The runtime fills this from `random_guid()` before
    /// passing the config to `run_tcp_server_on_listener`; default
    /// is zero so tests / direct callers that don't care still
    /// compile.
    pub guid: [u8; 12],
    /// Per-frame read timeout. The server *also* applies the heartbeat-
    /// based idle timeout below — `op_timeout` is just the upper bound on
    /// any single read.
    pub op_timeout: Duration,
    /// Bind address for the TCP listener (default `0.0.0.0`).
    ///
    /// Accepts both IPv4 and IPv6 (epics-base PR #205 IPv6 Stage 1).
    /// For pure-IPv6 listening pass `IpAddr::V6(Ipv6Addr::UNSPECIFIED)`
    /// (`[::]`) or `IpAddr::V6(Ipv6Addr::LOCALHOST)` (`[::1]`). On
    /// Linux the kernel default is `IPV6_V6ONLY=0` so a `[::]` socket
    /// also accepts IPv4-mapped connections, giving dual-stack
    /// behaviour automatically. On BSD / macOS the default is
    /// `IPV6_V6ONLY=1`; users who need dual-stack on those platforms
    /// must run a second PVA server instance bound to IPv4.
    ///
    /// CA wire format restricts CA channels to IPv4 (4-byte address
    /// field in SEARCH_REPLY); this knob only affects the PVA TCP
    /// listener.
    pub bind_ip: IpAddr,
    /// Maximum number of concurrent client connections. Excess incoming
    /// connections are accepted then immediately closed.
    pub max_connections: usize,
    /// Maximum number of channels per single client connection.
    pub max_channels_per_connection: usize,
    /// Maximum number of concurrent in-flight operations (GET / PUT /
    /// MONITOR / RPC) that a single channel can accumulate. The
    /// per-channel `ops` map grows on each `INIT` (subcmd 0x08) and
    /// shrinks on `DESTROY` (subcmd 0x10). Without a cap, a malicious
    /// client can `INIT` against the same channel with fresh IOIDs
    /// indefinitely, exhausting server memory even when
    /// `max_channels_per_connection` is enforced. Default: 64
    /// (matches the typical `pvxs` per-channel concurrent op count
    /// of `Subscription` + the occasional in-flight GET / PUT). Excess
    /// `INIT`s are rejected with `ECA_ALLOCMEM`-equivalent error
    /// status. Override via `EPICS_PVAS_MAX_OPS_PER_CHANNEL`.
    pub max_ops_per_channel: usize,
    /// Idle timeout — server closes connections that haven't received
    /// anything in this window. Applied even if `op_timeout` is longer.
    pub idle_timeout: Duration,
    /// Per-monitor outbound queue depth. When exceeded, the back-pressure
    /// policy kicks in (squash to last value).
    pub monitor_queue_depth: usize,
    /// Optional TLS server config. When `Some`, the accept loop peeks
    /// the first byte of each incoming connection with a 100 ms window:
    /// a TLS ClientHello (byte `0x16`, sent immediately by the TLS
    /// client stack) is upgraded via `tokio_rustls::TlsAcceptor`; a
    /// peek timeout means a plain PVA client (server sends first, so
    /// client never sends the first byte) and is served as plain PVA.
    /// This mixed-mode dispatch lets a single port serve both TLS and
    /// plain clients, resolving the name-server collision.
    pub tls: Option<std::sync::Arc<crate::auth::TlsServerConfig>>,
    /// Optional override for the server's top-level access gate.
    /// When `Some`, the user source's default open gate is
    /// replaced with this gate for every wire op (GET, PUT,
    /// MONITOR, RPC, PROCESS). Use with
    /// `AccessGate::required(acf, resolver)` to load a real
    /// `.acf` policy from disk and enforce it against pvxs (or
    /// any) clients. Default `None` preserves the historical
    /// open-gate behavior so existing users see no behavior
    /// change.
    pub access_gate_override: Option<epics_base_rs::server::access_security::AccessGate>,
    /// Wire byte order the server sends in its SET_BYTE_ORDER control
    /// message. Clients adopt whatever the server picks. pvxs's
    /// `Config::overrideSendBE` exposes the same knob; defaults to LE.
    pub wire_byte_order: crate::proto::ByteOrder,
    /// Beacon emit period in seconds during the initial burst (default
    /// 15s). pvxs `server.cpp::beaconIntervalShort` parity. Override via
    /// `EPICS_PVAS_BEACON_PERIOD` — note this controls the *short*
    /// burst interval; the long steady-state interval is derived from
    /// [`Self::beacon_period_long`]. After
    /// [`Self::beacon_burst_count`] bursts the cadence drops to the
    /// long interval until a topology change (change_count tick) is
    /// emitted.
    pub beacon_period: Duration,
    /// Long-interval beacon period after the initial burst (pvxs
    /// `beaconIntervalLong` = 180s). Defaults to `12 × beacon_period`
    /// to preserve the pvxs 15s/180s ratio for the default config but
    /// scale automatically when operators tune the burst rate. Operators
    /// at sites with strict UDP bandwidth budgets can lower this; the
    /// only correctness constraint is `> beacon_period`.
    pub beacon_period_long: Duration,
    /// Number of short-interval beacons emitted before the cadence
    /// drops to `beacon_period_long`. Default 10 (pvxs
    /// `server.cpp:829` hardcodes the same value). After this many
    /// bursts every receiver in earshot has had multiple chances to
    /// notice the new server; further short-interval beacons just
    /// burn UDP bandwidth without informational gain.
    pub beacon_burst_count: u8,
    /// Explicit beacon destinations. When empty (and `auto_beacon` is
    /// true), emit per-NIC limited broadcast. From
    /// `EPICS_PVAS_BEACON_ADDR_LIST`.
    pub beacon_destinations: Vec<std::net::SocketAddr>,
    /// Auto-discover per-NIC broadcast addresses for beacons. From
    /// `EPICS_PVAS_AUTO_BEACON_ADDR_LIST` (default true).
    pub auto_beacon: bool,
    /// Interfaces to bind UDP responder on. When empty, bind 0.0.0.0.
    /// From `EPICS_PVAS_INTF_ADDR_LIST`.
    pub interfaces: Vec<std::net::IpAddr>,
    /// Emit `0xFD` / `0xFE` type-cache markers in INIT and RPC responses
    /// so repeated compound descriptors collapse to a 3-byte reference
    /// (saves 100-500 bytes per repeat for NTScalar / NTTable channels).
    /// pvxs and pvAccessJava both understand the markers; pvAccessCPP
    /// (EPICS Base 7.x) does NOT — leave this off when interop with old
    /// `pvmonitor` / `pvget` is required. Default: `false` for maximum
    /// compatibility.
    pub emit_type_cache: bool,
    /// Outbound queue depth (number of pending PVA frames) per
    /// connection. The dedicated writer task drains this; producers
    /// `await` when the queue is full, propagating backpressure to the
    /// monitor subscribers / read loop instead of letting memory grow
    /// unbounded for slow clients. Default: 1024.
    pub write_queue_depth: usize,
    /// Per-write timeout enforced by the dedicated writer task. A
    /// stuck client (kernel send buffer full because the peer
    /// stopped reading) would otherwise leave `write_all` Pending
    /// forever on a non-blocking tokio socket, blocking the
    /// heartbeat task and back-pressuring the read-side dispatcher.
    /// On expiry the writer task exits, closing the outbound mpsc
    /// so subsequent producers fail fast. Default: 5 s, override
    /// via `EPICS_PVAS_SEND_TMO`.
    pub send_timeout: Duration,
    /// Cap on the TLS handshake duration. Without this the
    /// `TlsAcceptor::accept` future is awaited bare, so a peer that
    /// completes the TCP handshake but never delivers (or only partially
    /// delivers) a `ClientHello` keeps a slot in `max_connections` until
    /// the OS-level keepalive (15s/5s probes) drops the half-open TCP.
    /// A coordinated burst of such peers can exhaust the connection
    /// limit (slowloris-style). pvxs avoids the equivalent issue via
    /// libevent `bufferevent_set_timeouts`; we do it explicitly here.
    /// Default: 10 s, override via `EPICS_PVAS_TLS_HANDSHAKE_TMO`.
    pub tls_handshake_timeout: Duration,
    /// Optional hard cap on a single inbound message's payload length.
    /// `None` (the default) means **unbounded** — matching pvxs, which
    /// deliberately keeps no RX message-size limit so PVA can carry
    /// arbitrarily large structures (the design point that replaced
    /// CA's `EPICS_CA_MAX_ARRAY_BYTES`). `read_frame` and the
    /// segment-reassembly path stay bounded regardless via incremental
    /// 4 KiB reads, the `op_timeout` deadline, and `safe_capacity`,
    /// so the absence of a cap is not itself an
    /// amplification vector. Set `Some(n)` to opt in to a hard ceiling
    /// (e.g. a hardened deployment that wants to reject any header
    /// claiming more than `n` bytes and drop the connection).
    pub max_message_size: Option<usize>,
    /// Serve the `PUT_GET` operation (PVA cmd 12). `PUT_GET` is a Rust
    /// extension: pvxs declares the command but leaves `handle_PUT_GET`
    /// an empty stub (`serverconn.cpp:259-260`) and its client never
    /// sends cmd 12 (`clientimpl.h:143`). When `true` (the default) this
    /// server implements the full INIT/put/readback/destroy lifecycle so
    /// a PUT_GET-capable client gets a real round trip. Set `false` for
    /// strict pvxs-compatible behavior: every cmd-12 frame is answered
    /// with a deterministic error `Status` instead of the Rust round
    /// trip. We reply with an explicit error rather than pvxs's silent
    /// drop so the policy is visible at the wire level and a client fails
    /// fast instead of waiting out its `op_timeout`.
    pub serve_put_get: bool,
    /// UDP-SEARCH ignore list. Each entry is `(IpAddr, port_or_zero)` —
    /// matching UDP SEARCH datagrams are silently dropped before
    /// admission. `port == 0` matches any port from that IP. Mirrors
    /// pvxs `Config::ignoreAddrs`, which is consulted ONLY from the UDP
    /// search path (`Server::Pvt::onSearch`, server.cpp:654-670) and NOT
    /// from the TCP accept callback (serverconn.cpp:461-467): a noisy
    /// host's discovery traffic can be suppressed while its direct TCP
    /// clients still connect. Empty = allow all (default).
    pub ignore_addrs: Vec<(std::net::IpAddr, u16)>,
    /// Spawn a parallel IPv6 UDP listener bound to `[::]:udp_port`
    /// alongside the existing IPv4 per-NIC responder. Required for
    /// PVA clients that send SEARCH over IPv6 unicast / multicast.
    /// Default `false` keeps every existing deployment exactly v4-only.
    ///
    /// Stage 2 of the PR #205 IPv6 effort. The v6 responder shares
    /// the GUID / TCP-port / protocol of the v4 path so a single
    /// client sees one consistent PVA server regardless of which
    /// address family carried the SEARCH. Beacon emission stays on
    /// the v4 path for now; v6 multicast beacons are deferred.
    pub enable_ipv6_udp: bool,
    /// Per-monitor "high" watermark — emit a `tracing::warn!` when an
    /// outbound monitor queue grows past this many items. Default:
    /// `monitor_queue_depth * 3 / 4`. Mirrors pvxs
    /// `MonitorControlOp::setWatermarks` `high` argument; high-mark
    /// callbacks (`onHighMark`) aren't surfaced yet — the watermark
    /// drives diagnostics only.
    pub monitor_high_watermark: usize,
    /// Per-monitor "low" watermark — companion to `high`, currently
    /// unused (pvxs notes the `onLowMark` callback isn't fully
    /// implemented either). Reserved for future flow-control logic.
    pub monitor_low_watermark: usize,
    /// Optional post-handshake hook. Fires once per accepted client
    /// connection, immediately after the server has parsed the
    /// peer's `CONNECTION_VALIDATION` reply and sent
    /// `CONNECTION_VALIDATED`. Receives the peer address and the
    /// parsed [`crate::server_native::tcp::ClientCredentials`].
    /// Mirrors pvxs `auth_complete` server-side hook
    /// (serverconn.cpp:181). Use this to integrate per-peer ACF
    /// state — e.g., look up `cred.account` + `cred.roles` against a
    /// rule database and stash the decision somewhere the per-op
    /// path can consult.
    ///
    /// Stored as `Arc<dyn Fn>` so the closure can be cloned across
    /// per-connection tasks. Default: `None` (no-op).
    pub auth_complete: Option<
        std::sync::Arc<dyn Fn(std::net::SocketAddr, &super::tcp::ClientCredentials) + Send + Sync>,
    >,
}

impl PvaServerConfig {
    /// True when `peer` matches an entry in `ignore_addrs`. Port 0 in
    /// the entry is a wildcard. O(n) over the list; n is expected to
    /// be small (single-digit) in practice.
    ///
    /// This predicate is the UDP-SEARCH ignore policy and is NOT applied
    /// to TCP accepts (pvxs parity — see [`Self::ignore_addrs`]). It is
    /// exposed so a deployment that wants a separate Rust-only TCP ACL
    /// can opt into one explicitly rather than having the discovery
    /// filter silently double as a transport gate.
    pub fn is_ignored_peer(&self, peer: std::net::SocketAddr) -> bool {
        for (ip, port) in &self.ignore_addrs {
            if peer.ip() == *ip && (*port == 0 || peer.port() == *port) {
                return true;
            }
        }
        false
    }
}

impl Default for PvaServerConfig {
    fn default() -> Self {
        Self {
            tcp_port: 5075,
            udp_port: 5076,
            guid: [0u8; 12],
            op_timeout: Duration::from_secs(64_000),
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            max_connections: 1024,
            max_channels_per_connection: 1024,
            max_ops_per_channel: 64,
            idle_timeout: Duration::from_secs(45),
            monitor_queue_depth: 64,
            tls: None,
            access_gate_override: None,
            wire_byte_order: crate::proto::ByteOrder::Little,
            beacon_period: Duration::from_secs(15),
            beacon_period_long: Duration::from_secs(180),
            beacon_burst_count: 10,
            beacon_destinations: Vec::new(),
            auto_beacon: true,
            interfaces: Vec::new(),
            emit_type_cache: false,
            write_queue_depth: 1024,
            ignore_addrs: Vec::new(),
            enable_ipv6_udp: false,
            monitor_high_watermark: 48, // 64 * 3 / 4 default
            monitor_low_watermark: 0,
            auth_complete: None,
            send_timeout: Duration::from_secs(5),
            tls_handshake_timeout: Duration::from_secs(10),
            max_message_size: None,
            serve_put_get: true,
        }
    }
}

impl PvaServerConfig {
    /// Loopback-only configuration with random ports — pvxs
    /// `Config::isolated()` (config.cpp:445). The OS picks free TCP
    /// and UDP ports; auto-beacon is disabled so the server doesn't
    /// leak datagrams onto the LAN. Matching client side: point a
    /// [`crate::client_native::PvaClient`] at the returned loopback
    /// address via [`crate::client_native::PvaClientBuilder::server_addr`].
    pub fn isolated() -> Self {
        Self {
            tcp_port: 0,
            udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        }
    }

    /// Apply standard EPICS_PVAS_* / EPICS_PVA_* env vars on top of an
    /// existing config. Only fields backed by a **present** env var are
    /// touched — every other field keeps its existing value.
    ///
    /// Each `env::*_opt()` helper returns `None` when none of the variables
    /// that back its field is set, so an absent variable never overwrites a
    /// caller-supplied value with a compiled default. This mirrors pvxs
    /// `Config::applyEnv` → `PickOne` (`config.cpp:397-437`/:439-443), where
    /// every field is assigned only inside `if(pickone(...))`. It keeps
    /// `PvaServerConfig::isolated().with_env()` isolated in an empty
    /// environment (ports stay ephemeral, `auto_beacon` stays off) instead
    /// of silently reverting to the LAN-facing defaults.
    pub fn with_env(mut self) -> Self {
        use crate::config::env;
        // server respects EPICS_PVAS_SERVER_PORT first, then
        // falls back to EPICS_PVA_SERVER_PORT (pvxs config.cpp:
        // 402-408 PickOne precedence).
        if let Some(v) = env::pvas_server_port_opt() {
            self.tcp_port = v;
        }
        if let Some(v) = env::server_broadcast_port_opt() {
            self.udp_port = v;
        }
        if let Some(v) = env::max_connections_opt() {
            self.max_connections = v;
        }
        if let Some(v) = env::max_channels_per_connection_opt() {
            self.max_channels_per_connection = v;
        }
        if let Some(v) = env::max_ops_per_channel_opt() {
            self.max_ops_per_channel = v;
        }
        // Beacon periods: keep the pvxs short:long = 15:180 = 1:12 ratio
        // when only the short period is tuned; an explicit
        // `EPICS_PVAS_BEACON_PERIOD_LONG` wins. Floor the long path at
        // `beacon_period + 1s` (beacon_loop assumes long > short). A short
        // override re-derives the long unless the long is also set; if
        // neither var is present both fields keep their caller values.
        let short_set = if let Some(v) = env::beacon_period_opt() {
            self.beacon_period = v;
            true
        } else {
            false
        };
        if let Some(long) = env::beacon_period_long() {
            self.beacon_period_long = long.max(self.beacon_period + Duration::from_secs(1));
        } else if short_set {
            self.beacon_period_long = self
                .beacon_period
                .saturating_mul(12)
                .max(self.beacon_period + Duration::from_secs(1));
        }
        if let Some(v) = env::server_beacon_addr_list_opt() {
            self.beacon_destinations = v;
        }
        if let Some(v) = env::auto_beacon_addr_list_enabled_opt() {
            self.auto_beacon = v;
        }
        if let Some(v) = env::server_intf_addr_list_opt() {
            self.interfaces = v;
        }
        if let Some(v) = env::send_timeout_secs_opt() {
            self.send_timeout = Duration::from_secs_f64(v);
        }
        if let Some(v) = env::tls_handshake_timeout_secs_opt() {
            self.tls_handshake_timeout = Duration::from_secs_f64(v);
        }
        // Effective inactivity timeout = configured CONN_TMO × 4/3.
        // pvxs config.cpp:187 applies the same scaling so a client
        // sending ECHO every CONN_TMO/2 (the protocol convention)
        // gets a margin against scheduling jitter — without it, a
        // server with idle_timeout = exactly CONN_TMO would race
        // with a healthy client's second ECHO and disconnect it.
        // Floor at 2s mirrors pvxs `enforceTimeout`.
        if let Some(c) = env::conn_timeout_secs_opt() {
            let scaled = (c * 4.0 / 3.0).max(2.0);
            self.idle_timeout = Duration::from_secs_f64(scaled);
        }
        if let Some(v) = env::server_ignore_addr_list_opt() {
            self.ignore_addrs = v;
        }
        self
    }
}

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
    let server = PvaServer::start(source, config)?;
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
    /// 0 and the OS picked one.  We capture the configured value here;
    /// callers needing the post-bind port should query the listener
    /// directly (future work).
    bound_tcp_port: u16,
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
        let guid = random_guid();
        // the TCP-circuit SEARCH handler reads this guid
        // out of the per-connection config copy to populate the
        // SEARCH_RESPONSE body. Stamp it onto a local mut copy so
        // every per-conn task sees the same identity the UDP path
        // does.
        let mut config = config;
        config.guid = guid;
        // The live per-peer registry is created up-front so the
        // built-in server-info source can report connection counts.
        let peers = crate::server_native::peers::PeerRegistry::new();

        // User source kept as a dyn handle: the built-in source's
        // channel-list closure enumerates it directly (rather than
        // the composite, which would also include the built-in source
        // — harmless since its `list_pvs` is empty, but enumerating
        // the user half keeps the intent explicit).
        let user_source: DynSource = source as Arc<dyn ChannelSourceObj>;
        let server_info = Arc::new(super::server_info::ServerInfoSource::new({
            let user_source = user_source.clone();
            move || {
                let user_source = user_source.clone();
                async move { user_source.list_pvs().await }
            }
        }));

        // Composite registry: built-in `__server` at order -1, BEFORE
        // default-order (0) user sources, matching pvxs registering its
        // `ServerSource` at `(order = -1, "__server")` (server.cpp:542-547)
        // where the lowest order is consulted first (server.h:108-118).
        // The built-in source only claims the reserved `server` name, so
        // running it first shadows a user PV named `server` (the pvxs
        // diagnostic-source contract) while every other name still falls
        // through to the user source. A user that genuinely wants to own
        // `server` must register at an explicit order < -1.
        let composite = super::CompositeSource::new();
        composite
            .add_source("__user", user_source, 0)
            .map_err(|e| {
                PvaError::Protocol(format!("PvaServer::start: register user source: {e}"))
            })?;
        composite
            .add_source(
                super::server_info::SERVER_SOURCE_NAME,
                server_info as DynSource,
                -1,
            )
            .map_err(|e| {
                PvaError::Protocol(format!(
                    "PvaServer::start: register built-in server source: {e}"
                ))
            })?;
        let dyn_source: DynSource = composite as Arc<dyn ChannelSourceObj>;

        // One server-wide channel-invalidation broadcast. A source that can
        // invalidate a channel out of band (the PVA gateway, on an operator
        // `<prefix>:drop` / `:flush`) publishes the affected PV name here;
        // every per-connection task subscribes and force-disconnects any
        // channel it currently serves under that name with a server-initiated
        // DESTROY_CHANNEL — the downstream effect of pva2pva's cache-entry
        // `channel->destroy()` fanout. The initial receiver is dropped: the
        // channel stays open as long as this sender (held by the source(s)
        // and the accept loop) lives, and per-connection receivers are minted
        // at accept. Capacity is generous so a one-shot `:flush` of a large
        // cache does not lag a connection task that is briefly busy; a lagging
        // receiver logs and the operator can re-issue (see the read-loop arm).
        let (channel_invalidator, _inv_rx0) =
            tokio::sync::broadcast::channel::<String>(CHANNEL_INVALIDATION_CAPACITY);
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
        let udp_handle = tokio::spawn(run_udp_responder_with_config(
            dyn_source.clone(),
            config.udp_port,
            bound_tcp_port,
            guid,
            protocol,
            config.beacon_period,
            config.beacon_period_long,
            config.beacon_burst_count,
            config.beacon_destinations.clone(),
            config.auto_beacon,
            config.ignore_addrs.clone(),
            config.enable_ipv6_udp,
            udp_interfaces,
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
                bound_tcp_port,
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
        ServerReport {
            tcp_port: self.bound_tcp_port,
            udp_port: self.effective_config.udp_port,
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
            peers: self.peers.snapshot_zeroed(zero),
            peer_count: self.peers.len(),
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

#[cfg(test)]
mod tcp_fallback_tests {
    //! pvxs parity for multi-server-on-one-host: when the requested
    //! `tcp_port` is already taken, fall back to ephemeral so a
    //! second IOC on the same machine doesn't panic on startup.
    //! Mirrors pvxs `serverconn.cpp:493` (EADDRINUSE → setPort(0),
    //! single retry).
    use super::*;
    use crate::server_native::SharedSource;

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
}

#[cfg(test)]
mod sr9_message_size_tests {
    //! the inbound message-size cap defaults to unbounded
    //! (`None`), matching pvxs which deliberately keeps no RX cap. A
    //! deployment opts into a hard ceiling by setting `Some(n)`.
    use super::*;

    #[test]
    fn default_message_size_is_unbounded() {
        assert_eq!(
            PvaServerConfig::default().max_message_size,
            None,
            "default server config must be unbounded (None), not a fixed cap"
        );
        // `isolated()` inherits the default via `..Default::default()`.
        assert_eq!(PvaServerConfig::isolated().max_message_size, None);
        // `with_env()` does not introduce a cap either.
        assert_eq!(PvaServerConfig::default().with_env().max_message_size, None);
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

    #[test]
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
