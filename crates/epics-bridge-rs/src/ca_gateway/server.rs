//! Top-level gateway server.
//!
//! Ties together [`PvCache`], `UpstreamManager`, `DownstreamServer`,
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
// `CommandHandler` (the control-PV command owner) is used on all targets;
// `GatewayCommand` is referenced only by the cfg(unix) signal handler, so
// importing it unconditionally is an unused import on non-Unix.
#[cfg(tokio_backend)]
use super::command::CommandHandler;
#[cfg(unix)]
use super::command::GatewayCommand;
#[cfg(tokio_backend)]
use super::downstream::DownstreamServer;
use super::putlog::{PutLog, PutLogScope};
use super::pvlist::{PolicyHost, PvList};
use super::stats::Stats;
#[cfg(tokio_backend)]
use super::upstream::UpstreamManager;
use super::upstream::UpstreamManagerConfig;

/// Whether the gateway caches upstream values or forwards each read.
///
/// Mirrors C ca-gateway's `cacheMode` (`gateResources.h:116-117`,
/// default `true` — caching on; `-no_cache` clears it,
/// `gateway.cc:238/1162`).
///
/// - [`CacheMode::Cached`] (default): every resolved PV holds a
///   persistent upstream monitor; downstream GETs are served from the
///   last cached monitor value (the shadow PV's stored snapshot). This is
///   the original gateway behaviour.
/// - [`CacheMode::NoCache`]: a resolved PV does NOT hold a persistent
///   upstream monitor. Each downstream GET is forwarded as a fresh
///   upstream get (via a [`ReadHook`](epics_base_rs::server::pv::ReadHook)
///   on the shadow PV), and the upstream monitor is created lazily — only
///   while at least one downstream client is actually monitoring the PV —
///   then dropped when the last monitor leaves. Matches C ca-gateway
///   `-no_cache`: "Every get request will be forwarded to the ioc and
///   monitor will be created only if needed" (`gateway.cc:1454-1455`,
///   `gatePvData::getCB` no-cache branch `gatePv.cc:1737-1753`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Cache upstream values; serve GETs from the shadow snapshot;
    /// persistent upstream monitor per resolved PV. The default.
    #[default]
    Cached,
    /// Forward every GET to upstream; create the upstream monitor only
    /// while a downstream client is monitoring the PV.
    NoCache,
}

impl CacheMode {
    /// Whether reads are forwarded fresh to upstream rather than served
    /// from the shadow cache.
    pub fn is_no_cache(self) -> bool {
        matches!(self, CacheMode::NoCache)
    }
}

/// Configuration for `GatewayServer`.
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
    /// Path to the R1/R2/R3 report file. Defaults to `gateway.report`,
    /// matching C ca-gateway: its report file is initialised to
    /// `GATE_REPORT_FILE` in the resource constructor (`gateResources.h:24`,
    /// `gateResources.cc:334` `report_file=strDup(GATE_REPORT_FILE)`) and is
    /// overridden only when `-report <file>` is given (`gateway.cc:1159`
    /// `if(report_file) gr->setReportFile(report_file)`). When set, the
    /// `R1`/`R2`/`R3` commands and the SIGUSR2 shortcut append C-compatible
    /// report sections here (`gateServer.cc:689-979`); `None` — the
    /// Rust-only `--no-report` — keeps the log-only behaviour.
    pub report_path: Option<PathBuf>,
    /// Optional path to a file containing literal upstream PV names to
    /// pre-subscribe (one per line). When set, the gateway eagerly
    /// pre-fetches each name on startup. This is an opt-in convenience,
    /// not a requirement: resolution is otherwise lazy on-demand via
    /// `GatewayServer::install_search_resolver` (see `downstream.rs` doc
    /// comment).
    pub preload_path: Option<PathBuf>,
    /// CA server port (downstream side). `None` = resolve from the EPICS
    /// environment (`EPICS_CAS_SERVER_PORT` > `EPICS_CA_SERVER_PORT` >
    /// 5064), which is what C's `-sport` does — it *sets*
    /// `EPICS_CAS_SERVER_PORT` and lets the CAS read it back
    /// (`ca-gateway/gateway.cc:398-401`). `Some(0)` binds an ephemeral
    /// port; `Some(n)` binds `n`.
    pub server_port: Option<u16>,
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
    /// Cache mode (C `cacheMode` / `-no_cache`). [`CacheMode::Cached`]
    /// (default) holds a persistent upstream monitor per PV and serves
    /// GETs from the shadow cache; [`CacheMode::NoCache`] forwards each
    /// GET to upstream and creates the upstream monitor only while a
    /// downstream client is monitoring. See [`CacheMode`].
    pub cache_mode: CacheMode,
    /// Upstream-monitor event mask (C `-mask`). Selects which `DBE_*`
    /// events the gateway's upstream subscriptions request. Defaults to
    /// `DEFAULT_EVENT_MASK` (`DBE_VALUE | DBE_ALARM`), matching
    /// ca-gateway `gateResources.cc:339` — notably NOT `DBE_LOG`, which
    /// the raw `CaChannel::subscribe()` default would add. Build from a
    /// `-mask` spec string with [`resolve_event_mask`].
    pub event_mask: u16,
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

