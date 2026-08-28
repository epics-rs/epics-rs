//! Public `PvaClient` facade.
//!
//! Built on top of:
//!
//! - [`super::search_engine::SearchEngine`] — single background task,
//!   handles SEARCH retry backoff + beacon listening.
//! - [`super::channel::ConnectionPool`] — shared `ServerConn` per server
//!   address, with full handshake + heartbeat + auto-shutdown.
//! - [`super::channel::Channel`] — per-PV state machine (Searching →
//!   Connecting → Active → Reconnecting). Multiple ops share a single
//!   channel instance.
//! - [`super::ops_v2`] — GET / PUT / MONITOR / RPC; MONITOR transparently
//!   re-issues INIT + START on every reconnect.
//!
//! Public API stays compatible with the previous shape so existing callers
//! (pvget-rs, pvput-rs, pvmonitor-rs, pvinfo-rs) keep working.

// (1 search-timeout test gated out on the exec backend below; §4.2 UDP search,
// stage 3.)

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::RwLock;

use crate::error::{PvaError, PvaResult};
use crate::pvdata::{FieldDesc, PvField, RpcReply};

use super::channel::{Channel, ConnectionPool};
use super::ops_v2::{
    MonitorConnEvent, MonitorEvent, MonitorEventMask, RpcArg, SubscriptionHandle, op_get,
    op_get_get, op_get_put, op_monitor, op_monitor_events, op_monitor_handle,
    op_monitor_raw_frames_handle, op_monitor_raw_frames_handle_with_request, op_process,
    op_process_with_request, op_process_with_request_value, op_put, op_put_get, op_rpc,
};
use super::search_engine::{ClientSearchConfig, SearchEngine};

/// The empty pvRequest pvxs sends by default for a parameterless RPC
/// (`pvRequest()` — an empty top-level structure). Used as the RPC
/// INIT payload when the caller expresses a top-level null DATA
/// argument; an empty pvRequest selects all fields server-side and is
/// inert for RPC, which projects no fields.
fn empty_pv_request() -> (FieldDesc, PvField) {
    (
        FieldDesc::Structure {
            struct_id: String::new(),
            fields: Vec::new(),
        },
        PvField::Structure(crate::pvdata::PvStructure::new("")),
    )
}

#[derive(Debug, Clone)]
pub struct PvGetResult {
    pub pv_name: String,
    pub value: PvField,
    pub introspection: FieldDesc,
    pub server_addr: SocketAddr,
}

/// A single `pvinfo` describe result: the channel's introspection, the
/// server that replied, and the server's verified X.509 identity (the
/// credentials pvxs `pvxinfo -v` prints; `None` for a plain `pva://`
/// connection). Names the shape returned per-PV by the concurrent
/// [`PvaClient::pvinfo_many_full_with_credentials`] batch.
pub type PvInfoResult = (FieldDesc, SocketAddr, Option<crate::auth::X509Credentials>);

/// Records that this `PvaClient`'s upstream credentials are a
/// gateway-asserted identity derived from a *downstream* connection
/// that authenticated with a method this client cannot forward
/// verbatim on the wire.
///
/// the pvAccess CONNECTION_VALIDATION handshake only carries
/// the `ca` / `anonymous` auth methods (pvxs `clientconn.cpp:217-305`
/// — `handle_CONNECTION_VALIDATION` selects only `"ca"` / `"anonymous"`
/// and the `ca` credential carries solely `user` + `host`). There is
/// no wire method that forwards an `x509` method or its certificate
/// `AUTHORITY` upstream. A PVA-to-PVA gateway therefore cannot be
/// transparent for those: it converts the downstream identity into a
/// CA-style assertion (`user` = downstream account).
///
/// This struct makes that conversion *explicit and visible* instead
/// of silently indistinguishable from a native `ca` client: it pins
/// the original downstream `method` and certificate `authority` to
/// the client so audit/diagnostic output records that the upstream
/// `ca` credentials are a gateway assertion, not a first-party `ca`
/// login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertedIdentity {
    /// The auth method the downstream peer actually used
    /// (`"x509"`, `"ca"`, `"anonymous"`, ...). When this differs
    /// from `"ca"` the upstream `ca` credential is an assertion,
    /// not a verbatim forward.
    pub downstream_method: String,
    /// The certificate authority CommonName for an `x509` downstream
    /// method (ACF `AUTHORITY(...)` scope). Empty for non-TLS
    /// downstream methods.
    pub downstream_authority: String,
}

/// Builder for [`PvaClient`].
pub struct PvaClientBuilder {
    timeout: Duration,
    server_addr: Option<SocketAddr>,
    user: Option<String>,
    host: Option<String>,
    /// Default monitor pipeline window. `0` (the default) means the
    /// default monitor is **non-pipelined**, matching pvxs
    /// `MonitorBuilder` (`clientmon.cpp:50` — `pipeline=false`): the INIT
    /// is a plain `0x08` with no credit trailer and the server free-flows
    /// (squashing on overrun). Pipelining is opt-in, either via
    /// [`PvaClientBuilder::pipeline_size`] or a pvRequest carrying
    /// `record._options.pipeline` (honored by
    /// `MonitorFlow::from_record_options`). A non-zero value here turns
    /// every default monitor into a credit-windowed pipelined subscription.
    pipeline_size: u32,
    tls: Option<Arc<crate::auth::TlsClientConfig>>,
    name_servers: Vec<SocketAddr>,
    /// Operation priority hint, propagated to TCP `IPTOS_PREC_*` bits
    /// where the OS supports it. pvxs `CommonBuilder::priority(int)`
    /// (client.h:692) — 0..7, default 0 (BEST_EFFORT).
    priority: u8,
    /// TCP idle timeout for client-side connections. After this long
    /// without traffic the client closes the virtual circuit. pvxs
    /// `Config::tcpTimeout = 40s` (client.h:1040).
    tcp_timeout: Duration,
    /// Share a single SearchEngine across all `PvaClient` instances
    /// in this process. pvxs `Config::overrideShareUDP(true)`. Avoids
    /// holding multiple UDP search sockets when the user wires up
    /// per-purpose Contexts.
    share_udp: bool,
    /// set by a gateway when this client's `user`/`host`
    /// credentials are an assertion derived from a downstream peer
    /// whose auth method (e.g. `x509`) cannot be forwarded verbatim
    /// on the pvAccess wire. `None` for first-party clients.
    asserted_identity: Option<AssertedIdentity>,
    /// optional opt-in cap on a single inbound message's
    /// payload length. `None` (the default) is **unbounded**, matching
    /// pvxs, which keeps no client-side RX message-size limit. `Some(n)`
    /// drops any connection whose server announces a payload over `n`.
    max_message_size: Option<usize>,
    /// Per-client UDP SEARCH config (address list / auto-addr-list /
    /// broadcast & server ports). Defaults to the `EPICS_PVA_*`
    /// environment (pvxs `Config::fromEnv`); a caller can override any
    /// field via [`PvaClientBuilder::addr_list`] et al. pva2pva treats
    /// these as UDP SEARCH targets on the broadcast port, distinct from
    /// the TCP [`Self::name_servers`].
    search_config: ClientSearchConfig,
    /// True once any UDP-search knob was overridden programmatically.
    /// Forces a per-client engine even under `share_udp(true)`, since
    /// the process-wide engine reads the environment and cannot carry a
    /// caller's per-client address list.
    search_config_overridden: bool,
}

impl PvaClientBuilder {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            server_addr: None,
            user: None,
            host: None,
            // pvxs default monitor is non-pipelined (clientmon.cpp:50
            // `pipeline=false`); pipelining is opt-in via the builder or
            // a pvRequest `record._options.pipeline`. See the field doc.
            pipeline_size: 0,
            tls: None,
            name_servers: crate::config::env::name_servers(),
            priority: 0,
            // pvxs config.cpp:222,373-391: parse_timeout scales CONN_TMO by
            // 4/3; enforceTimeout clamps below 2 s and defaults to 40 s.
            tcp_timeout: super::server_conn::heartbeat_timeout(),
            share_udp: false,
            asserted_identity: None,
            max_message_size: None,
            search_config: ClientSearchConfig::from_env(),
            search_config_overridden: false,
        }
    }

    /// Mirrors pvxs `CommonBuilder::priority(int)` — propagates to
    /// the TCP TOS / DSCP byte where the OS supports it. Range 0..7;
    /// values outside the range are clamped.
    pub fn priority(mut self, p: u8) -> Self {
        self.priority = p.min(7);
        self
    }

    /// Client-side TCP idle timeout. Mirrors pvxs `Config::tcpTimeout`.
    /// Default 40s.
    pub fn tcp_timeout(mut self, d: Duration) -> Self {
        self.tcp_timeout = d;
        self
    }

    /// opt in to a hard cap on a single inbound message's
    /// payload length. By default the client is **unbounded** (pvxs
    /// parity — pvxs keeps no client-side RX message-size limit), since
    /// the streaming reader stays bounded by incremental 4 KiB reads
    /// plus the heartbeat/operation deadlines. Call this only for a
    /// hardened client that should reject (and drop) any server header
    /// announcing more than `cap` bytes.
    pub fn max_message_size(mut self, cap: usize) -> Self {
        self.max_message_size = Some(cap);
        self
    }

    /// Share a single process-wide [`SearchEngine`] across every
    /// `PvaClient` in this process. Mirrors pvxs
    /// `Config::overrideShareUDP(true)`. Saves one UDP socket per
    /// client when a single process opens multiple Contexts (e.g.,
    /// observability + control planes coexisting).
    pub fn share_udp(mut self, share: bool) -> Self {
        self.share_udp = share;
        self
    }

    /// Configure TCP name servers — pvxs `EPICS_PVA_NAME_SERVERS`
    /// equivalent. The client maintains a persistent TCP connection to each
    /// entry and sends SEARCH frames over it; SEARCH_RESPONSE can redirect
    /// to a different server (gateway redirect case) or to the NS itself
    /// (gateway self-serve case). Replaces any list parsed from env at
    /// `new()` time.
    pub fn name_servers(mut self, servers: Vec<SocketAddr>) -> Self {
        self.name_servers = servers;
        self
    }

    /// Per-client UDP SEARCH destinations — pvxs `Config::addressList`
    /// ("addresses to which search requests will be sent", client.h:1011-1017).
    /// A client's address list is the set of targets that SEARCH datagrams are
    /// sent to on the broadcast port, NOT TCP name servers (those are a
    /// separate `Config::nameServers`, client.h:1024-1027 → [`Self::name_servers`]).
    /// Each [`crate::config::Endpoint`] keeps any `@iface` multicast modifier
    /// so a `224.0.2.3@eth0` group is joined on the right interface. Replaces
    /// the list parsed from `EPICS_PVA_ADDR_LIST` at `new()` and pins this
    /// client to its own search engine even under [`Self::share_udp`].
    pub fn addr_list(mut self, addr_list: Vec<crate::config::Endpoint>) -> Self {
        self.search_config.addr_list = addr_list;
        self.search_config_overridden = true;
        self
    }

    /// Toggle auto-address-list expansion — pvxs `Config::autoAddrList`
    /// (client.h:1035-1036, "extend the addressList with local interface
    /// broadcast addresses"). When true (the default from
    /// `EPICS_PVA_AUTO_ADDR_LIST`), per-NIC directed broadcasts are added to
    /// the SEARCH targets. Pins this client to its own search engine even
    /// under [`Self::share_udp`].
    pub fn auto_addr_list(mut self, enabled: bool) -> Self {
        self.search_config.auto_addr_list = enabled;
        self.search_config_overridden = true;
        self
    }

    /// UDP SEARCH / beacon port — pvxs `Config::udp_port` (client.h:1029-1030,
    /// `EPICS_PVA_BROADCAST_PORT`, default 5076). The port SEARCH datagrams are
    /// sent to and beacons received on. Pins this client to its own search
    /// engine even under [`Self::share_udp`].
    pub fn broadcast_port(mut self, port: u16) -> Self {
        self.search_config.broadcast_port = port;
        self.search_config_overridden = true;
        self
    }

    /// Default server TCP port — pvxs `Config::tcp_port` (client.h:1031-1033,
    /// `EPICS_PVA_SERVER_PORT`, default 5075). The connect port for an
    /// advertised server endpoint that omits a port. Pins this client to its
    /// own search engine even under [`Self::share_udp`].
    pub fn server_port(mut self, port: u16) -> Self {
        self.search_config.server_port = port;
        self.search_config_overridden = true;
        self
    }

    /// Enable TLS for every connection. Pass an `Arc<TlsClientConfig>`
    /// from `crate::auth::tls::load_client_config()` (or built from
    /// scratch via `rustls`).
    pub fn with_tls(mut self, tls: Arc<crate::auth::TlsClientConfig>) -> Self {
        self.tls = Some(tls);
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn server_addr(mut self, addr: SocketAddr) -> Self {
        self.server_addr = Some(addr);
        self
    }

    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// declare that this client's `user`/`host` credentials are
    /// a gateway assertion derived from a downstream peer that
    /// authenticated with `downstream_method`. When the downstream
    /// method is not `"ca"` the upstream `ca` credential cannot be a
    /// verbatim forward — the pvAccess wire has no method for `x509`
    /// or its certificate `AUTHORITY` (pvxs `clientconn.cpp:217-305`).
    /// The recorded identity is surfaced through
    /// [`PvaClient::asserted_identity`] for audit/diagnostic output so
    /// the conversion is explicit, not silently indistinguishable from
    /// a first-party `ca` login.
    pub fn asserted_identity(mut self, id: AssertedIdentity) -> Self {
        self.asserted_identity = Some(id);
        self
    }

    /// Opt into pipelined (credit-windowed) monitors with window `n`
    /// (one ACK per `n/2` events). Default `0` = non-pipelined, matching
    /// pvxs; a pvRequest `record._options.pipeline` opts in per-request.
    pub fn pipeline_size(mut self, n: u32) -> Self {
        self.pipeline_size = n;
        self
    }

    pub fn build(self) -> PvaClient {
        let pool = ConnectionPool::new();
        if self.tls.is_some() {
            pool.set_tls(self.tls.clone());
        }
        // thread the opt-in cap into the pool so every dialed
        // connection enforces it (default `None` = unbounded).
        pool.set_max_message_size(self.max_message_size);
        PvaClient {
            inner: Arc::new(ClientInner {
                timeout: self.timeout,
                server_addr: self.server_addr,
                user: self
                    .user
                    .unwrap_or_else(super::super::auth::authnz_default_user),
                host: self
                    .host
                    .unwrap_or_else(super::super::auth::authnz_default_host),
                pipeline_size: self.pipeline_size,
                pool,
                channels: RwLock::new(HashMap::new()),
                search: OnceLock::new(),
                cache_cleaner: std::sync::Once::new(),
                name_servers: self.name_servers,
                priority: self.priority,
                tcp_timeout: self.tcp_timeout,
                share_udp: self.share_udp,
                asserted_identity: self.asserted_identity,
                max_message_size: self.max_message_size,
                search_config: self.search_config,
                search_config_overridden: self.search_config_overridden,
            }),
        }
    }
}

impl Default for PvaClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

struct ClientInner {
    timeout: Duration,
    server_addr: Option<SocketAddr>,
    user: String,
    host: String,
    pipeline_size: u32,
    pool: Arc<ConnectionPool>,
    channels: RwLock<HashMap<String, Arc<Channel>>>,
    /// Lazy: only spawn the search engine when we actually need to resolve.
    search: OnceLock<SearchEngine>,
    /// Spawns the periodic [`cache_clean_loop`] exactly once, on the first
    /// channel cached. Deferred to the first cache insert (rather than
    /// `build()`) so the sync builder never requires a Tokio runtime, mirroring
    /// the lazy `search` engine spawn above; there is nothing to clean until a
    /// channel exists.
    cache_cleaner: std::sync::Once,
    /// TCP name servers (EPICS_PVA_NAME_SERVERS). Passed into SearchEngine
    /// as persistent search peers; also reported by ClientReport::name_servers.
    name_servers: Vec<SocketAddr>,
    /// Operation priority hint (0..7). Stored for inspection /
    /// future TCP TOS wiring. pvxs `CommonBuilder::priority`.
    #[allow(dead_code)]
    priority: u8,
    /// Client TCP idle timeout threaded through to every `ServerConn`
    /// spawned via this client's `ConnectionPool`. Governs the heartbeat
    /// task's inactivity threshold. pvxs `Config::tcpTimeout`
    /// (clientconn.cpp:71-72).
    tcp_timeout: Duration,
    /// True when `build()` was told to share the process-wide search
    /// engine. Routes [`PvaClient::search_engine`] through the static
    /// `SHARED_SEARCH_ENGINE` instead of spawning per-client.
    share_udp: bool,
    /// present when this client's credentials are a gateway
    /// assertion of a downstream identity. Surfaced through
    /// [`PvaClient::asserted_identity`].
    asserted_identity: Option<AssertedIdentity>,
    /// opt-in inbound message-size cap (`None` = unbounded).
    /// Stored so `with_asserted_identity` can carry it onto the derived
    /// client's fresh pool. Set on the pool at `build()` time.
    max_message_size: Option<usize>,
    /// Per-client UDP SEARCH config. Passed to
    /// [`SearchEngine::spawn_with_config`] when a per-client engine is
    /// spawned (pvxs `Config` UDP-discovery fields).
    search_config: ClientSearchConfig,
    /// True when a UDP-search knob was overridden programmatically, so the
    /// shared engine (which reads the environment) must not be used.
    search_config_overridden: bool,
}

/// Process-wide singleton SearchEngine for `share_udp(true)` clients.
/// Lazily initialized on first use.
static SHARED_SEARCH_ENGINE: tokio::sync::OnceCell<SearchEngine> =
    tokio::sync::OnceCell::const_new();

/// Interval between automatic channel-cache sweeps. pvxs enables a
/// per-`ContextImpl` `cacheCleaner` timer at `channelCacheCleanInterval{10,0}`
/// (src/client.cpp:57, 666) that runs `cacheClean("", Context::Clean)` over the
/// whole channel map so a long-lived client probing many PV names does not
/// retain every idle channel until the context is closed.
const CHANNEL_CACHE_CLEAN_INTERVAL: Duration = Duration::from_secs(10);

