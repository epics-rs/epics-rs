//! Dual-protocol gateway daemon — single process running the CA
//! gateway and the PVA gateway side by side.
//!
//! Two independent gateway runtimes share one process; they don't
//! cross-translate (CA stays CA, PVA stays PVA). Use this when ops
//! prefers managing one daemon over two but the upstream IOC fleet
//! speaks both protocols (or different IOCs each speak one).
//!
//! Usage:
//!
//! ```text
//! dual-gateway-rs \
//!   --ca-pvlist /etc/gw/gateway.pvlist --ca-access /etc/gw/access.acf \
//!   --ca-port 5064 \
//!   --pva-tcp-port 5075 --pva-udp-port 5076
//! ```
//!
//! Either side can be disabled at runtime:
//!
//! ```text
//! dual-gateway-rs --no-ca   # PVA only
//! dual-gateway-rs --no-pva  # CA only (equivalent to ca-gateway-rs)
//! ```
//!
//! Lifecycle: a `tokio::select!` watches both gateway tasks; the
//! first one to exit terminates the process and aborts the other.
//! Mirrors the abort-the-loser pattern from `PvaServer::wait`.

use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

use epics_bridge_rs::ca_gateway::{GatewayConfig, GatewayServer, PutLogScope};
use epics_bridge_rs::pva_gateway::{PvaGateway, PvaGatewayConfig};
use epics_pva_rs::server_native::PvaServerConfig;

#[derive(Parser, Debug)]
#[command(
    name = "dual-gateway-rs",
    about = "Pure Rust EPICS dual-protocol gateway: runs CA and PVA gateways in one process",
    version
)]
struct Args {
    // ── CA-side flags ────────────────────────────────────────────────
    /// Disable the CA gateway entirely (PVA-only mode). Alias:
    /// `--pva-only`.
    #[arg(long, alias = "pva-only", conflicts_with = "no_pva")]
    no_ca: bool,

    /// Path to .pvlist access list file (CA gateway).
    #[arg(long)]
    ca_pvlist: Option<PathBuf>,

    /// Path to .access ACF file (CA gateway).
    #[arg(long)]
    ca_access: Option<PathBuf>,

    /// Path to put-event log file (CA gateway). TRAPWRITE-scoped by
    /// default (C contract); see `--ca-putlog-all`.
    #[arg(long)]
    ca_putlog: Option<PathBuf>,

    /// Broaden the CA gateway put log to a fail-loud audit: record every
    /// client write attempt with its outcome, not only TRAPWRITE-matched
    /// granted writes. Not the C ca-gateway contract.
    #[arg(long)]
    ca_putlog_all: bool,

    /// Path to a literal-PV preload list (CA gateway).
    #[arg(long)]
    ca_preload: Option<PathBuf>,

    /// Path to a SIGUSR1-triggered command file (CA gateway, Unix only).
    #[arg(long)]
    ca_command: Option<PathBuf>,

    /// CA server port (downstream side). 0 = use default 5064.
    #[arg(long, default_value_t = 0)]
    ca_port: u16,

    /// CA gateway: read-only mode (rejects all puts).
    #[arg(long)]
    ca_read_only: bool,

    /// CA gateway: stats PV namespace (C `-prefix`). The `:` separator is
    /// inserted automatically (`<prefix>:<name>`); pass the bare namespace.
    /// Defaults to the host name (fallback `gateway`); an explicit empty
    /// string disables stats PVs.
    #[arg(long)]
    ca_stats_prefix: Option<String>,

    /// CA gateway: heartbeat interval in seconds (0 = disable).
    #[arg(long, default_value_t = 1)]
    ca_heartbeat_interval: u64,

    /// CA gateway: cleanup interval in seconds.
    #[arg(long, default_value_t = 10)]
    ca_cleanup_interval: u64,

    /// CA gateway: stats refresh interval in seconds.
    #[arg(long, default_value_t = 10)]
    ca_stats_interval: u64,

    // ── PVA-side flags ───────────────────────────────────────────────
    /// Disable the PVA gateway entirely (CA-only mode). Alias:
    /// `--ca-only`.
    #[arg(long, alias = "ca-only", conflicts_with = "no_ca")]
    no_pva: bool,

    /// Bind IP for the downstream PVA TCP listener.
    #[arg(long, default_value = "0.0.0.0")]
    pva_bind: IpAddr,

    /// Downstream PVA TCP port (default 5075).
    #[arg(long, default_value_t = 5075)]
    pva_tcp_port: u16,

