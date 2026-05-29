//! Top-level gateway server.
//!
//! Ties together [`PvCache`], [`UpstreamManager`], [`DownstreamServer`],
//! [`PvList`], [`AccessConfig`], [`Stats`] into a single async daemon.
//!
//! ## Main event loop
//!
//! ```text
//! loop {
//!     tokio::select! {
//!         _ = downstream.run()    => break,    // CaServer drives downstream
//!         _ = cleanup_tick.tick() => cache.cleanup() + upstream.sweep_orphaned()
//!         _ = stats_tick.tick()   => stats.refresh() + publish to gateway:* PVs
//!         _ = heartbeat_tick.tick() => heartbeat counter ++
//!         _ = signal_handler      => reload pvlist / dump report
//!     }
//! }
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use epics_base_rs::server::database::PvDatabase;
use tokio::sync::RwLock;

use crate::error::BridgeResult;

use super::access::AccessConfig;
use super::beacon::BeaconAnomaly;
use super::cache::{CacheTimeouts, PvCache};
// Used by the cfg(unix) signal handler AND the control-PV command owner.
use super::command::CommandHandler;
use super::downstream::DownstreamServer;
use super::putlog::{PutLog, PutLogScope};
use super::pvlist::PvList;
use super::stats::Stats;
use super::upstream::{UpstreamManager, UpstreamManagerConfig};

/// Configuration for [`GatewayServer`].
///
/// `Debug` is implemented manually (see below) rather than derived:
/// the `ca-gateway-tls` `upstream_tls` field holds an
/// `epics_ca_rs::tls::TlsConfig`, which does not implement `Debug`.
/// The manual impl redacts the two TLS fields to a presence marker.
#[derive(Clone)]
pub struct GatewayConfig {
    /// Path to `.pvlist` file.
    pub pvlist_path: Option<PathBuf>,
    /// Inline pvlist content (alternative to file).
    pub pvlist_content: Option<String>,
    /// Path to `.access` (ACF) file.
    pub access_path: Option<PathBuf>,
    /// Optional path to put-event log file.
    pub putlog_path: Option<PathBuf>,
    /// Put-log scope. [`PutLogScope::TrapWrite`] (default) reproduces the
    /// C ca-gateway contract — only granted writes whose matched ACF rule
    /// carries `TRAPWRITE` are logged (`gateVc.cc:236`).
    /// [`PutLogScope::AllWrites`] opts into the broader fail-loud audit
    /// (every attempt, with outcome). Ignored when `putlog_path` is `None`.
    pub putlog_scope: PutLogScope,
    /// Optional path to a command file processed on SIGUSR1 (Unix only).
    /// Each non-comment line is a [`super::command::GatewayCommand`].
    pub command_path: Option<PathBuf>,
    /// Optional path to a file containing literal upstream PV names to
    /// pre-subscribe (one per line). When set, the gateway pre-fetches
    /// each name on startup. Used because lazy resolution is not yet
    /// supported (see `downstream.rs` doc comment).
    pub preload_path: Option<PathBuf>,
    /// CA server port (downstream side). 0 = use EPICS default.
    pub server_port: u16,
    /// Cache timeouts.
    pub timeouts: CacheTimeouts,
    /// Statistics PV namespace, the bare C `-prefix` string (e.g.
    /// `"gateway"` or a host name). The `:` separator is inserted at
    /// publish time, so PVs appear as `<prefix>:<name>` — matching C
    /// `sprintf("%s:%s", stat_prefix, name)` (`gateServer.cc:2097`). Do NOT
    /// include a trailing `:`. Empty disables stats (and control) PVs.
    pub stats_prefix: String,
    /// Cleanup sweep interval.
    pub cleanup_interval: Duration,
    /// Statistics refresh interval.
    pub stats_interval: Duration,
    /// Heartbeat increment interval. `None` disables the heartbeat PV.
    pub heartbeat_interval: Option<Duration>,
    /// Reconnect beacon-anomaly inhibit window (C `-reconnect_inhibit`,
    /// gateServer.cc:414-432). Minimum spacing between upstream-reconnect
    /// beacon anomalies. Defaults to 5 minutes
    /// (C++ `GATE_RECONNECT_INHIBIT`).
    pub reconnect_inhibit: Duration,
    /// Read-only mode: rejects all puts.
    pub read_only: bool,
    /// Optional TLS server config for downstream connections.
    /// Available with the `ca-gateway-tls` feature.
    #[cfg(feature = "ca-gateway-tls")]
    pub tls: Option<std::sync::Arc<epics_ca_rs::tls::ServerConfig>>,
    /// Optional TLS client config for the gateway's *upstream*
    /// connections to the real IOC (B10). Independent of the
    /// downstream [`Self::tls`] termination: a site can run plaintext
    /// downstream + TLS upstream, TLS both ends, or any mix. When
    /// `Some`, the upstream `CaClient` wraps every TCP virtual circuit
    /// to the IOC in TLS. `None` keeps upstream traffic plaintext.
    /// Available with the `ca-gateway-tls` feature.
    #[cfg(feature = "ca-gateway-tls")]
    pub upstream_tls: Option<epics_ca_rs::tls::TlsConfig>,
    /// Override SNI / cert-hostname-verification name for the upstream
    /// TLS connections. Forwarded to `CaClientConfig::tls_server_name`.
    /// When `None`, the upstream client falls back to the IOC's IP
    /// literal (which only validates IP-bound certs). Set this to the
    /// DNS name embedded in the upstream IOC's server certificate.
    /// Available with the `ca-gateway-tls` feature.
    #[cfg(feature = "ca-gateway-tls")]
    pub upstream_tls_server_name: Option<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            pvlist_path: None,
            pvlist_content: None,
            access_path: None,
            putlog_path: None,
            putlog_scope: PutLogScope::default(),
            command_path: None,
            preload_path: None,
            server_port: 0,
            timeouts: CacheTimeouts::default(),
            // Bare C `-prefix`; the `:` separator is inserted at publish.
            stats_prefix: "gateway".to_string(),
            cleanup_interval: Duration::from_secs(10),
            stats_interval: Duration::from_secs(10),
            heartbeat_interval: Some(Duration::from_secs(1)),
            // C++ GATE_RECONNECT_INHIBIT default (BeaconAnomaly::new).
            reconnect_inhibit: Duration::from_secs(60 * 5),
            read_only: false,
            #[cfg(feature = "ca-gateway-tls")]
            tls: None,
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls: None,
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls_server_name: None,
        }
    }
}

