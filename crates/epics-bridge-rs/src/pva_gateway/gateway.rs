//! Top-level [`PvaGateway`] handle — wires the upstream
//! [`PvaClient`] + [`ChannelCache`] into a downstream [`PvaServer`].
//!
//! Mirrors `pva2pva/p2pApp/gwmain.cpp`'s `configure_*` /
//! `main` flow: build a client to chase upstream PVs, build a server
//! that downstream clients connect to, and route every server op
//! through the cache.

use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::client::PvaClient;
use epics_pva_rs::error::PvaResult;
use epics_pva_rs::server_native::source::{ChannelSource, DynSource};
use epics_pva_rs::server_native::{
    CompositeSource, PvaServer, PvaServerConfig, runtime::ServerReport,
};

use super::channel_cache::{ChannelCache, DEFAULT_CLEANUP_INTERVAL};
use super::control::ControlSource;
use super::error::{GwError, GwResult};
use super::middleware::{
    AclConfig, AclLayer, AuditLayer, AuditSink, Layer, NoopAudit, ReadOnlyLayer,
};
use super::source::GatewayChannelSource;

/// Configuration for [`PvaGateway::start`]. All fields have sensible
/// defaults that mirror pvxs gateway behaviour; override only what
/// you need.
pub struct PvaGatewayConfig {
    /// Upstream PvaClient to use. When `None`, the gateway builds one
    /// with `PvaClient::builder().build()` so it picks up the
    /// `EPICS_PVA_*` environment defaults.
    pub upstream_client: Option<Arc<PvaClient>>,
    /// Downstream server bind config. Use [`PvaServerConfig::isolated`]
    /// for tests that should not pollute the real network.
    pub server_config: PvaServerConfig,
    /// How often the cache prunes idle entries. Pass
    /// [`DEFAULT_CLEANUP_INTERVAL`] (30 s) to match pvxs.
    pub cleanup_interval: Duration,
    /// Per-PV connect timeout: the maximum time `has_pv` /
    /// `get_value` / `subscribe` wait for the upstream IOC to deliver
    /// a first monitor event. Default 5 s.
    pub connect_timeout: Duration,
    /// Hard cap on the number of cached upstream entries. Past this,
    /// new lookups return `GwError::CacheFull` instead of growing the
    /// cache further (PG-G1 DoS defence). Default 50 000.
    pub max_cache_entries: usize,
    /// Hard cap on simultaneous downstream subscriber bridge tasks
    /// across all peers (PG-G3). Default 100 000.
    pub max_subscribers: usize,
    /// G-G2: optional namespace prefix for runtime-control PVs. When
    /// `Some(prefix)`, the gateway exposes a small set of read-only
    /// diagnostic PVs alongside the proxied namespace:
    ///
    /// - `<prefix>:cacheSize` — cached upstream entry count
    /// - `<prefix>:upstreamCount` — alias of cacheSize (pva2pva-compat)
    /// - `<prefix>:liveSubscribers` — current downstream bridge tasks
    /// - `<prefix>:report` — multi-line snapshot of the above
    ///
    /// Mirrors pva2pva `ServerConfig::control_prefix`. `None`
    /// disables the feature so the gateway only proxies upstream PVs.
    /// Override via `EPICS_PVA_GW_CONTROL_PREFIX` env var.
    pub control_prefix: Option<String>,

    /// CRITICAL-1: when `true`, every downstream PUT is rejected by a
    /// [`ReadOnlyLayer`] before it can reach the upstream — a
    /// read-only proxy deployment. Pre-fix the `read_only` intent had
    /// no config surface at all and the middleware was dead code.
    /// Override via `EPICS_PVA_GW_READONLY` (`YES`/`1`/`true`).
    pub read_only: bool,

    /// CRITICAL-1: optional pattern-matched access control. When
    /// `Some`, an [`AclLayer`] filters every op (`has_pv`, GET, PUT,
    /// MONITOR, RPC, `list_pvs`) by the configured glob / regex
    /// deny / allow lists, short-circuiting denied PV names before
    /// they reach the upstream proxy. `None` installs no ACL layer.
    pub acl: Option<AclConfig>,

    /// CRITICAL-1: optional PUT (and, if the sink opts in, GET /
    /// MONITOR / RPC) audit sink. When `Some`, an [`AuditLayer`]
    /// emits a structured [`super::middleware::AuditEvent`] for every
    /// PUT, carrying the downstream peer's credentials and the
    /// outcome. `None` installs no audit layer.
    pub audit: Option<Arc<dyn AuditSink>>,
}

impl Default for PvaGatewayConfig {
    fn default() -> Self {
        // PG-G13: gateways control both ends of the encode path
        // (server-side PVA, downstream pvxs/pvAccessJava clients
        // are common); enable type-cache marker emission so a
        // repeating-shape monitor stream collapses repeated 100+
        // byte introspection blocks to 3-byte 0xFE references.
        // Operators with old pvAccessCPP downstream can override.
        let mut server_config = PvaServerConfig::default();
        server_config.emit_type_cache = true;
        Self {
            upstream_client: None,
            server_config,
            cleanup_interval: DEFAULT_CLEANUP_INTERVAL,
            connect_timeout: Duration::from_secs(5),
            max_cache_entries: super::channel_cache::DEFAULT_MAX_ENTRIES,
            max_subscribers: 100_000,
            control_prefix: None,
            read_only: false,
            acl: None,
            audit: None,
        }
    }
}

