//! [`PvaServerConfig`] — the server's configuration record.
//!
//! Target-neutral, and deliberately its own module rather than part of
//! `super::runtime`. The config is named in the signature of eight
//! production functions in [`super::tcp`], which is protocol code with no
//! socket in it and therefore has to compile for RTEMS; `runtime` is the
//! host-only async driver that binds the listeners. Leaving the config in
//! `runtime` made the whole protocol layer inherit the host gate through one
//! type reference (`doc/pva-rtems-item7-design.md` §6).
//!
//! Nothing here touches the network. The fields describe *what* to bind and
//! *how* to behave once bound; `runtime` (host) and the blocking driver
//! (RTEMS) each read the same record.
//!
//! `super::runtime` also re-exports `PvaServerConfig`, so every existing
//! `server_native::runtime::PvaServerConfig` path still resolves on a hosted
//! build. The short `server_native::PvaServerConfig` comes from HERE, not
//! from `runtime`: routing it through the host-only module deleted the name
//! on every embedded target even though this module compiles for all of them.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

/// Default ceiling on a single inbound message
/// ([`PvaServerConfig::max_message_size`]): **16 MiB**.
///
/// The number is not chosen here — it is the one this workspace already gives
/// as its answer to "the largest body we will allocate because a remote header
/// said so". `epics_ca_rs::protocol::MAX_FRAME_BODY_BYTES` is the same 16 MiB,
/// documented there as a Tier 2 deviation standing in for C's *no limit* (C's
/// CA server is genuinely unbounded by default — `casExpandBuffer`
/// `rsrv/caservertask.c:1326-1358` only consults `rsrvSizeofLargeBufTCP` when
/// `EPICS_CA_AUTO_ARRAY_BYTES` is off, and it defaults on). Two protocols
/// answering that question with two different numbers would mean two
/// resource policies to reason about on one IOC, so PVA takes CA's.
///
/// Deliberately not derived from `MAX_FRAME_BODY_BYTES` by import: the crates
/// do not depend on each other, and a PVA server has to be configurable
/// without a CA server present. The coupling is a documented intent, not a
/// build-time one.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Runtime configuration for `runtime::run_pva_server`.
#[derive(Clone)]
pub struct PvaServerConfig {
    pub tcp_port: u16,
    pub udp_port: u16,
    /// Dedicated TLS listen port. Only consulted when [`Self::tls`] is
    /// `Some`: the runtime then binds a *second* TCP listener on this
    /// port (in addition to the plaintext [`Self::tcp_port`] listener)
    /// and advertises it as the `"tls"` endpoint in UDP / TCP-circuit
    /// SEARCH replies. Mirrors pvxs, which binds a separate per-interface
    /// TLS socket on `effective.tls_port` (`server.cpp:595-608`) and
    /// returns it for a protoTLS SEARCH (`server.cpp:849-852`,
    /// `serverchan.cpp:195,250`).
    ///
    /// `0` requests an OS-ephemeral TLS port (the bound value is stamped
    /// back here at `start()`, like [`Self::tcp_port`]). When the
    /// requested port collides with the bound plaintext port the runtime
    /// skips the second bind and serves TLS on the shared port via the
    /// first-byte dispatch in
    /// `tcp::run_tcp_server_on_listener`. Default
    /// `5076` (pvxs `netcommon.h:133`); parsed from
    /// `EPICS_PVAS_TLS_PORT` / `EPICS_PVA_TLS_PORT` by [`Self::with_env`].
    pub tls_port: u16,
    /// Server identity, propagated into the TCP-circuit `Command::Search`
    /// reply (`pvxs serverchan.cpp:215-235`); the UDP SEARCH_RESPONSE and the
    /// beacons carry the same 12 bytes.
    ///
    /// **Whatever you put here is not what gets served.** Every server
    /// constructor — `PvaServer::start`
    /// and `BlockingPvaServer::bind` — overwrites this field with
    /// `search_engine::random_guid()` before any thread or task can read it,
    /// or **fails to construct** if the platform has no entropy source. So
    /// this field is not an input a caller supplies; it is where the server
    /// records the identity it drew, and the [`Default`] zeros are a value no
    /// running server ever advertises.
    ///
    /// (An earlier version of this comment said "the runtime fills this",
    /// which read as an assumption about one code path rather than a rule. It
    /// stopped being true the moment a second server constructor existed.)
    ///
    /// The one exception is the low-level
    /// `run_tcp_server_on_listener`
    /// family, which serves the config verbatim because it is one listener of
    /// several and re-randomizing per listener would give a single server
    /// several identities. A caller driving those directly owns the GUID and
    /// must fill it.
    pub guid: [u8; 12],
    /// Per-frame read timeout. The server *also* applies the heartbeat-
    /// based idle timeout below — `op_timeout` is just the upper bound on
    /// how long the peer may go without completing a frame. Enforced
    /// coarsely by the connection loop's deadline ticker (period
    /// `op_timeout / 2`, capped at 15 s) rather than a per-read timer,
    /// so a stall is detected within one tick past the bound.
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
    /// The queue limit each MONITOR operation STARTS with, before the
    /// client's `record._options.queueSize` gets a say. When the queue is
    /// full the back-pressure policy kicks in (squash into the last
    /// value).
    ///
    /// This is pvxs `MonitorOp::limit` (`servermon.cpp:66`, `limit=4u`)
    /// made configurable — a per-operation initializer, NOT a server-wide
    /// capacity. pvxs has no server knob here, so
    /// [`crate::server_native::source::DEFAULT_MONITOR_QUEUE_LIMIT`] (4)
    /// is the default; a deployment may raise it, which is the same
    /// deviation as building pvxs with a different `limit` initializer.
    /// A valid client `queueSize >= 2` always wins over it
    /// (`servermon.cpp:533-543`), pipelined or not, and the resolved
    /// value is the single per-op depth: the squash threshold, the base
    /// of the `ackAny` arithmetic, and the reported queue limit
    /// (`MonitorOptions::queue_size`).
    pub monitor_queue_depth: usize,
    /// Require TLS — refuse plaintext sessions (anti-downgrade). Parsed
    /// from `disable_plaintext=true` in `EPICS_PVAS_TLS_OPTIONS` /
    /// `EPICS_PVA_TLS_OPTIONS` by [`Self::with_env`]; default `false`.
    ///
    /// pvxs carries `tls_disable_plaintext` in the shared `ConfigCommon`
    /// (`netcommon.h:172`) and enforces the downgrade refusal on the
    /// CLIENT — it drops a plaintext (`"tcp"`) SEARCH reply
    /// (`client.cpp:944`). Because the Rust server unifies TLS and
    /// plaintext on each listener via the first-byte peek
    /// (`run_tcp_server_on_listener`), the equivalent server-side
    /// guarantee is to refuse a non-TLS peer at accept time so a
    /// stripped/downgraded connection can never reach the plain code
    /// path. Inert unless [`Self::tls`] is `Some` (a server with no TLS
    /// identity cannot be TLS-only).
    pub disable_plaintext: bool,
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
    /// Explicit beacon destinations, each carrying optional pvxs multicast
    /// modifiers (TTL / outgoing interface; see [`crate::config::Endpoint`]).
    /// When empty (and `auto_beacon` is true), emit per-NIC limited broadcast.
    /// From `EPICS_PVAS_BEACON_ADDR_LIST`. A plain `SocketAddr` converts via
    /// `.into()` (no modifiers).
    pub beacon_destinations: Vec<crate::config::Endpoint>,
    /// Auto-discover per-NIC broadcast addresses for beacons. From
    /// `EPICS_PVAS_AUTO_BEACON_ADDR_LIST` (default true).
    pub auto_beacon: bool,
    /// Interfaces to bind UDP responder on. When empty, bind 0.0.0.0.
    /// From `EPICS_PVAS_INTF_ADDR_LIST`.
    pub interfaces: Vec<std::net::IpAddr>,
    /// PVX-82: deferred config error set by [`Self::with_env`] when
    /// `EPICS_PVA[S]_INTF_ADDR_LIST` named interface(s) that all failed to
    /// resolve. `PvaServer::start` refuses to bind in that case rather than
    /// silently promoting the (now-empty) `interfaces` to the wildcard
    /// `0.0.0.0`. `None` on any programmatically-built config — an empty
    /// `interfaces` set directly by a caller is an intentional wildcard.
    /// Public only because the config is built with struct-update syntax
    /// across the crate boundary; it is not a user-facing knob.
    pub intf_addr_error: Option<String>,
    /// PVX-82 (IGNORE sibling): deferred config error set by
    /// [`Self::with_env`] when `EPICS_PVAS_IGNORE_ADDR_LIST` named peer(s)
    /// to block that all failed to resolve. `PvaServer::start` refuses to
    /// start rather than running with a silently-empty blocklist (pvxs
    /// hard-fails the same config — `config.cpp:172-174`, `required=true`).
    /// `None` on any programmatically-built config. Public for the same
    /// struct-update reason as [`Self::intf_addr_error`]; not a user knob.
    pub ignore_addr_error: Option<String>,
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
    /// Hard cap on a single inbound message's payload length. Defaults to
    /// [`DEFAULT_MAX_MESSAGE_SIZE`]; `None` means unbounded.
    ///
    /// **This default is a deliberate deviation from pvxs, which keeps no RX
    /// message-size limit** so PVA can carry arbitrarily large structures (the
    /// design point that replaced CA's `EPICS_CA_MAX_ARRAY_BYTES`). Two
    /// reasons to cap anyway:
    ///
    /// * pvxs can afford to be uncapped because a failed allocation there is
    ///   a `bad_alloc` caught per connection (`conn.cpp:307-335`). Ours is now
    ///   fallible too (`try_reserve_or_shed`), but a cap refuses a doomed
    ///   message *before* the IOC spends the memory and the bandwidth, and
    ///   answers with a reason instead of an allocator failure.
    /// * On RTEMS the whole heap is a few hundred MiB, so "unbounded" and
    ///   "the machine" are the same number.
    ///
    /// Set `None` for pvxs-exact behaviour, or `Some(n)` to pick your own
    /// ceiling. The reassembly and receive paths stay bounded regardless via
    /// incremental 4 KiB reads, the `op_timeout` deadline, and
    /// `safe_capacity`, so this cap is a resource policy rather than the only
    /// thing standing between a peer and the heap.
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
    /// `monitor_queue_depth * 3 / 4` (3, for the pvxs per-op default
    /// depth of 4). Mirrors pvxs
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
    /// parsed [`ClientCredentials`].
    /// Mirrors pvxs `auth_complete` server-side hook
    /// (serverconn.cpp:181). Use this to integrate per-peer ACF
    /// state — e.g., look up `cred.account` + `cred.roles` against a
    /// rule database and stash the decision somewhere the per-op
    /// path can consult.
    ///
    /// Stored as `Arc<dyn Fn>` so the closure can be cloned across
    /// per-connection tasks. Default: `None` (no-op).
    pub auth_complete:
        Option<std::sync::Arc<dyn Fn(std::net::SocketAddr, &ClientCredentials) + Send + Sync>>,
}