// Manual `Debug` — `epics_ca_rs::tls::TlsConfig` (the `upstream_tls`
// field type) does not implement `Debug`, so the derive cannot be
// used. The TLS server config (`tls`) and upstream client config
// (`upstream_tls`) are redacted to a presence marker; certificate
// material has no business in a `Debug` dump anyway.
impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("GatewayConfig");
        d.field("pvlist_path", &self.pvlist_path)
            .field("pvlist_content", &self.pvlist_content)
            .field("access_path", &self.access_path)
            .field("putlog_path", &self.putlog_path)
            .field("putlog_scope", &self.putlog_scope)
            .field("command_path", &self.command_path)
            .field("preload_path", &self.preload_path)
            .field("server_port", &self.server_port)
            .field("timeouts", &self.timeouts)
            .field("stats_prefix", &self.stats_prefix)
            .field("cleanup_interval", &self.cleanup_interval)
            .field("stats_interval", &self.stats_interval)
            .field("heartbeat_interval", &self.heartbeat_interval)
            .field("reconnect_inhibit", &self.reconnect_inhibit)
            .field("read_only", &self.read_only);
        #[cfg(feature = "ca-gateway-tls")]
        {
            d.field("tls", &self.tls.as_ref().map(|_| "<ServerConfig>"));
            d.field(
                "upstream_tls",
                &self.upstream_tls.as_ref().map(|_| "<TlsConfig>"),
            );
            d.field("upstream_tls_server_name", &self.upstream_tls_server_name);
        }
        d.finish()
    }
}

/// The CA gateway server.
///
/// Construct via [`GatewayServer::build`], then call [`GatewayServer::run`]
/// to start the daemon.
pub struct GatewayServer {
    config: GatewayConfig,
    /// `ArcSwap` so live reload (SIGUSR1 `PVL`) can swap the pvlist
    /// atomically without taking a write lock against the put-hot-path.
    /// Every WriteHook reads via `load_full()` (wait-free).
    pvlist: Arc<ArcSwap<PvList>>,
    /// Same hot-reload pattern as `pvlist` — SIGUSR1 `AS` command
    /// stores a fresh `Arc<AccessConfig>` here; in-flight puts that
    /// already loaded the previous one continue with the old rules
    /// (acceptable; matches the pvlist semantics and what C ca-gateway
    /// does on reload).
    access: Arc<ArcSwap<AccessConfig>>,
    cache: Arc<RwLock<PvCache>>,
    shadow_db: Arc<PvDatabase>,
    upstream: Arc<UpstreamManager>,
    downstream: Arc<DownstreamServer>,
    stats: Arc<Stats>,
    putlog: Option<Arc<PutLog>>,
    beacon_anomaly: Arc<BeaconAnomaly>,
    /// Fired by a `quitFlag`/`quitServerFlag` control-PV write so the run
    /// loop tears down gracefully — the CA-triggered analogue of SIGINT.
    shutdown: Arc<tokio::sync::Notify>,
    /// Receiver for control-PV triggers, drained once by the command
    /// owner spawned in [`Self::run`]. `None` when the stats prefix is
    /// empty (control PVs disabled). Wrapped for interior take from the
    /// owned-`self` run loop.
    control_rx: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedReceiver<super::control::ControlEvent>>,
    >,
}