/// Default report-file name. Mirrors C ca-gateway's `GATE_REPORT_FILE`
/// (`gateResources.h:24`), the value its resource constructor installs
/// (`gateResources.cc:334`).
const GATE_REPORT_FILE: &str = "gateway.report";

impl GatewayConfig {
    /// Resolve the report-file path from the CLI layer over the built-in
    /// default. C ca-gateway defaults the report file to `gateway.report`
    /// and overrides it only when `-report <file>` is supplied
    /// (`gateway.cc:1159`); a report is therefore emitted by default. This
    /// keeps [`Self::default`]'s `report_path` authoritative for the default
    /// and applies the override only when the operator gave one. `disabled`
    /// is the Rust-only `--no-report` extension (C has no off switch).
    pub fn resolve_report_path(explicit: Option<PathBuf>, disabled: bool) -> Option<PathBuf> {
        if disabled {
            None
        } else {
            explicit.or_else(|| Self::default().report_path)
        }
    }
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
            report_path: Some(PathBuf::from(GATE_REPORT_FILE)),
            preload_path: None,
            server_port: None,
            timeouts: CacheTimeouts::default(),
            // Bare C `-prefix`; the `:` separator is inserted at publish.
            stats_prefix: "gateway".to_string(),
            cleanup_interval: Duration::from_secs(10),
            stats_interval: Duration::from_secs(10),
            heartbeat_interval: Some(Duration::from_secs(1)),
            // C++ GATE_RECONNECT_INHIBIT default (BeaconAnomaly::new).
            reconnect_inhibit: Duration::from_secs(60 * 5),
            read_only: false,
            cache_mode: CacheMode::default(),
            event_mask: DEFAULT_EVENT_MASK,
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
            .field("report_path", &self.report_path)
            .field("preload_path", &self.preload_path)
            .field("server_port", &self.server_port)
            .field("timeouts", &self.timeouts)
            .field("stats_prefix", &self.stats_prefix)
            .field("cleanup_interval", &self.cleanup_interval)
            .field("stats_interval", &self.stats_interval)
            .field("heartbeat_interval", &self.heartbeat_interval)
            .field("reconnect_inhibit", &self.reconnect_inhibit)
            .field("read_only", &self.read_only)
            .field("cache_mode", &self.cache_mode)
            .field("event_mask", &self.event_mask);
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

/// Default upstream-monitor event mask: `DBE_VALUE | DBE_ALARM`.
///
/// Mirrors ca-gateway `gateResources.cc:339`
/// (`setEventMask(DBE_VALUE | DBE_ALARM)`). Notably this is NOT the
/// `CaChannel::subscribe()` default (`DBE_VALUE | DBE_LOG | DBE_ALARM`):
/// ca-gateway does not request `DBE_LOG` (archive) traffic by default, so
/// the gateway must subscribe with an explicit mask rather than the
/// channel default.
pub(crate) const DEFAULT_EVENT_MASK: u16 =
    epics_ca_rs::protocol::DBE_VALUE | epics_ca_rs::protocol::DBE_ALARM;

/// Parse a ca-gateway `-mask` spec string into a `DBE_*` event mask.
///
/// Mirrors ca-gateway `gateway.cc:736-766`: each character selects one
/// DBE bit — `a`/`A` → `DBE_ALARM`, `v`/`V` → `DBE_VALUE`, `l`/`L` →
/// `DBE_LOG`, `p`/`P` → `DBE_PROPERTY`; any other character is ignored.
/// Returns `0` when the spec names no recognised bit, which
/// [`resolve_event_mask`] treats as "keep the default" exactly as
/// ca-gateway does (`gateway.cc:1146`: `if(mask) gr->setEventMask(mask)`).
pub(crate) fn parse_event_mask(spec: &str) -> u16 {
    use epics_ca_rs::protocol::{DBE_ALARM, DBE_LOG, DBE_PROPERTY, DBE_VALUE};
    let mut mask = 0u16;
    for c in spec.chars() {
        match c {
            'a' | 'A' => mask |= DBE_ALARM,
            'v' | 'V' => mask |= DBE_VALUE,
            'l' | 'L' => mask |= DBE_LOG,
            'p' | 'P' => mask |= DBE_PROPERTY,
            _ => {}
        }
    }
    mask
}

/// Resolve the upstream-monitor event mask from an optional `-mask` spec.
///
/// Applies ca-gateway's default-keep rule (`gateway.cc:1146`): a spec that
/// names no recognised DBE bit — or no `-mask` at all — keeps
/// `DEFAULT_EVENT_MASK` (`DBE_VALUE | DBE_ALARM`); otherwise the parsed
/// mask wins verbatim, so `-mask v`, `-mask va`, and `-mask vap` are all
/// reproducible.
pub fn resolve_event_mask(spec: Option<&str>) -> u16 {
    match spec.map(parse_event_mask) {
        Some(m) if m != 0 => m,
        _ => DEFAULT_EVENT_MASK,
    }
}

#[cfg(tokio_backend)]
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
    /// Shared control-PV flags, drained by the single control owner spawned
    /// in [`Self::run`]. The flag PV write hooks raise into this same
    /// `Arc`; the owner consumes the flags once per pass in C's fixed
    /// main-loop order. `None` when the stats prefix is empty (control PVs
    /// disabled).
    control_flags: Option<Arc<super::control::ControlFlags>>,
}

