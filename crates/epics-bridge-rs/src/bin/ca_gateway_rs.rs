//! CA gateway daemon binary.
//!
//! Usage:
//!
//! ```text
//! ca-gateway-rs --pvlist gateway.pvlist [--access gateway.access]
//!               [--preload preload.txt] [--port 5064]
//!               [--read-only] [--no-stats]
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use epics_bridge_rs::ca_gateway::{
    GatewayConfig, GatewayServer, PutLogScope, RestartPolicy, supervise,
};

#[derive(Parser, Debug)]
#[command(
    name = "ca-gateway-rs",
    about = "Pure Rust port of the EPICS CA gateway",
    version
)]
struct Args {
    /// Path to .pvlist access list file
    #[arg(long)]
    pvlist: Option<PathBuf>,

    /// Path to .access ACF file
    #[arg(long)]
    access: Option<PathBuf>,

    /// Path to a file listing literal upstream PV names to pre-subscribe
    /// (one per line, blank/# lines ignored).
    #[arg(long)]
    preload: Option<PathBuf>,

    /// Path to put-event log file. By default the log is TRAPWRITE-scoped
    /// (C ca-gateway contract): only granted writes whose matched ACF rule
    /// carries `TRAPWRITE` are recorded, without an outcome token.
    #[arg(long)]
    putlog: Option<PathBuf>,

    /// Broaden `--putlog` to a fail-loud audit: record every client write
    /// attempt — including access-denied and upstream-failed writes — with
    /// its `OK`/`DENIED`/`FAILED` outcome. Not the C ca-gateway contract.
    #[arg(long)]
    putlog_all: bool,

    /// Path to a command file processed when the gateway receives SIGUSR1 (Unix).
    #[arg(long)]
    command: Option<PathBuf>,

    /// Path to the R1/R2/R3 report file (C `-report`). Overrides the
    /// default `gateway.report`. The R1/R2/R3 commands and the SIGUSR2
    /// shortcut append C-compatible report sections here.
    #[arg(long)]
    report: Option<PathBuf>,

    /// Disable the report file entirely (Rust-only). C ca-gateway always
    /// has a report file (defaults to `gateway.report`, `gateResources.cc:334`);
    /// pass `--no-report` to opt out so no report is written.
    #[arg(long, conflicts_with = "report")]
    no_report: bool,

    /// CA server port (downstream side). Omit to take
    /// `EPICS_CAS_SERVER_PORT`, then `EPICS_CA_SERVER_PORT`, then 5064 —
    /// C's `-sport` sets `EPICS_CAS_SERVER_PORT` and the CAS reads it
    /// back (`gateway.cc:398-401`). `--port 0` binds an ephemeral port.
    #[arg(long)]
    port: Option<u16>,

    /// Downstream listen-interface list (C `-sip`): sets
    /// `EPICS_CAS_INTF_ADDR_LIST` so the gateway's CA server binds only the
    /// given interfaces. Space-separated, EPICS env-list syntax.
    #[arg(long)]
    sip: Option<String>,

    /// Downstream ignore-address list (C `-signore`): sets
    /// `EPICS_CAS_IGNORE_ADDR_LIST` so searches from these addresses are
    /// dropped. Space-separated.
    #[arg(long)]
    signore: Option<String>,

    /// Upstream search-address list (C `-cip`): sets `EPICS_CA_ADDR_LIST`
    /// for the gateway's upstream CA client AND forces
    /// `EPICS_CA_AUTO_ADDR_LIST=NO`, so the gateway searches only the named
    /// IOC domain and never broadcasts back onto its own downstream
    /// segment. Space-separated.
    #[arg(long)]
    cip: Option<String>,

    /// Upstream CA search port (C `-cport`): sets `EPICS_CA_SERVER_PORT`
    /// for the gateway's upstream client, letting it search an IOC domain
    /// on a non-default port.
    #[arg(long)]
    cport: Option<u16>,

    /// Read-only mode: reject all puts.
    #[arg(long)]
    read_only: bool,

    /// Disable caching (C `-no_cache`): forward every get request to the
    /// IOC and create the upstream monitor only while a downstream client
    /// is monitoring the PV. Default is caching on (a persistent upstream
    /// monitor per PV, GETs served from the cached value).
    #[arg(long)]
    no_cache: bool,

    /// Upstream monitor event mask (C `-mask`). A string of DBE selector
    /// characters: `v`=value, `a`=alarm, `l`=log/archive, `p`=property
    /// (case-insensitive; e.g. `va`, `v`, `vap`). Unset — or a value that
    /// names no recognised selector — keeps the ca-gateway default
    /// `va` (DBE_VALUE|DBE_ALARM); notably the default does NOT include
    /// `l`, so the gateway does not request DBE_LOG traffic unless asked.
    #[arg(long)]
    mask: Option<String>,

