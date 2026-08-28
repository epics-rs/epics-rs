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

// On `exec_backend` this program's `main` refuses instead of running, so
// everything below it is unreachable in that configuration by construction.
// The lint is reporting the intent, not dead code: the default build still
// lints this file in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser};

#[cfg(tokio_backend)]
use epics_bridge_rs::ca_gateway::{GatewayConfig, GatewayServer, PutLogScope};
#[cfg(tokio_backend)]
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

    /// CA gateway: R1/R2/R3 report file (C `-report`). Overrides the default
    /// `gateway.report`. Report commands and the SIGUSR2 shortcut append
    /// C-compatible sections here.
    #[arg(long)]
    ca_report: Option<PathBuf>,

    /// CA gateway: disable the report file entirely (Rust-only). C ca-gateway
    /// always has a report file (defaults to `gateway.report`); pass
    /// `--ca-no-report` to opt out so no report is written.
    #[arg(long, conflicts_with = "ca_report")]
    ca_no_report: bool,

    /// CA server port (downstream side). Omit to take
    /// `EPICS_CAS_SERVER_PORT`, then `EPICS_CA_SERVER_PORT`, then 5064.
    /// `--ca-port 0` binds an ephemeral port.
    #[arg(long)]
    ca_port: Option<u16>,

    /// CA gateway: downstream listen-interface list (C `-sip`), sets
    /// `EPICS_CAS_INTF_ADDR_LIST`. Space-separated.
    #[arg(long)]
    ca_sip: Option<String>,

    /// CA gateway: downstream ignore-address list (C `-signore`), sets
    /// `EPICS_CAS_IGNORE_ADDR_LIST`. Space-separated.
    #[arg(long)]
    ca_signore: Option<String>,

    /// CA gateway: upstream search-address list (C `-cip`), sets
    /// `EPICS_CA_ADDR_LIST` AND forces `EPICS_CA_AUTO_ADDR_LIST=NO` for the
    /// upstream client. Space-separated.
    #[arg(long)]
    ca_cip: Option<String>,

    /// CA gateway: upstream CA search port (C `-cport`), sets
    /// `EPICS_CA_SERVER_PORT` for the upstream client.
    #[arg(long)]
    ca_cport: Option<u16>,

    /// CA gateway: read-only mode (rejects all puts).
    #[arg(long)]
    ca_read_only: bool,

    /// CA gateway: disable caching (C `-no_cache`): forward every get
    /// request to the IOC and create the upstream monitor only while a
    /// downstream client is monitoring the PV. Default is caching on.
    #[arg(long)]
    ca_no_cache: bool,

    /// CA gateway: upstream monitor event mask (C `-mask`). DBE selector
    /// characters `v`=value, `a`=alarm, `l`=log/archive, `p`=property
    /// (case-insensitive). Unset — or no recognised selector — keeps the
    /// ca-gateway default `va` (DBE_VALUE|DBE_ALARM), which excludes
    /// DBE_LOG.
    #[arg(long)]
    ca_mask: Option<String>,

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

    /// CA gateway: upstream connect timeout in seconds
    /// (C `-connect_timeout`).
    #[arg(long, default_value_t = 1)]
    ca_connect_timeout: u64,

    /// CA gateway: inactive-PV retention in seconds
    /// (C `-inactive_timeout`).
    #[arg(long, default_value_t = 60 * 60 * 2)]
    ca_inactive_timeout: u64,

    /// CA gateway: dead-PV retention in seconds (C `-dead_timeout`).
    #[arg(long, default_value_t = 60 * 2)]
    ca_dead_timeout: u64,

    /// CA gateway: disconnected-PV retention in seconds
    /// (C `-disconnect_timeout`).
    #[arg(long, default_value_t = 60 * 60 * 2)]
    ca_disconnect_timeout: u64,

    /// CA gateway: reconnect beacon-anomaly inhibit window in seconds
    /// (C `-reconnect_inhibit`).
    #[arg(long, default_value_t = 60 * 5)]
    ca_reconnect_inhibit: u64,

    // ── PVA-side flags ───────────────────────────────────────────────
    /// Disable the PVA gateway entirely (CA-only mode). Alias:
    /// `--ca-only`.
    #[arg(long, alias = "ca-only", conflicts_with = "no_ca")]
    no_pva: bool,

    /// Bind IP for the downstream PVA TCP listener.
    #[arg(long, default_value = "0.0.0.0")]
    pva_bind: IpAddr,

    /// Downstream PVA TCP port. Omit to take `EPICS_PVAS_SERVER_PORT`,
    /// then `EPICS_PVA_SERVER_PORT`, then 5075.
    #[arg(long)]
    pva_tcp_port: Option<u16>,

    /// Downstream PVA UDP search port. Omit to take
    /// `EPICS_PVAS_BROADCAST_PORT`, then `EPICS_PVA_BROADCAST_PORT`,
    /// then 5076.
    #[arg(long)]
    pva_udp_port: Option<u16>,

    /// Per-PV upstream connect timeout in seconds (PVA gateway). Omit to
    /// take `EPICS_PVA_GW_CONNECT_TMO`, then the 5 s default.
    #[arg(long)]
    pva_connect_timeout_secs: Option<u64>,

    /// PVA cache cleanup interval in seconds. Omit to take
    /// `EPICS_PVA_GW_CLEANUP_INTERVAL`, then the 30 s default.
    #[arg(long)]
    pva_cleanup_interval_secs: Option<u64>,

    /// PVA control_prefix for runtime-diagnostic PVs. An explicit empty
    /// string disables the feature; omit to take
    /// `EPICS_PVA_GW_CONTROL_PREFIX`.
    #[arg(long)]
    pva_control_prefix: Option<String>,

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
    report: Option<PathBuf>,
    no_report: Option<bool>,
    port: Option<u16>,
    sip: Option<String>,
    signore: Option<String>,
    cip: Option<String>,
    cport: Option<u16>,
    read_only: Option<bool>,
    no_cache: Option<bool>,
    stats_prefix: Option<String>,
    heartbeat_interval: Option<u64>,
    cleanup_interval: Option<u64>,
    stats_interval: Option<u64>,
    connect_timeout: Option<u64>,
    inactive_timeout: Option<u64>,
    dead_timeout: Option<u64>,
    disconnect_timeout: Option<u64>,
    reconnect_inhibit: Option<u64>,
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
# report = "/var/log/gateway.report"   # override default report file (gateway.report)
# no_report = false                     # true = disable the report file entirely
# port = 5064
# sip = "192.168.1.10"                   # -sip: EPICS_CAS_INTF_ADDR_LIST
# signore = "192.168.9.0"                # -signore: EPICS_CAS_IGNORE_ADDR_LIST
# cip = "10.0.0.1 10.0.0.2"              # -cip: EPICS_CA_ADDR_LIST (+AUTO=NO)
# cport = 5066                           # -cport: EPICS_CA_SERVER_PORT
# read_only = false
# no_cache = false                       # -no_cache: forward gets, lazy monitor
# stats_prefix = "gateway"               # bare namespace; ':' added at publish
# heartbeat_interval = 1
# cleanup_interval = 10
# stats_interval = 10
# connect_timeout = 1                    # upstream connect timeout (s)
# inactive_timeout = 7200                # idle PV -> inactive (s)
# dead_timeout = 120                     # disconnected PV -> dead (s)
# disconnect_timeout = 7200              # active PV disconnect grace (s)
# reconnect_inhibit = 300                # post-beacon reconnect inhibit (s)

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