/// Apply a [`CacheAction`] to the channel map, returning the channels that were
/// removed so the caller can close `Disconnect`ed ones after releasing the map
/// lock. An empty `pv_name` is the pvxs wildcard over every cached name
/// (src/client.cpp:1341-1348). `Clean` keeps channels that still have a live
/// external reference (`Arc::strong_count > 1`, the analog of pvxs
/// `use_count > 1`); `Drop`/`Disconnect` remove unconditionally
/// (src/client.cpp:1350-1366).
fn apply_cache_action(
    chans: &mut HashMap<String, Arc<Channel>>,
    pv_name: &str,
    action: CacheAction,
) -> Vec<Arc<Channel>> {
    let names: Vec<String> = if pv_name.is_empty() {
        chans.keys().cloned().collect()
    } else {
        vec![pv_name.to_string()]
    };
    let mut removed = Vec::new();
    for name in names {
        if action == CacheAction::Clean {
            if let Some(c) = chans.get(&name) {
                // The map's own `Arc` is the single expected reference; any
                // extra clone means an in-use channel that `Clean` preserves.
                if Arc::strong_count(c) > 1 {
                    continue;
                }
            }
        }
        if let Some(ch) = chans.remove(&name) {
            removed.push(ch);
        }
    }
    removed
}

/// Cache-maintenance action for [`PvaClient::cache_clear_action`]. Mirrors
/// pvxs `Context::cacheAction` (client.h:576-591), which distinguishes three
/// behaviors that a single string-only `cache_clear` could not express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheAction {
    /// Remove only channels that are no longer in use (no live external
    /// `Arc<Channel>` beyond the cache's own, i.e. `use_count <= 1`), leaving
    /// in-use channels connected and reusable. pvxs default (src/client.cpp:1350-1357).
    #[default]
    Clean,
    /// Remove channels unconditionally so they will not be reused, but leave
    /// any in-progress operations running on the detached channel
    /// (src/client.cpp:1358-1366).
    Drop,
    /// Like [`CacheAction::Drop`], and additionally close each removed channel
    /// so its operation waiters, connect watchers, and monitor loops observe a
    /// disconnect transition — the analog of pvxs `trash->disconnect(trash)`
    /// (src/client.cpp:1367-1369).
    Disconnect,
}

/// Background channel-cache GC. Mirrors the pvxs `cacheCleaner` timer callback
/// `cacheClean("", Context::Clean)` (src/client.cpp:666, 1339-1383): every `period`
/// it removes cached channels whose only owner is the cache itself
/// (`Arc::strong_count == 1`, the analog of pvxs `use_count <= 1`), leaving
/// in-use channels connected and reusable.
///
/// Lifetime: the task holds a `Weak<ClientInner>`, so it never keeps the
/// context alive. It exits when the last [`PvaClient`] clone is dropped
/// (`upgrade()` fails) or after [`PvaClient::close`] sets the `ConnectionPool`
/// shutdown gate. Unlike pvxs, which removes the timer synchronously in
/// `ContextImpl::close` (src/client.cpp:693-725), the Rust task exits on its next
/// tick — the same idle-until-observed divergence already documented for the
/// search-engine timer in [`PvaClient::close`]; it does no work in the interim
/// (the shutdown gate short-circuits the sweep and `close()` already drained
/// the map).
///
/// It never spawns the search engine: pvxs `cacheClean` touches only the
/// channel map, so a direct-server client that never resolves a name keeps no
/// engine. A `Clean`-removed channel has `strong_count == 1`, which means no
/// in-flight op holds it and therefore no pending search references it (pending
/// searches are held only by an awaiting caller's `oneshot` receiver), so
/// dropping it here cannot orphan engine state.
async fn cache_clean_loop(inner: std::sync::Weak<ClientInner>, period: Duration) {
    // Through the seam so this loop runs on the RTEMS callback band as well as
    // the tokio runtime (stage 3). The seam ticker is `MissedTickBehavior::
    // Burst`, not `Delay`; for a periodic cache sweeper the two differ only in
    // how a *missed* deadline is made up (Burst fires the backlog immediately,
    // Delay spreads it) and the work is idempotent — an extra sweep removes
    // nothing a later one would have — so the distinction is immaterial here.
    let mut tick = epics_base_rs::runtime::task::interval(period);
    // pvxs arms the timer with `event_add(cacheCleaner, &channelCacheCleanInterval)`
    // (src/client.cpp:666), i.e. the first sweep fires after one interval, not at
    // startup. Consume the immediate first tick so the first real sweep is one
    // `period` away.
    tick.tick().await;
    loop {
        tick.tick().await;
        let Some(inner) = inner.upgrade() else {
            return; // last PvaClient clone dropped — nothing left to clean
        };
        if inner.pool.is_shutdown() {
            return; // close()d — terminal teardown already drained the cache
        }
        // Clean removes only channels whose sole reference is the map's own
        // Arc; the returned channels are dropped here. Clean never closes
        // in-use channels (src/client.cpp:1350-1357), so there is nothing to
        // close after releasing the lock.
        let _removed = {
            let mut chans = inner.channels.write();
            apply_cache_action(&mut chans, "", CacheAction::Clean)
        };
    }
}

#[derive(Clone)]
pub struct PvaClient {
    inner: Arc<ClientInner>,
}

impl std::fmt::Debug for PvaClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PvaClient")
            .field("timeout", &self.inner.timeout)
            .field("user", &self.inner.user)
            .field("host", &self.inner.host)
            .finish()
    }
}

impl PvaClient {
    pub fn builder() -> PvaClientBuilder {
        PvaClientBuilder::new()
    }

    pub fn new() -> PvaResult<Self> {
        Ok(Self::builder().build())
    }

    /// the gateway-asserted downstream identity behind this
    /// client's credentials, if any. `None` for a first-party client.
    /// `Some` means the client's `ca` `user`/`host` are an assertion
    /// the gateway made on behalf of a downstream peer whose auth
    /// method could not be forwarded verbatim on the pvAccess wire.
    pub fn asserted_identity(&self) -> Option<&AssertedIdentity> {
        self.inner.asserted_identity.as_ref()
    }

    /// Derive a new client that reaches the **same upstream server over
    /// the same transport** as `self`, but presents a different
    /// (gateway-asserted) identity.
    ///
    /// A PVA gateway keeps one upstream client per
    /// distinct downstream credential so the upstream IOC's access
    /// security sees the real identity. Every such client must still
    /// resolve the *same* upstream — only `user` / `host` /
    /// `asserted_identity` change. Building a fresh
    /// `PvaClient::builder()` instead would drop the gateway's
    /// `server_addr` (and timeout / TLS / name-server config), so the
    /// derived client would fall back to UDP search and never reach a
    /// pinned or discovery-isolated upstream. This carries every
    /// connection-config field across.
    pub fn with_asserted_identity(
        &self,
        user: String,
        host: String,
        asserted: AssertedIdentity,
    ) -> PvaClient {
        let mut builder = PvaClientBuilder::new()
            .timeout(self.inner.timeout)
            .user(user)
            .host(host)
            .pipeline_size(self.inner.pipeline_size)
            .priority(self.inner.priority)
            .tcp_timeout(self.inner.tcp_timeout)
            .share_udp(self.inner.share_udp)
            .name_servers(self.inner.name_servers.clone())
            .asserted_identity(asserted);
        if let Some(cap) = self.inner.max_message_size {
            builder = builder.max_message_size(cap);
        }
        if let Some(addr) = self.inner.server_addr {
            builder = builder.server_addr(addr);
        }
        if let Some(tls) = self.inner.pool.tls() {
            builder = builder.with_tls(tls);
        }
        builder.build()
    }

