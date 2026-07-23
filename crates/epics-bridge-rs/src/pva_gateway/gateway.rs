//! Top-level [`PvaGateway`] handle — wires the upstream
//! [`PvaClient`] + [`ChannelCache`] into a downstream [`PvaServer`].
//!
//! Mirrors `pva2pva/p2pApp/gwmain.cpp`'s `configure_*` /
//! `main` flow: build a client to chase upstream PVs, build a server
//! that downstream clients connect to, and route every server op
//! through the cache.

// RTEMS-EXEC-MODEL-ALLOW(2): checked to pass feature-ON under
// --features rtems-exec-model,pva-gateway (the gateway's spawns/timers ride the
// runtime::task seam). The default feature-ON gate omits `pva-gateway`, so re-run
// that combo when touching this module.

use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::access_security::{AccessSecurityConfig, parse_acf};
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::config::env as pva_env;
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

/// Read + parse an ACF file into an [`AccessSecurityConfig`], mapping
/// I/O and parse failures into [`GwError::Other`] so a misconfigured
/// control ACF fails the gateway at startup instead of silently leaving
/// the writable control surface closed.
pub(super) fn load_acf_file(path: &str) -> GwResult<AccessSecurityConfig> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| GwError::Other(format!("control ACF '{path}': {e}")))?;
    parse_acf(&content).map_err(|e| GwError::Other(format!("control ACF '{path}': {e}")))
}

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
    /// cache further (DoS defence). Default 50 000.
    pub max_cache_entries: usize,
    /// Hard cap on simultaneous downstream subscriber bridge tasks
    /// across all peers. Default 100 000.
    pub max_subscribers: usize,
    /// optional namespace prefix for runtime-control PVs. When
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

    /// when `true`, every downstream PUT is rejected by a
    /// [`ReadOnlyLayer`] before it can reach the upstream — a
    /// read-only proxy deployment. Pre-fix the `read_only` intent had
    /// no config surface at all and the middleware was dead code.
    /// Override via `EPICS_PVA_GW_READONLY` (`YES`/`1`/`true`).
    pub read_only: bool,

    /// optional pattern-matched access control. When
    /// `Some`, an [`AclLayer`] filters every op (`has_pv`, GET, PUT,
    /// MONITOR, RPC, `list_pvs`) by the configured glob / regex
    /// deny / allow lists, short-circuiting denied PV names before
    /// they reach the upstream proxy. `None` installs no ACL layer.
    pub acl: Option<AclConfig>,

    /// optional PUT (and, if the sink opts in, GET /
    /// MONITOR / RPC) audit sink. When `Some`, an [`AuditLayer`]
    /// emits a structured [`super::middleware::AuditEvent`] for every
    /// PUT, carrying the downstream peer's credentials and the
    /// outcome. `None` installs no audit layer.
    pub audit: Option<Arc<dyn AuditSink>>,

    /// B6: ACF file gating the writable control RPCs
    /// (`<prefix>:flush` / `:drop` / `:reload`). Read and parsed at
    /// [`PvaGateway::start`]; a flush/drop/reload is then allowed iff
    /// this ACF grants WRITE to the caller. `None` (the default)
    /// leaves the writable control surface closed — every control RPC
    /// is denied — so destructive controls are strictly opt-in. Only
    /// meaningful together with `control_prefix`. Override via
    /// `EPICS_PVA_GW_CONTROL_ACF`.
    pub control_acf_path: Option<String>,

    /// B6: default ACF file path the `<prefix>:reload` RPC re-parses
    /// (for the PROXIED-PV policy) when the caller omits an explicit
    /// `path` argument. `None` ⇒ `reload` requires the `path` argument.
    /// Override via `EPICS_PVA_GW_CONTROL_RELOAD_ACF`.
    pub control_reload_acf_path: Option<String>,
}