    /// Disable statistics PV publication.
    #[arg(long)]
    no_stats: bool,

    /// Statistics PV namespace (C `-prefix`). The `:` separator is added
    /// automatically, so PVs are published as `<prefix>:<name>` — pass the
    /// bare namespace, not a trailing `:`. Defaults to the host name
    /// (falling back to `gateway`), matching C ca-gateway. Use --no-stats
    /// to disable stats PVs entirely.
    #[arg(long)]
    stats_prefix: Option<String>,

    /// Heartbeat interval in seconds (0 = disable).
    #[arg(long, default_value_t = 1)]
    heartbeat_interval: u64,

    /// Cleanup interval in seconds.
    #[arg(long, default_value_t = 10)]
    cleanup_interval: u64,

    /// Statistics refresh interval in seconds.
    #[arg(long, default_value_t = 10)]
    stats_interval: u64,

    /// Upstream connect timeout in seconds (C `-connect_timeout`): how
    /// long a first search waits for the upstream IOC before the shadow
    /// PV is demoted from Connecting to Dead.
    #[arg(long, default_value_t = 1)]
    connect_timeout: u64,

    /// Inactive-PV retention in seconds (C `-inactive_timeout`): how long
    /// a cached PV with no downstream clients is kept before eviction.
    #[arg(long, default_value_t = 60 * 60 * 2)]
    inactive_timeout: u64,

    /// Dead-PV retention in seconds (C `-dead_timeout`): how long a PV
    /// whose upstream search never resolved is kept before eviction.
    #[arg(long, default_value_t = 60 * 2)]
    dead_timeout: u64,

    /// Disconnected-PV retention in seconds (C `-disconnect_timeout`):
    /// how long a PV is kept after its upstream disconnects.
    #[arg(long, default_value_t = 60 * 60 * 2)]
    disconnect_timeout: u64,

    /// Reconnect beacon-anomaly inhibit window in seconds
    /// (C `-reconnect_inhibit`): minimum spacing between
    /// upstream-reconnect beacon anomalies.
    #[arg(long, default_value_t = 60 * 5)]
    reconnect_inhibit: u64,

    /// Run under auto-restart supervisor (NRESTARTS pattern).
    #[arg(long)]
    supervised: bool,

    /// Max restarts within window (default 10).
    #[arg(long, default_value_t = 10)]
    max_restarts: u32,

    /// Restart window in seconds (default 600).
    #[arg(long, default_value_t = 600)]
    restart_window: u64,

    /// Restart delay in seconds (default 10).
    #[arg(long, default_value_t = 10)]
    restart_delay: u64,

    /// Server certificate chain (PEM). Required for TLS termination.
    /// Pair with --tls-key. Available with `--features ca-gateway-tls`.
    #[cfg(feature = "ca-gateway-tls")]
    #[arg(long)]
    tls_cert: Option<PathBuf>,

    /// Server private key (PEM).
    #[cfg(feature = "ca-gateway-tls")]
    #[arg(long)]
    tls_key: Option<PathBuf>,

    /// Optional client CA bundle (PEM) — when set, the gateway
    /// requires mTLS (client cert verified against this trust pool).
    #[cfg(feature = "ca-gateway-tls")]
    #[arg(long)]
    tls_client_ca: Option<PathBuf>,

    /// B10: CA-authority bundle (PEM) for verifying the *upstream*
    /// IOC's server certificate. When set, the gateway's upstream
    /// CaClient connects to the real IOC over TLS. Independent of the
    /// downstream `--tls-*` termination. Available with
    /// `--features ca-gateway-tls`.
    #[cfg(feature = "ca-gateway-tls")]
    #[arg(long)]
    upstream_tls_roots: Option<PathBuf>,

    /// B10: client certificate (PEM) presented to the upstream IOC
    /// for mTLS. Pair with --upstream-tls-client-key. Optional —
    /// omit for server-auth-only upstream TLS.
    #[cfg(feature = "ca-gateway-tls")]
    #[arg(long)]
    upstream_tls_client_cert: Option<PathBuf>,

    /// B10: client private key (PEM) for upstream mTLS. Required when
    /// --upstream-tls-client-cert is set.
    #[cfg(feature = "ca-gateway-tls")]
    #[arg(long)]
    upstream_tls_client_key: Option<PathBuf>,

    /// B10: SNI / cert-hostname-verification name for the upstream
    /// TLS connection. Set to the DNS name in the upstream IOC's
    /// server certificate when it is hostname-bound rather than
    /// IP-bound.
    #[cfg(feature = "ca-gateway-tls")]
    #[arg(long)]
    upstream_tls_server_name: Option<String>,
}

