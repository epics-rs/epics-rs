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

use epics_bridge_rs::ca_gateway::{GatewayConfig, GatewayServer, RestartPolicy, supervise};

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

    /// Path to put-event log file (records all client puts).
    #[arg(long)]
    putlog: Option<PathBuf>,

    /// Path to a command file processed when the gateway receives SIGUSR1 (Unix).
    #[arg(long)]
    command: Option<PathBuf>,

    /// CA server port (downstream side). 0 = use default 5064.
    #[arg(long, default_value_t = 0)]
    port: u16,

    /// Read-only mode: reject all puts.
    #[arg(long)]
    read_only: bool,

    /// Disable statistics PV publication.
    #[arg(long)]
    no_stats: bool,

    /// Statistics PV prefix (default: "gateway:").
    #[arg(long, default_value = "gateway:")]
    stats_prefix: String,

    /// Heartbeat interval in seconds (0 = disable).
    #[arg(long, default_value_t = 1)]
    heartbeat_interval: u64,

    /// Cleanup interval in seconds.
    #[arg(long, default_value_t = 10)]
    cleanup_interval: u64,

    /// Statistics refresh interval in seconds.
    #[arg(long, default_value_t = 10)]
    stats_interval: u64,

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
    let cfg = match (&args.upstream_tls_client_cert, &args.upstream_tls_client_key) {
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
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

    let args = Args::parse();

    let config = GatewayConfig {
        pvlist_path: args.pvlist.clone(),
        pvlist_content: None,
        access_path: args.access.clone(),
        putlog_path: args.putlog.clone(),
        command_path: args.command.clone(),
        preload_path: args.preload.clone(),
        server_port: args.port,
        timeouts: Default::default(),
        stats_prefix: if args.no_stats {
            String::new()
        } else {
            args.stats_prefix.clone()
        },
        cleanup_interval: std::time::Duration::from_secs(args.cleanup_interval),
        stats_interval: std::time::Duration::from_secs(args.stats_interval),
        heartbeat_interval: if args.heartbeat_interval == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(args.heartbeat_interval))
        },
        read_only: args.read_only,
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