    /// Backwards-compatible: targets a specific TCP port (UDP ignored —
    /// search uses the standard port machinery).
    pub fn with_ports(_udp_port: u16, tcp_port: u16) -> Self {
        let server_addr = SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            tcp_port,
        );
        Self::builder().server_addr(server_addr).build()
    }

    /// True when [`Self::search_engine`] resolves to the process-wide
    /// `SHARED_SEARCH_ENGINE` instead of spawning a per-client engine.
    ///
    /// The shared engine is a single `OnceCell` and cannot carry per-client
    /// name_servers (different clients may have different lists) NOR a
    /// per-client UDP-search config (address list / ports / auto-addr-list).
    /// pvxs keeps `nameServers` per-Context regardless of `overrideShareUDP`
    /// — shareUDP only shares the UDP socket. To match that and avoid
    /// silently dropping configured TCP name servers or a programmatic
    /// addr_list, a client that has name servers OR an overridden search
    /// config always uses its own per-client engine, even when
    /// `share_udp(true)` was requested. share_udp still saves the UDP
    /// socket for clients that have neither.
    fn uses_shared_search_engine(&self) -> bool {
        self.inner.share_udp
            && self.inner.name_servers.is_empty()
            && !self.inner.search_config_overridden
    }

    async fn search_engine(&self) -> PvaResult<&SearchEngine> {
        // The second owner-creation point on the client path, and for the same
        // reason as `channel_with_forced`: the engine's ring, its UDP receive
        // loop and its name-server connections are reactor-bound tasks that
        // outlive this call, and the builder that produced the client is a
        // plain `fn` with no executor to take the capability from.
        let reactor = epics_base_rs::runtime::task::Reactor::current()
            .expect("the search engine is built on the client's executor");
        if self.uses_shared_search_engine() {
            let engine = SHARED_SEARCH_ENGINE
                .get_or_try_init(|| async {
                    SearchEngine::spawn(&reactor, Vec::new(), Vec::new()).await
                })
                .await?;
            return Ok(engine);
        }
        if self.inner.search.get().is_none() {
            // Per-client engine carries the CA credentials so TCP name-server
            // handshakes authenticate as this client's user/host (pvxs has no
            // name-server auth exception, clientconn.cpp:215-263) and the
            // per-client UDP-search config so a caller's `addr_list` / ports
            // drive the SEARCH path instead of the process environment. The
            // shared engine above never has either, so it needs neither.
            let engine = SearchEngine::spawn_with_config(
                &reactor,
                self.inner.search_config.clone(),
                Vec::new(),
                self.inner.name_servers.clone(),
                self.inner.user.clone(),
                self.inner.host.clone(),
                self.inner.tcp_timeout,
            )
            .await?;
            let _ = self.inner.search.set(engine);
        }
        Ok(self.inner.search.get().unwrap())
    }

    async fn channel(&self, pv_name: &str) -> PvaResult<Arc<Channel>> {
        self.channel_with_forced(pv_name, None).await
    }

    async fn channel_with_forced(
        &self,
        pv_name: &str,
        forced: Option<SocketAddr>,
    ) -> PvaResult<Arc<Channel>> {
        // pvxs adab53e (2025-10): reject empty PV names at the
        // builder boundary instead of letting them flow through
        // SEARCH and surface as a confusing late-stage timeout.
        if pv_name.is_empty() {
            return Err(PvaError::InvalidValue("empty channel name".into()));
        }
        // Closed-context gate. pvxs `Channel::build()` refuses to
        // construct a channel once the context has left `Running`,
        // throwing "Context close()d" (src/client.cpp:349-352). `close()`
        // sets the single owner of that state — the per-client
        // `ConnectionPool` shutdown flag (via `pool.clear()`); every
        // channel factory routes through here, so this one gate makes
        // post-close GET/PUT/MONITOR/CONNECT all fail rather than
        // silently re-resolving and opening fresh sockets.
        if self.inner.pool.is_shutdown() {
            return Err(PvaError::Protocol("context closed".into()));
        }
        // The one mint on the client path. `PvaClientBuilder::build` is a
        // plain `fn` that may run before any executor exists, so the client
        // cannot hold the capability; every channel is built from here,
        // inside `async`, and carries it from then on.
        let reactor = epics_base_rs::runtime::task::Reactor::current()
            .expect("channel construction is awaited on the client's executor");
        // Forced-server channels skip the cache entirely — pinning is a
        // per-call request, not a global property of the PV name.
        if forced.is_none() {
            if let Some(c) = self.inner.channels.read().get(pv_name).cloned() {
                return Ok(c);
            }
        }

        let direct = forced.or(self.inner.server_addr);
        let ch = if let Some(addr) = direct {
            // Direct-server mode: no UDP search at all. Channel will go
            // straight to Connecting → Active using `addr`. Used for
            // both PvaClient-wide `server_addr` and per-channel
            // `forced_server` overrides (pvxs ConnectBuilder::server).
            Arc::new(Channel::new_direct(
                reactor.clone(),
                pv_name.to_string(),
                self.inner.user.clone(),
                self.inner.host.clone(),
                self.inner.timeout,
                self.inner.tcp_timeout,
                self.inner.pool.clone(),
                addr,
            ))
        } else {
            let search = self.search_engine().await?.clone();
            Arc::new(Channel::new(
                reactor.clone(),
                pv_name.to_string(),
                self.inner.user.clone(),
                self.inner.host.clone(),
                self.inner.timeout,
                self.inner.tcp_timeout,
                self.inner.pool.clone(),
                search,
            ))
        };

        if forced.is_some() {
            return Ok(ch);
        }

        // First channel about to enter the cache: arm the periodic cache
        // cleaner (pvxs starts `cacheCleaner` at context construction,
        // src/client.cpp:666). Deferred to here so the cleaner holds a
        // `Weak<ClientInner>` and is spawned inside the Tokio runtime that
        // every channel op already runs in; `Once` guarantees a single task.
        self.inner.cache_cleaner.call_once(|| {
            reactor.spawn(cache_clean_loop(
                Arc::downgrade(&self.inner),
                CHANNEL_CACHE_CLEAN_INTERVAL,
            ));
        });

        let mut map = self.inner.channels.write();
        if let Some(existing) = map.get(pv_name).cloned() {
            return Ok(existing);
        }
        map.insert(pv_name.to_string(), ch.clone());
        Ok(ch)
    }

    /// Resolve `pv_name` against a specific server, bypassing UDP
    /// search and any cached search results. Mirrors pvxs
    /// `ConnectBuilder::server` (src/client.cpp:208) — the returned future
    /// performs a one-shot operation against the pinned server. Useful
    /// when a gateway or testing harness wants to direct an op to a
    /// known endpoint without affecting the cache for that PV name.
    pub async fn pvget_from(&self, pv_name: &str, server: SocketAddr) -> PvaResult<PvField> {
        let ch = self.channel_with_forced(pv_name, Some(server)).await?;
        let (_, v) = op_get(&ch, &[], self.inner.timeout).await?;
        Ok(v)
    }

    /// Same as [`Self::pvput`] but pins the operation to `server`.
    pub async fn pvput_to(
        &self,
        pv_name: &str,
        server: SocketAddr,
        value_str: &str,
    ) -> PvaResult<()> {
        let ch = self.channel_with_forced(pv_name, Some(server)).await?;
        op_put(&ch, value_str, self.inner.timeout).await
    }

    pub async fn pvget(&self, pv_name: &str) -> PvaResult<PvField> {
        let ch = self.channel(pv_name).await?;
        let (_, v) = op_get(&ch, &[], self.inner.timeout).await?;
        Ok(v)
    }

    /// [`Self::pvget`] keeping the reply's marked leaves — the GET a PVA
    /// gateway forwards, which must re-frame the readback downstream with
    /// the leaves the UPSTREAM assigned rather than a synthesised full mask
    /// (the decoder zero-fills the unmarked ones). See
    /// [`crate::client_native::ops_v2::MarkedRead`].
    pub async fn pvget_marked(
        &self,
        pv_name: &str,
    ) -> PvaResult<crate::client_native::ops_v2::MarkedRead> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_get_marked(&ch, &[], self.inner.timeout).await
    }

    /// [`Self::pvget_pv_field_with_request_value`] keeping the reply's marked
    /// leaves. See [`Self::pvget_marked`].
    pub async fn pvget_pv_field_with_request_value_marked(
        &self,
        pv_name: &str,
        pv_request: &crate::pvdata::PvField,
    ) -> PvaResult<crate::client_native::ops_v2::MarkedRead> {
        let ch = self.channel(pv_name).await?;
        let bytes = self.encode_pv_request(&ch, pv_request).await?;
        crate::client_native::ops_v2::op_get_raw_marked(&ch, &bytes, self.inner.timeout).await
    }

    /// Serialize a decoded pvRequest in the channel connection's negotiated
    /// byte order — the INIT-time encoding every request-carrying op shares.
    async fn encode_pv_request(
        &self,
        ch: &std::sync::Arc<crate::client_native::channel::Channel>,
        pv_request: &crate::pvdata::PvField,
    ) -> PvaResult<Vec<u8>> {
        let order =
            crate::client_native::ops_v2::ensure_active_with_op_timeout(ch, self.inner.timeout)
                .await?
                .0
                .byte_order();
        let mut bytes = Vec::new();
        let desc = pv_request.descriptor();
        crate::pvdata::encode::encode_type_desc(&desc, order, &mut bytes);
        crate::pvdata::encode::encode_pv_field(pv_request, &desc, order, &mut bytes);
        Ok(bytes)
    }

    /// GET carrying the caller's decoded pvRequest (e.g. a PVA gateway's
    /// preserved `ChannelContext.pv_request`) rather than the default
    /// value-only request, returning `(introspection, value)`. The
    /// GET-side counterpart of [`Self::pvput_pv_field_with_request_value`]:
    /// the pvRequest — carrying field selection and provider create-time
    /// options such as `record._options.atomic` — is serialized in the
    /// connection's negotiated byte order and sent at GET INIT.
    ///
    /// pva2pva forwards the exact downstream pvRequest into
    /// `createChannelGet(..., pvRequest)` (`p2pApp/channel.cpp:109-115`), so
    /// upstream providers that interpret GET pvRequest options (e.g. pvxs
    /// QSRV group GET reading `record._options.atomic` from
    /// `getOperation->pvRequest()`, `groupsource.cpp:479-481`) see the
    /// downstream request; the default [`Self::pvget`] would drop it.
    pub async fn pvget_pv_field_with_request_value(
        &self,
        pv_name: &str,
        pv_request: &crate::pvdata::PvField,
    ) -> PvaResult<(FieldDesc, PvField)> {
        let ch = self.channel(pv_name).await?;
        let bytes = self.encode_pv_request(&ch, pv_request).await?;
        crate::client_native::ops_v2::op_get_raw(&ch, &bytes, self.inner.timeout).await
    }

    /// I-3: explicit "connect, then return" — wait for the named
    /// channel to reach `Active` state without issuing a GET/PUT/
    /// MONITOR. Mirrors pvxs `Context::connect(pvname)`. Useful
    /// when an application wants to validate that a PV resolves
    /// before kicking off real ops, or to pre-warm the connection
    /// pool. Returns the resolved server address.
    pub async fn pvconnect(&self, pv_name: &str) -> PvaResult<SocketAddr> {
        let ch = self.channel(pv_name).await?;
        // pvconnect is a one-shot user op (pvxs
        // `Context::connect(name)` waits up to the caller's timeout),
        // so bound the resolve through the single op-timeout owner —
        // never bare `ensure_active`, which would hang forever against
        // a never-existed PV now that the 200 ms inner cap is gone.
        let (server, _sid) =
            crate::client_native::ops_v2::ensure_active_with_op_timeout(&ch, self.inner.timeout)
                .await?;
        Ok(server.addr)
    }

    /// I-3: same as [`Self::pvconnect`] but pinned to a specific
    /// upstream server (skips UDP search). Mirrors pvxs
    /// `ConnectBuilder::server(addr).exec()`.
    pub async fn pvconnect_from(&self, pv_name: &str, server: SocketAddr) -> PvaResult<SocketAddr> {
        let ch = self.channel_with_forced(pv_name, Some(server)).await?;
        // one-shot user op — bound through the op-timeout
        // owner (see `pvconnect`); a pinned-but-dead server must fail at
        // `op_timeout`, not hang.
        let (sc, _sid) =
            crate::client_native::ops_v2::ensure_active_with_op_timeout(&ch, self.inner.timeout)
                .await?;
        Ok(sc.addr)
    }

    /// Typed `pvget` — returns the value already decoded into a Rust
    /// type that implements [`crate::nt::TypedNT`]. Most users get
    /// `T` from `#[derive(NTScalar)]` on their own struct.
    ///
    /// ```ignore
    /// // built-in types work without a derive
    /// let temp: f64 = client.pvget_typed("OVEN:TEMP").await?;
    ///
    /// // user-defined NT shape
    /// #[derive(NTScalar)]
    /// struct MotorPos {
    ///     value: f64,
    ///     #[nt(meta)] alarm: Alarm,
    ///     #[nt(meta)] timestamp: TimeStamp,
    /// }
    /// let pos: MotorPos = client.pvget_typed("MOTOR:VAL").await?;
    /// ```
    pub async fn pvget_typed<T: crate::nt::TypedNT>(&self, pv_name: &str) -> PvaResult<T> {
        let ch = self.channel(pv_name).await?;
        let (_, v) = op_get(&ch, &[], self.inner.timeout).await?;
        T::from_pv_field(&v).map_err(|e| crate::error::PvaError::InvalidValue(e.to_string()))
    }

    /// Typed `pvput` — encodes the value via [`crate::nt::TypedNT`]
    /// and writes it to the target PV. Skips the string-form
    /// round-trip entirely; the typed shape is sent as-is, so a
    /// `f64` ends up on the wire as 8 bytes regardless of locale or
    /// Display formatting.
    pub async fn pvput_typed<T: crate::nt::TypedNT>(
        &self,
        pv_name: &str,
        value: &T,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        let pv_field = value.to_pv_field();
        crate::client_native::ops_v2::op_put_value(&ch, &pv_field, self.inner.timeout).await
    }

    /// Like [`Self::pvput`] but takes a pre-built [`PvField`]. Skips
    /// the string-form round-trip — the value travels as-is on the
    /// wire, so a 1 M-element `ScalarArray<Double>` is one 8 MB
    /// memcpy rather than ~25 MB of `Display` allocations + 25 MB of
    /// pvput's parse-back. Used by pvalink OUT links where the
    /// EpicsValue → PvField shape is already known.
    pub async fn pvput_pv_field(
        &self,
        pv_name: &str,
        value: &crate::pvdata::PvField,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_put_value(&ch, value, self.inner.timeout).await
    }

    /// Typed `pvmonitor` — every wire event is decoded into `T`
    /// before the callback fires. Decode failures surface as
    /// [`crate::error::PvaError::InvalidValue`] and end the monitor.
    pub async fn pvmonitor_typed<T, F>(&self, pv_name: &str, mut callback: F) -> PvaResult<()>
    where
        T: crate::nt::TypedNT,
        F: FnMut(T) + Send,
    {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_monitor(
            &ch,
            &[],
            self.inner.pipeline_size,
            move |_desc, value| {
                if let Ok(typed) = T::from_pv_field(value) {
                    callback(typed);
                }
            },
        )
        .await
    }

    /// Start a GET and return a [`PvaOperation`](crate::client_native::PvaOperation) handle the caller can
    /// `wait()`, `cancel()`, or `interrupt()` from any task.
    /// pvxs `Operation` parity for callers that need to start now and
    /// wait later from a different context, or be able to cancel from
    /// outside the awaiting task.
    ///
    /// Drop semantics: dropping the handle aborts the spawned op.
    /// Wrap in `Arc<Mutex<>>` (or share via a channel) if you need to
    /// keep the op alive past the handle's local scope.
    pub fn start_get(
        &self,
        reactor: &epics_base_rs::runtime::task::Reactor,
        pv_name: &str,
    ) -> crate::client_native::operation::PvaOperation<PvField> {
        let client = self.clone();
        let name = pv_name.to_string();
        crate::client_native::operation::PvaOperation::spawn(reactor, async move {
            client.pvget(&name).await
        })
    }

    /// Start a PUT and return a [`PvaOperation`](crate::client_native::PvaOperation) handle.
    pub fn start_put(
        &self,
        reactor: &epics_base_rs::runtime::task::Reactor,
        pv_name: &str,
        value_str: &str,
    ) -> crate::client_native::operation::PvaOperation<()> {
        let client = self.clone();
        let name = pv_name.to_string();
        let val = value_str.to_string();
        crate::client_native::operation::PvaOperation::spawn(reactor, async move {
            client.pvput(&name, &val).await
        })
    }

    /// Start an RPC and return a [`PvaOperation`](crate::client_native::PvaOperation) handle.
    pub fn start_rpc(
        &self,
        reactor: &epics_base_rs::runtime::task::Reactor,
        pv_name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> crate::client_native::operation::PvaOperation<RpcReply> {
        let client = self.clone();
        let name = pv_name.to_string();
        crate::client_native::operation::PvaOperation::spawn(reactor, async move {
            client.pvrpc(&name, &request_desc, &request_value).await
        })
    }

    /// `pvget` with a custom pvRequest (field selection + record
    /// options). Mirrors pvxs `Context::get(name).pvRequest(expr)`:
    ///
    /// ```ignore
    /// use epics_pva_rs::pv_request::PvRequestBuilder;
    ///
    /// let req = PvRequestBuilder::new()
    ///     .field("value")
    ///     .field("alarm.severity")
    ///     .record("queueSize", "8")
    ///     .build();
    /// let v = client.pvget_with_request("MY:PV", &req).await?;
    /// ```
    pub async fn pvget_with_request(
        &self,
        pv_name: &str,
        request: &crate::pv_request::PvRequestExpr,
    ) -> PvaResult<PvField> {
        Ok(self.pvget_with_request_full(pv_name, request).await?.value)
    }

    /// Like [`Self::pvget_with_request`] but returns the full
    /// [`PvGetResult`] (value **plus** the response introspection and
    /// the answering server) so callers can format the result with the
    /// descriptor the GET actually negotiated — the parity-correct path
    /// for `pvget -r '<pvRequest>'`, where `record[...]` options and
    /// server-side `_filter` chains must reach the server verbatim
    /// rather than being reduced to a field-name list. pvxs
    /// `pvget.cpp:375-380` passes the whole `-r` string to
    /// `createRequest`.
    pub async fn pvget_with_request_full(
        &self,
        pv_name: &str,
        request: &crate::pv_request::PvRequestExpr,
    ) -> PvaResult<PvGetResult> {
        let ch = self.channel(pv_name).await?;
        let big_endian = matches!(
            crate::client_native::ops_v2::ensure_active_with_op_timeout(&ch, self.inner.timeout)
                .await?
                .0
                .byte_order(),
            crate::proto::ByteOrder::Big
        );
        let bytes = request.encode(big_endian);
        let (introspection, value) =
            crate::client_native::ops_v2::op_get_raw(&ch, &bytes, self.inner.timeout).await?;
        let server_addr = match ch.current_state() {
            super::channel::ChannelState::Active { server, .. } => server.addr,
            _ => SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0),
        };
        Ok(PvGetResult {
            pv_name: pv_name.to_string(),
            value,
            introspection,
            server_addr,
        })
    }

    /// Force the search engine into fast-tick mode for one revolution
    /// and reset every pending search's retry deadline. Mirrors pvxs
    /// `Context::hurryUp` (src/client.cpp:430). Useful when the application
    /// has out-of-band evidence that the network state changed (link
    /// bounce, new IOC announced via side channel) and wants pending
    /// searches to retry immediately rather than wait for their
    /// scheduled bucket.
    ///
    /// No-op in direct-server mode (no SearchEngine).
    pub async fn hurry_up(&self) {
        if let Ok(engine) = self.search_engine().await {
            engine.hurry_up().await;
        }
    }

    /// Drop cached state for `pv_name` using the default pvxs `Clean` action:
    /// only channels with no live external reference are removed, in-use
    /// channels are preserved, and the matching pending search (if any) is
    /// cancelled. Mirrors pvxs `Context::cacheClear` (src/client.cpp:441-451),
    /// whose default `cacheAction` is `Clean`.
    ///
    /// Pass an empty `pv_name` to sweep every cached name (the pvxs wildcard).
    /// For `Drop`/`Disconnect` semantics use [`PvaClient::cache_clear_action`].
    pub async fn cache_clear(&self, pv_name: &str) {
        self.cache_clear_action(pv_name, CacheAction::Clean).await;
    }

    /// Cache maintenance with explicit pvxs [`CacheAction`] semantics.
    ///
    /// `pv_name` empty is a wildcard over the whole channel map and pending
    /// search map (pvxs `cacheClean` skips the name filter when `name.empty()`,
    /// src/client.cpp:1341-1348). The channel-map effect is governed by `action`:
    /// `Clean` keeps in-use channels, `Drop` removes unconditionally, and
    /// `Disconnect` additionally closes each removed channel so in-progress
    /// operations observe the disconnect (src/client.cpp:1350-1369). The pending
    /// search for the name(s) is always cancelled regardless of `action`.
    pub async fn cache_clear_action(&self, pv_name: &str, action: CacheAction) {
        // Collect the channels to remove under the map lock, then release it
        // before closing any of them: `Channel::close()` takes the channel's
        // own state lock, and we must not hold the map write-lock across that.
        let closed: Vec<Arc<Channel>> = {
            let mut chans = self.inner.channels.write();
            apply_cache_action(&mut chans, pv_name, action)
        };
        if action == CacheAction::Disconnect {
            // Route the disconnect through the channel owner so operation
            // waiters, connect watchers, and monitor loops observe the same
            // Closed transition as a server-side detach.
            for ch in &closed {
                ch.close();
            }
        }
        if let Ok(engine) = self.search_engine().await {
            engine.cache_clear(pv_name).await;
        }
    }

    /// Send a DISCOVER ping (empty SEARCH) to broadcast targets so
    /// reachable servers reply immediately. Pair with the discovery
    /// stream from the search engine to learn about servers without
    /// waiting for the next beacon. Mirrors pvxs
    /// `Context::discover().pingAll(true)`.
    pub async fn ping_all(&self) -> PvaResult<()> {
        self.search_engine().await?.ping_all().await;
        Ok(())
    }

    /// Replace the server-GUID blocklist used by the search engine.
    /// SEARCH_RESPONSE frames (including discovery pongs) from a listed
    /// GUID are silently dropped; BEACONs from that server are still
    /// reported through `discover()` and still drive reconnect pokes.
    /// Mirrors pvxs `Context::ignoreServerGUIDs` — "Ignore any search
    /// replies with these GUIDs" (client.h:593-595, src/client.cpp:880).
    /// Pass an empty `Vec` to clear the list.
    pub async fn ignore_server_guids(&self, guids: Vec<[u8; 12]>) {
        if let Ok(engine) = self.search_engine().await {
            engine.ignore_server_guids(guids).await;
        }
    }

    /// Subscribe to server-discovery events: every beacon (or active
    /// DISCOVER reply) for a previously-unknown server / restarted
    /// GUID surfaces as a [`crate::client_native::search_engine::Discovered`]
    /// on the returned receiver. Combine with [`Self::ping_all`] to
    /// drive an active scan instead of waiting for the next beacon
    /// cycle. Mirrors pvxs `Context::discover(fn).exec()` minus the
    /// pingAll flag — call ping_all() yourself when you want both.
    /// Multiple concurrent subscribers are supported (each gets its
    /// own receiver). Drop the receiver to unsubscribe.
    pub async fn discover(
        &self,
    ) -> PvaResult<
        tokio::sync::mpsc::UnboundedReceiver<crate::client_native::search_engine::Discovered>,
    > {
        self.search_engine().await?.discover().await
    }

    /// Terminal shutdown: move the context to a Stopped state. Drops the
    /// channel cache and closes pooled connections, and — unlike the
    /// previous behavior — **refuses** all subsequent operations rather
    /// than transparently re-resolving. After `close()`, every
    /// `pvget` / `pvput` / `monitor` / `connect` fails with
    /// `Protocol("context closed")` and no new socket is opened, matching
    /// pvxs `Channel::build()`'s refusal once the context has left
    /// `Running` (src/client.cpp:349-352). `close()` is the single owner of
    /// that Stopped state: it sets the `ConnectionPool` shutdown flag
    /// (via `clear()`), which is enforced at both the channel-factory
    /// boundary (`channel_with_forced`) and the dial boundary
    /// (`ConnectionPool::get_or_connect`).
    ///
    /// The background search-engine task continues to run idle until the
    /// last `PvaClient` clone is dropped — the Stopped gate prevents any
    /// new search from being started, so it has no in-flight work, but
    /// it is not torn down here because the per-client engine lives in a
    /// shared `OnceLock` that a `&self` method cannot drain. pvxs removes
    /// its search/beacon timers in `ContextImpl::close` (src/client.cpp:693-725);
    /// the Rust idle-until-drop timer is a deliberate divergence.
    ///
    /// Mirrors pvxs `Context::close` (src/client.cpp:422). Idempotent.
    pub fn close(&self) {
        // pvxs `ContextImpl::close` (src/client.cpp:693-718) is the single
        // owner of terminal teardown: it moves out the channel and
        // connection maps and runs `Connection::cleanup()` on each
        // connection, which resets the socket and disconnects every channel
        // — existing operations are orphaned, not left running on a hidden
        // circuit. Clearing the maps alone is not enough: a live monitor
        // handle holds an `Arc<Channel>` and its subscription state holds
        // `(Arc<ServerConn>, sid, ioid)`, so dropping the maps leaves the
        // monitor receiving data and the TCP circuit alive until that handle
        // is separately dropped or the peer disconnects.
        //
        // Collect the cached channels under the lock, release it, then
        // `close()` each so the `set_state(Closed)` transition (which
        // touches the connection router) never runs while the channel-map
        // lock is held.
        let channels: Vec<Arc<Channel>> = {
            let mut map = self.inner.channels.write();
            map.drain().map(|(_, ch)| ch).collect()
        };
        for ch in channels {
            ch.close();
        }
        // `pool.clear()` closes every pooled connection (waking active
        // monitor streams via the router drain) before dropping the map and
        // setting the shutdown gate.
        self.inner.pool.clear();
    }

    pub async fn pvget_full(&self, pv_name: &str) -> PvaResult<PvGetResult> {
        let ch = self.channel(pv_name).await?;
        let (intro, value) = op_get(&ch, &[], self.inner.timeout).await?;
        let server_addr = match ch.current_state() {
            super::channel::ChannelState::Active { server, .. } => server.addr,
            _ => SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0),
        };
        Ok(PvGetResult {
            pv_name: pv_name.to_string(),
            value,
            introspection: intro,
            server_addr,
        })
    }

    pub async fn pvget_fields(&self, pv_name: &str, fields: &[&str]) -> PvaResult<PvGetResult> {
        let ch = self.channel(pv_name).await?;
        let (intro, value) = op_get(&ch, fields, self.inner.timeout).await?;
        let server_addr = match ch.current_state() {
            super::channel::ChannelState::Active { server, .. } => server.addr,
            _ => SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0),
        };
        Ok(PvGetResult {
            pv_name: pv_name.to_string(),
            value,
            introspection: intro,
            server_addr,
        })
    }

    pub async fn pvput(&self, pv_name: &str, value_str: &str) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        op_put(&ch, value_str, self.inner.timeout).await
    }

    /// PUT the legacy pvAccessCPP positional bare-token form
    /// (`pvput <PV> <size/ignored> <value> [<value>...]`), optionally
    /// with a custom pvRequest. The token list is carried verbatim and
    /// classified against the PUT prototype the server returns: a
    /// scalar-array `.value` drops the leading compatibility length and
    /// writes the rest, a lone `[...]` token is the JSON-array
    /// shortcut, and a scalar `.value` takes exactly one token. pvxs /
    /// pvAccessCPP `pvtoolsSrc/pvput.cpp:144-178` parity.
    pub async fn pvput_tokens(
        &self,
        pv_name: &str,
        tokens: &[String],
        request: Option<&crate::pv_request::PvRequestExpr>,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        match request {
            None => {
                crate::client_native::ops_v2::op_put_tokens(&ch, tokens, None, self.inner.timeout)
                    .await
            }
            Some(req) => {
                let big_endian = matches!(
                    crate::client_native::ops_v2::ensure_active_with_op_timeout(
                        &ch,
                        self.inner.timeout
                    )
                    .await?
                    .0
                    .byte_order(),
                    crate::proto::ByteOrder::Big
                );
                let bytes = req.encode(big_endian);
                crate::client_native::ops_v2::op_put_tokens(
                    &ch,
                    tokens,
                    Some(&bytes),
                    self.inner.timeout,
                )
                .await
            }
        }
    }

    /// PUT the raw `pvput` CLI value tokens, deferring every
    /// field-vs-bare classification to the server PUT prototype instead
    /// of guessing at parse time. A `field=value` token is a field
    /// assignment only when `field` exists in the prototype; otherwise
    /// it is a bare string value (when `.value` is a `string`) or warned
    /// and ignored — matching pvAccessCPP `pvtoolsSrc/pvput.cpp:109-235`.
    /// So `pvput STR:PV a=b` writes the literal `"a=b"` to a string PV,
    /// and an unknown field warns rather than failing the command.
    /// `request` supplies the INIT pvRequest (`-r`); when `None` the
    /// request selects all fields so the full writable prototype is
    /// available for classification.
    pub async fn pvput_args(
        &self,
        pv_name: &str,
        tokens: &[String],
        request: Option<&crate::pv_request::PvRequestExpr>,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        match request {
            None => {
                crate::client_native::ops_v2::op_put_args(&ch, tokens, None, self.inner.timeout)
                    .await
            }
            Some(req) => {
                let big_endian = matches!(
                    crate::client_native::ops_v2::ensure_active_with_op_timeout(
                        &ch,
                        self.inner.timeout
                    )
                    .await?
                    .0
                    .byte_order(),
                    crate::proto::ByteOrder::Big
                );
                let bytes = req.encode(big_endian);
                crate::client_native::ops_v2::op_put_args(
                    &ch,
                    tokens,
                    Some(&bytes),
                    self.inner.timeout,
                )
                .await
            }
        }
    }

    /// Value-only read-modify-write — fetch the channel's `.value`
    /// subfield, hand it to `build` as a mutable [`PvField`], then PUT
    /// the modified value back. Mirrors pvxs
    /// `PutBuilder::fetchPresent(true).build(cb)` (PVA-065). Returns
    /// the closure's error wrapped in [`PvaError::InvalidValue`] when
    /// the user signals a problem.
    ///
    /// Use this when the put depends on the current value (toggle a
    /// bit, increment a counter, splice into an array) — the read +
    /// the write share the same channel handle so reconnect-mid-RMW
    /// is the only edge case the caller has to think about (it's
    /// reported the same way as any other transient PvaError).
    ///
    /// **Scope**: closure sees and round-trips the `.value` field
    /// only. Modifications to `alarm`, `timeStamp`, `display`, or
    /// any other structure subfield are NOT persisted — the PUT
    /// pvRequest is `field(value)` to match pvxs `Put` semantics
    /// (and avoid silently writing back stale alarm/severity).
    /// Use [`Self::pvput_field`] if you need to put non-value
    /// subfields.
    pub async fn pvput_build<F>(&self, pv_name: &str, build: F) -> PvaResult<()>
    where
        F: FnOnce(&mut crate::pvdata::PvField) -> Result<(), String>,
    {
        let ch = self.channel(pv_name).await?;
        // Fetch only `.value` so the closure sees exactly what the
        // subsequent op_put_value will round-trip — alarm/timeStamp/
        // etc. are out of scope and would be silently dropped at PUT
        // time if the closure touched them.
        let (_intro, mut value) =
            crate::client_native::ops_v2::op_get(&ch, &["value"], self.inner.timeout).await?;
        if let Err(msg) = build(&mut value) {
            return Err(crate::error::PvaError::InvalidValue(msg));
        }
        crate::client_native::ops_v2::op_put_value(&ch, &value, self.inner.timeout).await
    }

    /// PUT a single dotted-path field of the channel's structure.
    /// pvxs `Context::put(name).set(field_path, value).exec()`
    /// parity. Examples:
    ///
    /// ```ignore
    /// client.pvput_field("MY:PV", "value", "42").await?;
    /// client.pvput_field("MY:PV", "alarm.severity", "2").await?;
    /// client.pvput_field("MY:PV", "display.units", "ms").await?;
    /// ```
    ///
    /// The server receives a value where only the named field carries
    /// the parsed string and the changed bitset has only that field's
    /// bit set; every other field is left untouched. Use
    /// [`Self::pvput`] for the common "PUT to .value" shortcut.
    pub async fn pvput_field(
        &self,
        pv_name: &str,
        field_path: &str,
        value_str: &str,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_put_field(&ch, field_path, value_str, self.inner.timeout)
            .await
    }

    /// `pvput` with a custom pvRequest. The most common use is
    /// `record[process=true]` to request a synchronous PROC after the
    /// PUT — RPC-like semantics for IOCs that have side effects on
    /// process. pvxs `Context::put(name).pvRequest(...)` parity.
    ///
    /// **Endian caveat**: the pvRequest is encoded once against the
    /// current server's byte order. PUT is a one-shot op so reconnect
    /// to a different-endian server isn't possible mid-call, but the
    /// monitor variant inherits this constraint — see
    /// [`Self::pvmonitor_with_request`].
    pub async fn pvput_with_request(
        &self,
        pv_name: &str,
        request: &crate::pv_request::PvRequestExpr,
        value_str: &str,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        let big_endian = matches!(
            crate::client_native::ops_v2::ensure_active_with_op_timeout(&ch, self.inner.timeout)
                .await?
                .0
                .byte_order(),
            crate::proto::ByteOrder::Big
        );
        let bytes = request.encode(big_endian);
        crate::client_native::ops_v2::op_put_raw(&ch, &bytes, value_str, self.inner.timeout).await
    }

    /// PUT that writes no field — an empty changed bitset under
    /// `request`'s `record._options`. The interoperable form of "make the
    /// remote record process": pvxs implements no CMD_PROCESS handler
    /// (`src/conn.cpp:249-276` drains the frame at `default:`), and its own
    /// pvalink forward link is exactly this PUT
    /// (`ioc/pvalink_lset.cpp:691` -> `ioc/pvalink_channel.cpp:225-263`).
    /// Prefer it over [`Self::pvprocess`] wherever the peer may be a pvxs
    /// server; `pvprocess` stays for peers that do implement cmd 16.
    pub async fn pvput_empty_with_request(
        &self,
        pv_name: &str,
        request: &crate::pv_request::PvRequestExpr,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        let big_endian = matches!(
            crate::client_native::ops_v2::ensure_active_with_op_timeout(&ch, self.inner.timeout)
                .await?
                .0
                .byte_order(),
            crate::proto::ByteOrder::Big
        );
        let bytes = request.encode(big_endian);
        crate::client_native::ops_v2::op_put_empty(&ch, &bytes, self.inner.timeout).await
    }

    /// PUT a dotted-path sub-field using a custom pvRequest. Combines
    /// the record-options of `pvput_with_request` with the field-targeting
    /// of `pvput_field`. `field_path` must be non-empty.
    ///
    /// pvxs `pvxs/ioc/pvalink_channel.cpp:31-38 + 138` parity: INIT carries
    /// `field() record[process=..,block=..]`, DATA targets `field_path`.
    pub async fn pvput_field_with_request(
        &self,
        pv_name: &str,
        field_path: &str,
        request: &crate::pv_request::PvRequestExpr,
        value_str: &str,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        let big_endian = matches!(
            crate::client_native::ops_v2::ensure_active_with_op_timeout(&ch, self.inner.timeout)
                .await?
                .0
                .byte_order(),
            crate::proto::ByteOrder::Big
        );
        let bytes = request.encode(big_endian);
        crate::client_native::ops_v2::op_put_field_with_request(
            &ch,
            field_path,
            &bytes,
            value_str,
            self.inner.timeout,
        )
        .await
    }

    /// PUT multiple `field=value` assignments as one prototype-based
    /// delta. Each `(field_path, value_str)` is applied by
    /// dotted path and only the assigned fields are marked. `request`
    /// supplies the INIT pvRequest (e.g. `record[process=true]`); when
    /// `None` the request selects exactly the assigned paths. Mirrors
    /// pvxs `pvxput`'s multi-field form.
    pub async fn pvput_fields(
        &self,
        pv_name: &str,
        assignments: &[(String, String)],
        request: Option<&crate::pv_request::PvRequestExpr>,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        let req_bytes = match request {
            Some(req) => {
                let big_endian = matches!(
                    crate::client_native::ops_v2::ensure_active_with_op_timeout(
                        &ch,
                        self.inner.timeout
                    )
                    .await?
                    .0
                    .byte_order(),
                    crate::proto::ByteOrder::Big
                );
                Some(req.encode(big_endian))
            }
            None => None,
        };
        crate::client_native::ops_v2::op_put_fields(
            &ch,
            assignments,
            req_bytes.as_deref(),
            self.inner.timeout,
        )
        .await
    }

    /// Typed multi-field PUT: assign each `(path, PutLeaf)` into the PUT
    /// prototype and send one combined delta. Unlike [`Self::pvput_fields`],
    /// a [`crate::client_native::ops_v2::PutLeaf::Typed`] value is placed
    /// into the selected descriptor leaf as pvData with no `Display`/parse
    /// round trip — so a typed scalar array travels as its original payload
    /// rather than a bracketed string that the field parser would split on
    /// commas. [`crate::client_native::ops_v2::PutLeaf::Str`] leaves still
    /// lower through the CLI parser.
    ///
    /// Used by pvalink OUT links that coalesce sibling fields into one
    /// shared PUT carrying typed staged values — pvxs `linkBuildPut`
    /// (`pvxs/ioc/pvalink_channel.cpp:127-184`) parity for the combined path.
    pub async fn pvput_fields_typed(
        &self,
        pv_name: &str,
        assignments: &[(String, crate::client_native::ops_v2::PutLeaf)],
        request: Option<&crate::pv_request::PvRequestExpr>,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        let req_bytes = match request {
            Some(req) => {
                let big_endian = matches!(
                    crate::client_native::ops_v2::ensure_active_with_op_timeout(
                        &ch,
                        self.inner.timeout
                    )
                    .await?
                    .0
                    .byte_order(),
                    crate::proto::ByteOrder::Big
                );
                Some(req.encode(big_endian))
            }
            None => None,
        };
        crate::client_native::ops_v2::op_put_fields_typed(
            &ch,
            assignments,
            req_bytes.as_deref(),
            self.inner.timeout,
        )
        .await
    }

    /// PUT a pre-built [`PvField`] with a custom pvRequest. Like
    /// `pvput_pv_field` but INIT carries the caller's record options
    /// (`process`, `block`). DATA still targets `"value"`.
    ///
    /// pvxs `pvxs/ioc/pvalink_channel.cpp:268` parity for typed OUT arrays.
    pub async fn pvput_pv_field_with_request(
        &self,
        pv_name: &str,
        request: &crate::pv_request::PvRequestExpr,
        value: &crate::pvdata::PvField,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        let big_endian = matches!(
            crate::client_native::ops_v2::ensure_active_with_op_timeout(&ch, self.inner.timeout)
                .await?
                .0
                .byte_order(),
            crate::proto::ByteOrder::Big
        );
        let bytes = request.encode(big_endian);
        crate::client_native::ops_v2::op_put_value_raw(&ch, &bytes, value, self.inner.timeout).await
    }

    /// Like [`Self::pvput_pv_field_with_request`] but takes the PUT INIT
    /// pvRequest as a decoded [`PvField`] value (e.g. the request a PVA
    /// gateway preserved into `ChannelContext.pv_request`) rather than a
    /// [`crate::pv_request::PvRequestExpr`]. The pvRequest — carrying
    /// `record._options.process`/`block` — is serialized in the
    /// connection's negotiated byte order and sent at PUT INIT, while the
    /// value targets the `value` bit as in
    /// [`Self::pvput_pv_field_with_request`].
    pub async fn pvput_pv_field_with_request_value(
        &self,
        pv_name: &str,
        pv_request: &crate::pvdata::PvField,
        value: &crate::pvdata::PvField,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        let order =
            crate::client_native::ops_v2::ensure_active_with_op_timeout(&ch, self.inner.timeout)
                .await?
                .0
                .byte_order();
        let mut bytes = Vec::new();
        let desc = pv_request.descriptor();
        crate::pvdata::encode::encode_type_desc(&desc, order, &mut bytes);
        crate::pvdata::encode::encode_pv_field(pv_request, &desc, order, &mut bytes);
        crate::client_native::ops_v2::op_put_value_raw(&ch, &bytes, value, self.inner.timeout).await
    }

    /// PUT a pre-built [`PvField`] into a single dotted-path sub-field
    /// using a caller-provided pvRequest. Combines the typed-value
    /// path of [`Self::pvput_pv_field_with_request`] with the
    /// field-targeting of [`Self::pvput_field_with_request`]: the
    /// typed value is placed at `field_path` (drilling into a leaf
    /// `value` sub-field when the target is an NT-style struct), and
    /// only that path's bit is marked changed. `field_path` must be
    /// non-empty.
    ///
    /// Used by pvalink OUT links carrying `field=<subfield>` together
    /// with a typed array/scalar value. pvxs `pvxs/ioc/pvalink_channel.cpp:127`
    /// (`linkBuildPut`) parity for typed PUTs into the link's
    /// `fieldName` target.
    pub async fn pvput_pv_field_field_with_request(
        &self,
        pv_name: &str,
        field_path: &str,
        request: &crate::pv_request::PvRequestExpr,
        value: &crate::pvdata::PvField,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        let big_endian = matches!(
            crate::client_native::ops_v2::ensure_active_with_op_timeout(&ch, self.inner.timeout)
                .await?
                .0
                .byte_order(),
            crate::proto::ByteOrder::Big
        );
        let bytes = request.encode(big_endian);
        crate::client_native::ops_v2::op_put_value_field_with_request(
            &ch,
            field_path,
            &bytes,
            value,
            self.inner.timeout,
        )
        .await
    }

    pub async fn pvmonitor<F>(&self, pv_name: &str, mut callback: F) -> PvaResult<()>
    where
        F: FnMut(&PvField) + Send,
    {
        let ch = self.channel(pv_name).await?;
        op_monitor(&ch, &[], self.inner.pipeline_size, move |_desc, value| {
            callback(value)
        })
        .await
    }

    /// monitor that surfaces **raw MONITOR DATA body bytes**
    /// (`changed | value | overrun` triplet from the wire) instead of
    /// a decoded [`PvField`]. Used by bridge `pva_gateway` upstream
    /// task to skip the decode-and-re-encode round-trip when fanning
    /// events out to many downstream subscribers.
    pub async fn pvmonitor_raw_frames<F>(&self, pv_name: &str, mut callback: F) -> PvaResult<()>
    where
        F: FnMut(&FieldDesc, bytes::Bytes, crate::proto::ByteOrder) + Send,
    {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_monitor_raw_frames(
            &ch,
            &[],
            self.inner.pipeline_size,
            move |desc, body, order| callback(desc, body, order),
        )
        .await
    }

    /// Like [`Self::pvmonitor_raw_frames`] but returns
    /// a [`SubscriptionHandle`] for pause/resume/stats. The bridge
    /// `pva_gateway` uses this to forward downstream watermark events
    /// into upstream pipeline-pause control msgs without an
    /// intermediate decode/encode cycle.
    ///
    /// `on_conn` carries the subscription's connection-state transitions —
    /// the only sanctioned source of upstream connect/disconnect for a
    /// handle monitor (see [`MonitorConnEvent`]).
    pub async fn pvmonitor_raw_frames_handle<F, C>(
        &self,
        pv_name: &str,
        callback: F,
        on_conn: C,
    ) -> PvaResult<SubscriptionHandle>
    where
        F: FnMut(&FieldDesc, bytes::Bytes, crate::proto::ByteOrder) + Send + 'static,
        C: FnMut(MonitorConnEvent) + Send + 'static,
    {
        let ch = self.channel(pv_name).await?;
        Ok(op_monitor_raw_frames_handle(
            ch,
            &[],
            self.inner.pipeline_size,
            callback,
            on_conn,
        ))
    }

    /// Like [`Self::pvmonitor_raw_frames_handle`] but opens the upstream
    /// monitor with a caller-supplied pvRequest VALUE rather than the
    /// default all-fields request. The PVA gateway forwards a downstream
    /// client's decoded MONITOR INIT pvRequest here so the upstream
    /// server applies the same field projection / `record._options.
    /// _filter` chain the client requested. The request value is
    /// re-encoded in the upstream connection's byte order on every
    /// (re)connect (so a reconnect to an opposite-endian peer stays
    /// correct). pva2pva `p2pApp/channel.cpp:157-193` forwards the
    /// serialized downstream pvRequest; `moncache.cpp:34-37` caches one
    /// upstream monitor per distinct request.
    pub async fn pvmonitor_raw_frames_handle_with_request<F, C>(
        &self,
        pv_name: &str,
        pv_request: crate::pvdata::PvField,
        callback: F,
        on_conn: C,
    ) -> PvaResult<SubscriptionHandle>
    where
        F: FnMut(&FieldDesc, bytes::Bytes, crate::proto::ByteOrder) + Send + 'static,
        C: FnMut(MonitorConnEvent) + Send + 'static,
    {
        let ch = self.channel(pv_name).await?;
        Ok(op_monitor_raw_frames_handle_with_request(
            ch,
            pv_request,
            self.inner.pipeline_size,
            callback,
            on_conn,
        ))
    }

    /// `pvmonitor` with a custom pvRequest. Common uses:
    ///   `record[queueSize=N]` — pipeline window size.
    ///   `record[pipeline=true]` — flow-control mode.
    ///   `field(value,alarm.severity)` — projection.
    /// The custom request is reused on every reconnect cycle, so the
    /// queueSize / pipeline negotiation is preserved across server
    /// restarts. pvxs `Context::monitor(name).pvRequest(...)` parity.
    pub async fn pvmonitor_with_request<F>(
        &self,
        pv_name: &str,
        request: &crate::pv_request::PvRequestExpr,
        mut callback: F,
    ) -> PvaResult<()>
    where
        F: FnMut(&PvField) + Send,
    {
        let ch = self.channel(pv_name).await?;
        // Distinct from the one-shot pre-reads: a monitor's
        // resolve is NOT bounded by `op_timeout`. `pvmonitor*` stays
        // pending until the server answers (or the caller drops the
        // future); its natural cancel path is `SubscriptionHandle` drop,
        // matching the reconnect loop inside `op_monitor_raw`. Bounding
        // it would fail a live-but-slow monitor at `op_timeout` instead
        // of retrying — wrong for a long-lived subscription.
        let big_endian = matches!(
            ch.ensure_active().await?.0.byte_order(),
            crate::proto::ByteOrder::Big
        );
        // Flow control comes from the SAME request the wire bytes are
        // built from: `record._options.{pipeline,queueSize,ackAny}`
        // drive the INIT pipeline bit / `nack` trailer and the ACK
        // cadence, not the client's fixed builder window. pvxs
        // `MonitorBuilder::exec()` (clientmon.cpp:761-808). The builder
        // `pipeline_size` is only the queueSize fallback when the
        // request enables pipelining without naming a window.
        let flow = crate::client_native::ops_v2::MonitorFlow::from_record_options(
            &request.record_options,
            self.inner.pipeline_size,
        );
        let bytes = request.encode(big_endian);
        crate::client_native::ops_v2::op_monitor_raw(&ch, bytes, flow, move |_desc, value| {
            callback(value)
        })
        .await
    }

    /// `pvmonitor` variant whose callback also receives the field
    /// descriptor. Useful for clients that want to introspect the
    /// shape on every event (e.g. for adaptive UI). For typed
    /// access against a known `T: TypedNT` shape, prefer
    /// [`Self::pvmonitor_typed`] which decodes into the Rust type
    /// directly.
    pub async fn pvmonitor_with_descriptor<F>(&self, pv_name: &str, callback: F) -> PvaResult<()>
    where
        F: FnMut(&FieldDesc, &PvField) + Send,
    {
        let ch = self.channel(pv_name).await?;
        op_monitor(&ch, &[], self.inner.pipeline_size, callback).await
    }

    /// Begin a pausable monitor that can be paused/resumed and queried
    /// for stats. Mirrors pvxs `Context::monitor(name).exec()` →
    /// `Subscription`. The returned handle owns the inner task; call
    /// `stop()` to terminate or drop after `stop()` returns.
    ///
    /// `on_conn` carries the subscription's connection-state transitions —
    /// the only sanctioned source of upstream connect/disconnect for a
    /// handle monitor (see [`MonitorConnEvent`]).
    pub async fn pvmonitor_handle<F, C>(
        &self,
        pv_name: &str,
        callback: F,
        on_conn: C,
    ) -> PvaResult<SubscriptionHandle>
    where
        F: FnMut(&FieldDesc, &PvField) + Send + 'static,
        C: FnMut(MonitorConnEvent) + Send + 'static,
    {
        let ch = self.channel(pv_name).await?;
        Ok(op_monitor_handle(
            ch,
            &[],
            self.inner.pipeline_size,
            callback,
            on_conn,
        ))
    }

    /// Like [`Self::pvmonitor_handle`] but pinned to `server`. Mirrors
    /// pvxs `MonitorBuilder::server(addr).exec()`. The handle owns its
    /// own per-call channel — it does not affect the shared cache for
    /// `pv_name`.
    pub async fn pvmonitor_handle_from<F, C>(
        &self,
        pv_name: &str,
        server: SocketAddr,
        callback: F,
        on_conn: C,
    ) -> PvaResult<SubscriptionHandle>
    where
        F: FnMut(&FieldDesc, &PvField) + Send + 'static,
        C: FnMut(MonitorConnEvent) + Send + 'static,
    {
        let ch = self.channel_with_forced(pv_name, Some(server)).await?;
        Ok(op_monitor_handle(
            ch,
            &[],
            self.inner.pipeline_size,
            callback,
            on_conn,
        ))
    }

    /// Monitor with typed events (`Connected`/`Data`/`Disconnected`/
    /// `Finished`). Mirrors pvxs's MonitorBuilder + Subscription
    /// exception-based stream API. `mask` defaults are
    /// pvxs-compatible: `maskConnected=true`, `maskDisconnected=false`.
    ///
    /// `request` is an optional custom pvRequest (`None` = the default
    /// all-fields request) applied at MONITOR INIT, so the descriptor in
    /// each [`MonitorEvent::Data`] reflects exactly the requested
    /// projection / filter shape — the same request reused across
    /// reconnects, and its `record._options.{pipeline,queueSize}` driving
    /// the flow window (pvxs `MonitorBuilder::pvRequest`).
    pub async fn pvmonitor_events<F>(
        &self,
        pv_name: &str,
        request: Option<&crate::pv_request::PvRequestExpr>,
        mask: MonitorEventMask,
        callback: F,
    ) -> PvaResult<()>
    where
        F: FnMut(MonitorEvent) + Send,
    {
        let ch = self.channel(pv_name).await?;
        let (raw_pv_req, flow) = match request {
            Some(req) => {
                let big_endian = matches!(
                    ch.ensure_active().await?.0.byte_order(),
                    crate::proto::ByteOrder::Big
                );
                let flow = crate::client_native::ops_v2::MonitorFlow::from_record_options(
                    &req.record_options,
                    self.inner.pipeline_size,
                );
                (Some(req.encode(big_endian)), flow)
            }
            None => (
                None,
                crate::client_native::ops_v2::MonitorFlow::window(self.inner.pipeline_size),
            ),
        };
        op_monitor_events(&ch, raw_pv_req, flow, mask, callback).await
    }

    /// Fetch the channel's introspection (FieldDesc) using PVA's
    /// dedicated GET_FIELD message — cheaper than [`Self::pvget`]
    /// because no value bytes are transferred. Previously
    /// it was implemented as a full GET that discarded the value;
    /// now uses the proper introspection-only path matching pvxs
    /// `Context::info(name).exec()`. Critical for large NTNDArray /
    /// NTTable PVs where pvinfo() was paying a multi-MiB transfer
    /// cost just to discover the shape.
    pub async fn pvinfo(&self, pv_name: &str) -> PvaResult<FieldDesc> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_get_field(&ch, "", self.inner.timeout).await
    }

    /// Like [`Self::pvinfo`] but also reports which server replied —
    /// useful for diagnostics on multi-source / failover deployments.
    pub async fn pvinfo_full(&self, pv_name: &str) -> PvaResult<(FieldDesc, SocketAddr)> {
        let (intro, addr, _cred) = self.pvinfo_full_with_credentials(pv_name).await?;
        Ok((intro, addr))
    }

    /// Like [`Self::pvinfo_full`] but additionally reports the
    /// server's verified X.509 identity (`pvas://` only) — the
    /// credentials pvxs `pvxinfo -v` prints. The third tuple element
    /// is `None` for a plain `pva://` connection or a TLS server that
    /// presented no usable certificate.
    pub async fn pvinfo_full_with_credentials(
        &self,
        pv_name: &str,
    ) -> PvaResult<(FieldDesc, SocketAddr, Option<crate::auth::X509Credentials>)> {
        let ch = self.channel(pv_name).await?;
        let intro = crate::client_native::ops_v2::op_get_field(&ch, "", self.inner.timeout).await?;
        let (server_addr, cred) = match ch.current_state() {
            super::channel::ChannelState::Active { server, .. } => {
                (server.addr, server.server_identity().cloned())
            }
            _ => (
                SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0),
                None,
            ),
        };
        Ok((intro, server_addr, cred))
    }

    pub async fn pvrpc(
        &self,
        pv_name: &str,
        request_desc: &FieldDesc,
        request_value: &PvField,
    ) -> PvaResult<RpcReply> {
        let ch = self.channel(pv_name).await?;
        // The RPC INIT pvRequest and the RPC DATA argument are distinct
        // wire values: pvxs `clientget.cpp:350-352` serializes the
        // operation pvRequest at INIT and `:325-329` the argument at
        // EXEC. Send the default empty pvRequest at INIT and carry
        // `(request_desc, request_value)` as the DATA argument; do NOT
        // also send the argument as the INIT pvRequest, which would make
        // an upstream `createChannelRPC(..., pvRequest)` provider see the
        // argument where it expected the operation request.
        let (req_desc, req_value) = empty_pv_request();
        op_rpc(
            &ch,
            &req_desc,
            &req_value,
            RpcArg::Typed {
                desc: request_desc,
                value: request_value,
            },
            self.inner.timeout,
        )
        .await
    }

    /// Like [`Self::pvrpc`] but sends a caller-provided RPC INIT
    /// pvRequest, kept distinct from the RPC DATA argument. A PVA-to-PVA
    /// gateway uses this to forward the downstream
    /// `createChannelRPC(..., pvRequest)` create-time request upstream
    /// (pva2pva `channel.cpp:140-149`) while carrying the downstream
    /// argument as the DATA value. pvxs `clientget.cpp:350-352` serializes
    /// the pvRequest at INIT and `:325-329` the argument at EXEC.
    pub async fn pvrpc_with_request(
        &self,
        pv_name: &str,
        pv_request_desc: &FieldDesc,
        pv_request_value: &PvField,
        arg_desc: &FieldDesc,
        arg_value: &PvField,
    ) -> PvaResult<RpcReply> {
        let ch = self.channel(pv_name).await?;
        op_rpc(
            &ch,
            pv_request_desc,
            pv_request_value,
            RpcArg::Typed {
                desc: arg_desc,
                value: arg_value,
            },
            self.inner.timeout,
        )
        .await
    }

    /// RPC with pvxs's **top-level null** argument
    /// (`Context::rpc(name, Value())`): the DATA phase carries the
    /// single `0xff` null-type tag with no value body, not an `any`
    /// carrying null. Providers that use a null query object to mean
    /// "no argument" require this exact wire shape; a present `any`
    /// (the only shape [`Self::pvrpc`] can express) is distinguishable
    /// from it. The INIT pvRequest is the empty pvRequest pvxs sends by
    /// default for a parameterless RPC.
    pub async fn pvrpc_null(&self, pv_name: &str) -> PvaResult<RpcReply> {
        let ch = self.channel(pv_name).await?;
        let (req_desc, req_value) = empty_pv_request();
        op_rpc(&ch, &req_desc, &req_value, RpcArg::Null, self.inner.timeout).await
    }

    /// Same as [`Self::pvrpc`] but pins the operation to a specific
    /// server, bypassing UDP search. Mirrors pvxs
    /// `ctxt.rpc(name).server(addr)` (`tools/list.cpp`). Required for
    /// querying server-internal PVs (e.g. the special `server` PV used
    /// by `pvlist <ip>`) which are not announced via search.
    pub async fn pvrpc_from(
        &self,
        pv_name: &str,
        server: SocketAddr,
        request_desc: &FieldDesc,
        request_value: &PvField,
    ) -> PvaResult<RpcReply> {
        let ch = self.channel_with_forced(pv_name, Some(server)).await?;
        // See [`Self::pvrpc`]: default empty pvRequest at INIT, the
        // caller's value as the DATA argument (no INIT/DATA conflation).
        let (req_desc, req_value) = empty_pv_request();
        op_rpc(
            &ch,
            &req_desc,
            &req_value,
            RpcArg::Typed {
                desc: request_desc,
                value: request_value,
            },
            self.inner.timeout,
        )
        .await
    }

    /// Like [`Self::pvrpc_null`] but pins the operation to a specific
    /// server, bypassing UDP search (mirrors [`Self::pvrpc_from`]).
    pub async fn pvrpc_from_null(&self, pv_name: &str, server: SocketAddr) -> PvaResult<RpcReply> {
        let ch = self.channel_with_forced(pv_name, Some(server)).await?;
        let (req_desc, req_value) = empty_pv_request();
        op_rpc(&ch, &req_desc, &req_value, RpcArg::Null, self.inner.timeout).await
    }

    /// PVA `PUT_GET` (cmd 12) — atomic put-then-get. PUTs `value_str`
    /// to the channel's `.value` field and returns the (possibly
    /// post-processed) value back in one round trip. Returns the
    /// readback `(introspection, value)`.
    ///
    /// Use this when a write has a side effect on the value (a record
    /// that recalculates on process) and you want the updated value
    /// without a separate GET — it is a single wire operation and the
    /// server applies the put then reads back atomically.
    pub async fn pvput_get(
        &self,
        pv_name: &str,
        value_str: &str,
    ) -> PvaResult<(FieldDesc, PvField)> {
        let ch = self.channel(pv_name).await?;
        op_put_get(&ch, value_str, self.inner.timeout).await
    }

    /// Atomic `PUT_GET` (cmd 12) of a pre-built [`PvField`] using the
    /// default `field(value)` pvRequest — the typed-value counterpart of
    /// [`Self::pvput_get`] (which parses a string). Returns the post-put
    /// readback `(introspection, value)`.
    pub async fn pvput_get_pv_field(
        &self,
        pv_name: &str,
        value: &crate::pvdata::PvField,
    ) -> PvaResult<(FieldDesc, PvField)> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_put_get_value(&ch, value, self.inner.timeout).await
    }

    /// Atomic `PUT_GET` (cmd 12) of a pre-built [`PvField`] carrying the
    /// caller's decoded pvRequest (e.g. a PVA gateway's preserved
    /// `ChannelContext.pv_request`): one upstream operation that applies
    /// the put and returns the post-put readback. Like
    /// [`Self::pvput_pv_field_with_request_value`] but the round trip is a
    /// single PVA `PUT_GET` rather than a plain PUT — the value is written
    /// and the server's post-put value returned atomically, with the
    /// pvRequest's `record._options.process`/`block` honored. The
    /// pvRequest is serialized in the connection's negotiated byte order at
    /// INIT; the value targets the `value` bit as in
    /// [`Self::pvput_pv_field_with_request_value`].
    ///
    /// pva2pva `p2pApp/channel.cpp:129-138` (`GWChannel::createChannelPutGet`)
    /// forwards the original pvRequest verbatim and returns the upstream
    /// readback.
    pub async fn pvput_get_pv_field_with_request_value(
        &self,
        pv_name: &str,
        pv_request: &crate::pvdata::PvField,
        value: &crate::pvdata::PvField,
    ) -> PvaResult<(FieldDesc, PvField)> {
        let ch = self.channel(pv_name).await?;
        let bytes = self.encode_pv_request(&ch, pv_request).await?;
        crate::client_native::ops_v2::op_put_get_value_raw(&ch, &bytes, value, self.inner.timeout)
            .await
    }

    /// [`Self::pvput_get_pv_field_with_request_value`] keeping the readback's
    /// marked leaves — the PUT_GET a PVA gateway forwards. See
    /// [`Self::pvget_marked`].
    pub async fn pvput_get_pv_field_with_request_value_marked(
        &self,
        pv_name: &str,
        pv_request: &crate::pvdata::PvField,
        value: &crate::pvdata::PvField,
    ) -> PvaResult<crate::client_native::ops_v2::MarkedRead> {
        let ch = self.channel(pv_name).await?;
        let bytes = self.encode_pv_request(&ch, pv_request).await?;
        crate::client_native::ops_v2::op_put_get_value_raw_marked(
            &ch,
            &bytes,
            value,
            self.inner.timeout,
        )
        .await
    }

    /// [`Self::pvput_get_pv_field`] keeping the readback's marked leaves.
    /// See [`Self::pvget_marked`].
    pub async fn pvput_get_pv_field_marked(
        &self,
        pv_name: &str,
        value: &crate::pvdata::PvField,
    ) -> PvaResult<crate::client_native::ops_v2::MarkedRead> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_put_get_value_marked(&ch, value, self.inner.timeout).await
    }

    /// PVA `PUT_GET` `getGet` subcommand (`QOS_GET`, 0x40) — read the
    /// channel's current get-side data with no put leg, returning the
    /// readback `(introspection, value)`. The EPICS pvAccess
    /// `ChannelPutGet::getGet()` (`clientContextImpl.cpp:1233-1255`)
    /// counterpart; unlike [`Self::pvput_get`] it never writes.
    pub async fn pvget_get(&self, pv_name: &str) -> PvaResult<(FieldDesc, PvField)> {
        let ch = self.channel(pv_name).await?;
        op_get_get(&ch, self.inner.timeout).await
    }

    /// PVA `PUT_GET` `getPut` subcommand (`QOS_GET_PUT`, 0x80) — read the
    /// channel's current put-side data with no put leg. The EPICS pvAccess
    /// `ChannelPutGet::getPut()` (`clientContextImpl.cpp:1262-1288`)
    /// counterpart.
    pub async fn pvget_put(&self, pv_name: &str) -> PvaResult<(FieldDesc, PvField)> {
        let ch = self.channel(pv_name).await?;
        op_get_put(&ch, self.inner.timeout).await
    }

    /// Like [`Self::pvget_get`] but carries a caller-supplied pvRequest at
    /// INIT, so a `getField(...)` selector projects the get-leg structure's
    /// readback (pvDatabaseCPP `ChannelPutGetLocal::getGet`,
    /// modules/pvDatabase/src/pvAccess/channelLocal.cpp). The pvRequest is
    /// serialized in the connection's negotiated byte order. Absent a
    /// `getField`, the server falls back to the common `field` selection.
    pub async fn pvget_get_with_request_value(
        &self,
        pv_name: &str,
        pv_request: &crate::pvdata::PvField,
    ) -> PvaResult<(FieldDesc, PvField)> {
        let ch = self.channel(pv_name).await?;
        let order =
            crate::client_native::ops_v2::ensure_active_with_op_timeout(&ch, self.inner.timeout)
                .await?
                .0
                .byte_order();
        let mut bytes = Vec::new();
        let desc = pv_request.descriptor();
        crate::pvdata::encode::encode_type_desc(&desc, order, &mut bytes);
        crate::pvdata::encode::encode_pv_field(pv_request, &desc, order, &mut bytes);
        crate::client_native::ops_v2::op_get_get_with_request(&ch, &bytes, self.inner.timeout).await
    }

    /// Like [`Self::pvget_put`] but carries a caller-supplied pvRequest at
    /// INIT, so a `putField(...)` selector projects the put-leg structure's
    /// readback (pvDatabaseCPP `ChannelPutGetLocal::getPut`,
    /// modules/pvDatabase/src/pvAccess/channelLocal.cpp). The pvRequest is
    /// serialized in the connection's negotiated byte order. Absent a
    /// `putField`, the server falls back to the common `field` selection.
    pub async fn pvget_put_with_request_value(
        &self,
        pv_name: &str,
        pv_request: &crate::pvdata::PvField,
    ) -> PvaResult<(FieldDesc, PvField)> {
        let ch = self.channel(pv_name).await?;
        let order =
            crate::client_native::ops_v2::ensure_active_with_op_timeout(&ch, self.inner.timeout)
                .await?
                .0
                .byte_order();
        let mut bytes = Vec::new();
        let desc = pv_request.descriptor();
        crate::pvdata::encode::encode_type_desc(&desc, order, &mut bytes);
        crate::pvdata::encode::encode_pv_field(pv_request, &desc, order, &mut bytes);
        crate::client_native::ops_v2::op_get_put_with_request(&ch, &bytes, self.inner.timeout).await
    }

    /// PVA `PROCESS` (cmd 16) — trigger record processing without
    /// transferring a value. The wire equivalent of an EPICS
    /// `caput .PROC` / `dbProcess`. Succeeds with `()`; a processing
    /// failure surfaces as a [`PvaError::Protocol`].
    ///
    /// Sends the empty default pvRequest, matching EPICS base pvaClient
    /// `createProcess("")`. Use [`Self::pvprocess_with_request`] to send a
    /// provider-specific PROCESS request.
    pub async fn pvprocess(&self, pv_name: &str) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        op_process(&ch, self.inner.timeout).await
    }

    /// Like [`Self::pvprocess`] but sends a caller-supplied PROCESS
    /// pvRequest (e.g. built via [`Self::request`] →
    /// [`crate::pv_request::PvRequestExpr::encode`], or
    /// `record[block=true]`). pvAccess serializes the request on PROCESS
    /// INIT and the server provider can inspect it during
    /// `createChannelProcess`. Mirrors pvaClient
    /// `PvaClientChannel::createProcess(pvRequest)`.
    pub async fn pvprocess_with_request(&self, pv_name: &str, pv_request: &[u8]) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        op_process_with_request(&ch, pv_request, self.inner.timeout).await
    }

    /// Like [`Self::pvprocess_with_request`] but takes the PROCESS INIT
    /// pvRequest as a decoded [`PvField`] value rather than pre-encoded
    /// bytes; it is serialized in the connection's negotiated byte order.
    /// A PVA-to-PVA gateway uses this to forward the downstream PROCESS
    /// create-time pvRequest — preserved into `ChannelContext.pv_request`
    /// — upstream, matching pva2pva `createChannelProcess(..., pvRequest)`
    /// (channel.cpp:98-107).
    pub async fn pvprocess_with_request_value(
        &self,
        pv_name: &str,
        pv_request: &PvField,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        op_process_with_request_value(&ch, pv_request, self.inner.timeout).await
    }

    /// ChannelArray `getArray` (cmd 14): read the `[offset, count, stride]`
    /// slice of `pv_name`'s array field. `count == 0` reads to the end.
    /// Returns the array `(introspection, value)`. Errors with a protocol
    /// status when the server's source does not serve a windowed array.
    pub async fn pvarray_get(
        &self,
        pv_name: &str,
        offset: u32,
        count: u32,
        stride: u32,
    ) -> PvaResult<(FieldDesc, PvField)> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_array_get(&ch, offset, count, stride, self.inner.timeout)
            .await
    }

    /// ChannelArray `putArray` (cmd 14): splice `value` into `pv_name`'s
    /// array field at `offset` with `stride`.
    pub async fn pvarray_put(
        &self,
        pv_name: &str,
        value: &PvField,
        offset: u32,
        stride: u32,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_array_put(&ch, value, offset, stride, self.inner.timeout)
            .await
    }

    /// ChannelArray `setLength` (cmd 14): resize `pv_name`'s array field.
    pub async fn pvarray_set_length(&self, pv_name: &str, length: u32) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_array_set_length(&ch, length, self.inner.timeout).await
    }

    /// ChannelArray `getLength` (cmd 14): query the element count of
    /// `pv_name`'s array field.
    pub async fn pvarray_get_length(&self, pv_name: &str) -> PvaResult<u32> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_array_get_length(&ch, self.inner.timeout).await
    }

    /// ChannelArray INIT-only descriptor probe: open the array op (default
    /// `field(value)` selection), read back the bound array field's
    /// introspection, then DESTROY. Used by a PVA gateway to resolve the
    /// upstream array descriptor it must report on a downstream ARRAY INIT.
    pub async fn pvarray_describe(&self, pv_name: &str) -> PvaResult<FieldDesc> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_array_describe(&ch, None, self.inner.timeout).await
    }

    /// [`Self::pvarray_describe`] forwarding the caller's `pv_request`
    /// (selects the bound array field) verbatim into the INIT frame.
    pub async fn pvarray_describe_with_request_value(
        &self,
        pv_name: &str,
        pv_request: &PvField,
    ) -> PvaResult<FieldDesc> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_array_describe(&ch, Some(pv_request), self.inner.timeout)
            .await
    }

    /// [`Self::pvarray_get`] forwarding the caller's `pv_request` (selects
    /// the bound array field) verbatim into the INIT frame.
    pub async fn pvarray_get_with_request_value(
        &self,
        pv_name: &str,
        pv_request: &PvField,
        offset: u32,
        count: u32,
        stride: u32,
    ) -> PvaResult<(FieldDesc, PvField)> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_array_get_with_request(
            &ch,
            pv_request,
            offset,
            count,
            stride,
            self.inner.timeout,
        )
        .await
    }

    /// [`Self::pvarray_put`] forwarding the caller's `pv_request` (selects
    /// the bound array field) verbatim into the INIT frame.
    pub async fn pvarray_put_with_request_value(
        &self,
        pv_name: &str,
        pv_request: &PvField,
        value: &PvField,
        offset: u32,
        stride: u32,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_array_put_with_request(
            &ch,
            pv_request,
            value,
            offset,
            stride,
            self.inner.timeout,
        )
        .await
    }

    /// [`Self::pvarray_set_length`] forwarding the caller's `pv_request`
    /// (selects the bound array field) verbatim into the INIT frame.
    pub async fn pvarray_set_length_with_request_value(
        &self,
        pv_name: &str,
        pv_request: &PvField,
        length: u32,
    ) -> PvaResult<()> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_array_set_length_with_request(
            &ch,
            pv_request,
            length,
            self.inner.timeout,
        )
        .await
    }

    /// [`Self::pvarray_get_length`] forwarding the caller's `pv_request`
    /// (selects the bound array field) verbatim into the INIT frame.
    pub async fn pvarray_get_length_with_request_value(
        &self,
        pv_name: &str,
        pv_request: &PvField,
    ) -> PvaResult<u32> {
        let ch = self.channel(pv_name).await?;
        crate::client_native::ops_v2::op_array_get_length_with_request(
            &ch,
            pv_request,
            self.inner.timeout,
        )
        .await
    }

    /// Snapshot of the client's current state — channel cache size,
    /// connection-pool peers, name-server count, and per-connection /
    /// per-channel byte counters. Mirrors pvxs `Context::report`
    /// (client.h:597-599 / src/client.cpp:464-501): each [`ConnReport`] carries
    /// its [`ConnReport::channels`] list with per-channel RX/TX counters.
    ///
    /// Like pvxs `Report report(bool zero=true)`, the no-argument form
    /// **zeros** the byte counters after snapshotting, so periodic
    /// `report()` calls yield per-interval deltas rather than ever-growing
    /// cumulative totals (src/client.cpp:464-500 resets `statTx`/`statRx` when
    /// `zero`). Use [`Self::report_zeroed`]`(false)` for a non-resetting
    /// cumulative snapshot.
    pub fn report(&self) -> ClientReport {
        self.report_zeroed(true)
    }

    /// like [`Self::report`] but, when `zero` is true, resets
    /// each connection's byte counters after the snapshot — pvxs
    /// `Context::report(bool zero)`, so a subsequent report returns the
    /// per-connection deltas since this one.
    pub fn report_zeroed(&self, zero: bool) -> ClientReport {
        let channels = self.inner.channels.read();
        let mut active = 0usize;
        let mut searching = 0usize;
        let mut connecting = 0usize;
        let mut closed = 0usize;
        for ch in channels.values() {
            // Use `is_active()` so server-destroyed channels (whose
            // raw state is still `Active` until the next
            // ensure_active runs) don't get counted as live. The
            // raw-state pattern matches in the rest of the branches
            // are only reached when is_active() is false.
            if ch.is_active() {
                active += 1;
                continue;
            }
            match ch.current_state() {
                super::channel::ChannelState::Active { .. } => {
                    // is_active() returned false → conn dead OR
                    // destroyed. Treat as searching for reporting:
                    // the next ensure_active will move it for real.
                    searching += 1;
                }
                super::channel::ChannelState::Searching => searching += 1,
                super::channel::ChannelState::Connecting => connecting += 1,
                super::channel::ChannelState::Closed => closed += 1,
                super::channel::ChannelState::Idle => {}
            }
        }
        let connections = self
            .inner
            .pool
            .connection_byte_reports(zero)
            .into_iter()
            .map(|(peer, bytes_rx, bytes_tx, alive, channels)| ConnReport {
                peer,
                bytes_rx,
                bytes_tx,
                alive,
                channels: channels
                    .into_iter()
                    .map(|(name, sid, bytes_rx, bytes_tx)| ChanReport {
                        name,
                        sid,
                        bytes_rx,
                        bytes_tx,
                    })
                    .collect(),
            })
            .collect();
        ClientReport {
            channels_total: channels.len(),
            channels_active: active,
            channels_searching: searching,
            channels_connecting: connecting,
            channels_closed: closed,
            name_servers: self.inner.name_servers.len(),
            direct_mode: self.inner.server_addr.is_some(),
            connections,
        }
    }

    /// Begin building a custom pvRequest. Returns a fresh
    /// [`crate::pv_request::PvRequestBuilder`] that callers can chain
    /// `field()` / `record()` / `pv_request(str)` on, then materialize
    /// with `.build()`. Mirrors pvxs `Context::request()` (client.h:553)
    /// — included on the context for parity even though the builder is
    /// stateless and could be constructed standalone.
    pub fn request(&self) -> crate::pv_request::PvRequestBuilder {
        crate::pv_request::PvRequestBuilder::new()
    }

    /// Begin a `connect` builder for `pv_name`. Use this to attach
    /// onConnect/onDisconnect callbacks that fire whenever the channel
    /// transitions across the Active boundary. Mirrors pvxs's
    /// `Context::connect(name).onConnect(...).exec()`.
    pub fn connect(&self, pv_name: &str) -> ConnectBuilder<'_> {
        ConnectBuilder {
            client: self,
            pv_name: pv_name.to_string(),
            on_connect: None,
            on_disconnect: None,
            server: None,
            sync_cancel: true,
        }
    }

    /// Concurrent multi-PV GET. Resolves channels and issues GET ops
    /// in parallel. Returns results in the same order as the input PV
    /// names. Failed PVs map to `Err`.
    ///
    /// PVA's GET is a stateful 3-frame exchange (INIT → DATA → DESTROY),
    /// so true wire-level batching (like CA's caget_many) isn't possible.
    /// Instead we dispatch N independent GETs concurrently — channels
    /// are cached, so PVs on the same server share one TCP connection.
    ///
    /// ```ignore
    /// let results = client.pvget_many(&["PV:A", "PV:B", "PV:C"]).await;
    /// for (i, r) in results.iter().enumerate() {
    ///     match r {
    ///         Ok(field) => println!("PV[{i}] = {field}"),
    ///         Err(e)    => println!("PV[{i}] failed: {e}"),
    ///     }
    /// }
    /// ```
    pub async fn pvget_many(&self, pv_names: &[&str]) -> Vec<PvaResult<PvField>> {
        let n = pv_names.len();
        let mut results: Vec<PvaResult<PvField>> = (0..n).map(|_| Err(PvaError::Timeout)).collect();

        // Phase 1: ensure each channel has a warm-GET cache populated.
        // The first call per channel pays the full INIT+GET cost
        // (op_get does it transparently); subsequent calls only need
        // a single GET frame.
        //
        // We do this serially on the first miss per name to keep code
        // simple — the overwhelming common case is "all channels are
        // already warm from a previous bulk call", in which case this
        // loop is a fast vec-build.
        struct WarmReq {
            idx: usize,
            channel: Arc<Channel>,
            warm: super::channel::CachedGet,
            // oneshot the writer-task signal will fulfil
            rx: tokio::sync::oneshot::Receiver<super::decode::Frame>,
            // for decode: cached intro
            intro: Arc<FieldDesc>,
        }
        let mut warm_reqs: Vec<WarmReq> = Vec::with_capacity(n);
        let mut by_server: HashMap<SocketAddr, Vec<usize>> = HashMap::new();
        let mut combined_frames: HashMap<SocketAddr, Vec<u8>> = HashMap::new();
        let mut server_handles: HashMap<SocketAddr, Arc<super::server_conn::ServerConn>> =
            HashMap::new();

        for (idx, name) in pv_names.iter().enumerate() {
            // Resolve channel.
            let channel = match self.channel(name).await {
                Ok(c) => c,
                Err(e) => {
                    results[idx] = Err(e);
                    continue;
                }
            };

            // Cold path on first call — populates cached_get.
            if channel.cached_get.lock().is_none() {
                match op_get(&channel, &[], self.inner.timeout).await {
                    Ok((_intro, value)) => {
                        // Result is delivered via cold path; record it now.
                        results[idx] = Ok(value);
                        continue;
                    }
                    Err(e) => {
                        results[idx] = Err(e);
                        continue;
                    }
                }
            }

            // Take warm state for batching.
            let warm = match channel.cached_get.lock().take() {
                Some(w) => w,
                None => continue,
            };
            let server = match warm.server.upgrade() {
                Some(s) if s.is_alive() => s,
                _ => {
                    // Stale — fall back to cold.
                    match op_get(&channel, &[], self.inner.timeout).await {
                        Ok((_intro, value)) => results[idx] = Ok(value),
                        Err(e) => results[idx] = Err(e),
                    }
                    continue;
                }
            };
            let order = server.byte_order();
            let codec = crate::codec::PvaCodec {
                big_endian: matches!(order, crate::proto::ByteOrder::Big),
            };

            // Refill the slot with a fresh oneshot, build GET frame,
            // append to per-server combined buffer.
            let (tx, rx) = tokio::sync::oneshot::channel();
            *warm.slot.lock() = Some(tx);
            let frame = codec.build_get(warm.sid, warm.ioid);

            by_server
                .entry(server.addr)
                .or_default()
                .push(warm_reqs.len());
            combined_frames
                .entry(server.addr)
                .or_default()
                .extend_from_slice(&frame);
            server_handles
                .entry(server.addr)
                .or_insert_with(|| server.clone());
            let intro = warm.intro.clone();
            warm_reqs.push(WarmReq {
                idx,
                channel,
                warm,
                rx,
                intro,
            });
        }

        // Phase 2: per server, send the combined frame in ONE TCP
        // write. The PVA protocol parses messages by header length so
        // back-to-back GETs in the same buffer are processed in
        // order; the server replies with N data frames that the
        // reader task routes to each ioid's Reusable slot.
        let mut failed_servers: Vec<SocketAddr> = Vec::new();
        for (addr, frame) in combined_frames {
            if let Some(server) = server_handles.get(&addr) {
                if server.send_sync(frame).is_err() {
                    failed_servers.push(addr);
                }
            }
        }
        // Phase-2 send failures must be tracked in their own set,
        // keyed by `warm_reqs` index. The result vector is initialized to
        // `Err(PvaError::Timeout)` for every slot, so using
        // `results[idx].is_err()` as the Phase-3 skip predicate skipped
        // EVERY warm request, not just the failed sends. A successfully
        // sent warm request would never have its response awaited.
        let mut failed_warm: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for addr in &failed_servers {
            if let Some(indices) = by_server.get(addr) {
                for &wi in indices {
                    failed_warm.insert(wi);
                    let req = &warm_reqs[wi];
                    req.warm.slot.lock().take();
                    // the cached warm GET this `warm` was
                    // taken from already became stale (server send
                    // failed). Restoring it later would re-use the
                    // dead (sid, ioid) on the next pvget_many call.
                    // Mark the cache as gone here so the loop below
                    // skips the restore. Mirror pvxs `clientget.cpp:
                    // 188-200`: on send failure, clear the reusable
                    // slot so the next call falls back to a cold
                    // INIT.
                    let _ = req.warm.server.upgrade().map(|srv| {
                        srv.unregister_ioid(req.warm.ioid);
                    });
                    results[req.idx] = Err(PvaError::Protocol("server send failed".into()));
                }
            }
        }

        // Phase 3: sequential await over the per-server signalled
        // oneshots. The reader task fires all oneshots back-to-back
        // as the per-server response burst arrives, so most rx's are
        // already ready by the time we reach them — sequential is
        // effectively as fast as join_all here without pulling in
        // futures-util as a dep.
        use super::decode::OpResponse;
        let op_timeout = self.inner.timeout;
        for (wi, req) in warm_reqs.into_iter().enumerate() {
            let WarmReq {
                idx,
                channel,
                warm,
                rx,
                intro,
            } = req;
            // Skip await + DO NOT restore cache only for
            // warm reqs whose Phase-2 send actually failed. The skip
            // predicate is the dedicated `failed_warm` set — using
            // `results[idx].is_err()` here would skip every warm request
            // because the result vector starts as `Err(Timeout)`.
            // Phase-2 already cleared the oneshot slot and unregistered
            // the IOID for these failed reqs, so the cache is correctly
            // not restored (the `warm` is dropped here).
            if failed_warm.contains(&wi) {
                continue;
            }
            let frame_res = epics_base_rs::runtime::task::timeout(op_timeout, rx).await;
            let value = match frame_res {
                // Decode with no shared cache. The reader side
                // (`flatten_type_cache_markers`) has already flattened every
                // `0xFD`/`0xFE` type-cache marker — including any
                // `any`/`variant` `0xFE <slot>` back-reference inside the GET
                // DATA value — into a self-contained frame in wire order, so
                // this frame embeds its own inline types.
                // If the circuit is already gone the warm op cannot complete.
                Ok(Ok(frame)) => match warm.server.upgrade() {
                    Some(_) => match super::decode::decode_op_response(&frame, Some(&intro)) {
                        Ok(OpResponse::Data(d)) if d.status.is_success() => Ok(d.value),
                        Ok(OpResponse::Data(d)) => Err(PvaError::RemoteError(d.status)),
                        Ok(other) => Err(PvaError::Protocol(format!(
                            "expected GET data, got {other:?}"
                        ))),
                        Err(e) => Err(e),
                    },
                    None => Err(PvaError::Protocol("warm GET server gone".into())),
                },
                Ok(Err(_)) => Err(PvaError::Protocol("warm GET channel closed".into())),
                Err(_) => Err(PvaError::Timeout),
            };
            // only restore cache on a successful DATA
            // response. Pre-fix Rust restored after timeout, decode
            // error, wrong response kind, channel-closed one-shot,
            // and non-success GET status — leaking dead (sid, ioid)
            // reuses. Matches pvxs `clientget.cpp:188-200` which
            // sends DestroyRequest + erases IOID maps on cancel /
            // implicit abandon.
            if value.is_ok() {
                *channel.cached_get.lock() = Some(warm);
            } else {
                // Tear down the abandoned (sid, ioid) — best effort.
                let order = warm.server.upgrade().map(|srv| srv.byte_order());
                if let (Some(srv), Some(order)) = (warm.server.upgrade(), order) {
                    let codec = crate::codec::PvaCodec {
                        big_endian: matches!(order, crate::proto::ByteOrder::Big),
                    };
                    let dr = codec.build_destroy_request(warm.sid, warm.ioid);
                    let _ = srv.send_sync(dr);
                    srv.unregister_ioid(warm.ioid);
                }
            }
            results[idx] = value;
        }

        results
    }

    /// Streaming multi-PV `pvget`: every GET is *started* before any is
    /// awaited (one tokio task per PV), the client's UDP search is hurried
    /// so siblings discover in parallel, and then each PV's result is
    /// handed to `on_result(idx, result)` the instant *its* task
    /// completes — in completion order, not after the whole batch joins.
    /// `idx` is the PV's position in `pv_names` so the caller can recover
    /// the name. A slow or missing PV no longer blocks its siblings from
    /// starting *or* from being reported, and the batch is bounded by one
    /// timeout instead of N.
    ///
    /// This is the per-operation `.result()` callback shape of pvxs
    /// `pvxget` (`tools/get.cpp:102-133`): the tool `exec()`s every op,
    /// installs a result callback that prints the PV the moment its op
    /// finishes, calls `ctxt.hurryUp()`, and waits once on the shared
    /// completion event. A fast PV is therefore visible before a slow or
    /// missing sibling's timeout expires.
    ///
    /// [`Self::pvget_many_full`] is the ordered-collection wrapper over
    /// this for library callers that need an input-order `Vec`; CLIs that
    /// surface partial progress must call this directly so a completed PV
    /// is printed immediately instead of buffered behind a slow sibling.
    pub async fn pvget_many_full_streaming<F>(
        &self,
        pv_names: &[&str],
        request: Option<&crate::pv_request::PvRequestExpr>,
        mut on_result: F,
    ) where
        F: FnMut(usize, PvaResult<PvGetResult>),
    {
        // The fan-out goes through the executor seam, not `tokio::task`. Every
        // future below is a wait on the client's own channels — the sockets
        // belong to the connection tasks — so it is placeable on whichever
        // executor is polling this call, and `JoinSet::spawn` would instead
        // reach for a tokio runtime that a callback-band caller does not have.
        let reactor = epics_base_rs::runtime::task::Reactor::current()
            .expect("pvget_many_full_streaming is awaited on an executor");
        let mut set = epics_base_rs::runtime::task::TaskSet::new();
        for (idx, name) in pv_names.iter().enumerate() {
            let client = self.clone();
            let name = name.to_string();
            let request = request.cloned();
            set.spawn(&reactor, async move {
                let r = match &request {
                    Some(req) => client.pvget_with_request_full(&name, req).await,
                    None => client.pvget_full(&name).await,
                };
                (idx, r)
            });
        }
        // All ops started — hurry discovery so the spawned channels
        // search immediately instead of waiting for the periodic tick
        // (pvxs `ctxt.hurryUp()` after the exec loop).
        self.hurry_up().await;
        while let Some(join_result) = set.join_next().await {
            if let Ok((idx, pva_result)) = join_result {
                on_result(idx, pva_result);
            }
        }
    }

    /// Same as [`Self::pvget_many`] but returns full introspection +
    /// server address for each PV, and applies an optional custom
    /// pvRequest (`None` = the default all-fields GET) to every PV.
    /// Results are in input order; a failed PV maps to `Err`.
    ///
    /// Ordered-collection wrapper over
    /// [`Self::pvget_many_full_streaming`] for library callers that want
    /// the whole batch as one `Vec`. CLIs that print partial progress must
    /// call the streaming method directly so a fast PV is reported as soon
    /// as it completes rather than buffered behind the slowest sibling.
    pub async fn pvget_many_full(
        &self,
        pv_names: &[&str],
        request: Option<&crate::pv_request::PvRequestExpr>,
    ) -> Vec<PvaResult<PvGetResult>> {
        let mut results: Vec<PvaResult<PvGetResult>> = (0..pv_names.len())
            .map(|_| Err(PvaError::Timeout))
            .collect();
        self.pvget_many_full_streaming(pv_names, request, |idx, r| {
            results[idx] = r;
        })
        .await;
        results
    }

    /// Streaming multi-PV `pvinfo`: starts a GET_FIELD describe op for
    /// every PV at once, hurries discovery, then hands each PV's result to
    /// `on_result(idx, result)` the instant its task completes — the same
    /// start-all, report-at-completion structure as
    /// [`Self::pvget_many_full_streaming`], mirroring pvxs
    /// `tools/info.cpp:83-112` (`exec()` every op, install a per-op
    /// `.result()` callback, `hurryUp()`, one shared wait). A slow or
    /// missing PV does not block its siblings from being reported, and the
    /// batch is bounded by one timeout. `idx` is the PV's position in
    /// `pv_names`. [`Self::pvinfo_many_full_with_credentials`] is the
    /// ordered-collection wrapper over this.
    pub async fn pvinfo_many_full_streaming<F>(&self, pv_names: &[&str], mut on_result: F)
    where
        F: FnMut(usize, PvaResult<PvInfoResult>),
    {
        // Same seam as `pvget_many_full_streaming`, for the same reason.
        let reactor = epics_base_rs::runtime::task::Reactor::current()
            .expect("pvinfo_many_full_streaming is awaited on an executor");
        let mut set = epics_base_rs::runtime::task::TaskSet::new();
        for (idx, name) in pv_names.iter().enumerate() {
            let client = self.clone();
            let name = name.to_string();
            set.spawn(&reactor, async move {
                (idx, client.pvinfo_full_with_credentials(&name).await)
            });
        }
        self.hurry_up().await;
        while let Some(join_result) = set.join_next().await {
            if let Ok((idx, r)) = join_result {
                on_result(idx, r);
            }
        }
    }

    /// Concurrent multi-PV `pvinfo`, input-order collection. Ordered
    /// wrapper over [`Self::pvinfo_many_full_streaming`]; a failed PV maps
    /// to `Err`. CLIs that print partial progress must call the streaming
    /// method directly.
    pub async fn pvinfo_many_full_with_credentials(
        &self,
        pv_names: &[&str],
    ) -> Vec<PvaResult<PvInfoResult>> {
        let mut results: Vec<PvaResult<PvInfoResult>> = (0..pv_names.len())
            .map(|_| Err(PvaError::Timeout))
            .collect();
        self.pvinfo_many_full_streaming(pv_names, |idx, r| {
            results[idx] = r;
        })
        .await;
        results
    }
}

