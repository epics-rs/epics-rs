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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::RwLock;

use crate::error::{PvaError, PvaResult};
use crate::pvdata::{FieldDesc, PvField};

use super::channel::{Channel, ConnectionPool};
use super::ops_v2::{
    DEFAULT_PIPELINE_SIZE, MonitorEvent, MonitorEventMask, SubscriptionHandle, op_get, op_monitor,
    op_monitor_events, op_monitor_handle, op_monitor_raw_frames_handle, op_put, op_rpc,
};
use super::search_engine::SearchEngine;

#[derive(Debug, Clone)]
pub struct PvGetResult {
    pub pv_name: String,
    pub value: PvField,
    pub introspection: FieldDesc,
    pub server_addr: SocketAddr,
}

/// Builder for [`PvaClient`].
pub struct PvaClientBuilder {
    timeout: Duration,
    server_addr: Option<SocketAddr>,
    user: Option<String>,
    host: Option<String>,
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
}

impl PvaClientBuilder {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            server_addr: None,
            user: None,
            host: None,
            pipeline_size: DEFAULT_PIPELINE_SIZE,
            tls: None,
            name_servers: crate::config::env::name_servers(),
            priority: 0,
            tcp_timeout: Duration::from_secs(40),
            share_udp: false,
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
    /// equivalent. When UDP search yields no responder for a PV, each
    /// name server is tried as a direct-connect candidate (gateway
    /// self-serve case). Replaces any list parsed from env at
    /// `new()` time.
    ///
    /// Note: this is currently a fallback-only treatment. pvxs
    /// additionally sends SEARCH frames over a persistent TCP
    /// connection to each name server and accepts SEARCH_RESPONSE
    /// pointing at a *different* server (redirect). For pure-gateway
    /// scenarios (the gateway answers itself) the simpler fallback
    /// works; redirect-style chains aren't supported yet.
    pub fn name_servers(mut self, servers: Vec<SocketAddr>) -> Self {
        self.name_servers = servers;
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

    /// Override the monitor pipeline size (default 4 — one ack per 4 events).
    /// Set to 0 to disable pipelining.
    pub fn pipeline_size(mut self, n: u32) -> Self {
        self.pipeline_size = n;
        self
    }

    pub fn build(self) -> PvaClient {
        let pool = ConnectionPool::new();
        if self.tls.is_some() {
            pool.set_tls(self.tls.clone());
        }
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
                name_servers: self.name_servers,
                priority: self.priority,
                tcp_timeout: self.tcp_timeout,
                share_udp: self.share_udp,
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
    /// TCP `EPICS_PVA_NAME_SERVERS` fallbacks — used as last-resort
    /// direct-connect candidates when UDP search returns nothing.
    name_servers: Vec<SocketAddr>,
    /// Operation priority hint (0..7). Stored for inspection /
    /// future TCP TOS wiring. pvxs `CommonBuilder::priority`.
    #[allow(dead_code)]
    priority: u8,
    /// Client TCP idle timeout. Stored for inspection / future
    /// keepalive plumbing. pvxs `Config::tcpTimeout`.
    #[allow(dead_code)]
    tcp_timeout: Duration,
    /// True when `build()` was told to share the process-wide search
    /// engine. Routes [`PvaClient::search_engine`] through the static
    /// `SHARED_SEARCH_ENGINE` instead of spawning per-client.
    share_udp: bool,
}

/// Process-wide singleton SearchEngine for `share_udp(true)` clients.
/// Lazily initialized on first use.
static SHARED_SEARCH_ENGINE: tokio::sync::OnceCell<SearchEngine> =
    tokio::sync::OnceCell::const_new();

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

    /// Backwards-compatible: targets a specific TCP port (UDP ignored —
    /// search uses the standard port machinery).
    pub fn with_ports(_udp_port: u16, tcp_port: u16) -> Self {
        let server_addr = SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            tcp_port,
        );
        Self::builder().server_addr(server_addr).build()
    }