impl Default for PvaGatewayConfig {
    fn default() -> Self {
        // gateways control both ends of the encode path
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
            control_acf_path: None,
            control_reload_acf_path: None,
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
    /// - `EPICS_PVA_GW_CONTROL_PREFIX` (string)
    /// - `EPICS_PVA_GW_READONLY` (`YES`/`TRUE`/`1`)
    /// - `EPICS_PVA_GW_CONTROL_ACF` (path — control-RPC authorization)
    /// - `EPICS_PVA_GW_CONTROL_RELOAD_ACF` (path — `:reload` default)
    ///
    /// The underlying `PvaServerConfig` is left untouched — call
    /// `.with_env()` on it separately if you also want
    /// `EPICS_PVA[S]_*` applied to the downstream server.
    pub fn with_env(mut self) -> Self {
        // Both timeout doubles go through the PVA env resolver (pvxs
        // `parse_timeout`): an out-of-range value such as `1e300` is finite
        // and positive, so a bare filter let it reach `from_secs_f64`, which
        // panics above `u64::MAX` seconds. The resolver rejects it and the
        // configured default stands.
        if let Ok(s) = std::env::var("EPICS_PVA_GW_CLEANUP_INTERVAL") {
            if let Some(secs) = pva_env::parse_timeout_env("EPICS_PVA_GW_CLEANUP_INTERVAL", &s) {
                self.cleanup_interval = Duration::from_secs_f64(secs);
            }
        }
        if let Ok(s) = std::env::var("EPICS_PVA_GW_CONNECT_TMO") {
            if let Some(secs) = pva_env::parse_timeout_env("EPICS_PVA_GW_CONNECT_TMO", &s) {
                self.connect_timeout = Duration::from_secs_f64(secs);
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
        // read-only deployments are commonly toggled by
        // env in containerised gateways; `acl` / `audit` carry
        // structured state and stay programmatic-only.
        if let Ok(s) = std::env::var("EPICS_PVA_GW_READONLY") {
            let t = s.trim();
            self.read_only =
                t.eq_ignore_ascii_case("YES") || t.eq_ignore_ascii_case("TRUE") || t == "1";
        }
        if let Ok(s) = std::env::var("EPICS_PVA_GW_CONTROL_ACF") {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                self.control_acf_path = Some(trimmed.to_string());
            }
        }
        if let Ok(s) = std::env::var("EPICS_PVA_GW_CONTROL_RELOAD_ACF") {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                self.control_reload_acf_path = Some(trimmed.to_string());
            }
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
    /// the `read_only` / `acl` / `audit` config fields are
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
        // Carry the configured cache policy to per-credential caches too,
        // not just the shared cache built above.
        source.cleanup_interval = config.cleanup_interval;
        source.per_credential_max_entries = config.max_cache_entries;

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

        // B6: parse the control-RPC authorization ACF up front so a
        // bad path / unparseable file fails the gateway at startup
        // rather than silently leaving the writable surface closed.
        // `None` ⇒ the writable control surface stays closed-by-default.
        let control_acf = match &config.control_acf_path {
            Some(path) => Some(load_acf_file(path)?),
            None => None,
        };

        // when control_prefix is set, run the proxy and the
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
                &source,
                &config.control_prefix,
                control_acf,
                config.control_reload_acf_path,
                config.server_config,
            )?
        } else {
            let layered = AuditLayer::new(audit_sink).layer(acl_layer);
            Self::start_server(
                layered,
                &source,
                &config.control_prefix,
                control_acf,
                config.control_reload_acf_path,
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
        source: &GatewayChannelSource,
        control_prefix: &Option<String>,
        control_acf: Option<AccessSecurityConfig>,
        control_reload_acf_path: Option<String>,
        server_config: PvaServerConfig,
    ) -> GwResult<PvaServer>
    where
        S: ChannelSource + 'static,
    {
        match control_prefix {
            Some(prefix) if !prefix.is_empty() => {
                let composite = CompositeSource::new();
                let mut control = ControlSource::new(prefix, source.clone());
                // B6: gate the writable RPCs through the configured
                // control ACF (closed when absent); wire the `:reload`
                // default path so an operator need not pass it per RPC.
                if let Some(cfg) = control_acf {
                    control = control.with_control_acf(cfg);
                }
                if let Some(path) = control_reload_acf_path {
                    control = control.with_acf_path(path);
                }
                composite
                    .add_source("__gw_control", Arc::new(control) as DynSource, -100)
                    .map_err(|e| GwError::Other(format!("control source registration: {e}")))?;
                composite
                    .add_source("gateway", Arc::new(layered) as DynSource, 0)
                    .map_err(|e| GwError::Other(format!("gateway source registration: {e}")))?;
                Ok(PvaServer::start(composite, server_config)?)
            }
            _ => Ok(PvaServer::start(Arc::new(layered), server_config)?),
        }
    }

    /// Convenience: loopback-only gateway with auto-picked free
    /// ports. Mirrors `PvaServer::isolated` semantics — useful for
    /// in-process tests where the gateway should not interact with
    /// the real network.
    pub fn isolated(client: Arc<PvaClient>) -> GwResult<Self> {
        let cache = ChannelCache::new(client, DEFAULT_CLEANUP_INTERVAL);
        let source = GatewayChannelSource::new(cache.clone());
        let server = PvaServer::isolated(Arc::new(source.clone()))?;
        Ok(Self {
            cache,
            server,
            source,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated_cfg() -> PvaGatewayConfig {
        PvaGatewayConfig {
            server_config: PvaServerConfig::isolated(),
            ..PvaGatewayConfig::default()
        }
    }

    /// R16-32: `EPICS_PVA_GW_*` timeout doubles are finite and positive at
    /// `1e300`, so the old guard passed them to `Duration::from_secs_f64`,
    /// which panics above `u64::MAX` seconds — the gateway aborted at
    /// startup. The shared pvxs `parse_timeout` resolver rejects the value
    /// and the default stands; a whitespace-padded value is accepted, as
    /// pvxs's `stod`/`parseTo` do.
    #[test]
    #[serial_test::serial(epics_env)]
    fn gw_timeout_env_out_of_range_keeps_default() {
        let names = ["EPICS_PVA_GW_CLEANUP_INTERVAL", "EPICS_PVA_GW_CONNECT_TMO"];
        let prev: Vec<_> = names.iter().map(|n| std::env::var(n).ok()).collect();
        unsafe {
            for n in names {
                std::env::set_var(n, "1e300");
            }
        }
        let cfg = PvaGatewayConfig::default().with_env();
        assert_eq!(cfg.cleanup_interval, DEFAULT_CLEANUP_INTERVAL);
        assert_eq!(cfg.connect_timeout, Duration::from_secs(5));
        unsafe {
            for n in names {
                std::env::set_var(n, " 7 ");
            }
        }
        let cfg = PvaGatewayConfig::default().with_env();
        assert_eq!(cfg.cleanup_interval, Duration::from_secs(7));
        assert_eq!(cfg.connect_timeout, Duration::from_secs(7));
        unsafe {
            for (n, p) in names.iter().zip(prev) {
                match p {
                    Some(v) => std::env::set_var(n, v),
                    None => std::env::remove_var(n),
                }
            }
        }
    }

    #[test]
    fn load_acf_file_errors_on_missing_path() {
        let err = load_acf_file("/no/such/pva_gw_control.acf").unwrap_err();
        assert!(
            matches!(err, GwError::Other(_)),
            "missing ACF must surface as GwError::Other, got {err:?}"
        );
    }

    /// B6: an unreadable `control_acf_path` must fail the gateway at
    /// startup, not silently leave the writable control surface closed.
    #[tokio::test]
    async fn start_fails_when_control_acf_path_unreadable() {
        let cfg = PvaGatewayConfig {
            control_prefix: Some("gw".to_string()),
            control_acf_path: Some("/no/such/pva_gw_control.acf".to_string()),
            ..isolated_cfg()
        };
        let res = PvaGateway::start(cfg);
        assert!(
            res.is_err(),
            "unreadable control ACF must fail startup, not silently close the surface"
        );
    }

    /// B6: a valid `control_acf_path` is parsed and threaded into the
    /// control source; the gateway starts.
    #[tokio::test]
    async fn start_succeeds_with_valid_control_acf() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pva_gw_b6_start_{}.acf", std::process::id()));
        std::fs::write(
            &path,
            "ASG(DEFAULT) {\n  RULE(1, READ)\n  RULE(1, WRITE)\n}\n",
        )
        .unwrap();
        let cfg = PvaGatewayConfig {
            control_prefix: Some("gw".to_string()),
            control_acf_path: Some(path.to_str().unwrap().to_string()),
            ..isolated_cfg()
        };
        let gw = PvaGateway::start(cfg).expect("gateway with a valid control ACF must start");
        gw.stop();
        let _ = std::fs::remove_file(&path);
    }
}