/// Snapshot returned by [`PvaClient::report`]. pvxs Report
/// counterpart, summary-only.
#[derive(Debug, Clone)]
pub struct ClientReport {
    /// Channels currently registered in the local cache (any state).
    pub channels_total: usize,
    /// Channels that have a live `ServerConn` and a server-assigned sid.
    pub channels_active: usize,
    /// Channels currently issuing UDP search requests.
    pub channels_searching: usize,
    /// Channels mid-TCP-handshake / mid-CREATE_CHANNEL.
    pub channels_connecting: usize,
    /// Channels explicitly closed via `pvclient.close()`.
    pub channels_closed: usize,
    /// Configured TCP name-server count.
    pub name_servers: usize,
    /// True when the client is in direct-server mode (no UDP search).
    pub direct_mode: bool,
    /// live per-server-connection byte counters. pvxs
    /// `Context::report` parity at the "connection list" level.
    pub connections: Vec<ConnReport>,
}

/// one entry in [`ClientReport::connections`] — a live
/// connection to a PVA server with its byte counters and the channels it
/// carries. Mirrors pvxs `Report::Connection` (netcommon.h:54-68).
#[derive(Debug, Clone)]
pub struct ConnReport {
    /// Server endpoint this connection talks to.
    pub peer: std::net::SocketAddr,
    /// Bytes read off this connection's socket (since the last zeroing
    /// report — `report()` or `report_zeroed(true)`).
    pub bytes_rx: u64,
    /// Bytes written to this connection's socket.
    pub bytes_tx: u64,
    /// Whether the connection is currently alive.
    pub alive: bool,
    /// The channels currently bound to this connection, each with its own
    /// byte counters. pvxs `Report::Connection::channels`
    /// (src/client.cpp:495-496).
    pub channels: Vec<ChanReport>,
}