    /// Downstream PVA UDP search port (default 5076).
    #[arg(long, default_value_t = 5076)]
    pva_udp_port: u16,

    /// Per-PV upstream connect timeout in seconds (PVA gateway).
    #[arg(long, default_value_t = 5)]
    pva_connect_timeout_secs: u64,

    /// PVA cache cleanup interval in seconds.
    #[arg(long, default_value_t = 30)]
    pva_cleanup_interval_secs: u64,

    /// PVA control_prefix for runtime-diagnostic PVs. Empty
    /// disables the feature.
    #[arg(long, default_value = "")]
    pva_control_prefix: String,

    /// Pre-warm the PVA cache with these names (comma-separated).
    #[arg(long = "pva-prefetch", num_args = 1.., value_delimiter = ',')]
    pva_prefetch: Vec<String>,

    // ── shared flags ─────────────────────────────────────────────────
    /// Path to a TOML config file. Values from the file fill in
    /// defaults; explicit CLI flags still take precedence so
    /// operators can override per-run without editing the file.
    /// See `--print-default-config` for the schema.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Print a default TOML config to stdout and exit. Useful as a
    /// `--config` template.
    #[arg(long)]
    print_default_config: bool,

    /// Bump tracing verbosity. Repeat for more (`-v` info, `-vv`
    /// debug, `-vvv` trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

/// TOML schema. All fields optional — anything missing falls back
/// to the CLI default. CLI flags override TOML when both are
/// supplied. Section names mirror the `--ca-*` / `--pva-*`
/// flag prefixes so config files read like the CLI.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
struct ConfigFile {
    #[serde(default)]
    ca: CaSection,
    #[serde(default)]
    pva: PvaSection,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct CaSection {
    enabled: Option<bool>,
    pvlist: Option<PathBuf>,
    access: Option<PathBuf>,
    putlog: Option<PathBuf>,
    putlog_all: Option<bool>,
    preload: Option<PathBuf>,
    command: Option<PathBuf>,
    port: Option<u16>,
    read_only: Option<bool>,
    stats_prefix: Option<String>,
    heartbeat_interval: Option<u64>,
    cleanup_interval: Option<u64>,
    stats_interval: Option<u64>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct PvaSection {
    enabled: Option<bool>,
    bind: Option<String>,
    tcp_port: Option<u16>,
    udp_port: Option<u16>,
    connect_timeout_secs: Option<u64>,
    cleanup_interval_secs: Option<u64>,
    control_prefix: Option<String>,
    prefetch: Option<Vec<String>>,
}

fn load_config(path: &PathBuf) -> Result<ConfigFile, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("read config {}: {e}", path.display()))?;
    toml::from_str::<ConfigFile>(&raw).map_err(|e| format!("parse config {}: {e}", path.display()))
}

fn default_config_toml() -> &'static str {
    r#"# dual-gateway-rs configuration. Every value is optional; missing
# fields use the CLI default. CLI flags override TOML at runtime.

[ca]
# enabled = true                        # set false to disable the CA gateway
# pvlist = "/etc/gw/gateway.pvlist"
# access = "/etc/gw/access.acf"
# putlog = "/var/log/gateway-puts.log"
# putlog_all = false                    # true = broader fail-loud audit (non-C)
# preload = "/etc/gw/preload.txt"
# command = "/etc/gw/command.cmd"      # SIGUSR1-triggered (Unix)
# port = 5064
# read_only = false
# stats_prefix = "gateway"               # bare namespace; ':' added at publish
# heartbeat_interval = 1
# cleanup_interval = 10
# stats_interval = 10

[pva]
# enabled = true                        # set false to disable the PVA gateway
# bind = "0.0.0.0"
# tcp_port = 5075
# udp_port = 5076
# connect_timeout_secs = 5
# cleanup_interval_secs = 30
# control_prefix = "gw"
# prefetch = ["UPS:VOLTAGE", "UPS:CURRENT"]
"#
}

fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| level.to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .try_init();
}