#[cfg(feature = "ca-gateway-tls")]
fn build_tls(
    args: &Args,
) -> Result<Option<std::sync::Arc<epics_ca_rs::tls::ServerConfig>>, String> {
    use epics_ca_rs::tls::{TlsConfig, load_certs, load_private_key, load_root_store};
    let (cert_path, key_path) = match (&args.tls_cert, &args.tls_key) {
        (Some(c), Some(k)) => (c, k),
        (None, None) => return Ok(None),
        _ => {
            return Err("--tls-cert and --tls-key must both be set or both unset".into());
        }
    };
    let chain = load_certs(cert_path.to_str().unwrap_or_default())
        .map_err(|e| format!("loading cert chain: {e}"))?;
    let key = load_private_key(key_path.to_str().unwrap_or_default())
        .map_err(|e| format!("loading key: {e}"))?;
    let cfg = if let Some(ca_path) = &args.tls_client_ca {
        let roots = load_root_store(ca_path.to_str().unwrap_or_default())
            .map_err(|e| format!("loading client CA: {e}"))?;
        TlsConfig::server_mtls_from_pem(chain, key, roots)
    } else {
        TlsConfig::server_from_pem(chain, key)
    }
    .map_err(|e| format!("TLS server build: {e}"))?;
    match cfg {
        TlsConfig::Server(arc) => Ok(Some(arc)),
        TlsConfig::Client(_) => Err("expected server TlsConfig".into()),
    }
}

/// B10: build the upstream-side TLS client config from the
/// `--upstream-tls-*` flags. Returns `Ok(None)` when
/// `--upstream-tls-roots` is unset (upstream stays plaintext, or
/// falls back to `EPICS_CA_TLS_*` env vars inside `CaClient::new`).
#[cfg(feature = "ca-gateway-tls")]
fn build_upstream_tls(args: &Args) -> Result<Option<epics_ca_rs::tls::TlsConfig>, String> {
    use epics_ca_rs::tls::{TlsConfig, load_certs, load_private_key, load_root_store};
    let roots_path = match &args.upstream_tls_roots {
        Some(p) => p,
        None => return Ok(None),
    };
    let roots = load_root_store(roots_path.to_str().unwrap_or_default())
        .map_err(|e| format!("loading upstream TLS roots: {e}"))?;
    let cfg = match (
        &args.upstream_tls_client_cert,
        &args.upstream_tls_client_key,
    ) {
        (None, None) => TlsConfig::client_from_roots(roots),
        (Some(cert), Some(key)) => {
            let chain = load_certs(cert.to_str().unwrap_or_default())
                .map_err(|e| format!("loading upstream client cert: {e}"))?;
            let priv_key = load_private_key(key.to_str().unwrap_or_default())
                .map_err(|e| format!("loading upstream client key: {e}"))?;
            TlsConfig::client_mtls(roots, chain, priv_key)
                .map_err(|e| format!("upstream mTLS build: {e}"))?
        }
        _ => {
            return Err(
                "--upstream-tls-client-cert and --upstream-tls-client-key must both be \
                 set or both unset"
                    .into(),
            );
        }
    };
    Ok(Some(cfg))
}

async fn run_once(config: GatewayConfig) -> Result<(), String> {
    tracing::info!("ca-gateway-rs: starting");
    let server = GatewayServer::build(config)
        .await
        .map_err(|e| format!("build failed: {e}"))?;
    server
        .run()
        .await
        .map_err(|e| format!("runtime error: {e}"))
}

fn main() -> ExitCode {
    let args = Args::parse();

    // Apply C ca-gateway's split network routing (-sip/-signore/-cip/-cport)
    // by exporting the EPICS env vars `startEverything` sets
    // (gateway.cc:359-402) BEFORE the tokio runtime spawns worker threads,
    // so the downstream CaServer (EPICS_CAS_*) and upstream CaClient
    // (EPICS_CA_*) read them at construction. Distinct namespaces => the
    // two sides cannot collide. Done here, while the process is still
    // single-threaded, to satisfy `set_var`'s safety contract.
    // C reads the *current* EPICS_CAS_AUTO_BEACON_ADDR_LIST via getenv inside
    // the -cip branch (gateway.cc:367-372) and rewrites a present non-NO value
    // to YES; mirror that read here and pass it into the pure mapping.
    let beacon_auto = std::env::var("EPICS_CAS_AUTO_BEACON_ADDR_LIST").ok();
    let routing = epics_bridge_rs::ca_gateway::routing_env_pairs(
        args.sip.as_deref(),
        args.signore.as_deref(),
        args.cip.as_deref(),
        args.cport,
        beacon_auto.as_deref(),
    );
    // SAFETY: no runtime threads exist yet — the multi-thread tokio runtime
    // is built below, after this loop returns.
    unsafe {
        for (key, value) in &routing {
            std::env::set_var(key, value);
        }
    }

    // One worker, reactor-style — see qsrv_rs.rs: the default per-CPU
    // pool migrates the mostly-serial serving work across idle workers,
    // costing ~35 µs of extra CPU per op on a 96-core host
    // (doc/qsrv-put-perf.md). Multi-thread flavor is kept so
    // `block_on_sync` works from runtime tasks.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ca-gateway-rs: failed to build tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async_main(args))
}