/// one channel entry in [`ConnReport::channels`]. Mirrors pvxs
/// `Report::Channel` (netcommon.h:43-52): the PV name plus the channel's
/// own transmit/receive byte counters.
#[derive(Debug, Clone)]
pub struct ChanReport {
    /// PV name of the channel.
    pub name: String,
    /// Server-assigned channel id (SID).
    pub sid: u32,
    /// Bytes received for this channel's operations (since the last
    /// zeroing report — `report()` or `report_zeroed(true)`).
    pub bytes_rx: u64,
    /// Bytes transmitted for this channel's operations.
    pub bytes_tx: u64,
}

/// Callback type for [`ConnectBuilder::on_connect`] /
/// [`ConnectBuilder::on_disconnect`].
type ConnectCb = Box<dyn Fn() + Send + Sync + 'static>;

/// Builder for a connect-watcher operation. Configure callbacks then
/// call `exec()` to spawn a watcher task. The returned [`ConnectHandle`]
/// owns the task — drop it to stop watching.
pub struct ConnectBuilder<'a> {
    client: &'a PvaClient,
    pv_name: String,
    on_connect: Option<ConnectCb>,
    on_disconnect: Option<ConnectCb>,
    /// Per-call forced server pinning. Mirrors pvxs
    /// `ConnectBuilder::server(s)` (client.h:952).
    server: Option<SocketAddr>,
    /// Selects how the returned [`ConnectHandle`] stops its watcher on
    /// `Drop`: `true` aborts (prompt), `false` detaches (graceful). Modeled
    /// on pvxs `syncCancel(bool)` (client.h:950), but unlike pvxs this flag
    /// does NOT make `Drop` block — async Rust cannot block in `Drop`. The
    /// synchronous teardown boundary is [`ConnectHandle::wait`]; see the
    /// [`ConnectHandle`] docs.
    sync_cancel: bool,
}