async fn run_ca_gateway(args: &Args) -> Result<(), String> {
    let config = GatewayConfig {
        pvlist_path: args.ca_pvlist.clone(),
        pvlist_content: None,
        access_path: args.ca_access.clone(),
        putlog_path: args.ca_putlog.clone(),
        putlog_scope: if args.ca_putlog_all {
            PutLogScope::AllWrites
        } else {
            PutLogScope::TrapWrite
        },
        command_path: args.ca_command.clone(),
        preload_path: args.ca_preload.clone(),
        server_port: args.ca_port,
        timeouts: Default::default(),
        stats_prefix: args
            .ca_stats_prefix
            .clone()
            .unwrap_or_else(epics_bridge_rs::ca_gateway::default_stats_prefix),
        cleanup_interval: Duration::from_secs(args.ca_cleanup_interval),
        stats_interval: Duration::from_secs(args.ca_stats_interval),
        heartbeat_interval: if args.ca_heartbeat_interval == 0 {
            None
        } else {
            Some(Duration::from_secs(args.ca_heartbeat_interval))
        },
        read_only: args.ca_read_only,
        #[cfg(feature = "ca-gateway-tls")]
        tls: None,
        // B10: the dual-gateway binary does not yet expose upstream
        // TLS flags; upstream TLS falls back to the `EPICS_CA_TLS_*`
        // environment variables honoured by `CaClient::new`.
        #[cfg(feature = "ca-gateway-tls")]
        upstream_tls: None,
        #[cfg(feature = "ca-gateway-tls")]
        upstream_tls_server_name: None,
    };
    tracing::info!("dual-gateway-rs: building CA gateway");
    let server = GatewayServer::build(config)
        .await
        .map_err(|e| format!("CA build failed: {e}"))?;
    server
        .run()
        .await
        .map_err(|e| format!("CA runtime error: {e}"))
}

async fn run_pva_gateway(args: &Args) -> Result<(), String> {
    // PR #205 IPv6 Stage 1: PvaServerConfig::bind_ip is `IpAddr`.
    let server_config = PvaServerConfig {
        tcp_port: args.pva_tcp_port,
        udp_port: args.pva_udp_port,
        bind_ip: args.pva_bind,
        ..PvaServerConfig::default()
    };
    let control_prefix = if args.pva_control_prefix.trim().is_empty() {
        None
    } else {
        Some(args.pva_control_prefix.clone())
    };
    let cfg = PvaGatewayConfig {
        upstream_client: None,
        server_config,
        cleanup_interval: Duration::from_secs(args.pva_cleanup_interval_secs),
        connect_timeout: Duration::from_secs(args.pva_connect_timeout_secs),
        control_prefix,
        ..PvaGatewayConfig::default()
    };
    tracing::info!("dual-gateway-rs: starting PVA gateway");
    let gateway = PvaGateway::start(cfg).map_err(|e| format!("PVA start failed: {e}"))?;
    if !args.pva_prefetch.is_empty() {
        let names: Vec<&str> = args.pva_prefetch.iter().map(String::as_str).collect();
        tracing::info!(count = names.len(), "pva-gateway: pre-warming cache");
        gateway.prefetch(&names).await;
    }
    let report = gateway.report();
    tracing::info!(
        tcp = report.tcp_port,
        udp = report.udp_port,
        "dual-gateway-rs: PVA listener up"
    );
    gateway
        .run()
        .await
        .map_err(|e| format!("PVA runtime error: {e}"))
}