async fn async_main(args: Args) -> ExitCode {
    // Initialize structured logging — RUST_LOG controls verbosity. The
    // gateway's hot paths (cache eviction, command processing, signal
    // handler, conn-event dispatch) all emit via `tracing` so a
    // single env-filter governs the lot.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let config = GatewayConfig {
        pvlist_path: args.pvlist.clone(),
        pvlist_content: None,
        access_path: args.access.clone(),
        putlog_path: args.putlog.clone(),
        putlog_scope: if args.putlog_all {
            PutLogScope::AllWrites
        } else {
            PutLogScope::TrapWrite
        },
        command_path: args.command.clone(),
        report_path: GatewayConfig::resolve_report_path(args.report.clone(), args.no_report),
        preload_path: args.preload.clone(),
        server_port: args.port,
        timeouts: epics_bridge_rs::ca_gateway::CacheTimeouts {
            connect_timeout: std::time::Duration::from_secs(args.connect_timeout),
            inactive_timeout: std::time::Duration::from_secs(args.inactive_timeout),
            dead_timeout: std::time::Duration::from_secs(args.dead_timeout),
            disconnect_timeout: std::time::Duration::from_secs(args.disconnect_timeout),
        },
        reconnect_inhibit: std::time::Duration::from_secs(args.reconnect_inhibit),
        stats_prefix: if args.no_stats {
            String::new()
        } else {
            args.stats_prefix
                .clone()
                .unwrap_or_else(epics_bridge_rs::ca_gateway::default_stats_prefix)
        },
        cleanup_interval: std::time::Duration::from_secs(args.cleanup_interval),
        stats_interval: std::time::Duration::from_secs(args.stats_interval),
        heartbeat_interval: if args.heartbeat_interval == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(args.heartbeat_interval))
        },
        read_only: args.read_only,
        // C `cacheMode` / `-no_cache` (gateway.cc:238/1162): NoCache
        // forwards each get to the IOC and gates the upstream monitor on
        // live downstream monitor interest.
        cache_mode: if args.no_cache {
            epics_bridge_rs::ca_gateway::CacheMode::NoCache
        } else {
            epics_bridge_rs::ca_gateway::CacheMode::Cached
        },
        // C `-mask` resolution (gateway.cc:736-766 char→DBE, :1146
        // keep-default-if-empty): an absent/unrecognised spec keeps the
        // DBE_VALUE|DBE_ALARM default rather than the DBE_LOG-bearing
        // CaChannel subscribe() default.
        event_mask: epics_bridge_rs::ca_gateway::resolve_event_mask(args.mask.as_deref()),
        #[cfg(feature = "ca-gateway-tls")]
        tls: build_tls(&args).unwrap_or_else(|e| {
            tracing::error!(error = %e, "ca-gateway-rs: TLS init failed");
            std::process::exit(2);
        }),
        #[cfg(feature = "ca-gateway-tls")]
        upstream_tls: build_upstream_tls(&args).unwrap_or_else(|e| {
            tracing::error!(error = %e, "ca-gateway-rs: upstream TLS init failed");
            std::process::exit(2);
        }),
        #[cfg(feature = "ca-gateway-tls")]
        upstream_tls_server_name: args.upstream_tls_server_name.clone(),
    };

    if args.supervised {
        let policy = RestartPolicy {
            max_restarts: args.max_restarts,
            window: std::time::Duration::from_secs(args.restart_window),
            delay: std::time::Duration::from_secs(args.restart_delay),
        };
        tracing::info!(
            max_restarts = args.max_restarts,
            window_secs = args.restart_window,
            "ca-gateway-rs: running under supervisor"
        );
        let result = supervise(policy, || {
            let cfg = config.clone();
            async move { run_once(cfg).await }
        })
        .await;

        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(error = %e, "ca-gateway-rs: supervisor exit");
                ExitCode::FAILURE
            }
        }
    } else {
        match run_once(config).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(error = %e, "ca-gateway-rs: error");
                ExitCode::FAILURE
            }
        }
    }
}