impl<'a> ConnectBuilder<'a> {
    /// Register a callback that fires every time the channel becomes
    /// Active (initial connect + every reconnect).
    pub fn on_connect<F>(mut self, f: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.on_connect = Some(Box::new(f));
        self
    }

    /// Register a callback that fires every time the channel leaves
    /// Active (disconnect + close).
    pub fn on_disconnect<F>(mut self, f: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.on_disconnect = Some(Box::new(f));
        self
    }

    /// Pin the channel to a specific server, bypassing UDP search.
    /// Mirrors pvxs `ConnectBuilder::server(s)`.
    pub fn server(mut self, addr: SocketAddr) -> Self {
        self.server = Some(addr);
        self
    }

    /// Select the `Drop` teardown promptness of the returned
    /// [`ConnectHandle`]: `true` (the default) aborts the watcher, `false`
    /// detaches it. Modeled on pvxs `ConnectBuilder::syncCancel(b)`
    /// (client.h:950), but — unlike pvxs's blocking destructor — neither
    /// value makes `Drop` synchronous; async Rust cannot block in `Drop`.
    /// Use [`ConnectHandle::wait`] for a synchronous teardown boundary. See
    /// the [`ConnectHandle`] docs.
    pub fn sync_cancel(mut self, sync: bool) -> Self {
        self.sync_cancel = sync;
        self
    }