/// Merge a TOML `ConfigFile` into the CLI [`Args`]. CLI values that
/// were explicitly supplied (i.e. differ from clap default) win;
/// otherwise we fall back to the TOML value. We use the simple rule
/// "TOML fills in any field where the CLI is at its default" rather
/// than introspecting clap's `matches.contains_id` because most
/// dual-gateway operators set things explicitly via either CLI *or*
/// TOML, not both.
fn merge_config(args: &mut Args, cfg: &ConfigFile) {
    // CA section
    if let Some(enabled) = cfg.ca.enabled {
        if !enabled {
            args.no_ca = true;
        }
    }
    if args.ca_pvlist.is_none() {
        args.ca_pvlist = cfg.ca.pvlist.clone();
    }
    if args.ca_access.is_none() {
        args.ca_access = cfg.ca.access.clone();
    }
    if args.ca_putlog.is_none() {
        args.ca_putlog = cfg.ca.putlog.clone();
    }
    if !args.ca_putlog_all {
        if let Some(true) = cfg.ca.putlog_all {
            args.ca_putlog_all = true;
        }
    }
    if args.ca_preload.is_none() {
        args.ca_preload = cfg.ca.preload.clone();
    }
    if args.ca_command.is_none() {
        args.ca_command = cfg.ca.command.clone();
    }
    if args.ca_port == 0 {
        if let Some(p) = cfg.ca.port {
            args.ca_port = p;
        }
    }
    if !args.ca_read_only {
        if let Some(true) = cfg.ca.read_only {
            args.ca_read_only = true;
        }
    }
    if args.ca_stats_prefix.is_none() {
        args.ca_stats_prefix = cfg.ca.stats_prefix.clone();
    }
    if args.ca_heartbeat_interval == 1 {
        if let Some(v) = cfg.ca.heartbeat_interval {
            args.ca_heartbeat_interval = v;
        }
    }
    if args.ca_cleanup_interval == 10 {
        if let Some(v) = cfg.ca.cleanup_interval {
            args.ca_cleanup_interval = v;
        }
    }
    if args.ca_stats_interval == 10 {
        if let Some(v) = cfg.ca.stats_interval {
            args.ca_stats_interval = v;
        }
    }

    // PVA section
    if let Some(enabled) = cfg.pva.enabled {
        if !enabled {
            args.no_pva = true;
        }
    }
    if args.pva_bind.to_string() == "0.0.0.0" {
        if let Some(b) = &cfg.pva.bind {
            if let Ok(ip) = b.parse() {
                args.pva_bind = ip;
            }
        }
    }
    if args.pva_tcp_port == 5075 {
        if let Some(p) = cfg.pva.tcp_port {
            args.pva_tcp_port = p;
        }
    }
    if args.pva_udp_port == 5076 {
        if let Some(p) = cfg.pva.udp_port {
            args.pva_udp_port = p;
        }
    }
    if args.pva_connect_timeout_secs == 5 {
        if let Some(v) = cfg.pva.connect_timeout_secs {
            args.pva_connect_timeout_secs = v;
        }
    }
    if args.pva_cleanup_interval_secs == 30 {
        if let Some(v) = cfg.pva.cleanup_interval_secs {
            args.pva_cleanup_interval_secs = v;
        }
    }
    if args.pva_control_prefix.is_empty() {
        if let Some(s) = &cfg.pva.control_prefix {
            args.pva_control_prefix = s.clone();
        }
    }
    if args.pva_prefetch.is_empty() {
        if let Some(v) = &cfg.pva.prefetch {
            args.pva_prefetch = v.clone();
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let mut args = Args::parse();
    init_tracing(args.verbose);

    if args.print_default_config {
        print!("{}", default_config_toml());
        return ExitCode::SUCCESS;
    }

    if let Some(path) = args.config.clone() {
        match load_config(&path) {
            Ok(cfg) => merge_config(&mut args, &cfg),
            Err(e) => {
                eprintln!("dual-gateway-rs: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if args.no_ca && args.no_pva {
        eprintln!("dual-gateway-rs: --no-ca and --no-pva together leave nothing to run");
        return ExitCode::FAILURE;
    }

    tracing::info!(
        ca_enabled = !args.no_ca,
        pva_enabled = !args.no_pva,
        "dual-gateway-rs: starting"
    );

    // Run both sides under a single tokio::select!. Whichever exits
    // first terminates the process; the loser is dropped (its
    // gateway's Drop chains tear down sockets/tasks). Matches the
    // abort-the-loser pattern from `PvaServer::wait`.
    let ca_task = async {
        if args.no_ca {
            // Park forever — `select!` ignores this branch.
            std::future::pending::<()>().await;
            Ok(())
        } else {
            run_ca_gateway(&args).await
        }
    };
    let pva_task = async {
        if args.no_pva {
            std::future::pending::<()>().await;
            Ok(())
        } else {
            run_pva_gateway(&args).await
        }
    };

    let result = tokio::select! {
        biased;
        // Ctrl-C handler wins so a normal SIGINT exits both gateways
        // cleanly via the implicit drop chain.
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("dual-gateway-rs: SIGINT received");
            Ok(())
        }
        r = ca_task => match r {
            Ok(()) => {
                tracing::warn!("dual-gateway-rs: CA gateway exited; tearing down PVA");
                Ok(())
            }
            Err(e) => {
                tracing::error!(error = %e, "dual-gateway-rs: CA gateway failed");
                Err(e)
            }
        },
        r = pva_task => match r {
            Ok(()) => {
                tracing::warn!("dual-gateway-rs: PVA gateway exited; tearing down CA");
                Ok(())
            }
            Err(e) => {
                tracing::error!(error = %e, "dual-gateway-rs: PVA gateway failed");
                Err(e)
            }
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