#[cfg(tokio_backend)]
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
        report_path: GatewayConfig::resolve_report_path(args.ca_report.clone(), args.ca_no_report),
        preload_path: args.ca_preload.clone(),
        server_port: args.ca_port,
        timeouts: epics_bridge_rs::ca_gateway::CacheTimeouts {
            connect_timeout: Duration::from_secs(args.ca_connect_timeout),
            inactive_timeout: Duration::from_secs(args.ca_inactive_timeout),
            dead_timeout: Duration::from_secs(args.ca_dead_timeout),
            disconnect_timeout: Duration::from_secs(args.ca_disconnect_timeout),
        },
        reconnect_inhibit: Duration::from_secs(args.ca_reconnect_inhibit),
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
        // C `cacheMode` / `-no_cache`: NoCache forwards every get to the
        // IOC and lazily creates the upstream monitor only while a
        // downstream client is monitoring; default Cached keeps a
        // persistent upstream monitor and serves gets from the cache.
        cache_mode: if args.ca_no_cache {
            epics_bridge_rs::ca_gateway::CacheMode::NoCache
        } else {
            epics_bridge_rs::ca_gateway::CacheMode::Cached
        },
        // C `-mask` resolution (gateway.cc:736-766 / :1146): absent or
        // unrecognised spec keeps DBE_VALUE|DBE_ALARM, never the
        // DBE_LOG-bearing CaChannel subscribe() default.
        event_mask: epics_bridge_rs::ca_gateway::resolve_event_mask(args.ca_mask.as_deref()),
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