#[cfg(tokio_backend)]
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
            // C `cacheMode` / `-no_cache`: NoCache forwards each GET to
            // upstream and gates the upstream monitor on live downstream
            // monitor interest; Cached holds a persistent monitor and
            // serves GETs from the shadow snapshot.
            cache_mode: config.cache_mode,
            // Single connect-timeout owner: the lazy-resolution
            // `wait_connected` gate uses the same configured budget as the
            // cache reaper instead of a local constant (parity with C
            // gateResources::connectTimeout).
            connect_timeout: config.timeouts.connect_timeout,
            // Upstream monitor mask (C `-mask`, default DBE_VALUE|DBE_ALARM):
            // the gateway subscribes with this explicit mask instead of the
            // CaChannel default that would also request DBE_LOG.
            event_mask: config.event_mask,
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
        let server_port = config
            .server_port
            .unwrap_or_else(epics_base_rs::runtime::net::cas_server_port);
        let downstream = Arc::new({
            #[cfg(feature = "ca-gateway-tls")]
            {
                if let Some(ref tls) = config.tls {
                    DownstreamServer::new_with_tls(shadow_db.clone(), server_port, tls.clone())
                        .await?
                } else {
                    DownstreamServer::new(shadow_db.clone(), server_port).await?
                }
            }
            #[cfg(not(feature = "ca-gateway-tls"))]
            {
                DownstreamServer::new(shadow_db.clone(), server_port).await?
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

        // Snapshot the downstream CaServer's cumulative subscription-event
        // counters so the stats refresh can derive serverEventRate /
        // serverPostRate from their per-interval delta (gateServer.cc:
        // 2147-2148). Captured BEFORE `downstream.run()` consumes the inner
        // CaServer; the `Arc` keeps the shared counters alive afterwards.
        if let Some(server_stats) = downstream.server_stats().await {
            stats.install_server_stats(server_stats);
        }

        // Publish C-compatible control flag PVs (commandFlag, report*Flag,
        // newAsFlag, quitFlag, quitServerFlag) under the stats prefix so
        // operators can trigger command-file execution, reports, reload,
        // and shutdown via `caput` — the cross-platform alternative to
        // SIGUSR1 (gateServer.cc:1877-2102). The receiver is drained by a
        // single control owner in `run()`; `None` when stats are disabled.
        let control_flags = super::control::publish_control_pvs(&shadow_db, &stats_prefix).await;

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
            control_flags,
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
                            Some(addr) => {
                                pvlist.match_name_for_host(&name, &PolicyHost::from_peer(addr))
                            }
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
    /// The host-scoped `.pvlist` admission (1) applies to the gateway's own
    /// stat / heartbeat / control PVs too, not just cached shadow PVs: C
    /// runs `findEntry` on every name before its stat-prefix branch
    /// (gateServer.cc:1564-1573), so under a restrictive `.pvlist` that
    /// omits the stat prefix those PVs answer does-not-exist as in C. They
    /// skip only the connection-state check (2), having no upstream entry.
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
                    // Host-scoped `.pvlist` admission runs FIRST on every
                    // name — gateway-managed shadow PVs AND the gateway's
                    // own stat / heartbeat / control PVs alike. C runs
                    // `gateAs::findEntry(pvname, host)` (gateAs.h:270-276)
                    // on every name in `pvExistTest` before the stat-prefix
                    // branch is even reached (gateServer.cc:1564-1573), so a
                    // restrictive `.pvlist` that omits the stat prefix yields
                    // does-not-exist for the stat PVs too. A host-less
                    // internal lookup (`peer: None`) falls back to the global
                    // rule decision, matching the search resolver. With no
                    // `.pvlist` configured the implicit `.* ALLOW` admits
                    // everything, so a pass-through gateway still serves its
                    // stats.
                    let admitted = {
                        let pvlist = pvlist.load_full();
                        match peer {
                            Some(addr) => pvlist
                                .match_name_for_host(&name, &PolicyHost::from_peer(addr))
                                .is_some(),
                            None => pvlist.match_name(&name).is_some(),
                        }
                    };
                    if !admitted {
                        return false;
                    }
                    // Stat / heartbeat / control PVs are published straight
                    // into the shadow DB and never enter the upstream cache.
                    // They carry no upstream connection state, so once
                    // `.pvlist`-admitted they exist unconditionally — no
                    // `is_existent()` check is owed (C's stat branch returns
                    // `pverExistsHere` directly once findEntry passes,
                    // gateServer.cc:1586-1616).
                    let entry = match cache.read().await.get(&name) {
                        Some(e) => e,
                        None => return true,
                    };
                    // Gateway-managed shadow PV: additionally require an
                    // existent upstream connection state. C `pvExistTest`
                    // replies `pverExistsHere` only for `gatePvInactive` /
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

        // The gateway's run loop owns every long-lived task below, so it is
        // where the capability is taken; the signal handler gets it by
        // parameter rather than re-deriving it from its own thread.
        let reactor = epics_base_rs::runtime::task::Reactor::current()
            .expect("the CA gateway run loop is awaited on its reactor");

        // Cleanup task
        let cache_for_cleanup = cache.clone();
        let upstream_for_cleanup = upstream.clone();
        let stats_for_cleanup = stats.clone();
        let cleanup_handle = reactor.spawn(async move {
            let mut tick = epics_base_rs::runtime::task::interval(cleanup_interval);
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
        let stats_handle = reactor.spawn(async move {
            let mut tick = epics_base_rs::runtime::task::interval(stats_interval);
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
            Some(reactor.spawn(async move {
                let mut tick = epics_base_rs::runtime::task::interval(period);
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
        let signal_handle = self.spawn_signal_handler(&reactor);

        // Control-PV command owner: drains commandFlag/report*Flag/
        // newAsFlag/quitFlag writes and dispatches each through the SAME
        // CommandHandler the SIGUSR1 path uses — one command owner for
        // every trigger source (mirrors C routing all flags through the
        // gateServer main loop). `quit*` fires `self.shutdown`.
        let control_handle = {
            self.control_flags.clone().map(|flags| {
                let handler = CommandHandler::new(
                    self.cache.clone(),
                    self.pvlist.clone(),
                    self.access.clone(),
                    self.config.pvlist_path.clone(),
                    self.config.access_path.clone(),
                )
                .with_upstream(self.upstream.clone())
                .with_beacon_anomaly(self.beacon_anomaly.clone())
                .with_stats(stats.clone())
                .with_report_path(self.config.report_path.clone());
                super::control::spawn_control_owner(
                    &reactor,
                    flags,
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
        let conn_rx = downstream.connection_events(&reactor).await;
        let conn_handle = if let Some(mut rx) = conn_rx {
            let stats_for_conn = stats.clone();
            let cache_for_conn = self.cache.clone();
            // No-cache: the connection-event owner is also the owner of
            // the lazy upstream-monitor lifetime — it calls
            // `ensure_monitor`/`release_monitor` on the first/last
            // downstream monitor. Cached mode never touches these.
            let upstream_for_conn = self.upstream.clone();
            let cache_mode = self.config.cache_mode;
            Some(reactor.spawn(async move {
                use super::downstream::ConnEventRecv;
                use epics_ca_rs::protocol::DBE_PROPERTY;
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
                // No-cache: per-peer OPEN-MONITOR registry, parallel to
                // `peer_channels` but keyed on `EVENT_ADD`/`EVENT_CANCEL`
                // (sub_id) rather than CREATE/CLEAR (cid) — a plain caget
                // opens a channel but no monitor. Drives the lazy upstream
                // monitor and is drained on a hard `Disconnected` so a
                // peer that vanishes without `EVENT_CANCEL` still releases
                // its monitor interest. Stays empty in cached mode.
                let mut peer_monitors: HashMap<std::net::SocketAddr, Vec<(String, u32)>> =
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
                            // No-cache safety net: a hard close may skip
                            // EVENT_CANCEL, so drain this peer's open
                            // monitors and drop the upstream monitor for
                            // any PV that hit zero monitor interest.
                            if let Some(monitors) = peer_monitors.remove(&addr) {
                                for (pv_name, msid) in monitors {
                                    // Withdraw both interests by sid — the
                                    // property removal is a no-op for a
                                    // value-only subscription (see
                                    // SubscriptionClosed).
                                    let (became_empty, prop_became_empty) =
                                        match cache_for_conn.read().await.get(&pv_name) {
                                            Some(entry) => {
                                                let mut e = entry.write().await;
                                                let v = e.remove_monitor_interest(msid);
                                                let p = e.remove_property_interest(msid);
                                                (v, p)
                                            }
                                            None => (false, false),
                                        };
                                    if became_empty {
                                        upstream_for_conn.release_monitor(&pv_name);
                                    }
                                    if prop_became_empty {
                                        upstream_for_conn.release_prop_monitor(&pv_name);
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
                        // No-cache lazy upstream monitor: the FIRST
                        // downstream monitor (`EVENT_ADD`) on a PV creates
                        // the upstream subscription; the LAST monitor
                        // (`EVENT_CANCEL` / teardown) drops it. Cached mode
                        // ignores these — its monitor is always present.
                        // Mirrors C ca-gateway no-cache `vc->needPosting()`
                        // gating `pv->monitor()` (gatePv.cc:1737-1753).
                        ServerConnectionEvent::SubscriptionOpened {
                            peer,
                            pv_name,
                            sub_id,
                            mask,
                        } => {
                            if cache_mode.is_no_cache() {
                                let msid = synthetic_sid(peer, &pv_name, sub_id);
                                // A DBE_PROPERTY subscription also satisfies
                                // C `needPosting()`, so it enables BOTH the
                                // value and the property monitor — add value
                                // interest unconditionally, property interest
                                // only when the property bit is set.
                                let (became_first, prop_became_first) =
                                    match cache_for_conn.read().await.get(&pv_name) {
                                        Some(entry) => {
                                            let mut e = entry.write().await;
                                            let v = e.add_monitor_interest(msid);
                                            // Mirrors C `getCB` [NO_CACHE]
                                            // `propMonitor()` gate on
                                            // `client_mask == DBE_PROPERTY`,
                                            // set from the EVENT_ADD mask's
                                            // DBE_PROPERTY bit
                                            // (gateVc.cc:1222-1223,
                                            // gatePv.cc:1749-1752).
                                            let p = if mask & DBE_PROPERTY != 0 {
                                                e.add_property_interest(msid)
                                            } else {
                                                false
                                            };
                                            (v, p)
                                        }
                                        None => (false, false),
                                    };
                                if became_first {
                                    upstream_for_conn.ensure_monitor(&pv_name);
                                }
                                if prop_became_first {
                                    upstream_for_conn.ensure_prop_monitor(&pv_name);
                                }
                                peer_monitors.entry(peer).or_default().push((pv_name, msid));
                            }
                        }
                        ServerConnectionEvent::SubscriptionClosed {
                            peer,
                            pv_name,
                            sub_id,
                        } => {
                            if cache_mode.is_no_cache() {
                                let msid = synthetic_sid(peer, &pv_name, sub_id);
                                // Close carries no mask, so withdraw both
                                // interests by sid — `remove_property_interest`
                                // is a no-op for a value-only subscription
                                // whose sid was never added to `prop_interest`.
                                let (became_empty, prop_became_empty) =
                                    match cache_for_conn.read().await.get(&pv_name) {
                                        Some(entry) => {
                                            let mut e = entry.write().await;
                                            let v = e.remove_monitor_interest(msid);
                                            let p = e.remove_property_interest(msid);
                                            (v, p)
                                        }
                                        None => (false, false),
                                    };
                                if became_empty {
                                    upstream_for_conn.release_monitor(&pv_name);
                                }
                                if prop_became_empty {
                                    upstream_for_conn.release_prop_monitor(&pv_name);
                                }
                                if let Some(monitors) = peer_monitors.get_mut(&peer) {
                                    monitors.retain(|(p, s)| !(p == &pv_name && *s == msid));
                                    if monitors.is_empty() {
                                        peer_monitors.remove(&peer);
                                    }
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

    /// Spawn the Unix signal watchers.
    ///
    /// - **SIGUSR1** re-reads the command file (when one is configured),
    ///   dispatching each command token.
    /// - **SIGUSR2** is C ca-gateway's shortcut for the R2 process-variable
    ///   report (`report2_flag`, gateServer.cc:2403-2407): it runs `R2`
    ///   directly, so it works even without a command file.
    ///
    /// Returns None only on non-Unix. On Unix the watcher is always armed
    /// (SIGUSR2 needs no command file); the handle is aborted at shutdown.
    #[cfg(unix)]
    fn spawn_signal_handler(
        &self,
        reactor: &epics_base_rs::runtime::task::Reactor,
    ) -> Option<epics_base_rs::runtime::task::TaskHandle<()>> {
        let cmd_path = self.config.command_path.clone();
        let pvlist_path = self.config.pvlist_path.clone();
        let access_path = self.config.access_path.clone();
        let report_path = self.config.report_path.clone();
        let cache = self.cache.clone();
        let pvlist = self.pvlist.clone();
        let access = self.access.clone();
        let upstream = self.upstream.clone();
        let beacon_anomaly = self.beacon_anomaly.clone();
        let stats = self.stats.clone();

        Some(reactor.spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigusr1 = match signal(SignalKind::user_defined1()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "ca-gateway-rs: failed to install SIGUSR1 handler");
                    return;
                }
            };
            let mut sigusr2 = match signal(SignalKind::user_defined2()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "ca-gateway-rs: failed to install SIGUSR2 handler");
                    return;
                }
            };
            let handler = CommandHandler::new(cache, pvlist, access, pvlist_path, access_path)
                .with_upstream(upstream)
                .with_beacon_anomaly(beacon_anomaly)
                .with_stats(stats)
                .with_report_path(report_path);
            tracing::info!(
                command_file = ?cmd_path.as_ref().map(|p| p.display().to_string()),
                "ca-gateway-rs: SIGUSR1/SIGUSR2 handlers armed"
            );
            loop {
                tokio::select! {
                    r = sigusr1.recv() => {
                        if r.is_none() {
                            break;
                        }
                        match &cmd_path {
                            Some(path) => {
                                tracing::info!(
                                    "ca-gateway-rs: SIGUSR1 received — processing command file"
                                );
                                match handler.process_file(path).await {
                                    Ok(out) if !out.is_empty() => tracing::info!(
                                        output = %out.trim_end(),
                                        "ca-gateway-rs: command output"
                                    ),
                                    Ok(_) => {}
                                    Err(e) => tracing::warn!(
                                        error = %e,
                                        "ca-gateway-rs: command file error"
                                    ),
                                }
                            }
                            None => tracing::warn!(
                                "ca-gateway-rs: SIGUSR1 received but no command file configured"
                            ),
                        }
                    }
                    r = sigusr2.recv() => {
                        if r.is_none() {
                            break;
                        }
                        // C report2_flag shortcut: run R2 directly.
                        tracing::info!(
                            "ca-gateway-rs: SIGUSR2 received — running R2 (process variable report)"
                        );
                        match handler.dispatch(GatewayCommand::ReportSummary).await {
                            Ok(out) if !out.is_empty() => tracing::info!(
                                status = %out.trim_end(),
                                "ca-gateway-rs: R2 report"
                            ),
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "ca-gateway-rs: R2 report error"),
                        }
                    }
                }
            }
        }))
    }

    /// Stub for non-Unix platforms (no SIGUSR1/SIGUSR2).
    #[cfg(not(unix))]
    fn spawn_signal_handler(
        &self,
        _reactor: &epics_base_rs::runtime::task::Reactor,
    ) -> Option<epics_base_rs::runtime::task::TaskHandle<()>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_ca_rs::protocol::{DBE_ALARM, DBE_LOG, DBE_PROPERTY, DBE_VALUE};

    #[test]
    fn gateway_default_event_mask_is_value_alarm_without_log() {
        // ca-gateway gateResources.cc:339 sets DBE_VALUE|DBE_ALARM as the
        // default — NOT the CaChannel subscribe() default that also carries
        // DBE_LOG. The DBE_LOG-free default is the core of BR-23.
        let cfg = GatewayConfig::default();
        assert_eq!(cfg.event_mask, DBE_VALUE | DBE_ALARM);
        assert_eq!(
            cfg.event_mask & DBE_LOG,
            0,
            "default upstream monitor mask must not request DBE_LOG"
        );
    }

    #[test]
    fn parse_event_mask_maps_dbe_selectors_case_insensitively() {
        // gateway.cc:736-766 char→DBE map; unrecognised chars ignored.
        assert_eq!(parse_event_mask("v"), DBE_VALUE);
        assert_eq!(parse_event_mask("a"), DBE_ALARM);
        assert_eq!(parse_event_mask("l"), DBE_LOG);
        assert_eq!(parse_event_mask("p"), DBE_PROPERTY);
        assert_eq!(parse_event_mask("VA"), DBE_VALUE | DBE_ALARM);
        assert_eq!(
            parse_event_mask("vap"),
            DBE_VALUE | DBE_ALARM | DBE_PROPERTY
        );
        assert_eq!(parse_event_mask(""), 0);
        assert_eq!(parse_event_mask("xyz"), 0);
        assert_eq!(parse_event_mask("vx?a"), DBE_VALUE | DBE_ALARM);
    }

    #[test]
    fn report_path_defaults_to_gateway_report() {
        // C ca-gateway emits a report to gateway.report by default
        // (gateResources.cc:334 report_file=strDup(GATE_REPORT_FILE)).
        assert_eq!(
            GatewayConfig::default().report_path,
            Some(PathBuf::from("gateway.report"))
        );
    }

    #[test]
    fn resolve_report_path_layers_cli_over_default() {
        // No flag → the built-in default report file.
        assert_eq!(
            GatewayConfig::resolve_report_path(None, false),
            Some(PathBuf::from("gateway.report"))
        );
        // --report <file> overrides the default (gateway.cc:1159).
        assert_eq!(
            GatewayConfig::resolve_report_path(Some(PathBuf::from("/tmp/x.report")), false),
            Some(PathBuf::from("/tmp/x.report"))
        );
        // --no-report disables the report entirely (Rust-only off switch),
        // even if a path was also supplied.
        assert_eq!(GatewayConfig::resolve_report_path(None, true), None);
        assert_eq!(
            GatewayConfig::resolve_report_path(Some(PathBuf::from("/tmp/x.report")), true),
            None
        );
    }

    #[test]
    fn resolve_event_mask_keeps_default_when_empty_or_unrecognised() {
        // gateway.cc:1146 `if(mask) gr->setEventMask(mask)` — only a spec
        // that names at least one recognised bit overrides the default.
        assert_eq!(resolve_event_mask(None), DEFAULT_EVENT_MASK);
        assert_eq!(resolve_event_mask(Some("")), DEFAULT_EVENT_MASK);
        assert_eq!(resolve_event_mask(Some("zzz")), DEFAULT_EVENT_MASK);
        // A recognised spec wins verbatim, so `-mask v` drops DBE_ALARM
        // and `-mask l` is reproducible (neither stays at the default).
        assert_eq!(resolve_event_mask(Some("v")), DBE_VALUE);
        assert_eq!(resolve_event_mask(Some("l")), DBE_LOG);
        assert_eq!(
            resolve_event_mask(Some("vap")),
            DBE_VALUE | DBE_ALARM | DBE_PROPERTY
        );
    }

    // `GatewayServer::build` constructs the gateway's upstream `CaClient`.
    // Under this feature that client's search engine is name-servers-only
    // (`epics-ca-rs` `search::SearchTransport` has no `Udp` variant on the
    // exec backend, because a future spawned through the `runtime::task`
    // seam runs on a callback-pool worker with no tokio reactor), and a
    // name-servers-only engine with an empty `EPICS_CA_NAME_SERVERS` is
    // refused at construction — it could reach no server at all. The
    // gateway is a hosted daemon that is never built in the exec model, so
    // the configuration these tests use is not one it has to satisfy.
    #[cfg(tokio_backend)]
    #[tokio::test]
    async fn build_with_minimal_config() {
        let config = GatewayConfig {
            pvlist_content: Some("".to_string()),
            // Ephemeral bind: decouple from the ambient
            // EPICS_CA_SERVER_PORT/EPICS_CAS_SERVER_PORT env vars, which
            // other tests in this binary (e.g. upstream.rs's
            // `#[serial(epics_env)]` group) mutate process-wide and can
            // point at a port a concurrently-running test already owns
            // exclusively.
            server_port: Some(0),
            ..Default::default()
        };
        let server = GatewayServer::build(config).await;
        assert!(server.is_ok(), "build failed: {:?}", server.err());
    }

    // Same reason as `build_with_minimal_config`: the gateway builds a name-servers-only
    // upstream `CaClient` with no name server under this feature.
    #[cfg(tokio_backend)]
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
            // Ephemeral bind — see `build_with_minimal_config`.
            server_port: Some(0),
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
    // Same reason as `build_with_minimal_config`: the gateway builds a name-servers-only
    // upstream `CaClient` with no name server under this feature.
    #[cfg(tokio_backend)]
    #[tokio::test]
    async fn build_with_upstream_tls() {
        use epics_ca_rs::tls::{Roots, TlsConfig};
        let config = GatewayConfig {
            pvlist_content: Some("".to_string()),
            upstream_tls: Some(TlsConfig::client_from_roots(Roots::empty())),
            upstream_tls_server_name: Some("ioc.example.com".to_string()),
            // Ephemeral bind — see `build_with_minimal_config`.
            server_port: Some(0),
            ..Default::default()
        };
        let server = GatewayServer::build(config).await;
        assert!(
            server.is_ok(),
            "build with upstream TLS failed: {:?}",
            server.err()
        );
    }

    // Same reason as `build_with_minimal_config`: the gateway builds a name-servers-only
    // upstream `CaClient` with no name server under this feature.
    #[cfg(tokio_backend)]
    #[tokio::test]
    async fn build_no_pvlist_installs_implicit_allow_all() {
        // No pvlist path AND no inline content: C ca-gateway serves every
        // PV through an implicit `.* ALLOW` rule (gateAs.cc:430-445), not a
        // deny-all. Assumes no `gateway.pvlist` exists in the crate's
        // working directory (it does not) — the default-file probe is the
        // thin `is_file()` branch above, exercised by `parse_pvlist_file`.
        let config = GatewayConfig {
            // Ephemeral bind — see `build_with_minimal_config`.
            server_port: Some(0),
            ..Default::default()
        };
        let server = GatewayServer::build(config).await.unwrap();
        let pvlist = server.pvlist().load_full();
        assert!(
            pvlist.match_name("Any:Random:PV").is_some(),
            "no-pvlist gateway must serve all PVs via implicit .* ALLOW"
        );
        assert!(pvlist.match_name("another.unlikely.name").is_some());
    }

    // Same reason as `build_with_minimal_config`: the gateway builds a name-servers-only
    // upstream `CaClient` with no name server under this feature.
    #[cfg(tokio_backend)]
    #[tokio::test]
    async fn build_empty_inline_content_stays_deny_all() {
        // An explicitly-supplied empty pvlist is the operator's deny-all
        // request: it must NOT be promoted to the implicit allow-all that
        // the no-config path installs. This pins the two intents apart.
        let config = GatewayConfig {
            pvlist_content: Some(String::new()),
            // Ephemeral bind — see `build_with_minimal_config`.
            server_port: Some(0),
            ..Default::default()
        };
        let server = GatewayServer::build(config).await.unwrap();
        let pvlist = server.pvlist().load_full();
        assert!(
            pvlist.match_name("Any:Random:PV").is_none(),
            "explicit empty pvlist must deny all (distinct from no-config)"
        );
    }

    // Same reason as `build_with_minimal_config`: the gateway builds a name-servers-only
    // upstream `CaClient` with no name server under this feature.
    #[cfg(tokio_backend)]
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
            // Ephemeral bind — see `build_with_minimal_config`.
            server_port: Some(0),
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

    // Same reason as `build_with_minimal_config`: the gateway builds a name-servers-only
    // upstream `CaClient` with no name server under this feature.
    #[cfg(tokio_backend)]
    #[tokio::test]
    async fn existence_gate_hides_disconnected_shadow_pv() {
        // A shadow PV whose upstream has disconnected must answer
        // does-not-exist to a new search/create even though the PV remains
        // in `simple_pvs` and the requesting host is allowed — parity with
        // C pvExistTest returning pverDoesNotExistHere for gatePvDisconnect.
        use std::net::SocketAddr;

        let config = GatewayConfig {
            pvlist_content: Some("PV.* ALLOW\n".to_string()),
            // Ephemeral bind — see `build_with_minimal_config`.
            server_port: Some(0),
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

    /// A restrictive `.pvlist` that omits the stat prefix must hide the
    /// gateway's own stat / heartbeat / control PVs too. C runs
    /// `gateAs::findEntry` (gateAs.h:270-276) on every name before its
    /// stat-prefix branch (gateServer.cc:1564-1573), so a stat PV not
    /// admitted by `.pvlist` answers does-not-exist. Pre-fix the gate's
    /// cache-miss path returned `true` unconditionally — stat PVs live in
    /// `simple_pvs` and never in the upstream cache, so the gate always
    /// cache-misses on them and leaked them past a restrictive pvlist.
    // Same reason as `build_with_minimal_config`: the gateway builds a name-servers-only
    // upstream `CaClient` with no name server under this feature.
    #[cfg(tokio_backend)]
    #[tokio::test]
    async fn existence_gate_applies_pvlist_admission_to_stat_pvs() {
        use std::net::SocketAddr;

        let peer: SocketAddr = "192.0.2.5:5064".parse().unwrap();

        // Restrictive pvlist: only `PV.*` is allowed; the `gateway:` stat
        // prefix is NOT. `build()` publishes the stat PVs into the shadow
        // DB, so `gateway:heartbeat` is in `simple_pvs` but must not be
        // advertised under this pvlist.
        let config = GatewayConfig {
            pvlist_content: Some("PV.* ALLOW\n".to_string()),
            stats_prefix: "gateway".to_string(),
            // Ephemeral bind — see `build_with_minimal_config`.
            server_port: Some(0),
            ..Default::default()
        };
        let server = GatewayServer::build(config).await.unwrap();
        // Registered in the shadow DB (gate-independent lookup).
        assert!(
            server
                .shadow_db
                .find_pv("gateway:heartbeat")
                .await
                .is_some(),
            "build() must register the stat PV in the shadow DB"
        );
        // But hidden by the restrictive pvlist that omits the stat prefix.
        assert!(
            !server
                .shadow_db
                .has_name_from("gateway:heartbeat", Some(peer))
                .await,
            "restrictive pvlist omitting the stat prefix must hide stat PVs (C findEntry)"
        );

        // Permissive pvlist that DOES admit the stat prefix: the same stat
        // PV is advertised.
        let config = GatewayConfig {
            pvlist_content: Some("PV.* ALLOW\ngateway.* ALLOW\n".to_string()),
            stats_prefix: "gateway".to_string(),
            // Ephemeral bind — see `build_with_minimal_config`.
            server_port: Some(0),
            ..Default::default()
        };
        let server = GatewayServer::build(config).await.unwrap();
        assert!(
            server
                .shadow_db
                .has_name_from("gateway:heartbeat", Some(peer))
                .await,
            "a pvlist admitting the stat prefix must serve stat PVs"
        );
    }

    #[cfg(tokio_backend)]
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