    /// Spawn the connect operation. The returned handle owns the task;
    /// drop it to stop watching. The channel itself stays in the
    /// client's channel map so other ops can keep using it.
    ///
    /// Unlike a passive watcher, this actively drives resolution:
    /// pvxs `Channel::build()` starts initial discovery immediately
    /// (src/client.cpp:347-390) — a searchable channel is pushed into the
    /// initial search bucket, a forced-server channel opens the
    /// connection and sends `createChannels()`. A background driver
    /// task calls `ensure_active()` to start (and, across reconnects,
    /// keep) the connection without issuing any GET/PUT/MONITOR, so
    /// `connect()` is a self-contained connection primitive instead of
    /// depending on some other operation to resolve the same channel.
    pub async fn exec(self) -> PvaResult<ConnectHandle> {
        let sync_cancel = self.sync_cancel;
        let ch = match self.server {
            Some(addr) => {
                self.client
                    .channel_with_forced(&self.pv_name, Some(addr))
                    .await?
            }
            None => self.client.channel(&self.pv_name).await?,
        };
        // pvxs samples the channel state in one worker dispatch right after
        // `Channel::build()` and BEFORE the channel processes its
        // CREATE_CHANNEL response (src/client.cpp:274-282): a freshly built
        // channel is not yet Active, so the first connector fires the
        // synthetic initial `onDisconnect`; an already-active shared channel
        // fires only `onConnect`. Take that snapshot synchronously here,
        // before the resolution driver below can run `ensure_active()` and
        // flip the channel Active. Re-sampling inside the watcher task races
        // the driver: on a fast/direct server the driver connects first and
        // the first connector would skip the initial `on_disconnect`.
        let initial_active = ch.is_active();
        let on_connect = self.on_connect;
        let on_disconnect = self.on_disconnect;
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_task = cancel.clone();

        // Resolution driver: start the search/connect immediately and
        // re-drive after each disconnect, mirroring pvxs where a built
        // channel lives in the search ring and reconnects automatically
        // until the connector is removed. `ensure_active()` for a
        // missing PV stays pending (no server reply) — that future is
        // held under the cancel select, so dropping the handle stops it.
        let ch_drive = ch.clone();
        let cancel_drive = cancel.clone();
        let exec_reactor = ch.reactor().clone();
        let driver = exec_reactor.spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel_drive.cancelled() => break,
                    r = ch_drive.ensure_active() => {
                        // Closed / fatal channel: stop driving. A pending
                        // search (missing PV) never lands here.
                        if r.is_err() {
                            break;
                        }
                    }
                }
                // Connected: wait for the next inactive transition
                // before re-driving so we don't spin while Active.
                tokio::select! {
                    biased;
                    _ = cancel_drive.cancelled() => break,
                    _ = ch_drive.wait_until_inactive() => {}
                }
            }
        });

        let task = exec_reactor.spawn(async move {
            // pvxs invokes the initial callback right after building the
            // channel: `onConnect` if already active, otherwise
            // `onDisconnect` once (src/client.cpp:274-282). A fresh channel
            // is Idle, so a searchable connect fires `on_disconnect`
            // first — the previous passive watcher fired nothing for the
            // initial not-connected state. Use the snapshot captured in
            // `exec()` before the resolution driver started, not a fresh
            // `ch.is_active()` here: re-sampling inside this spawned task
            // races the driver, which on a fast server can set the channel
            // Active first and skip the initial `on_disconnect`.
            let mut was_active = initial_active;
            if was_active {
                if let Some(cb) = &on_connect {
                    cb();
                }
            } else if let Some(cb) = &on_disconnect {
                cb();
            }

            loop {
                // Register the wakeup futures BEFORE re-sampling state:
                // `state_changed` uses `notify_waiters()` (no stored
                // permit), so a transition racing between the sample and
                // the await would be lost otherwise. `enable()` registers
                // the waiter eagerly — same idiom as
                // `Channel::wait_until_inactive`.
                let state_n = ch.state_changed.notified();
                let destroyed_n = ch.server_destroyed_notify().notified();
                tokio::pin!(state_n);
                tokio::pin!(destroyed_n);
                state_n.as_mut().enable();
                destroyed_n.as_mut().enable();

                // Use `is_active()` (not the raw `ChannelState::Active`
                // pattern) so a server-initiated CMD_DESTROY_CHANNEL —
                // which sets `server_destroyed` without firing
                // `state_changed` — flips us to `active_now = false` and
                // lets `on_disconnect` run.
                let active_now = ch.is_active();
                if active_now && !was_active {
                    if let Some(cb) = &on_connect {
                        cb();
                    }
                } else if !active_now && was_active {
                    if let Some(cb) = &on_disconnect {
                        cb();
                    }
                }
                was_active = active_now;

                tokio::select! {
                    _ = state_n => {}
                    // server_destroyed_notify is the explicit DESTROY
                    // signal; without this arm the watcher stays blocked
                    // on state_changed even after the flag flips.
                    _ = destroyed_n => {}
                    _ = cancel_task.cancelled() => break,
                }
            }
            // Tear the resolution driver down with the watcher (it also
            // observes the shared cancel token, so this is belt-and-
            // suspenders).
            driver.abort();
        });

        Ok(ConnectHandle {
            cancel,
            task: Some(task),
            sync_cancel,
        })
    }
}

/// Handle returned by [`ConnectBuilder::exec`]. Drop to stop the
/// watcher task; the channel itself is unaffected.
///
/// **The synchronous teardown boundary is [`ConnectHandle::wait`], never
/// `Drop`.** pvxs `~Connect()` runs `loop.tryInvoke(syncCancel, ...)`
/// (src/client.cpp:255-267) and, with the default `syncCancel(true)`, *blocks
/// the destructor* until the worker has removed the connector and any
/// in-progress callback has completed — so pvxs callback code may borrow
/// caller-owned state. Async Rust cannot block in `Drop` (no `.await`), so
/// that blocking destructor has no `Drop` analog here: `wait()` is the
/// explicit, awaitable equivalent of pvxs `syncCancel(true)` and the only
/// teardown after which no further callback can fire. (Rust callbacks are
/// `'static`, so they cannot borrow a caller stack frame the way a pvxs
/// lambda can — the lifetime hazard pvxs's blocking destructor guards
/// against is already prevented here by the bound.)
///
/// `Drop` is therefore **always asynchronous cancellation** — it never
/// blocks for an in-progress callback. The [`ConnectBuilder::sync_cancel`]
/// flag only selects how promptly the detached watcher stops, not whether
/// `Drop` waits:
///
/// - `sync_cancel(false)`: `Drop` cancels and **detaches** the watcher. The
///   task observes the cancel token at its next `select!` and winds down
///   cleanly on its own.
/// - `sync_cancel(true)` (the builder default): `Drop` cancels and
///   **aborts** the watcher for a prompt stop. Callbacks are synchronous
///   `Fn()` invoked between `await` points, so an abort cannot tear one
///   apart mid-call, but `Drop` still returns before a concurrently-running
///   callback finishes. For a guaranteed await-for-completion boundary, call
///   [`ConnectHandle::wait`].
pub struct ConnectHandle {
    cancel: tokio_util::sync::CancellationToken,
    task: Option<epics_base_rs::runtime::task::TaskHandle<()>>,
    /// Teardown mode selected by [`ConnectBuilder::sync_cancel`]; consulted
    /// only by `Drop` (an explicit `wait()` is always synchronous).
    sync_cancel: bool,
}

impl ConnectHandle {
    /// Cancel the watcher and await its termination. This is the
    /// guaranteed-synchronous teardown — after it returns, the task has
    /// fully stopped and no further callbacks can fire (pvxs
    /// `syncCancel(true)` semantics, exposed explicitly so the caller
    /// chooses when to await). Independent of the builder flag.
    pub async fn wait(mut self) {
        if let Some(t) = self.task.take() {
            self.cancel.cancel();
            let _ = t.await;
        }
    }
}