    async fn search_engine(&self) -> PvaResult<&SearchEngine> {
        if self.inner.share_udp {
            let engine = SHARED_SEARCH_ENGINE
                .get_or_try_init(|| async { SearchEngine::spawn(Vec::new()).await })
                .await?;
            return Ok(engine);
        }
        if self.inner.search.get().is_none() {
            let engine = SearchEngine::spawn(Vec::new()).await?;
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
                pv_name.to_string(),
                self.inner.user.clone(),
                self.inner.host.clone(),
                self.inner.timeout,
                self.inner.pool.clone(),
                addr,
            ))
        } else {
            let search = self.search_engine().await?.clone();
            Arc::new(Channel::new_with_name_servers(
                pv_name.to_string(),
                self.inner.user.clone(),
                self.inner.host.clone(),
                self.inner.timeout,
                self.inner.pool.clone(),
                search,
                self.inner.name_servers.clone(),
            ))
        };

        if forced.is_some() {
            return Ok(ch);
        }

        let mut map = self.inner.channels.write();
        if let Some(existing) = map.get(pv_name).cloned() {
            return Ok(existing);
        }
        map.insert(pv_name.to_string(), ch.clone());
        Ok(ch)
    }

    /// Resolve `pv_name` against a specific server, bypassing UDP
    /// search and any cached search results. Mirrors pvxs
    /// `ConnectBuilder::server` (client.cpp:208) — the returned future
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

    /// I-3: explicit "connect, then return" — wait for the named
    /// channel to reach `Active` state without issuing a GET/PUT/
    /// MONITOR. Mirrors pvxs `Context::connect(pvname)`. Useful
    /// when an application wants to validate that a PV resolves
    /// before kicking off real ops, or to pre-warm the connection
    /// pool. Returns the resolved server address.
    pub async fn pvconnect(&self, pv_name: &str) -> PvaResult<SocketAddr> {
        let ch = self.channel(pv_name).await?;
        let (server, _sid) = ch.ensure_active().await?;
        Ok(server.addr)
    }

    /// I-3: same as [`Self::pvconnect`] but pinned to a specific
    /// upstream server (skips UDP search). Mirrors pvxs
    /// `ConnectBuilder::server(addr).exec()`.
    pub async fn pvconnect_from(&self, pv_name: &str, server: SocketAddr) -> PvaResult<SocketAddr> {
        let ch = self.channel_with_forced(pv_name, Some(server)).await?;
        let (sc, _sid) = ch.ensure_active().await?;
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

    /// Start a GET and return a [`PvaOperation`] handle the caller can
    /// `wait()`, `cancel()`, or `interrupt()` from any task. F-G8 —
    /// pvxs `Operation` parity for callers that need to start now and
    /// wait later from a different context, or be able to cancel from
    /// outside the awaiting task.
    ///
    /// Drop semantics: dropping the handle aborts the spawned op.
    /// Wrap in `Arc<Mutex<>>` (or share via a channel) if you need to
    /// keep the op alive past the handle's local scope.
    pub fn start_get(
        &self,
        pv_name: &str,
    ) -> crate::client_native::operation::PvaOperation<PvField> {
        let client = self.clone();
        let name = pv_name.to_string();
        crate::client_native::operation::PvaOperation::spawn(
            async move { client.pvget(&name).await },
        )
    }

    /// Start a PUT and return a [`PvaOperation`] handle. F-G8.
    pub fn start_put(
        &self,
        pv_name: &str,
        value_str: &str,
    ) -> crate::client_native::operation::PvaOperation<()> {
        let client = self.clone();
        let name = pv_name.to_string();
        let val = value_str.to_string();
        crate::client_native::operation::PvaOperation::spawn(async move {
            client.pvput(&name, &val).await
        })
    }

    /// Start an RPC and return a [`PvaOperation`] handle. F-G8.
    pub fn start_rpc(
        &self,
        pv_name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> crate::client_native::operation::PvaOperation<(FieldDesc, PvField)> {
        let client = self.clone();
        let name = pv_name.to_string();
        crate::client_native::operation::PvaOperation::spawn(async move {
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
        let ch = self.channel(pv_name).await?;
        let big_endian = matches!(
            ch.ensure_active().await?.0.byte_order,
            crate::proto::ByteOrder::Big
        );
        let bytes = request.encode(big_endian);
        let (_, v) =
            crate::client_native::ops_v2::op_get_raw(&ch, &bytes, self.inner.timeout).await?;
        Ok(v)
    }

    /// Force the search engine into fast-tick mode for one revolution
    /// and reset every pending search's retry deadline. Mirrors pvxs
    /// `Context::hurryUp` (client.cpp:430). Useful when the application
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

    /// Drop cached state for `pv_name`: cancels any in-flight search,
    /// removes the channel from the local map, and forces the next
    /// operation to start a fresh search round. Mirrors pvxs
    /// `Context::cacheClear` (client.cpp:440). Use when an IOC moved
    /// servers and the cached connection target is stale.
    pub async fn cache_clear(&self, pv_name: &str) {
        self.inner.channels.write().remove(pv_name);
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
    /// Beacons and search responses from any listed GUID are silently
    /// dropped. Mirrors pvxs `Context::ignoreServerGUIDs`
    /// (client.cpp:453). Pass an empty `Vec` to clear the list.
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
    ) -> PvaResult<tokio::sync::mpsc::Receiver<crate::client_native::search_engine::Discovered>>
    {
        self.search_engine().await?.discover().await
    }

    /// Graceful shutdown: drop the channel cache, close pooled
    /// connections, and stop accepting new operations. Any subsequent
    /// `pvget` / `pvput` / etc. on this `PvaClient` will fail or
    /// re-establish from scratch (depending on the operation's
    /// reconnect policy). The background search-engine task continues
    /// to run idle in the spawn until the last `PvaClient` clone is
    /// dropped — there's no in-flight work left for it to do.
    /// Mirrors pvxs `Context::close` (client.cpp:422). Idempotent.
    pub fn close(&self) {
        // Drop channels first so their Arc<ServerConn> drops; this
        // gives the pool a chance to skip "still-in-use" connections.
        self.inner.channels.write().clear();
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
            ch.ensure_active().await?.0.byte_order,
            crate::proto::ByteOrder::Big
        );
        let bytes = request.encode(big_endian);
        crate::client_native::ops_v2::op_put_raw(&ch, &bytes, value_str, self.inner.timeout).await
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

    /// F-G12: monitor that surfaces **raw MONITOR DATA body bytes**
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

    /// PG-G9 final form: like [`Self::pvmonitor_raw_frames`] but returns
    /// a [`SubscriptionHandle`] for pause/resume/stats. The bridge
    /// `pva_gateway` uses this to forward downstream watermark events
    /// into upstream pipeline-pause control msgs without an
    /// intermediate decode/encode cycle.
    pub async fn pvmonitor_raw_frames_handle<F>(
        &self,
        pv_name: &str,
        callback: F,
    ) -> PvaResult<SubscriptionHandle>
    where
        F: FnMut(&FieldDesc, bytes::Bytes, crate::proto::ByteOrder) + Send + 'static,
    {
        let ch = self.channel(pv_name).await?;
        Ok(op_monitor_raw_frames_handle(
            ch,
            &[],
            self.inner.pipeline_size,
            callback,
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
        let big_endian = matches!(
            ch.ensure_active().await?.0.byte_order,
            crate::proto::ByteOrder::Big
        );
        let bytes = request.encode(big_endian);
        crate::client_native::ops_v2::op_monitor_raw(
            &ch,
            bytes,
            self.inner.pipeline_size,
            move |_desc, value| callback(value),
        )
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
    pub async fn pvmonitor_handle<F>(
        &self,
        pv_name: &str,
        callback: F,
    ) -> PvaResult<SubscriptionHandle>
    where
        F: FnMut(&FieldDesc, &PvField) + Send + 'static,
    {
        let ch = self.channel(pv_name).await?;
        Ok(op_monitor_handle(
            ch,
            &[],
            self.inner.pipeline_size,
            callback,
        ))
    }

    /// Like [`Self::pvmonitor_handle`] but pinned to `server`. Mirrors
    /// pvxs `MonitorBuilder::server(addr).exec()`. The handle owns its
    /// own per-call channel — it does not affect the shared cache for
    /// `pv_name`.
    pub async fn pvmonitor_handle_from<F>(
        &self,
        pv_name: &str,
        server: SocketAddr,
        callback: F,
    ) -> PvaResult<SubscriptionHandle>
    where
        F: FnMut(&FieldDesc, &PvField) + Send + 'static,
    {
        let ch = self.channel_with_forced(pv_name, Some(server)).await?;
        Ok(op_monitor_handle(
            ch,
            &[],
            self.inner.pipeline_size,
            callback,
        ))
    }

    /// Monitor with typed events (`Connected`/`Data`/`Disconnected`/
    /// `Finished`). Mirrors pvxs's MonitorBuilder + Subscription
    /// exception-based stream API. `mask` defaults are
    /// pvxs-compatible: `maskConnected=true`, `maskDisconnected=false`.
    pub async fn pvmonitor_events<F>(
        &self,
        pv_name: &str,
        mask: MonitorEventMask,
        callback: F,
    ) -> PvaResult<()>
    where
        F: FnMut(MonitorEvent) + Send,
    {
        let ch = self.channel(pv_name).await?;
        op_monitor_events(&ch, &[], self.inner.pipeline_size, mask, callback).await
    }

    /// Fetch the channel's introspection (FieldDesc) using PVA's
    /// dedicated GET_FIELD message — cheaper than [`Self::pvget`]
    /// because no value bytes are transferred. F-G4 (April 2026):
    /// previously implemented as a full GET that discarded the value;
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
        let ch = self.channel(pv_name).await?;
        let intro = crate::client_native::ops_v2::op_get_field(&ch, "", self.inner.timeout).await?;
        let server_addr = match ch.current_state() {
            super::channel::ChannelState::Active { server, .. } => server.addr,
            _ => SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0),
        };
        Ok((intro, server_addr))
    }

    pub async fn pvrpc(
        &self,
        pv_name: &str,
        request_desc: &FieldDesc,
        request_value: &PvField,
    ) -> PvaResult<(FieldDesc, PvField)> {
        let ch = self.channel(pv_name).await?;
        op_rpc(&ch, request_desc, request_value, self.inner.timeout).await
    }

    /// Snapshot of the client's current state — channel cache size,
    /// connection-pool peers, name-server count, etc. Mirrors pvxs
    /// `Context::report` (client.h:599) at the "summary counters"
    /// level. Per-channel detail (peer, RX/TX bytes) isn't surfaced
    /// here yet; pvxs's full Report is heavier than most callers need.
    pub fn report(&self) -> ClientReport {
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
        ClientReport {
            channels_total: channels.len(),
            channels_active: active,
            channels_searching: searching,
            channels_connecting: connecting,
            channels_closed: closed,
            name_servers: self.inner.name_servers.len(),
            direct_mode: self.inner.server_addr.is_some(),
        }
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
            let order = server.byte_order;
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
        for addr in &failed_servers {
            if let Some(indices) = by_server.get(addr) {
                for &wi in indices {
                    let req = &warm_reqs[wi];
                    req.warm.slot.lock().take();
                    // Replace with a closed-error placeholder; we'll
                    // skip the await for these.
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
        for req in warm_reqs {
            let WarmReq {
                idx,
                channel,
                warm,
                rx,
                intro,
            } = req;
            // Skip await if Phase 2 already marked this server failed.
            if results[idx].is_err() {
                *channel.cached_get.lock() = Some(warm);
                continue;
            }
            let frame_res = tokio::time::timeout(op_timeout, rx).await;
            let value = match frame_res {
                Ok(Ok(frame)) => match super::decode::decode_op_response(&frame, Some(&intro)) {
                    Ok(OpResponse::Data(d)) if d.status.is_success() => Ok(d.value),
                    Ok(OpResponse::Data(d)) => {
                        Err(PvaError::Protocol(format!("warm GET data: {:?}", d.status)))
                    }
                    Ok(other) => Err(PvaError::Protocol(format!(
                        "expected GET data, got {other:?}"
                    ))),
                    Err(e) => Err(e),
                },
                Ok(Err(_)) => Err(PvaError::Protocol("warm GET channel closed".into())),
                Err(_) => Err(PvaError::Timeout),
            };
            // Restore cache so the next call also takes the batched path.
            *channel.cached_get.lock() = Some(warm);
            results[idx] = value;
        }

        results
    }

    /// Same as [`Self::pvget_many`] but returns full introspection +
    /// server address for each PV.
    pub async fn pvget_many_full(&self, pv_names: &[&str]) -> Vec<PvaResult<PvGetResult>> {
        let n = pv_names.len();
        let mut set = tokio::task::JoinSet::new();
        for (idx, name) in pv_names.iter().enumerate() {
            let client = self.clone();
            let name = name.to_string();
            set.spawn(async move { (idx, client.pvget_full(&name).await) });
        }
        let mut results: Vec<PvaResult<PvGetResult>> =
            (0..n).map(|_| Err(PvaError::Timeout)).collect();
        while let Some(join_result) = set.join_next().await {
            if let Ok((idx, pva_result)) = join_result {
                results[idx] = pva_result;
            }
        }
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
    /// pvxs `syncCancel(bool)` (client.h:950) — when true, drop on the
    /// returned handle blocks until the watcher task exits; when
    /// false, drop just signals and returns. Currently advisory: our
    /// watcher always tears down within one tick, so the difference
    /// is bounded.
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

    /// Mirrors pvxs `ConnectBuilder::syncCancel(b)`. See the field
    /// docstring for the current semantics.
    pub fn sync_cancel(mut self, sync: bool) -> Self {
        self.sync_cancel = sync;
        self
    }

    /// Spawn the watcher task. The returned handle owns the task; drop
    /// it to stop watching. The channel itself stays in the client's
    /// channel map so other ops can keep using it.
    pub async fn exec(self) -> PvaResult<ConnectHandle> {
        let ch = match self.server {
            Some(addr) => {
                self.client
                    .channel_with_forced(&self.pv_name, Some(addr))
                    .await?
            }
            None => self.client.channel(&self.pv_name).await?,
        };
        let on_connect = self.on_connect;
        let on_disconnect = self.on_disconnect;
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_task = cancel.clone();

        let task = tokio::spawn(async move {
            let mut was_active = false;
            loop {
                // Use `is_active()` (not the raw `ChannelState::Active`
                // pattern) so a server-initiated CMD_DESTROY_CHANNEL
                // — which sets `server_destroyed` without firing
                // `state_changed` — flips us to `active_now = false`
                // and lets `on_disconnect` run. Without this the
                // watcher kept reporting Active until the next
                // ensure_active(), so user-installed disconnect
                // callbacks missed the destroy event entirely.
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
                    _ = ch.state_changed.notified() => {}
                    // server_destroyed_notify is the explicit DESTROY
                    // signal; without this arm the watcher stays
                    // blocked on state_changed even after the flag
                    // flips.
                    _ = ch.server_destroyed_notify().notified() => {}
                    _ = cancel_task.cancelled() => break,
                }
            }
        });

        Ok(ConnectHandle {
            cancel,
            task: Some(task),
        })
    }
}

/// Handle returned by [`ConnectBuilder::exec`]. Drop to stop the
/// watcher task; the channel itself is unaffected.
pub struct ConnectHandle {
    cancel: tokio_util::sync::CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ConnectHandle {
    /// Wait for the watcher task to terminate. Use after dropping
    /// callbacks to ensure no further events fire — pvxs
    /// `syncCancel(true)` semantics, exposed explicitly so the caller
    /// can decide when to await.
    pub async fn wait(mut self) {
        if let Some(t) = self.task.take() {
            self.cancel.cancel();
            let _ = t.await;
        }
    }
}

impl Drop for ConnectHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}