impl GatewayServer {
    /// Build the gateway from configuration.
    ///
    /// Loads pvlist + access files, initializes cache + upstream client,
    /// constructs downstream CA server. Does not start any I/O — call
    /// [`GatewayServer::run`] for that.
    pub async fn build(config: GatewayConfig) -> BridgeResult<Self> {
        // Load .pvlist
        let pvlist = if let Some(path) = &config.pvlist_path {
            let mut p = super::pvlist::parse_pvlist_file(path)?;
            p.resolve_hosts().await;
            p
        } else if let Some(content) = &config.pvlist_content {
            // Inline content. An explicitly-supplied string (even empty)
            // is the operator's literal pvlist: `Some("")` deliberately
            // serves nothing. This is the documented "deny-all" escape
            // hatch — distinct from *no* pvlist config at all (the `else`
            // arm below), which C ca-gateway treats as serve-everything.
            let mut p = super::pvlist::parse_pvlist(content)?;
            p.resolve_hosts().await;
            p
        } else {
            // No pvlist configured. C ca-gateway does NOT deny everything
            // here: `gateResources.cc:318-321` auto-loads a `gateway.pvlist`
            // from the working directory when present, and `gateAs.cc:430-445`
            // otherwise installs an implicit `.* ALLOW` rule so every PV is
            // served (documented at Gateway.html:742-745). Mirror that so a
            // no-argument / default-file deployment is a pass-through gateway
            // rather than a black hole that rejects every downstream search
            // before any upstream lookup. An operator who genuinely wants
            // deny-all sets `pvlist_content: Some(String::new())` (handled
            // above), so the two intents stay distinct.
            const DEFAULT_PVLIST_FILE: &str = "gateway.pvlist";
            let default_path = std::path::Path::new(DEFAULT_PVLIST_FILE);
            if default_path.is_file() {
                tracing::info!(
                    file = DEFAULT_PVLIST_FILE,
                    "ca-gateway-rs: no pvlist configured; loading default \
                     gateway.pvlist from the working directory \
                     (parity with gateResources.cc:318-321)"
                );
                let mut p = super::pvlist::parse_pvlist_file(default_path)?;
                p.resolve_hosts().await;
                p
            } else {
                tracing::info!(
                    "ca-gateway-rs: no pvlist configured and no default \
                     gateway.pvlist found; installing implicit '.* ALLOW' rule \
                     (parity with gateAs.cc:430-445)"
                );
                // Reuse the parser so the implicit rule is byte-for-byte the
                // structure of a hand-written `.* ALLOW` line: an anchored
                // `^.*$` regex, default ASG, and ASL 1 (via
                // `PvListMatch::effective_asl`). No host tokens, so
                // `resolve_hosts()` is a no-op and is skipped.
                super::pvlist::parse_pvlist(".* ALLOW")?
            }
        };
        let pvlist = Arc::new(ArcSwap::from_pointee(pvlist));

        // Load .access (optional). `ArcSwap` for the same lock-free
        // hot-reload pattern as `pvlist`.
        // no .access file defaults to READ-ONLY (C ca-gateway
        // installs `ASG(DEFAULT) { RULE(1,READ) }`, gateAs.cc:735-737) —
        // allow_all() would fail open, forwarding writes upstream.
        let access = if let Some(path) = &config.access_path {
            AccessConfig::from_file(path)?
        } else {
            AccessConfig::read_only()
        };
        let access = Arc::new(ArcSwap::from_pointee(access));

        // Cache + shadow database
        let cache = Arc::new(RwLock::new(PvCache::new()));
        let shadow_db = Arc::new(PvDatabase::new());

        // Normalise the stats namespace once: `config.stats_prefix` is the
        // bare C `-prefix`, and the `:` separator is inserted at publish
        // (C `sprintf("%s:%s", stat_prefix, name)`, gateServer.cc:2097).
        // The single normalised value feeds BOTH the stats PVs and the
        // control PVs so they cannot diverge. Empty stays empty (disabled).
        let stats_prefix = super::stats::prefix_with_separator(&config.stats_prefix);

        // Stats — needed before UpstreamManager so per-PV WriteHook
        // closures can capture the same Arc.
        let stats = Arc::new(Stats::new(stats_prefix.clone()));

        // Put-event logger (optional) — also captured by every WriteHook.
        let putlog = config
            .putlog_path
            .as_ref()
            .map(|p| Arc::new(PutLog::new(p.clone())));

        // Beacon anomaly throttle — constructed BEFORE UpstreamManager
        // so every per-PV WriteHookEnv captures it (the
        // forwarding task fires `request()` on upstream reconnect to
        // tell other gateway-aware downstream clients to re-search).
        // The pulse handle (CaServer beacon-reset Notify) is wired in
        // below once the downstream server has been constructed, so
        // honored `request()` calls actually emit a beacon — without
        // this the throttle just tracked timestamps and
        // `generateBeaconAnomaly` was silent on the wire.
        // Honour the configured reconnect-inhibit window (C
        // `-reconnect_inhibit`, gateServer.cc:414-432) instead of the
        // compiled-in 5-minute default, so the CLI/API knob governs how
        // often an upstream-reconnect beacon anomaly may fire.
        let beacon_anomaly = Arc::new(BeaconAnomaly::with_inhibit(config.reconnect_inhibit));

        // Upstream manager — receives the full WriteHook environment so
        // every PV's hook can enforce read_only / ACL / host-deny / putlog
        // before forwarding the put to upstream.
        let upstream = UpstreamManager::new(UpstreamManagerConfig {
            cache: cache.clone(),
            shadow_db: shadow_db.clone(),
            access: access.clone(),
            pvlist: pvlist.clone(),
            putlog: putlog.clone(),
            putlog_scope: config.putlog_scope,
            stats: stats.clone(),
            read_only: config.read_only,
            // Single connect-timeout owner: the lazy-resolution
            // `wait_connected` gate uses the same configured budget as the
            // cache reaper instead of a local constant (parity with C
            // gateResources::connectTimeout).
            connect_timeout: config.timeouts.connect_timeout,
            beacon_anomaly: beacon_anomaly.clone(),
            // B10: forward the upstream-side TLS config so the
            // gateway's CaClient to the real IOC can also use TLS,
            // independently of downstream TLS termination.
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls: config.upstream_tls.clone(),
            #[cfg(feature = "ca-gateway-tls")]
            upstream_tls_server_name: config.upstream_tls_server_name.clone(),
        })
        .await?;
        let upstream = Arc::new(upstream);

        // Downstream server — wrap each accepted client in TLS when
        // configured. Upstream traffic to the IOC is encrypted
        // independently via `GatewayConfig::upstream_tls` (B10,
        // wired into `UpstreamManager::new` above).
        let downstream = Arc::new({
            #[cfg(feature = "ca-gateway-tls")]
            {
                if let Some(ref tls) = config.tls {
                    DownstreamServer::new_with_tls(
                        shadow_db.clone(),
                        config.server_port,
                        tls.clone(),
                    )
                } else {
                    DownstreamServer::new(shadow_db.clone(), config.server_port)
                }
            }
            #[cfg(not(feature = "ca-gateway-tls"))]
            {
                DownstreamServer::new(shadow_db.clone(), config.server_port)
            }
        });

        // Now that the downstream CaServer is built, snapshot its
        // beacon-reset handle and install it on the throttle so honored
        // `request()` calls actually emit a beacon. Captured BEFORE
        // `downstream.run()` consumes the inner CaServer.
        if let Some(pulse) = downstream.beacon_anomaly_handle().await {
            beacon_anomaly.install_pulse(pulse);
        }

        // Snapshot the downstream CaServer's access-rights notifier and
        // install it on the upstream manager (built above, before the
        // server existed). With it, an upstream IOC write-access flip or an
        // AS/PVL reload re-pushes CA_PROTO_ACCESS_RIGHTS to already-connected
        // clients instead of only updating the hook flag (gateVc.cc:1624-1638
        // postAccessRights). Captured BEFORE `downstream.run()` consumes the
        // inner CaServer; the handle stays valid afterwards.
        if let Some(notifier) = downstream.access_rights_notifier().await {
            upstream.install_access_notifier(notifier);
        }

        // Publish C-compatible control flag PVs (commandFlag, report*Flag,
        // newAsFlag, quitFlag, quitServerFlag) under the stats prefix so
        // operators can trigger command-file execution, reports, reload,
        // and shutdown via `caput` — the cross-platform alternative to
        // SIGUSR1 (gateServer.cc:1877-2102). The receiver is drained by a
        // single command owner in `run()`; `None` when stats are disabled.
        let control_rx = super::control::publish_control_pvs(&shadow_db, &stats_prefix).await;

        let server = Self {
            config,
            pvlist,
            access,
            cache,
            shadow_db,
            upstream,
            downstream,
            stats,
            putlog,
            beacon_anomaly,
            shutdown: Arc::new(tokio::sync::Notify::new()),
            control_rx: std::sync::Mutex::new(control_rx),
        };

        // Pre-register stats PVs in shadow database so downstream can read them
        server.stats.publish_initial(&server.shadow_db).await;

        // Install lazy search resolver: when an unknown name is searched
        // for, check the .pvlist and (if allowed) subscribe upstream.
        server.install_search_resolver().await;

        // Install the per-request existence gate: an already-cached shadow
        // PV must re-run host-scoped `.pvlist` admission before it is
        // advertised to a requester, so a denied host cannot reuse a PV an
        // allowed host already instantiated.
        server.install_existence_gate().await;

        Ok(server)
    }