impl PvaServerConfig {
    /// [`Self::monitor_queue_depth`] as the `u32` the MONITOR INIT
    /// negotiation works in — the value a fresh `MonitorOp::limit` starts
    /// at. Saturating (a `usize` above `u32::MAX` is not a wire-legal
    /// queueSize) and floored at 1, so the negotiated limit a monitor ends
    /// up with is never 0 and the squash comparison always admits a first
    /// event.
    pub fn monitor_queue_limit(&self) -> u32 {
        u32::try_from(self.monitor_queue_depth)
            .unwrap_or(u32::MAX)
            .max(1)
    }

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
            tls_port: 5076,
            guid: [0u8; 12],
            op_timeout: Duration::from_secs(64_000),
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            max_connections: 1024,
            max_channels_per_connection: 1024,
            max_ops_per_channel: 64,
            idle_timeout: Duration::from_secs(45),
            monitor_queue_depth: super::source::DEFAULT_MONITOR_QUEUE_LIMIT as usize,
            disable_plaintext: false,
            tls: None,
            access_gate_override: None,
            wire_byte_order: crate::proto::ByteOrder::Little,
            beacon_period: Duration::from_secs(15),
            beacon_period_long: Duration::from_secs(180),
            beacon_burst_count: 10,
            beacon_destinations: Vec::new(),
            auto_beacon: true,
            interfaces: Vec::new(),
            intf_addr_error: None,
            ignore_addr_error: None,
            emit_type_cache: false,
            write_queue_depth: 1024,
            ignore_addrs: Vec::new(),
            enable_ipv6_udp: false,
            monitor_high_watermark: (super::source::DEFAULT_MONITOR_QUEUE_LIMIT as usize) * 3 / 4,
            monitor_low_watermark: 0,
            auth_complete: None,
            send_timeout: Duration::from_secs(5),
            tls_handshake_timeout: Duration::from_secs(10),
            max_message_size: Some(DEFAULT_MAX_MESSAGE_SIZE),
            serve_put_get: true,
        }
    }
}