impl PvaGatewayConfig {
    /// Apply gateway-specific environment variables on top of an
    /// existing config. Recognised:
    ///
    /// - `EPICS_PVA_GW_CLEANUP_INTERVAL` (seconds, float)
    /// - `EPICS_PVA_GW_CONNECT_TMO` (seconds, float)
    /// - `EPICS_PVA_GW_MAX_CACHE_ENTRIES` (usize)
    /// - `EPICS_PVA_GW_MAX_SUBSCRIBERS` (usize)
    ///
    /// The underlying `PvaServerConfig` is left untouched — call
    /// `.with_env()` on it separately if you also want
    /// `EPICS_PVA[S]_*` applied to the downstream server.
    pub fn with_env(mut self) -> Self {
        if let Ok(s) = std::env::var("EPICS_PVA_GW_CLEANUP_INTERVAL") {
            if let Ok(secs) = s.parse::<f64>() {
                if secs > 0.0 && secs.is_finite() {
                    self.cleanup_interval = Duration::from_secs_f64(secs);
                }
            }
        }
        if let Ok(s) = std::env::var("EPICS_PVA_GW_CONNECT_TMO") {
            if let Ok(secs) = s.parse::<f64>() {
                if secs > 0.0 && secs.is_finite() {
                    self.connect_timeout = Duration::from_secs_f64(secs);
                }
            }
        }
        if let Ok(s) = std::env::var("EPICS_PVA_GW_MAX_CACHE_ENTRIES") {
            if let Ok(n) = s.parse::<usize>() {
                if n > 0 {
                    self.max_cache_entries = n;
                }
            }
        }
        if let Ok(s) = std::env::var("EPICS_PVA_GW_MAX_SUBSCRIBERS") {
            if let Ok(n) = s.parse::<usize>() {
                if n > 0 {
                    self.max_subscribers = n;
                }
            }
        }
        if let Ok(s) = std::env::var("EPICS_PVA_GW_CONTROL_PREFIX") {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                self.control_prefix = Some(trimmed.to_string());
            }
        }
        // CRITICAL-1: read-only deployments are commonly toggled by
        // env in containerised gateways; `acl` / `audit` carry
        // structured state and stay programmatic-only.
        if let Ok(s) = std::env::var("EPICS_PVA_GW_READONLY") {
            let t = s.trim();
            self.read_only =
                t.eq_ignore_ascii_case("YES") || t.eq_ignore_ascii_case("TRUE") || t == "1";
        }
        self
    }
}

/// Running PVA gateway. Hold this for the lifetime of the gateway
/// process; consume it via [`Self::run`] for daemons or drop to
/// tear everything down.
pub struct PvaGateway {
    cache: Arc<ChannelCache>,
    server: PvaServer,
    /// Cloned `ChannelSource` retained so callers can attach the same
    /// gateway resolution to a second server (rare, but pvxs supports
    /// it for dual-protocol setups).
    source: GatewayChannelSource,
}

impl PvaGateway {
    /// Start a gateway. The downstream server begins accepting on the
    /// configured port; upstream channels are opened lazily on the
    /// first downstream search for each PV.
    ///
    /// CRITICAL-1: the `read_only` / `acl` / `audit` config fields are
    /// wired here into the [`super::middleware`] layer chain. The
    /// chain wrapping the proxy source is
    /// `Audit( ReadOnly?( Acl( GatewayChannelSource ) ) )`:
    ///
    /// - `Acl` is innermost so a denied PV name short-circuits before
    ///   the call reaches the proxy (no upstream search for a denied
    ///   PV) — and `list_pvs` is filtered at the proxy boundary.
    /// - `ReadOnly` (only when `read_only`) sits above `Acl` so it
    ///   rejects every PUT regardless of upstream policy.
    /// - `Audit` is outermost so it records the *final* outcome,
    ///   including ACL / read-only denials, not just PUTs that
    ///   reached the upstream.
    pub fn start(config: PvaGatewayConfig) -> GwResult<Self> {
        let client = config
            .upstream_client
            .unwrap_or_else(|| Arc::new(PvaClient::builder().build()));
        let cache = ChannelCache::with_max_entries(
            client,
            config.cleanup_interval,
            config.max_cache_entries,
        );
        let mut source = GatewayChannelSource::new(cache.clone());
        source.connect_timeout = config.connect_timeout;
        source.max_subscribers = config.max_subscribers;

        // Build the middleware chain over a clone of the proxy source.
        // The retained `source` field stays the *unlayered*
        // `GatewayChannelSource` so `set_acf` / `set_asg_resolver` /
        // `prefetch` keep operating on the live policy holder; the
        // ACL/ReadOnly/Audit layers forward `access()` to it.
        //
        // `Acl` and `Audit` are always present (permissive `AclConfig`
        // / `NoopAudit` when not configured) so the final type is
        // uniform; only `read_only` is a genuine branch. The audit
        // sink is type-erased to `Arc<dyn AuditSink>` so the wrapped
        // type does not depend on the concrete sink.
        let acl_cfg = config.acl.clone().unwrap_or_default();
        let audit_sink: Arc<dyn AuditSink> =
            config.audit.clone().unwrap_or_else(|| Arc::new(NoopAudit));

        let acl_layer = AclLayer::new(acl_cfg).layer(source.clone());

        // G-G2: when control_prefix is set, run the proxy and the
        // diagnostic PVs through a CompositeSource. The control source
        // is registered at order=-100 so its PV-name lookups always
        // win over the proxy (which would otherwise try to forward
        // `<prefix>:cacheSize` upstream and time out). The control
        // source is intentionally NOT layered — its PVs are already
        // read-only diagnostics and must stay reachable.
        let server = if config.read_only {
            let layered = AuditLayer::new(audit_sink).layer(ReadOnlyLayer.layer(acl_layer));
            Self::start_server(
                layered,
                &cache,
                &source,
                &config.control_prefix,
                config.server_config,
            )?
        } else {
            let layered = AuditLayer::new(audit_sink).layer(acl_layer);
            Self::start_server(
                layered,
                &cache,
                &source,
                &config.control_prefix,
                config.server_config,
            )?
        };
        Ok(Self {
            cache,
            server,
            source,
        })
    }