    /// Install the lazy search resolver into the shadow PvDatabase.
    ///
    /// This implements the equivalent of C++ ca-gateway's
    /// `gateServer::pvExistTest()` (gateServer.cc:1484), but at a
    /// different layer: C++ overrides `caServer::pvExistTest`, while
    /// epics-rs hooks `PvDatabase::set_search_resolver`. The effect is
    /// the same — when a downstream client searches for an unknown
    /// name, the gateway is given a chance to consult the `.pvlist`,
    /// subscribe upstream, and report whether the name became
    /// resolvable.
    ///
    /// Called once during build().
    async fn install_search_resolver(&self) {
        let pvlist = self.pvlist.clone();
        let upstream = self.upstream.clone();
        let stats = self.stats.clone();
        let beacon_anomaly = self.beacon_anomaly.clone();

        let resolver: epics_base_rs::server::database::SearchResolver = std::sync::Arc::new(
            move |name: String,
                  peer: Option<std::net::SocketAddr>|
                  -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> {
                let pvlist = pvlist.clone();
                let upstream = upstream.clone();
                let stats = stats.clone();
                let beacon_anomaly = beacon_anomaly.clone();
                Box::pin(async move {
                    // 1. Check pvlist. when the downstream
                    //    client address is known, evaluate `.pvlist`
                    //    admission host-aware so `DENY FROM host` rules
                    //    are honored at search/create time (parity with C
                    //    ca-gateway `pvExistTest` → `gateAs::findEntry`).
                    //    The host is the bracket-less socket-address form
                    //    (`127.0.0.1`, `::1`) that `is_host_denied`
                    //    expects. A `None` peer (host-less internal
                    //    lookup) falls back to the global rule decision.
                    let m = {
                        let pvlist = pvlist.load_full();
                        match peer {
                            Some(addr) => pvlist.match_name_for_host(&name, &addr.ip().to_string()),
                            None => pvlist.match_name(&name),
                        }
                    };
                    let m = match m {
                        Some(m) => m,
                        None => return false,
                    };

                    // 2. Subscribe upstream — this also adds the PV to the
                    //    shadow database via UpstreamManager::ensure_subscribed.
                    //    ensure_subscribed must only succeed when the
                    //    upstream actually connects, else this positive reply
                    //    black-holes a non-existent PV (C answers does-not-exist
                    //    via gatePvData::death(), gatePv.cc:622).
                    //    Pass the matched ASG/ASL through so the per-PV
                    //    WriteHook can do the right ACL check.
                    //    serve under the searched name (`name`,
                    //    the alias) while connecting upstream to the resolved
                    //    real PV (`m.resolved_name`), so an alias search
                    //    yields a downstream entry under the alias.
                    if upstream
                        .ensure_subscribed(
                            &name,
                            &m.resolved_name,
                            m.asg.clone(),
                            m.effective_asl(),
                        )
                        .await
                        .is_err()
                    {
                        return false;
                    }

                    // 3. trigger a beacon anomaly so other
                    //    gateway-aware downstream clients re-search
                    //    and discover this gateway as the server for
                    //    the just-added PV. Mirrors C++ ca-gateway
                    //    `gateServer::generateBeaconAnomaly` on the
                    //    add-PV path.
                    beacon_anomaly.request();

                    // 4. Stats: count this as a pvExistTest resolution,
                    //    NOT an upstream event. C ca-gateway bumps a
                    //    separate `exist_count` (existTestRate,
                    //    gateServer.cc:1497); routing it through
                    //    record_event() inflated total_events / eventRate
                    //    / clientEventCount with search traffic.
                    stats.record_exist_test();
                    true
                })
            },
        );

        self.shadow_db.set_search_resolver(resolver).await;
    }