impl PvaServerConfig {
    /// Loopback-only configuration with random ports — pvxs
    /// `Config::isolated()` (config.cpp:445). The OS picks free TCP
    /// and UDP ports; auto-beacon is disabled so the server doesn't
    /// leak datagrams onto the LAN. Matching client side: point a
    /// `client_native::PvaClient` at the returned loopback
    /// address via `client_native::PvaClientBuilder::server_addr`.
    pub fn isolated() -> Self {
        Self {
            tcp_port: 0,
            udp_port: 0,
            tls_port: 0,
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
        // Server TLS listen port: EPICS_PVAS_TLS_PORT first, then the
        // shared EPICS_PVA_TLS_PORT (pvxs config.cpp:513-519 PickOne).
        // Only takes effect when `tls` is configured (see `tls_port`).
        if let Some(v) = env::pvas_tls_port_opt() {
            self.tls_port = v;
        }
        if let Some(v) = env::server_broadcast_port_opt() {
            self.udp_port = v;
        }
        // Anti-downgrade: `disable_plaintext=true` in the TLS options makes
        // the server refuse plaintext (pvxs parseTLSOptions, config.cpp:453).
        if let Some(v) = env::server_tls_disable_plaintext_opt() {
            self.disable_plaintext = v;
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
        if let Some(v) = env::server_beacon_endpoints_opt() {
            self.beacon_destinations = v;
        }
        if let Some(v) = env::auto_beacon_addr_list_enabled_opt() {
            self.auto_beacon = v;
        }
        // PVX-82: a non-blank INTF list whose tokens all fail to resolve is
        // a misconfiguration — record it so `PvaServer::start` refuses to
        // bind rather than silently falling back to the wildcard. A blank /
        // unset var leaves `interfaces` untouched (caller value preserved;
        // empty ⟹ intentional wildcard at bind).
        match env::server_intf_addr_list_checked() {
            Ok(Some(v)) => self.interfaces = v,
            Ok(None) => {}
            Err(msg) => self.intf_addr_error = Some(msg),
        }
        if let Some(v) = env::send_timeout_secs_opt() {
            self.send_timeout = Duration::from_secs_f64(v);
        }
        if let Some(v) = env::tls_handshake_timeout_secs_opt() {
            self.tls_handshake_timeout = Duration::from_secs_f64(v);
        }
        // Effective inactivity timeout = configured CONN_TMO × 4/3, then
        // pvxs `enforceTimeout` — BOTH bounds, via the single owner
        // `env::effective_tcp_timeout_secs`. The scaling gives a client
        // sending ECHO every CONN_TMO/2 (the protocol convention) a margin
        // against scheduling jitter; the 2 s floor and the
        // `>= double(time_t::max)` → 40 s reset are the `enforceTimeout`
        // halves the server must not reproduce partially.
        if let Some(c) = env::conn_timeout_secs_opt() {
            self.idle_timeout = Duration::from_secs_f64(env::effective_tcp_timeout_secs(c));
        }
        // PVX-82 (IGNORE sibling): same all-unresolvable gate as INTF —
        // a non-blank IGNORE list that resolves to nothing means the
        // requested blocklist is silently empty; record it so
        // `PvaServer::start` refuses rather than running unfiltered. A
        // blank / unset var leaves a caller-supplied `ignore_addrs` intact.
        match env::server_ignore_addr_list_checked() {
            Ok(Some(v)) => self.ignore_addrs = v,
            Ok(None) => {}
            Err(msg) => self.ignore_addr_error = Some(msg),
        }
        self
    }
}

/// Identity used for per-connection authorisation.
///
/// Mirrors pvxs `server::ClientCredentials` (serverconn.cpp:73-234).
/// Two population paths feed it:
///
/// - **`ca` / `anonymous`** — parsed off the CONNECTION_VALIDATION reply
///   (`parse_client_credentials`).
/// - **`x509`** — derived from the *verified* TLS peer certificate chain
///   after the handshake (pvxs `SSLContext::fill_credentials`). The TLS
///   identity is authoritative: it overrides whatever the client claims
///   in CONNECTION_VALIDATION, because the chain was cryptographically
///   verified against the configured root CA.
///
/// The structured form is consumed by the server's ACF access gate
/// (`AccessGate::check`) and lands in `tracing` for audit.
#[derive(Debug, Clone)]
pub struct ClientCredentials {
    /// Selected auth method ("anonymous" / "ca" / "x509" / ...).
    pub method: String,
    /// Account name (e.g., the `ca` auth's `user` field, or the x509
    /// leaf cert subject CommonName). Empty when the auth method does
    /// not carry one.
    pub account: String,
    /// Host identity of the peer, as the ACF `HAG(...)` gate matches it:
    /// the connection's peer address in numeric form, port stripped and
    /// IPv4-mapped IPv6 collapsed to IPv4 (QSRV `ioc/credentials.cpp:27-29`).
    ///
    /// SECURITY: this is NEVER populated from the wire. A client MAY
    /// advertise a `host` field in CONNECTION_VALIDATION, and this server
    /// ignores it — trusting it would let any client type the string its
    /// `HAG` rules are matched against and grant itself every host-scoped
    /// rule, including the `unresolved:<name>` sentinel a failed ACF-load
    /// DNS lookup leaves behind. pvxs makes this impossible by having no
    /// host field at all (`src/pvxs/srvcommon.h:36-56`); we make it
    /// impossible by deriving the value in `with_server_derived` from the
    /// peer socket, which is the same funnel that derives [`Self::roles`],
    /// and by leaving the CONNECTION_VALIDATION parser no arm that can write
    /// here.
    pub host: String,
    /// Certificate authority for the `x509` method: the root CA's
    /// subject CommonName (pvxs `PeerCredentials::authority`). Empty for
    /// non-TLS methods. ACF `RULE(... ){ AUTHORITY("...") }` scopes
    /// match against this.
    pub authority: String,
    /// Group / role memberships of [`Self::account`], re-derived
    /// SERVER-SIDE from the local passwd/group DB by
    /// `tcp::ClientCredentials::with_server_derived` (pvxs
    /// `ClientCredentials::roles()` →
    /// `osdGetRoles`, serverconn.cpp:33-37). ACF rules of the form
    /// `R member group:operators` match against this set.
    ///
    /// SECURITY: this is NEVER populated from the wire. A client
    /// advertises a `groups`/`roles` field in CONNECTION_VALIDATION, but
    /// trusting it would let `account="nobody", roles=["admin"]` satisfy
    /// any group-gated rule — an ACL bypass. Every constructor funnels
    /// through `with_server_derived`, so a wire value can never reach here.
    pub roles: Vec<String>,
}