    /// Stand up the downstream `PvaServer` over the fully-layered
    /// gateway source, optionally behind a `CompositeSource` that also
    /// hosts the runtime-control diagnostic PVs. Generic over the
    /// concrete layered source type so `start` branches only on
    /// `read_only`.
    fn start_server<S>(
        layered: S,
        cache: &Arc<ChannelCache>,
        source: &GatewayChannelSource,
        control_prefix: &Option<String>,
        server_config: PvaServerConfig,
    ) -> GwResult<PvaServer>
    where
        S: ChannelSource + 'static,
    {
        match control_prefix {
            Some(prefix) if !prefix.is_empty() => {
                let composite = CompositeSource::new();
                let control = ControlSource::new(prefix, cache.clone(), source.clone());
                composite
                    .add_source("__gw_control", Arc::new(control) as DynSource, -100)
                    .map_err(|e| GwError::Other(format!("control source registration: {e}")))?;
                composite
                    .add_source("gateway", Arc::new(layered) as DynSource, 0)
                    .map_err(|e| GwError::Other(format!("gateway source registration: {e}")))?;
                Ok(PvaServer::start(composite, server_config))
            }
            _ => Ok(PvaServer::start(Arc::new(layered), server_config)),
        }
    }

    /// Convenience: loopback-only gateway with auto-picked free
    /// ports. Mirrors `PvaServer::isolated` semantics — useful for
    /// in-process tests where the gateway should not interact with
    /// the real network.
    pub fn isolated(client: Arc<PvaClient>) -> Self {
        let cache = ChannelCache::new(client, DEFAULT_CLEANUP_INTERVAL);
        let source = GatewayChannelSource::new(cache.clone());
        let server = PvaServer::isolated(Arc::new(source.clone()));
        Self {
            cache,
            server,
            source,
        }
    }

    /// Cache handle for diagnostics / iocsh `gwstats`.
    pub fn cache(&self) -> &Arc<ChannelCache> {
        &self.cache
    }

    /// `ChannelSource` clone — useful when you want to attach the
    /// gateway's PV resolution to a separate server (e.g. a
    /// dual-protocol setup).
    pub fn source(&self) -> GatewayChannelSource {
        self.source.clone()
    }

    /// Snapshot of server health: bound ports, alive flags, etc.
    pub fn report(&self) -> ServerReport {
        self.server.report()
    }

    /// Programmatic interrupt — trips `run()` from another task /
    /// thread. Mirrors pvxs `Server::interrupt`.
    pub fn interrupt(&self) {
        self.server.interrupt();
    }

    /// Build a `PvaClient` pre-pointed at the gateway's downstream
    /// listener. Useful for in-process tests where the gateway should
    /// be tested against a known address without UDP discovery.
    /// Mirrors pvxs `Server::clientConfig`.
    pub fn client_config(&self) -> PvaClient {
        self.server.client_config()
    }

    /// Block until SIGINT / SIGTERM, [`Self::interrupt`], or a
    /// subsystem task fails. Mirrors `PvaServer::run`.
    pub async fn run(self) -> PvaResult<()> {
        self.server.run().await
    }

    /// Stop accepting new connections. Existing in-flight ops finish
    /// on their own. Mirrors `PvaServer::stop`.
    pub fn stop(&self) {
        self.server.stop();
    }

    /// Convenience: pre-warm the cache by opening upstream channels
    /// for the listed PV names. Useful in tests that want
    /// determinism, or in production for a "warm-start" sweep.
    pub async fn prefetch(&self, pv_names: &[&str]) {
        for name in pv_names {
            let _ = self.cache.lookup(name, self.source.connect_timeout).await;
        }
    }
}