    /// Install the per-request existence gate into the shadow PvDatabase.
    ///
    /// The lazy search resolver admits *new* names host-aware, but an
    /// already-instantiated shadow PV is returned straight from the
    /// simple-PV cache by `find_entry_from` / `has_name_from` without
    /// re-checking the requester. That makes a `DENY FROM host` rule
    /// "first creator wins": a host an allowed peer already cached the PV
    /// for could then create a channel to it. This gate closes that
    /// short-circuit — the database consults it before returning a cached
    /// shadow PV — so per-request admission is re-evaluated on every
    /// request, parity with C ca-gateway re-running
    /// `gateAs::findEntry(pvname, host)` and inspecting cache state on each
    /// `pvExistTest` (gateServer.cc:1516-1637). The gate enforces two
    /// things a cached shadow PV must satisfy to be advertised: (1)
    /// host-scoped `.pvlist` admission for the requester, and (2) an
    /// existent upstream connection state (`Inactive`/`Active`) — a
    /// disconnected shadow PV answers does-not-exist without being removed.
    ///
    /// Called once during build().
    async fn install_existence_gate(&self) {
        let pvlist = self.pvlist.clone();
        let cache = self.cache.clone();

        let gate: epics_base_rs::server::database::ExistenceGate = std::sync::Arc::new(
            move |name: String,
                  peer: Option<std::net::SocketAddr>|
                  -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> {
                let pvlist = pvlist.clone();
                let cache = cache.clone();
                Box::pin(async move {
                    // Stats / heartbeat PVs are not gateway-managed (they
                    // are published straight into the shadow DB and never
                    // enter the upstream cache); they always exist and are
                    // subject to neither `.pvlist` admission nor upstream
                    // connection state.
                    let entry = match cache.read().await.get(&name) {
                        Some(e) => e,
                        None => return true,
                    };
                    // Gateway-managed shadow PV: re-run host-scoped
                    // `.pvlist` admission for this requester. A host-less
                    // internal lookup (`peer: None`) falls back to the
                    // global rule decision, matching the search resolver.
                    let admitted = {
                        let pvlist = pvlist.load_full();
                        match peer {
                            Some(addr) => pvlist
                                .match_name_for_host(&name, &addr.ip().to_string())
                                .is_some(),
                            None => pvlist.match_name(&name).is_some(),
                        }
                    };
                    if !admitted {
                        return false;
                    }
                    // Upstream connection state: a disconnected shadow PV
                    // must answer does-not-exist. C `pvExistTest` replies
                    // `pverExistsHere` only for `gatePvInactive` /
                    // `gatePvActive`; `gatePvDisconnect` (and Connecting /
                    // Dead) reply `pverDoesNotExistHere`
                    // (gateServer.cc:1618-1637). The shadow PV stays in the
                    // database (its cached value remains for diagnostics),
                    // but the gate hides it until the upstream monitor
                    // reconnects and flips the state back to existent.
                    entry.read().await.state.is_existent()
                })
            },
        );

        self.shadow_db.set_existence_gate(gate).await;
    }

    /// Pre-subscribe to upstream PVs from the preload file.
    pub async fn preload_pvs(&self) -> BridgeResult<usize> {
        let path = match &self.config.preload_path {
            Some(p) => p,
            None => return Ok(0),
        };
        let content = std::fs::read_to_string(path)?;
        let mut count = 0;

        for line in content.lines() {
            let name = line.trim();
            if name.is_empty() || name.starts_with('#') {
                continue;
            }

            // Resolve through pvlist (alias or allow check)
            let m = {
                let pvlist = self.pvlist.load_full();
                pvlist.match_name(name)
            };
            let m = match m {
                Some(m) => m,
                None => continue, // Denied or not in list
            };

            // serve under the preload-file name (which may be
            // an alias) while connecting to the resolved real PV.
            self.upstream
                .ensure_subscribed(name, &m.resolved_name, m.asg.clone(), m.effective_asl())
                .await?;
            count += 1;
        }

        Ok(count)
    }

    /// Access the shadow database (for stats publication, testing).
    pub fn shadow_database(&self) -> &Arc<PvDatabase> {
        &self.shadow_db
    }

    /// Access the cache (for stats, introspection).
    pub fn cache(&self) -> &Arc<RwLock<PvCache>> {
        &self.cache
    }

    /// Access the pvlist slot (`ArcSwap` for atomic hot reload).
    pub fn pvlist(&self) -> &Arc<ArcSwap<PvList>> {
        &self.pvlist
    }

    /// Access the access-security config slot. SIGUSR1 `AS`
    /// (RELOAD_ACCESS) `store`s a fresh `Arc<AccessConfig>` here;
    /// in-flight puts that already loaded the previous one continue
    /// with the old rules, while later puts pick up the new ones.
    pub fn access(&self) -> &Arc<ArcSwap<AccessConfig>> {
        &self.access
    }

    /// Access stats.
    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    /// Access the put-event logger (if configured).
    pub fn putlog(&self) -> Option<&Arc<PutLog>> {
        self.putlog.as_ref()
    }

    /// Access the beacon anomaly throttle.
    pub fn beacon_anomaly(&self) -> &Arc<BeaconAnomaly> {
        &self.beacon_anomaly
    }