#[cfg(tokio_backend)]
async fn run_pva_gateway(args: &Args) -> Result<(), String> {
    // Precedence is flag > TOML > EPICS env > compiled default. The TOML
    // layer has already been merged into `args` (`merge_config`), so a
    // still-`None` field means neither source spoke and the environment
    // decides.
    //
    // PR #205 IPv6 Stage 1: PvaServerConfig::bind_ip is `IpAddr`.
    let server_config = PvaServerConfig {
        tcp_port: args
            .pva_tcp_port
            .unwrap_or_else(epics_pva_rs::config::env::pvas_server_port),
        udp_port: args
            .pva_udp_port
            .unwrap_or_else(epics_pva_rs::config::env::server_broadcast_port),
        bind_ip: args.pva_bind,
        ..PvaServerConfig::default()
    };
    let base = PvaGatewayConfig::default().with_env();
    let control_prefix = match args.pva_control_prefix.as_deref() {
        // An explicit empty string is the operator disabling the feature.
        Some(s) if s.trim().is_empty() => None,
        Some(s) => Some(s.to_string()),
        None => base.control_prefix.clone(),
    };
    let cfg = PvaGatewayConfig {
        upstream_client: None,
        server_config,
        cleanup_interval: args
            .pva_cleanup_interval_secs
            .map(Duration::from_secs)
            .unwrap_or(base.cleanup_interval),
        connect_timeout: args
            .pva_connect_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(base.connect_timeout),
        control_prefix,
        ..base
    };
    tracing::info!("dual-gateway-rs: starting PVA gateway");
    let reactor = epics_base_rs::runtime::task::Reactor::current()
        .expect("run_pva_gateway is awaited on the daemon's runtime");
    let gateway = PvaGateway::start(&reactor, cfg).map_err(|e| format!("PVA start failed: {e}"))?;
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

/// Merge a TOML `ConfigFile` into the CLI [`Args`]. A TOML value fills a
/// field only when the operator did NOT pass that flag on the command
/// line; an explicit CLI flag always wins.
///
/// Which fields came from the command line is read from clap via
/// [`ArgMatches::value_source`], not by comparing each field against its
/// hardcoded default literal. The literal comparison silently stopped
/// honouring the TOML the moment a default changed, and could not tell
/// "operator typed the default value" from "operator typed nothing" —
/// `value_source` distinguishes them by construction.
fn merge_config(args: &mut Args, cfg: &ConfigFile, matches: &ArgMatches) {
    // True when flag `id` was supplied on the command line (its value did
    // not come from clap's default), so the CLI must override the TOML.
    let cli_set = |id: &str| {
        !matches!(
            matches.value_source(id),
            None | Some(ValueSource::DefaultValue)
        )
    };

    // ── CA section ────────────────────────────────────────────────
    // `enabled = false` disables the CA side. `--no-ca` already set the
    // flag and TOML never clears it, so an explicit CLI still wins.
    if cfg.ca.enabled == Some(false) {
        args.no_ca = true;
    }
    if !cli_set("ca_pvlist") {
        if let Some(v) = &cfg.ca.pvlist {
            args.ca_pvlist = Some(v.clone());
        }
    }
    if !cli_set("ca_access") {
        if let Some(v) = &cfg.ca.access {
            args.ca_access = Some(v.clone());
        }
    }
    if !cli_set("ca_putlog") {
        if let Some(v) = &cfg.ca.putlog {
            args.ca_putlog = Some(v.clone());
        }
    }
    if !cli_set("ca_putlog_all") {
        if let Some(v) = cfg.ca.putlog_all {
            args.ca_putlog_all = v;
        }
    }
    if !cli_set("ca_preload") {
        if let Some(v) = &cfg.ca.preload {
            args.ca_preload = Some(v.clone());
        }
    }
    if !cli_set("ca_command") {
        if let Some(v) = &cfg.ca.command {
            args.ca_command = Some(v.clone());
        }
    }
    if !cli_set("ca_report") {
        if let Some(v) = &cfg.ca.report {
            args.ca_report = Some(v.clone());
        }
    }
    if !cli_set("ca_no_report") {
        if let Some(v) = cfg.ca.no_report {
            args.ca_no_report = v;
        }
    }
    if !cli_set("ca_port") {
        if let Some(v) = cfg.ca.port {
            args.ca_port = Some(v);
        }
    }
    if !cli_set("ca_sip") {
        if let Some(v) = &cfg.ca.sip {
            args.ca_sip = Some(v.clone());
        }
    }
    if !cli_set("ca_signore") {
        if let Some(v) = &cfg.ca.signore {
            args.ca_signore = Some(v.clone());
        }
    }
    if !cli_set("ca_cip") {
        if let Some(v) = &cfg.ca.cip {
            args.ca_cip = Some(v.clone());
        }
    }
    if !cli_set("ca_cport") {
        if let Some(v) = cfg.ca.cport {
            args.ca_cport = Some(v);
        }
    }
    if !cli_set("ca_read_only") {
        if let Some(v) = cfg.ca.read_only {
            args.ca_read_only = v;
        }
    }
    if !cli_set("ca_no_cache") {
        if let Some(v) = cfg.ca.no_cache {
            args.ca_no_cache = v;
        }
    }
    if !cli_set("ca_stats_prefix") {
        if let Some(v) = &cfg.ca.stats_prefix {
            args.ca_stats_prefix = Some(v.clone());
        }
    }
    if !cli_set("ca_heartbeat_interval") {
        if let Some(v) = cfg.ca.heartbeat_interval {
            args.ca_heartbeat_interval = v;
        }
    }
    if !cli_set("ca_cleanup_interval") {
        if let Some(v) = cfg.ca.cleanup_interval {
            args.ca_cleanup_interval = v;
        }
    }
    if !cli_set("ca_stats_interval") {
        if let Some(v) = cfg.ca.stats_interval {
            args.ca_stats_interval = v;
        }
    }
    if !cli_set("ca_connect_timeout") {
        if let Some(v) = cfg.ca.connect_timeout {
            args.ca_connect_timeout = v;
        }
    }
    if !cli_set("ca_inactive_timeout") {
        if let Some(v) = cfg.ca.inactive_timeout {
            args.ca_inactive_timeout = v;
        }
    }
    if !cli_set("ca_dead_timeout") {
        if let Some(v) = cfg.ca.dead_timeout {
            args.ca_dead_timeout = v;
        }
    }
    if !cli_set("ca_disconnect_timeout") {
        if let Some(v) = cfg.ca.disconnect_timeout {
            args.ca_disconnect_timeout = v;
        }
    }
    if !cli_set("ca_reconnect_inhibit") {
        if let Some(v) = cfg.ca.reconnect_inhibit {
            args.ca_reconnect_inhibit = v;
        }
    }

    // ── PVA section ───────────────────────────────────────────────
    if cfg.pva.enabled == Some(false) {
        args.no_pva = true;
    }
    if !cli_set("pva_bind") {
        if let Some(b) = &cfg.pva.bind {
            if let Ok(ip) = b.parse() {
                args.pva_bind = ip;
            }
        }
    }
    if !cli_set("pva_tcp_port") {
        if let Some(v) = cfg.pva.tcp_port {
            args.pva_tcp_port = Some(v);
        }
    }
    if !cli_set("pva_udp_port") {
        if let Some(v) = cfg.pva.udp_port {
            args.pva_udp_port = Some(v);
        }
    }
    if !cli_set("pva_connect_timeout_secs") {
        if let Some(v) = cfg.pva.connect_timeout_secs {
            args.pva_connect_timeout_secs = Some(v);
        }
    }
    if !cli_set("pva_cleanup_interval_secs") {
        if let Some(v) = cfg.pva.cleanup_interval_secs {
            args.pva_cleanup_interval_secs = Some(v);
        }
    }
    if !cli_set("pva_control_prefix") {
        if let Some(s) = &cfg.pva.control_prefix {
            args.pva_control_prefix = Some(s.clone());
        }
    }
    if !cli_set("pva_prefetch") {
        if let Some(v) = &cfg.pva.prefetch {
            args.pva_prefetch = v.clone();
        }
    }
}

#[cfg(tokio_backend)]
fn main() -> ExitCode {
    // Keep the raw `ArgMatches` so `merge_config` can ask clap which
    // flags the operator actually typed (vs. clap-supplied defaults),
    // then materialise the typed `Args` from the same matches.
    let matches = Args::command().get_matches();
    let mut args = match Args::from_arg_matches(&matches) {
        Ok(args) => args,
        Err(e) => e.exit(),
    };
    init_tracing(args.verbose);

    if args.print_default_config {
        print!("{}", default_config_toml());
        return ExitCode::SUCCESS;
    }

    if let Some(path) = args.config.clone() {
        match load_config(&path) {
            Ok(cfg) => merge_config(&mut args, &cfg, &matches),
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

    // Apply the CA gateway's split network routing (-sip/-signore/-cip/-cport)
    // by exporting the EPICS env vars C's `startEverything` sets
    // (gateway.cc:359-402) BEFORE the tokio runtime spawns worker threads, so
    // the downstream CaServer (EPICS_CAS_*) and upstream CaClient (EPICS_CA_*)
    // read them at construction. Only when the CA side is enabled; the PVA
    // side uses the EPICS_PVA_* namespace and is unaffected.
    if !args.no_ca {
        // C reads the *current* EPICS_CAS_AUTO_BEACON_ADDR_LIST via getenv
        // inside the -cip branch (gateway.cc:367-372) and rewrites a present
        // non-NO value to YES; mirror that read here and pass it in.
        let beacon_auto = std::env::var("EPICS_CAS_AUTO_BEACON_ADDR_LIST").ok();
        let routing = epics_bridge_rs::ca_gateway::routing_env_pairs(
            args.ca_sip.as_deref(),
            args.ca_signore.as_deref(),
            args.ca_cip.as_deref(),
            args.ca_cport,
            beacon_auto.as_deref(),
        );
        // SAFETY: still single-threaded — the multi-thread runtime is built
        // on the next statement, after this loop returns.
        unsafe {
            for (key, value) in &routing {
                std::env::set_var(key, value);
            }
        }
    }

    // One worker, reactor-style — see qsrv_rs.rs: the default per-CPU
    // pool migrates the mostly-serial serving work across idle workers,
    // costing ~35 µs of extra CPU per op on a 96-core host. Multi-thread
    // flavor is kept so `block_on_sync` works from runtime tasks.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("dual-gateway-rs: failed to build tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async_main(args))
}

#[cfg(tokio_backend)]
async fn async_main(args: Args) -> ExitCode {
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

/// The `exec_backend` arm. Both gateways are clients *and* servers through the
/// reactor-bound front-ends, so on the reactor-free backend neither half
/// exists. Nothing replaces it: an RTEMS image runs an IOC, not a gateway.
#[cfg(exec_backend)]
fn main() -> ExitCode {
    eprintln!(
        "dual-gateway-rs: this build selects the reactor-free execution \
         backend (EPICS_RS_BUILD_EXEC_BACKEND=thread), and the CA and PVA gateway halves both \
         need a tokio reactor. Rebuild without that feature to run the gateway."
    );
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_and_matches(argv: &[&str]) -> (Args, ArgMatches) {
        let matches = Args::command()
            .try_get_matches_from(argv)
            .expect("args parse");
        let args = Args::from_arg_matches(&matches).expect("args from matches");
        (args, matches)
    }

    fn cfg(toml_src: &str) -> ConfigFile {
        toml::from_str(toml_src).expect("toml parse")
    }

    #[test]
    fn toml_fills_fields_the_cli_omitted() {
        let (mut args, matches) = args_and_matches(&["dual-gateway-rs"]);
        let c =
            cfg("[ca]\ndead_timeout = 45\npvlist = \"/etc/gw.pvlist\"\n[pva]\ntcp_port = 6000\n");
        merge_config(&mut args, &c, &matches);
        assert_eq!(args.ca_dead_timeout, 45);
        assert_eq!(
            args.ca_pvlist.as_deref(),
            Some(std::path::Path::new("/etc/gw.pvlist"))
        );
        assert_eq!(args.pva_tcp_port, Some(6000));
    }

    #[test]
    fn explicit_cli_flag_beats_toml() {
        let (mut args, matches) = args_and_matches(&[
            "dual-gateway-rs",
            "--ca-dead-timeout",
            "999",
            "--pva-tcp-port",
            "7000",
        ]);
        let c = cfg("[ca]\ndead_timeout = 45\n[pva]\ntcp_port = 6000\n");
        merge_config(&mut args, &c, &matches);
        assert_eq!(args.ca_dead_timeout, 999);
        assert_eq!(args.pva_tcp_port, Some(7000));
    }

    #[test]
    fn cli_typing_the_default_value_still_blocks_toml() {
        // The exact case the old default-literal comparison got wrong:
        // typing the clap default (`120`) explicitly must still count as
        // an operator choice and block the TOML override.
        let (mut args, matches) =
            args_and_matches(&["dual-gateway-rs", "--ca-dead-timeout", "120"]);
        let c = cfg("[ca]\ndead_timeout = 45\n");
        merge_config(&mut args, &c, &matches);
        assert_eq!(args.ca_dead_timeout, 120);
    }

    #[test]
    fn defaults_survive_when_neither_source_sets() {
        let (mut args, matches) = args_and_matches(&["dual-gateway-rs"]);
        merge_config(&mut args, &cfg(""), &matches);
        assert_eq!(args.ca_dead_timeout, 120);
    }

    /// A port neither the CLI nor the TOML set must stay `None` all the
    /// way through `merge_config`, so the EPICS environment — not a clap
    /// literal — decides the bind port. Asserting a concrete 5064/5075
    /// here is exactly what pinned R19-22: the literal shadowed
    /// `EPICS_CAS_SERVER_PORT` / `EPICS_PVAS_SERVER_PORT`.
    #[test]
    fn unset_ports_stay_none_for_the_environment_to_resolve() {
        let (mut args, matches) = args_and_matches(&["dual-gateway-rs"]);
        merge_config(&mut args, &cfg(""), &matches);
        assert_eq!(args.ca_port, None);
        assert_eq!(args.pva_tcp_port, None);
        assert_eq!(args.pva_udp_port, None);
        assert_eq!(args.pva_connect_timeout_secs, None);
        assert_eq!(args.pva_cleanup_interval_secs, None);
    }

    #[test]
    fn toml_enabled_false_disables_that_side_only() {
        let (mut args, matches) = args_and_matches(&["dual-gateway-rs"]);
        merge_config(&mut args, &cfg("[pva]\nenabled = false\n"), &matches);
        assert!(args.no_pva);
        assert!(!args.no_ca);
    }
}