impl Drop for ConnectHandle {
    fn drop(&mut self) {
        // Single teardown owner: always cancel, then either abort (sync
        // mode — prompt stop) or detach (async mode — let the task wind
        // down at its next select). See the type docs for the pvxs mapping.
        self.cancel.cancel();
        if let Some(t) = self.task.take() {
            if self.sync_cancel {
                t.abort();
            }
            // async mode: drop `t` here, detaching the still-running task;
            // the cancel above guarantees it exits at its next select.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // PVX-42: the default monitor must be non-pipelined, matching pvxs
    // `MonitorBuilder` (`clientmon.cpp:50` `pipeline=false`). The builder
    // default `pipeline_size` is the single origin of the default-path
    // `MonitorFlow::window(..)` decision, so pinning it to 0 keeps every
    // default monitor INIT a plain `0x08` (no credit trailer / ACKs).
    // Pre-fix this defaulted to DEFAULT_PIPELINE_SIZE (4) → every default
    // subscription went out pipelined (`0x88`).
    #[test]
    fn default_monitor_is_non_pipelined() {
        let b = PvaClientBuilder::new();
        assert_eq!(b.pipeline_size, 0, "default monitor must be non-pipelined");
        let flow = super::super::ops_v2::MonitorFlow::window(b.pipeline_size);
        assert!(!flow.pipeline, "window(default) must yield a plain monitor");
        // Opt-in still works: a non-zero builder window pipelines.
        assert!(PvaClientBuilder::new().pipeline_size(4).pipeline_size > 0);
    }

    // ── cache_clear CacheAction policy (pvxs Clean/Drop/Disconnect) ─────────

    /// Must be called from an async test body — see `channel.rs`'s
    /// `make_channel`: the executor is part of a `Channel`, not of its caller.
    fn cache_test_channel(name: &str) -> Arc<Channel> {
        let addr: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        Arc::new(Channel::new_direct(
            crate::test_reactor(),
            name.to_string(),
            "user".into(),
            "host".into(),
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
            ConnectionPool::new(),
            addr,
        ))
    }

    #[epics_macros_rs::epics_test]
    async fn cache_action_clean_keeps_in_use_removes_unused() {
        let mut chans: HashMap<String, Arc<Channel>> = HashMap::new();
        chans.insert("unused".into(), cache_test_channel("unused"));
        let in_use = cache_test_channel("in_use");
        chans.insert("in_use".into(), Arc::clone(&in_use)); // extra live ref

        // Wildcard Clean over the whole map.
        let removed = apply_cache_action(&mut chans, "", CacheAction::Clean);
        assert!(
            !chans.contains_key("unused"),
            "Clean removes a channel with no live external reference"
        );
        assert!(
            chans.contains_key("in_use"),
            "Clean preserves an in-use channel (use_count > 1)"
        );
        assert_eq!(removed.len(), 1, "only the unused channel is removed");
    }

    #[epics_macros_rs::epics_test]
    async fn cache_action_drop_removes_in_use_unconditionally() {
        let mut chans: HashMap<String, Arc<Channel>> = HashMap::new();
        let in_use = cache_test_channel("pv");
        chans.insert("pv".into(), Arc::clone(&in_use));

        let removed = apply_cache_action(&mut chans, "pv", CacheAction::Drop);
        assert!(
            !chans.contains_key("pv"),
            "Drop removes an in-use channel unconditionally"
        );
        assert_eq!(removed.len(), 1);
    }

    #[epics_macros_rs::epics_test]
    async fn cache_action_disconnect_removes_and_closes() {
        use crate::client_native::channel::ChannelState;
        let mut chans: HashMap<String, Arc<Channel>> = HashMap::new();
        let in_use = cache_test_channel("pv");
        chans.insert("pv".into(), Arc::clone(&in_use));

        let removed = apply_cache_action(&mut chans, "pv", CacheAction::Disconnect);
        assert!(!chans.contains_key("pv"), "Disconnect removes the channel");
        assert_eq!(removed.len(), 1);
        // The owner closes Disconnected channels after releasing the map lock;
        // closing drives the channel into the Closed disconnect transition.
        for ch in &removed {
            ch.close();
        }
        assert!(
            matches!(in_use.current_state(), ChannelState::Closed),
            "Disconnect close() drives the channel to the Closed transition"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn cache_action_empty_name_is_wildcard() {
        let mut chans: HashMap<String, Arc<Channel>> = HashMap::new();
        chans.insert("a".into(), cache_test_channel("a"));
        chans.insert("b".into(), cache_test_channel("b"));

        let removed = apply_cache_action(&mut chans, "", CacheAction::Drop);
        assert!(chans.is_empty(), "empty name sweeps every cached channel");
        assert_eq!(removed.len(), 2);
    }

    #[epics_macros_rs::epics_test]
    async fn periodic_cache_cleaner_sweeps_only_unreferenced_channels() {
        // Drive the same loop the client arms on its first cached channel.
        // `tokio`'s "full" feature excludes "test-util", so paused time is
        // unavailable here; a short real interval plus bounded polling keeps the
        // test deterministic without depending on one sleep landing on a tick.
        let client = PvaClient::builder().build();
        let period = Duration::from_millis(25);
        crate::test_reactor().spawn(cache_clean_loop(Arc::downgrade(&client.inner), period));

        // (a) a cached channel with no external Arc is swept within a few ticks.
        client
            .inner
            .channels
            .write()
            .insert("unused".into(), cache_test_channel("unused"));
        let mut swept = false;
        for _ in 0..200 {
            if !client.inner.channels.read().contains_key("unused") {
                swept = true;
                break;
            }
            epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            swept,
            "cleaner removes a cached channel with no live external reference"
        );

        // (b) a channel with an extra live Arc survives repeated sweeps.
        let held = cache_test_channel("held");
        client
            .inner
            .channels
            .write()
            .insert("held".into(), Arc::clone(&held));
        epics_base_rs::runtime::task::sleep(period * 5).await;
        assert!(
            client.inner.channels.read().contains_key("held"),
            "cleaner preserves a channel that still has a live external reference"
        );

        // (c) once that extra reference drops, a later sweep removes it.
        drop(held);
        let mut swept = false;
        for _ in 0..200 {
            if !client.inner.channels.read().contains_key("held") {
                swept = true;
                break;
            }
            epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            swept,
            "cleaner removes the channel on a sweep after its last external ref drops"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn periodic_cache_cleaner_exits_when_client_dropped() {
        let client = PvaClient::builder().build();
        let weak = Arc::downgrade(&client.inner);
        let handle =
            crate::test_reactor().spawn(cache_clean_loop(weak.clone(), Duration::from_millis(25)));

        // Dropping the only PvaClient clone releases the last strong ref; the
        // cleaner's next `Weak::upgrade` fails and the task returns.
        drop(client);
        assert_eq!(weak.strong_count(), 0, "no strong ClientInner ref remains");
        let mut finished = false;
        for _ in 0..200 {
            if handle.is_finished() {
                finished = true;
                break;
            }
            epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            finished,
            "cleaner task exits once the last PvaClient clone is dropped"
        );
    }

    // Regression: a client built with both `share_udp(true)` and a
    // non-empty `name_servers` list must NOT be routed through the
    // process-wide `SHARED_SEARCH_ENGINE` singleton — that singleton is
    // spawned with an empty name-server list and would silently drop the
    // configured TCP name servers. Such a client must fall back to its
    // own per-client engine, which carries the name-server list.
    //
    // The assertions are the client-local observables only: the singleton
    // itself is process-wide and populated by whichever concurrently
    // running test resolves the shared path first, so its emptiness is not
    // observable soundly here. `search_engine()`'s early return makes the
    // two paths mutually exclusive by construction, so the routing decision
    // plus this client's engine cell fully pin the behavior.
    #[epics_macros_rs::epics_test]
    async fn mr_r9_share_udp_with_name_servers_uses_per_client_engine() {
        let ns: SocketAddr = "127.0.0.1:5099".parse().unwrap();
        let client = PvaClient::builder()
            .share_udp(true)
            .name_servers(vec![ns])
            .build();

        assert!(
            !client.uses_shared_search_engine(),
            "share_udp(true) + name_servers must route to the per-client \
             engine, not the shared singleton that drops name servers"
        );

        // Resolving the engine must spawn a per-client engine.
        let _engine = client.search_engine().await.expect("engine spawn");
        assert!(
            client.inner.search.get().is_some(),
            "per-client engine must be populated"
        );
    }

    // Control: `share_udp(true)` with no name servers still shares the
    // process-wide engine — share_udp continues to save the UDP socket.
    #[epics_macros_rs::epics_test]
    async fn mr_r9_share_udp_without_name_servers_uses_shared_engine() {
        let client = PvaClient::builder().share_udp(true).build();
        assert!(
            client.uses_shared_search_engine(),
            "share_udp(true) with no name servers must use the shared engine"
        );
        let _engine = client.search_engine().await.expect("engine spawn");
        assert!(
            client.inner.search.get().is_none(),
            "shared-engine path must not spawn a per-client engine"
        );
    }

    /// Regression at the *context* layer: a one-shot client op
    /// against a never-resolving server must fail at the operation-level
    /// timeout, NOT hang. This is the bypass the 200 ms
    /// `MULTI_SERVER_WINDOW` removal exposed — `pvconnect` (connect-and-
    /// return) and `pvget*`/`pvput*` (byte-order pre-read before encoding
    /// the pvRequest) each awaited a bare `ensure_active()`, which has no
    /// timeout once the inner cap is gone. The fix routes every one-shot
    /// resolve through `ops_v2::ensure_active_with_op_timeout`. Pre-fix
    /// these hung indefinitely (the original symptom was four pvalink
    /// `b4_*` tests timing out at 120 s in epics-bridge-rs).
    ///
    /// Companion to `channel::tests::initial_search_failure_is_owned_by_op_timeout_not_200ms`,
    /// which guards the same invariant at the `ensure_active` layer; this
    /// guards that the public `context.rs` one-shot APIs actually route
    /// through the op-timeout owner rather than bare `ensure_active`.
    // Drives a search that never resolves and asserts the op-timeout owner
    // fires; the search engine's spawned tick `interval` now runs on the
    // reactor-less callback pool under `exec_backend` (§4.2 UDP search is
    // deferred). Reactor-dependent — gated out on the exec backend (stage
    // 3).
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "current_thread")]
    async fn pva_fr_12_one_shot_ops_fail_at_op_timeout_not_hang() {
        use std::time::Duration;
        // Suppress broadcast so no LAN server resolves the channel out
        // from under the test. SAFETY: env vars are process-global;
        // `current_thread` keeps other tokio tests from observing partial
        // state, and the per-client engine reads these at spawn time.
        unsafe {
            std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        }
        let op_timeout = Duration::from_millis(400);
        // share_udp default false → per-client engine spawned with no
        // broadcast targets (AUTO_ADDR_LIST=NO), so searches never resolve.
        let client = PvaClient::builder().timeout(op_timeout).build();

        // 20 s outer guard: if the one-shot op hangs (bypass reintroduced)
        // the guard fires and `.expect` panics with the diagnostic; a
        // bounded op resolves (with an error) at ~op_timeout, well inside.
        let guard = Duration::from_secs(20);

        // 1) connect-and-return path (was bare `ensure_active` in pvconnect).
        let t0 = std::time::Instant::now();
        let connect = tokio::time::timeout(guard, client.pvconnect("PVAFR12:CTX:MISSING:1"))
            .await
            .expect(
                "pvconnect hung past 20 s — one-shot resolve is not bounded \
                 by op_timeout (bypass reintroduced in context.rs)",
            );
        let connect_elapsed = t0.elapsed();
        assert!(connect.is_err(), "pvconnect to a missing PV must error");
        assert!(
            connect_elapsed >= op_timeout,
            "pvconnect must wait the op timeout, got {connect_elapsed:?}"
        );

        // 2) byte-order pre-read path (pvget_with_request resolves the
        //    channel's byte order before encoding the pvRequest).
        let req = crate::pv_request::PvRequestBuilder::new()
            .field("value")
            .build();
        let t1 = std::time::Instant::now();
        let get = tokio::time::timeout(
            guard,
            client.pvget_with_request("PVAFR12:CTX:MISSING:2", &req),
        )
        .await
        .expect(
            "pvget_with_request hung past 20 s — the byte-order pre-read \
             bypassed the op-timeout owner",
        );
        let get_elapsed = t1.elapsed();
        assert!(
            get.is_err(),
            "pvget_with_request to a missing PV must error"
        );
        assert!(
            get_elapsed >= op_timeout,
            "pvget_with_request must wait the op timeout, got {get_elapsed:?}"
        );
    }

    /// `ConnectBuilder::exec()`
    /// must fire the pvxs initial callback for the not-yet-connected
    /// channel WITHOUT depending on a separate GET/PUT/MONITOR
    /// operation. pvxs fires `onDisconnect` right after `Channel::build`
    /// when the channel is not yet active (src/client.cpp:274-282); the old
    /// passive watcher fired nothing for the initial idle state and only
    /// produced events if some other op happened to resolve the channel.
    ///
    /// A forced-server connect to an address with no server keeps the
    /// channel Idle, so no `on_connect` can race in — the only callback
    /// expected is the initial `on_disconnect`, and it must arrive with
    /// no other operation issued on the client.
    #[epics_macros_rs::epics_test]
    async fn pva_rs_48_connect_fires_initial_on_disconnect_without_other_op() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let client = PvaClient::builder()
            .timeout(Duration::from_millis(300))
            .build();

        let fired = std::sync::Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        let handle = client
            .connect("PVA48:NOOP")
            .server(dead)
            .on_disconnect(move || {
                f.fetch_add(1, Ordering::SeqCst);
            })
            .exec()
            .await
            .expect("connect builder");

        // No pvget/pvput/monitor is issued. The initial on_disconnect
        // must still fire promptly. Pre-fix `fired` stayed 0 forever.
        epics_base_rs::runtime::task::timeout(Duration::from_secs(2), async {
            loop {
                if fired.load(Ordering::SeqCst) >= 1 {
                    break;
                }
                epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("initial on_disconnect must fire without a separate operation");

        drop(handle);
    }

    /// `sync_cancel` must actually select the `Drop` teardown path. Built
    /// directly so the test is deterministic and does not depend on a live
    /// server: async mode detaches (the task observes the cancel token and
    /// runs to completion), sync mode aborts (the task is cancelled before
    /// its pending work finishes).
    #[epics_macros_rs::epics_test]
    async fn pva_rs_49_drop_async_detaches_sync_aborts() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio_util::sync::CancellationToken;

        // Async mode: Drop cancels + detaches. The task awaits the cancel
        // token, then finishes its graceful wind-down and sets the flag.
        let cancel = CancellationToken::new();
        let reached_end = Arc::new(AtomicBool::new(false));
        let handle = ConnectHandle {
            cancel: cancel.clone(),
            task: Some(crate::test_reactor().spawn({
                let flag = reached_end.clone();
                let c = cancel.clone();
                async move {
                    c.cancelled().await;
                    epics_base_rs::runtime::task::yield_now().await;
                    flag.store(true, Ordering::SeqCst);
                }
            })),
            sync_cancel: false,
        };
        drop(handle);
        let mut detached_finished = false;
        for _ in 0..100 {
            if reached_end.load(Ordering::SeqCst) {
                detached_finished = true;
                break;
            }
            epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await;
        }
        assert!(
            detached_finished,
            "async sync_cancel(false) Drop must detach and let the task wind down gracefully"
        );

        // Sync mode: Drop cancels + aborts. The task's pending work (a long
        // sleep) is cancelled at the await point before it can set the flag.
        let cancel2 = CancellationToken::new();
        let did_complete = Arc::new(AtomicBool::new(false));
        let handle2 = ConnectHandle {
            cancel: cancel2.clone(),
            task: Some(crate::test_reactor().spawn({
                let flag = did_complete.clone();
                async move {
                    epics_base_rs::runtime::task::sleep(Duration::from_secs(5)).await;
                    flag.store(true, Ordering::SeqCst);
                }
            })),
            sync_cancel: true,
        };
        drop(handle2);
        epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
        assert!(
            !did_complete.load(Ordering::SeqCst),
            "sync sync_cancel(true) Drop must abort the task before its pending work completes"
        );
    }

    /// pvxs `syncCancel(true)` blocks the destructor until any in-progress
    /// callback completes; async Rust cannot block in `Drop`, so only
    /// `wait()` provides that boundary. This proves the contract directly:
    /// with the watcher parked mid-work, dropping a default
    /// (`sync_cancel(true)`) handle returns WITHOUT the parked work having
    /// finished, while `wait()` awaits full task termination.
    #[epics_macros_rs::epics_test]
    async fn connect_handle_only_wait_bounds_callback_completion() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio_util::sync::CancellationToken;

        // Drop path (default sync_cancel = true). The task parks at a gate
        // (standing in for an in-progress callback) and only sets `finished`
        // after the gate opens. `Drop` aborts at the await point and returns
        // immediately, so `finished` must still be false right after it —
        // `Drop` is not a synchronous completion boundary.
        let cancel = CancellationToken::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let handle = ConnectHandle {
            cancel: cancel.clone(),
            task: Some(crate::test_reactor().spawn({
                let g = gate.clone();
                let s = started.clone();
                let f = finished.clone();
                async move {
                    s.store(true, Ordering::SeqCst);
                    g.notified().await;
                    f.store(true, Ordering::SeqCst);
                }
            })),
            sync_cancel: true,
        };
        for _ in 0..100 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await;
        }
        assert!(
            started.load(Ordering::SeqCst),
            "task must have reached the gate"
        );
        drop(handle);
        assert!(
            !finished.load(Ordering::SeqCst),
            "default sync_cancel(true) Drop must NOT block until the parked work completes"
        );

        // wait() path. The task completes its work once cancelled; after
        // `wait()` returns, that work has run — wait() is the boundary.
        let cancel2 = CancellationToken::new();
        let done = Arc::new(AtomicBool::new(false));
        let handle2 = ConnectHandle {
            cancel: cancel2.clone(),
            task: Some(crate::test_reactor().spawn({
                let c = cancel2.clone();
                let d = done.clone();
                async move {
                    c.cancelled().await;
                    d.store(true, Ordering::SeqCst);
                }
            })),
            sync_cancel: false,
        };
        epics_base_rs::runtime::task::timeout(Duration::from_secs(2), handle2.wait())
            .await
            .expect("wait() must terminate synchronously, not hang");
        assert!(
            done.load(Ordering::SeqCst),
            "wait() must await full task termination — the synchronous boundary"
        );
    }

    /// `wait()` is the guaranteed-synchronous teardown regardless of the
    /// builder flag: after it returns the watcher task has stopped and no
    /// further callbacks fire. Exercised against an unreachable forced
    /// server so the watcher only fires its initial `on_disconnect`.
    #[epics_macros_rs::epics_test]
    async fn pva_rs_49_wait_is_synchronous_teardown() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap(); // nothing listens
        let client = PvaClient::builder()
            .server_addr(addr)
            .timeout(Duration::from_secs(3))
            .build();
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let handle = client
            .connect("dut")
            .server(addr)
            .sync_cancel(true)
            .on_disconnect(move || {
                c2.fetch_add(1, Ordering::Relaxed);
            })
            .exec()
            .await
            .expect("exec");

        epics_base_rs::runtime::task::timeout(Duration::from_secs(2), handle.wait())
            .await
            .expect("wait() must terminate synchronously, not hang");

        let after = calls.load(Ordering::Relaxed);
        epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            calls.load(Ordering::Relaxed),
            after,
            "no callbacks may fire after a synchronous wait() teardown"
        );
    }
}