    /// Run the gateway daemon. Blocks until shutdown.
    pub async fn run(self) -> BridgeResult<()> {
        // Pre-load configured upstream PVs
        let preloaded = self.preload_pvs().await?;
        tracing::info!(preloaded, "ca-gateway-rs: preloaded upstream PVs");

        let downstream = self.downstream.clone();
        let cache = self.cache.clone();
        let upstream = self.upstream.clone();
        let stats = self.stats.clone();
        let shadow_db = self.shadow_db.clone();
        let timeouts = self.config.timeouts;
        let cleanup_interval = self.config.cleanup_interval;
        let stats_interval = self.config.stats_interval;
        let heartbeat_interval = self.config.heartbeat_interval;

        // Cleanup task
        let cache_for_cleanup = cache.clone();
        let upstream_for_cleanup = upstream.clone();
        let stats_for_cleanup = stats.clone();
        let cleanup_handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval(cleanup_interval);
            tick.tick().await; // first tick is immediate, skip
            loop {
                tick.tick().await;
                // B5 RATE_STATS: count one gateway run-loop iteration.
                // The cleanup tick is the gateway's canonical periodic
                // maintenance loop — the tokio analogue of the C++
                // fdManager event-loop pass that drives gateServer::
                // loopCount.
                stats_for_cleanup.record_loop();
                let removed = cache_for_cleanup.write().await.cleanup(&timeouts).await;
                if !removed.is_empty() {
                    upstream_for_cleanup.sweep_orphaned().await;
                    tracing::info!(evicted = removed.len(), "ca-gateway-rs: cache eviction");
                }
            }
        });

        // Stats refresh task
        let cache_for_stats = cache.clone();
        let upstream_for_stats = upstream.clone();
        let stats_for_refresh = stats.clone();
        let db_for_stats = shadow_db.clone();
        let stats_handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval(stats_interval);
            tick.tick().await;
            loop {
                tick.tick().await;
                let cache_size = cache_for_stats.read().await.len();
                let upstream_count = upstream_for_stats.subscription_count();
                stats_for_refresh
                    .refresh(&cache_for_stats, &db_for_stats, cache_size, upstream_count)
                    .await;
            }
        });

        // Heartbeat task
        let heartbeat_handle = if let Some(period) = heartbeat_interval {
            let stats_hb = stats.clone();
            let db_hb = shadow_db.clone();
            Some(tokio::spawn(async move {
                let mut tick = tokio::time::interval(period);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    stats_hb.heartbeat_tick(&db_hb).await;
                }
            }))
        } else {
            None
        };

        // SIGUSR1 → command file processing (Unix only)
        let signal_handle = self.spawn_signal_handler();

        // Control-PV command owner: drains commandFlag/report*Flag/
        // newAsFlag/quitFlag writes and dispatches each through the SAME
        // CommandHandler the SIGUSR1 path uses — one command owner for
        // every trigger source (mirrors C routing all flags through the
        // gateServer main loop). `quit*` fires `self.shutdown`.
        let control_handle = {
            let taken = self.control_rx.lock().unwrap().take();
            taken.map(|rx| {
                let handler = CommandHandler::new(
                    self.cache.clone(),
                    self.pvlist.clone(),
                    self.access.clone(),
                    self.config.pvlist_path.clone(),
                    self.config.access_path.clone(),
                )
                .with_upstream(self.upstream.clone())
                .with_beacon_anomaly(self.beacon_anomaly.clone());
                super::control::spawn_control_owner(
                    rx,
                    handler,
                    self.shadow_db.clone(),
                    self.config.command_path.clone(),
                    self.shutdown.clone(),
                )
            })
        };

        // Connection event subscriber.
        //
        // - `Connected`/`Disconnected`: per-host stats tracking (matches
        //   the C ca-gateway "connected client count" diagnostic PV).
        // - `ChannelCreated`/`ChannelCleared`: per-PV subscriber tracking
        //   for the cache FSM. A channel-create flips the corresponding
        //   `GwPvEntry` from `Inactive` → `Active`; a channel-clear
        //   reverses the transition once subscribers drop to zero.
        //   Without this wiring the `Active` state is unreachable and
        //   the C-gateway parity is incomplete (see review §3).
        //
        // Subscriber-id passed into `add_subscriber`/`remove_subscriber`
        // is a synthetic hash of `(peer, pv_name, cid)`. Including the
        // CA cid is critical: a single client can open multiple
        // channels to the same PV (camonitor + caget loop, etc.), and
        // hashing only `(peer, pv_name)` would collapse N channels
        // into one refcount slot — `Active` would flip back to
        // `Inactive` on the first CLEAR even with channels still open.
        //
        // `Lagged` is handled by replay (B11): `connection_events`
        // returns a `ReplayingReceiver` that, on a broadcast lag,
        // recovers the exact missed events from a bounded ring buffer
        // before resuming the live stream. The consumer below never
        // sees a silent gap, so the per-PV refcounts stay correct.
        // Any genuinely unrecoverable hole — a lag that overflows the
        // replay log, or the forwarder skipping a span on its own raw
        // lag — surfaces as `ConnEventRecv::GapTruncated` and is logged.
        let conn_rx = downstream.connection_events().await;
        let conn_handle = if let Some(mut rx) = conn_rx {
            let stats_for_conn = stats.clone();
            let cache_for_conn = self.cache.clone();
            Some(tokio::spawn(async move {
                use super::downstream::ConnEventRecv;
                use epics_ca_rs::server::ServerConnectionEvent;
                use std::collections::HashMap;
                use std::collections::hash_map::DefaultHasher;
                use std::hash::{Hash, Hasher};
                fn synthetic_sid(peer: std::net::SocketAddr, pv: &str, cid: u32) -> u32 {
                    let mut h = DefaultHasher::new();
                    peer.hash(&mut h);
                    pv.hash(&mut h);
                    cid.hash(&mut h);
                    h.finish() as u32
                }
                // Per-peer subscription registry. On a hard close
                // (TCP RST, process kill) the underlying CaServer may
                // emit only `Disconnected(addr)` without the matching
                // ChannelCleared events — leaving cache subscriber
                // refcounts inflated and entries stuck in the Active
                // state. We mirror every Created here and drain the
                // peer's entries on Disconnected as a safety net.
                let mut peer_channels: HashMap<std::net::SocketAddr, Vec<(String, u32)>> =
                    HashMap::new();
                loop {
                    let event = match rx.recv().await {
                        ConnEventRecv::Event(ev) => ev,
                        ConnEventRecv::GapTruncated { missed } => {
                            // The event sequence jumped — either a lag
                            // overflowed the replay ring buffer or the
                            // forwarder skipped a span on its own raw
                            // lag. Either way those events are
                            // unrecoverable. Far rarer than the
                            // channel-depth lag that replay covers;
                            // warn so the operator notices.
                            tracing::warn!(
                                missed,
                                "ca-gateway-rs: connection-event lag exceeded the \
                                 replay log — per-PV refcount may be transiently \
                                 off until the next CREATE/CLEAR cycle"
                            );
                            continue;
                        }
                        ConnEventRecv::Closed => break,
                    };
                    match event {
                        ServerConnectionEvent::Connected(addr) => {
                            stats_for_conn.record_host(&addr.ip().to_string()).await;
                        }
                        ServerConnectionEvent::Disconnected(addr) => {
                            stats_for_conn.forget_host(&addr.ip().to_string()).await;
                            if let Some(channels) = peer_channels.remove(&addr) {
                                let cache = cache_for_conn.read().await;
                                for (pv_name, sid) in channels {
                                    if let Some(entry) = cache.get(&pv_name) {
                                        entry.write().await.remove_subscriber(sid);
                                    }
                                }
                            }
                        }
                        ServerConnectionEvent::ChannelCreated { peer, pv_name, cid } => {
                            let sid = synthetic_sid(peer, &pv_name, cid);
                            if let Some(entry) = cache_for_conn.read().await.get(&pv_name) {
                                entry.write().await.add_subscriber(sid);
                            }
                            peer_channels.entry(peer).or_default().push((pv_name, sid));
                        }
                        ServerConnectionEvent::ChannelCleared { peer, pv_name, cid } => {
                            let sid = synthetic_sid(peer, &pv_name, cid);
                            if let Some(entry) = cache_for_conn.read().await.get(&pv_name) {
                                entry.write().await.remove_subscriber(sid);
                            }
                            if let Some(channels) = peer_channels.get_mut(&peer) {
                                channels.retain(|(p, s)| !(p == &pv_name && *s == sid));
                                if channels.is_empty() {
                                    peer_channels.remove(&peer);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }))
        } else {
            None
        };

        // Run downstream CaServer until either the server returns
        // (fatal I/O) or SIGINT/SIGTERM arrives (graceful
        // shutdown). On signal we tear down upstream subscriptions
        // first so the upstream IOC sees a clean disconnect, then
        // abort the auxiliary tasks.
        let shutdown = self.shutdown.clone();
        let downstream_result = {
            let downstream_run = downstream.run();
            tokio::pin!(downstream_run);
            let ctrl_c = tokio::signal::ctrl_c();
            tokio::pin!(ctrl_c);
            tokio::select! {
                r = &mut downstream_run => r,
                _ = &mut ctrl_c => {
                    tracing::info!("ca-gateway-rs: SIGINT received — shutting down");
                    self.upstream.shutdown().await;
                    Ok(())
                }
                // quitFlag/quitServerFlag control-PV write (a stored
                // Notify permit is honored even if it fired before we
                // reached this select). Same teardown as SIGINT.
                _ = shutdown.notified() => {
                    tracing::info!("ca-gateway-rs: shutdown requested via control PV");
                    self.upstream.shutdown().await;
                    Ok(())
                }
            }
        };

        // Cleanup
        cleanup_handle.abort();
        stats_handle.abort();
        if let Some(h) = heartbeat_handle {
            h.abort();
        }
        if let Some(h) = signal_handle {
            h.abort();
        }
        if let Some(h) = control_handle {
            h.abort();
        }
        if let Some(h) = conn_handle {
            h.abort();
        }
        // B11: stop the connection-event forwarder task spawned by
        // `connection_events()` so it does not outlive the server.
        downstream.stop_connection_events().await;

        downstream_result
    }

    /// Spawn a Unix SIGUSR1 watcher that re-reads the command file.
    /// Returns None on non-Unix or when no command file is configured.
    #[cfg(unix)]
    fn spawn_signal_handler(&self) -> Option<tokio::task::JoinHandle<()>> {
        let cmd_path = self.config.command_path.clone()?;
        let pvlist_path = self.config.pvlist_path.clone();
        let access_path = self.config.access_path.clone();
        let cache = self.cache.clone();
        let pvlist = self.pvlist.clone();
        let access = self.access.clone();
        let upstream = self.upstream.clone();
        let beacon_anomaly = self.beacon_anomaly.clone();

        Some(tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigusr1 = match signal(SignalKind::user_defined1()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "ca-gateway-rs: failed to install SIGUSR1 handler");
                    return;
                }
            };
            let handler = CommandHandler::new(cache, pvlist, access, pvlist_path, access_path)
                .with_upstream(upstream)
                .with_beacon_anomaly(beacon_anomaly);
            tracing::info!(
                command_file = %cmd_path.display(),
                "ca-gateway-rs: SIGUSR1 handler armed"
            );
            loop {
                if sigusr1.recv().await.is_none() {
                    break;
                }
                tracing::info!("ca-gateway-rs: SIGUSR1 received — processing command file");
                match handler.process_file(&cmd_path).await {
                    Ok(out) => {
                        if !out.is_empty() {
                            tracing::info!(output = %out.trim_end(), "ca-gateway-rs: command output");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "ca-gateway-rs: command file error");
                    }
                }
            }
        }))
    }

    /// Stub for non-Unix platforms (no SIGUSR1).
    #[cfg(not(unix))]
    fn spawn_signal_handler(&self) -> Option<tokio::task::JoinHandle<()>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_with_minimal_config() {
        let config = GatewayConfig {
            pvlist_content: Some("".to_string()),
            ..Default::default()
        };
        let server = GatewayServer::build(config).await;
        assert!(server.is_ok(), "build failed: {:?}", server.err());
    }

    #[tokio::test]
    async fn build_with_inline_pvlist() {
        let config = GatewayConfig {
            pvlist_content: Some(
                r#"
                EVALUATION ORDER ALLOW, DENY
                Beam:.* ALLOW BeamGroup 1
                test.* DENY
                "#
                .to_string(),
            ),
            ..Default::default()
        };
        let server = GatewayServer::build(config).await.unwrap();
        let pvlist = server.pvlist().load_full();
        assert!(pvlist.match_name("Beam:current").is_some());
        assert!(pvlist.match_name("test:foo").is_none());
    }

    /// B10: `GatewayServer::build` must succeed when an upstream TLS
    /// client config is supplied. The config flows
    /// `GatewayConfig::upstream_tls` → `UpstreamManagerConfig` →
    /// `CaClient::new_with_config`. No upstream IOC is contacted at
    /// build time, so this exercises the plumbing end to end.
    #[cfg(feature = "ca-gateway-tls")]
    #[tokio::test]
    async fn build_with_upstream_tls() {
        use epics_ca_rs::tls::{Roots, TlsConfig};
        let config = GatewayConfig {
            pvlist_content: Some("".to_string()),
            upstream_tls: Some(TlsConfig::client_from_roots(Roots::empty())),
            upstream_tls_server_name: Some("ioc.example.com".to_string()),
            ..Default::default()
        };
        let server = GatewayServer::build(config).await;
        assert!(
            server.is_ok(),
            "build with upstream TLS failed: {:?}",
            server.err()
        );
    }

    #[tokio::test]
    async fn build_no_pvlist_installs_implicit_allow_all() {
        // No pvlist path AND no inline content: C ca-gateway serves every
        // PV through an implicit `.* ALLOW` rule (gateAs.cc:430-445), not a
        // deny-all. Assumes no `gateway.pvlist` exists in the crate's
        // working directory (it does not) — the default-file probe is the
        // thin `is_file()` branch above, exercised by `parse_pvlist_file`.
        let config = GatewayConfig::default();
        let server = GatewayServer::build(config).await.unwrap();
        let pvlist = server.pvlist().load_full();
        assert!(
            pvlist.match_name("Any:Random:PV").is_some(),
            "no-pvlist gateway must serve all PVs via implicit .* ALLOW"
        );
        assert!(pvlist.match_name("another.unlikely.name").is_some());
    }

    #[tokio::test]
    async fn build_empty_inline_content_stays_deny_all() {
        // An explicitly-supplied empty pvlist is the operator's deny-all
        // request: it must NOT be promoted to the implicit allow-all that
        // the no-config path installs. This pins the two intents apart.
        let config = GatewayConfig {
            pvlist_content: Some(String::new()),
            ..Default::default()
        };
        let server = GatewayServer::build(config).await.unwrap();
        let pvlist = server.pvlist().load_full();
        assert!(
            pvlist.match_name("Any:Random:PV").is_none(),
            "explicit empty pvlist must deny all (distinct from no-config)"
        );
    }

    #[tokio::test]
    async fn existence_gate_hides_cached_shadow_pv_from_denied_host() {
        // `PV.*` is allowed in general but denied from 127.0.0.1. Even
        // after an allowed host instantiated the shadow PV, a denied
        // host's search/create must answer does-not-exist — closing the
        // "first creator wins" bypass (parity with C re-running
        // gateAs::findEntry per pvExistTest).
        use std::net::SocketAddr;

        let config = GatewayConfig {
            pvlist_content: Some("PV.* ALLOW\nPV.* DENY FROM 127.0.0.1\n".to_string()),
            ..Default::default()
        };
        let server = GatewayServer::build(config).await.unwrap();

        // Simulate an allowed host having already cached PV:x: register
        // the shadow PV and mark its cache entry connected (Active).
        server
            .shadow_db
            .add_pv("PV:x", epics_base_rs::types::EpicsValue::Double(0.0))
            .await
            .unwrap();
        {
            let mut cache = server.cache.write().await;
            let entry = cache.get_or_create("PV:x");
            entry
                .write()
                .await
                .set_state(crate::ca_gateway::PvState::Active);
        }

        let denied: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let allowed: SocketAddr = "192.0.2.5:5064".parse().unwrap();

        // Denied host: not advertised on either path.
        assert!(!server.shadow_db.has_name_from("PV:x", Some(denied)).await);
        assert!(
            server
                .shadow_db
                .find_entry_from("PV:x", Some(denied))
                .await
                .is_none()
        );

        // Allowed host: still served from the cache.
        assert!(server.shadow_db.has_name_from("PV:x", Some(allowed)).await);
        assert!(
            server
                .shadow_db
                .find_entry_from("PV:x", Some(allowed))
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn existence_gate_hides_disconnected_shadow_pv() {
        // A shadow PV whose upstream has disconnected must answer
        // does-not-exist to a new search/create even though the PV remains
        // in `simple_pvs` and the requesting host is allowed — parity with
        // C pvExistTest returning pverDoesNotExistHere for gatePvDisconnect.
        use std::net::SocketAddr;

        let config = GatewayConfig {
            pvlist_content: Some("PV.* ALLOW\n".to_string()),
            ..Default::default()
        };
        let server = GatewayServer::build(config).await.unwrap();
        server
            .shadow_db
            .add_pv("PV:y", epics_base_rs::types::EpicsValue::Double(0.0))
            .await
            .unwrap();

        let peer: SocketAddr = "192.0.2.5:5064".parse().unwrap();

        // Connected (Active): the allowed host is served.
        {
            let mut cache = server.cache.write().await;
            cache
                .get_or_create("PV:y")
                .write()
                .await
                .set_state(crate::ca_gateway::PvState::Active);
        }
        assert!(server.shadow_db.has_name_from("PV:y", Some(peer)).await);

        // Upstream disconnects: still in simple_pvs, host still allowed,
        // but the cache state is now Disconnect → does-not-exist.
        {
            let cache = server.cache.read().await;
            cache
                .get("PV:y")
                .unwrap()
                .write()
                .await
                .set_state(crate::ca_gateway::PvState::Disconnect);
        }
        assert!(!server.shadow_db.has_name_from("PV:y", Some(peer)).await);
        assert!(
            server
                .shadow_db
                .find_entry_from("PV:y", Some(peer))
                .await
                .is_none()
        );

        // Reconnect (Inactive): advertised again without re-registration.
        {
            let cache = server.cache.read().await;
            cache
                .get("PV:y")
                .unwrap()
                .write()
                .await
                .set_state(crate::ca_gateway::PvState::Inactive);
        }
        assert!(server.shadow_db.has_name_from("PV:y", Some(peer)).await);
    }

    #[tokio::test]
    async fn build_unknown_acf_path_returns_error() {
        let config = GatewayConfig {
            access_path: Some(PathBuf::from("/nonexistent/file.acf")),
            ..Default::default()
        };
        let result = GatewayServer::build(config).await;
        assert!(result.is_err());
    }
}
