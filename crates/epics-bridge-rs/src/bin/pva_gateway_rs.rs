//! PVA-to-PVA gateway daemon binary.
//!
//! Two run modes:
//!
//! ```text
//! # 1. Flag mode — one upstream client, one downstream server:
//! pva-gateway-rs [--bind 0.0.0.0|::|::1] [--tcp-port 5075] [--udp-port 5076]
//!                [--connect-timeout-secs 5] [--cleanup-interval-secs 30]
//!                [--prefetch PV1 PV2 ...]
//!
//! # 2. Config mode — pva2pva-compatible JSON describing named clients
//! #    and servers (mirrors `pva2pva` `[-vhiIC] <config file>`):
//! pva-gateway-rs [-C] <config.json>
//! ```
//!
//! `--bind` accepts both IPv4 (`0.0.0.0`, `127.0.0.1`, ...) and IPv6
//! (`::`, `::1`, ...) addresses (PR #205 IPv6 Stage 1). On Linux a
//! `[::]` bind dual-stacks automatically; BSD/macOS need a parallel
//! v4 instance for dual-stack.
//!
//! ## pva2pva JSON config (config mode)
//!
//! Mirrors the schema `pva2pva/p2pApp/gwmain.cpp:39-59` defines and the
//! `loopback.conf` example: a top-level `version` / `readOnly`, a
//! `clients` array (each carrying `provider` / `addrlist` /
//! `autoaddrlist` / `serverport` / `bcastport`), and a `servers` array
//! (each selecting named `clients`, with `interface` / `addrlist` /
//! `autoaddrlist` / `serverport` / `bcastport` / `control_prefix`).
//!
//! Validation mirrors `gwmain.cpp:99-130` (version must be 1; at least
//! one client and one server) plus `:291-322` (duplicate names rejected,
//! each server must resolve every referenced client). `-C` /
//! `--check-config` parses and validates without binding any socket,
//! mirroring `gwmain.cpp`'s `-C` preflight. Named clients/servers are
//! wired through [`MultiTenantPvaGatewayBuilder`], so one downstream can
//! be backed by a selected subset of named upstream providers
//! (`gwmain.cpp:133-188`), instead of collapsing to one client/server
//! pair.
//!
//! Two faithful-but-bounded mappings, because epics-pva-rs's PVA stack
//! is the only provider available here:
//! - a client's `provider` must be `"pva"`; a `"ca"` upstream would need
//!   a CA client (a different binary), so it is rejected with a clear
//!   error rather than silently ignored.
//! - a client's `addrlist` × `serverport` maps to the gateway client's
//!   TCP name-server list ([`PvaClientBuilder::name_servers`]) — the
//!   gateway-appropriate "pin these upstream endpoints" mechanism. The
//!   client-side UDP broadcast knobs (`bcastport` / `autoaddrlist`) have
//!   no per-client builder surface and are not applied to clients (they
//!   ARE applied to servers, whose beacon emission maps to
//!   `udp_port` / `auto_beacon`).

use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use serde::Deserialize;

use epics_bridge_rs::pva_gateway::{
    MultiTenantPvaGateway, MultiTenantPvaGatewayBuilder, PvaGateway, PvaGatewayConfig,
};
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::server_native::PvaServerConfig;

#[derive(Parser, Debug)]
#[command(
    name = "pva-gateway-rs",
    about = "Pure Rust PVA-to-PVA gateway (mirrors pva2pva)",
    version
)]
struct Args {
    /// pva2pva-compatible JSON config file. When given, the gateway runs
    /// in multi-client / multi-server mode from the file and the
    /// single-gateway flags below are ignored. Without it, the flags
    /// configure one upstream client and one downstream server.
    #[arg(value_name = "CONFIG")]
    config: Option<PathBuf>,

    /// Validate the config file and exit without binding any socket
    /// (mirrors pva2pva `-C`). Requires <CONFIG>.
    #[arg(short = 'C', long = "check-config")]
    check_config: bool,

    /// Bind IP for the downstream TCP listener (flag mode only).
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,

    /// Downstream TCP port (default 5075, flag mode only).
    #[arg(long, default_value_t = 5075)]
    tcp_port: u16,

    /// Downstream UDP search port (default 5076, flag mode only).
    #[arg(long, default_value_t = 5076)]
    udp_port: u16,

    /// Per-PV upstream connect timeout in seconds. Precedence: this
    /// flag overrides `EPICS_PVA_GW_CONNECT_TMO`, which overrides the
    /// 5 s default. Omit to take the env value (or the default).
    #[arg(long)]
    connect_timeout_secs: Option<u64>,

    /// Cache cleanup interval in seconds (idle entries dropped after
    /// one full tick with zero downstream subscribers). Precedence:
    /// this flag overrides `EPICS_PVA_GW_CLEANUP_INTERVAL`, which
    /// overrides the 30 s default. Omit to take the env value (or the
    /// default).
    #[arg(long)]
    cleanup_interval_secs: Option<u64>,

    /// Pre-warm the cache with these PV names (flag mode only). Useful
    /// when you know the workload ahead of time and want the first
    /// downstream search to hit the fast path.
    #[arg(long = "prefetch", num_args = 1.., value_delimiter = ',')]
    prefetch: Vec<String>,

    /// Bump tracing verbosity. Repeat for more (`-v` info, `-vv`
    /// debug, `-vvv` trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

// ── pva2pva JSON config schema (gwmain.cpp:39-59) ──────────────────────

/// Top-level config object. `version` defaults to 0 so a missing key is
/// distinguishable from an explicit `1` (gwmain.cpp:118-122 warns on a
/// missing version and assumes 1).
#[derive(Debug, Deserialize)]
struct GatewayConfigFile {
    #[serde(default)]
    version: u32,
    #[serde(rename = "readOnly", default)]
    read_only: bool,
    #[serde(default)]
    clients: Vec<ClientSpec>,
    #[serde(default)]
    servers: Vec<ServerSpec>,
}

#[derive(Debug, Deserialize)]
struct ClientSpec {
    name: String,
    #[serde(default = "default_provider")]
    provider: String,
    #[serde(default)]
    addrlist: String,
    #[serde(default)]
    autoaddrlist: bool,
    #[serde(default = "default_server_port")]
    serverport: u16,
    #[serde(default = "default_bcast_port")]
    bcastport: u16,
}

#[derive(Debug, Deserialize)]
struct ServerSpec {
    name: String,
    #[serde(default)]
    clients: Vec<String>,
    #[serde(default = "default_interface")]
    interface: String,
    #[serde(default)]
    addrlist: String,
    #[serde(default)]
    autoaddrlist: bool,
    #[serde(default = "default_server_port")]
    serverport: u16,
    #[serde(default = "default_bcast_port")]
    bcastport: u16,
    #[serde(default)]
    control_prefix: Option<String>,
    /// B6 (gateway-rs extension): ACF file gating the writable control
    /// RPCs (`<prefix>:flush` / `:drop` / `:reload`). Absent ⇒ the
    /// writable control surface stays closed; destructive controls are
    /// opt-in. Only meaningful with `control_prefix`.
    #[serde(default)]
    control_acf: Option<String>,
    /// B6 (gateway-rs extension): default ACF path the `:reload` RPC
    /// re-parses (for the PROXIED-PV policy) when the caller omits an
    /// explicit `path` argument.
    #[serde(default)]
    control_reload_acf: Option<String>,
}

fn default_provider() -> String {
    "pva".to_string()
}
fn default_server_port() -> u16 {
    5075
}
fn default_bcast_port() -> u16 {
    5076
}
fn default_interface() -> String {
    "0.0.0.0".to_string()
}

fn parse_config(text: &str) -> Result<GatewayConfigFile, String> {
    serde_json::from_str(text).map_err(|e| format!("config file is not valid JSON: {e}"))
}

/// Validate a parsed config without touching the network. Mirrors the
/// gwmain.cpp checks: version (`:118-127`), non-empty clients/servers
/// (`:128-135`), unique names + server→client resolution (`:291-322`),
/// plus the PVA-only-provider constraint specific to this binary.
fn validate(cfg: &GatewayConfigFile) -> Result<(), String> {
    if cfg.version != 0 && cfg.version != 1 {
        return Err(format!(
            "config file version mis-match: expect 1, found {}",
            cfg.version
        ));
    }
    if cfg.clients.is_empty() {
        return Err("No clients configured".to_string());
    }
    if cfg.servers.is_empty() {
        return Err("No servers configured".to_string());
    }
    // Unique client names — duplicates would silently shadow under
    // server→client resolution.
    for (i, a) in cfg.clients.iter().enumerate() {
        // C gwmain.cpp:298-299 rejects an empty client name before the
        // duplicate check, configure_client, or any provider use.
        // Regression R0604-BRPVAGW-CONFIG-EMPTY-NAMES-1.
        if a.name.is_empty() {
            return Err("Client with empty name not allowed".to_string());
        }
        if a.provider != "pva" {
            return Err(format!(
                "client '{}': provider '{}' is not supported by pva-gateway-rs \
                 (PVA-to-PVA only); use a CA gateway for CA upstreams",
                a.name, a.provider
            ));
        }
        for b in &cfg.clients[i + 1..] {
            if a.name == b.name {
                return Err(format!("duplicate client name '{}'", a.name));
            }
        }
    }
    for (i, a) in cfg.servers.iter().enumerate() {
        // C gwmain.cpp:314-315 rejects an empty server name before the
        // duplicate check. Regression R0604-BRPVAGW-CONFIG-EMPTY-NAMES-1.
        if a.name.is_empty() {
            return Err("Server with empty name not allowed".to_string());
        }
        for b in &cfg.servers[i + 1..] {
            if a.name == b.name {
                return Err(format!("duplicate server name '{}'", a.name));
            }
        }
        if a.clients.is_empty() {
            return Err(format!(
                "server '{}' must reference at least one client",
                a.name
            ));
        }
        for needed in &a.clients {
            if !cfg.clients.iter().any(|c| &c.name == needed) {
                return Err(format!(
                    "server '{}' references non-existent client '{needed}'",
                    a.name
                ));
            }
        }
    }
    Ok(())
}

/// Parse an EPICS-style whitespace-separated address list into socket
/// addresses, combining bare IPs with `default_port`. Empty / blank
/// input yields an empty list (the caller then keeps environment
/// defaults rather than pinning an empty set).
fn parse_addr_list(list: &str, default_port: u16) -> Result<Vec<SocketAddr>, String> {
    let mut out = Vec::new();
    for tok in list.split_whitespace() {
        if let Ok(sa) = tok.parse::<SocketAddr>() {
            out.push(sa);
        } else if let Ok(ip) = tok.parse::<IpAddr>() {
            out.push(SocketAddr::new(ip, default_port));
        } else {
            return Err(format!("invalid address-list entry '{tok}'"));
        }
    }
    Ok(out)
}

/// Build (but do not run) the multi-tenant gateway described by `cfg`.
/// Assumes `cfg` already passed [`validate`].
///
/// `cleanup` / `connect` / `max_cache_entries` / `max_subscribers` are the
/// gateway-rs knobs absent from the pva2pva JSON schema; they arrive already
/// resolved against `EPICS_PVA_GW_*` env. The schema's `readOnly` /
/// `control_prefix` stay file-authoritative and are read from `cfg`.
fn build_gateway(
    cfg: &GatewayConfigFile,
    cleanup: Duration,
    connect: Duration,
    max_cache_entries: usize,
    max_subscribers: usize,
) -> Result<MultiTenantPvaGateway, String> {
    let mut builder = MultiTenantPvaGatewayBuilder::new()
        .cleanup_interval(cleanup)
        .connect_timeout(connect)
        .max_cache_entries(max_cache_entries)
        .max_subscribers(max_subscribers);

    for c in &cfg.clients {
        // addrlist × serverport → TCP name servers (the gateway pins its
        // upstream endpoints; gwmain.cpp:133-150 configure_client).
        let name_servers = parse_addr_list(&c.addrlist, c.serverport)
            .map_err(|e| format!("client '{}' addrlist: {e}", c.name))?;
        let mut cb = PvaClient::builder();
        if !name_servers.is_empty() {
            cb = cb.name_servers(name_servers);
        }
        // The client-side UDP broadcast-search knobs (`autoaddrlist` /
        // `bcastport`) have no per-client builder surface — this gateway
        // pins upstreams via TCP name servers instead. Surface that
        // explicitly so an operator who set them is not silently ignored.
        if c.autoaddrlist {
            eprintln!(
                "pva-gateway-rs: client '{}': autoaddrlist=true and bcastport={} are not applied \
                 (no per-client UDP broadcast-search surface); pin upstreams via addrlist × serverport",
                c.name, c.bcastport
            );
        }
        builder = builder.add_upstream(c.name.clone(), Arc::new(cb.build()));
    }

    for s in &cfg.servers {
        let bind_ip: IpAddr = s
            .interface
            .trim()
            .parse()
            .map_err(|_| format!("server '{}': invalid interface '{}'", s.name, s.interface))?;
        // server addrlist × bcastport → beacon destinations
        // (EPICS_PVAS_BEACON_ADDR_LIST; gwmain.cpp:160-166 configure_server).
        // Use the endpoint-preserving parser so a multicast beacon target's
        // `,ttl@iface` modifiers (pvxs SockEndpoint grammar) reach the UDP
        // send path. Malformed tokens are dropped with a debug log — pvxs
        // parity: a bad beacon address never aborts server startup.
        let beacon = epics_pva_rs::config::parse_endpoints_with_port(&s.addrlist, s.bcastport);
        let server_config = PvaServerConfig {
            tcp_port: s.serverport,
            udp_port: s.bcastport,
            bind_ip,
            interfaces: if bind_ip.is_unspecified() {
                Vec::new()
            } else {
                vec![bind_ip]
            },
            beacon_destinations: beacon,
            auto_beacon: s.autoaddrlist,
            ..PvaServerConfig::default()
        };
        let upstream_refs: Vec<&str> = s.clients.iter().map(String::as_str).collect();
        let control_prefix = s.control_prefix.as_ref().filter(|p| !p.is_empty()).cloned();
        let control_acf = s.control_acf.as_ref().filter(|p| !p.is_empty()).cloned();
        let control_reload_acf = s
            .control_reload_acf
            .as_ref()
            .filter(|p| !p.is_empty())
            .cloned();
        builder = builder
            .add_downstream(
                s.name.clone(),
                server_config,
                &upstream_refs,
                control_prefix,
            )
            // Top-level readOnly applies to every downstream (gwmain.cpp
            // sets the global `p2pReadOnly` at :117). No ACL / audit from
            // the pva2pva schema.
            .downstream_access(None, cfg.read_only, None)
            // B6: gate the writable control RPCs through the configured
            // control ACF (closed when absent); wire the `:reload`
            // default path. Parse errors surface at `builder.start()`.
            .downstream_control_acf(control_acf, control_reload_acf);
    }

    builder.start().map_err(|e| e.to_string())
}

async fn run_from_config(
    path: &Path,
    check_only: bool,
    cleanup: Duration,
    connect: Duration,
    max_cache_entries: usize,
    max_subscribers: usize,
) -> ExitCode {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "pva-gateway-rs: cannot read config '{}': {e}",
                path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let cfg = match parse_config(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pva-gateway-rs: {e}");
            return ExitCode::FAILURE;
        }
    };
    if cfg.version == 0 {
        eprintln!("Warning: config file missing \"version\" key. Assuming 1");
    }
    if let Err(e) = validate(&cfg) {
        eprintln!("pva-gateway-rs: {e}");
        return ExitCode::FAILURE;
    }
    if check_only {
        eprintln!("Config file OK");
        return ExitCode::SUCCESS;
    }

    let gw = match build_gateway(&cfg, cleanup, connect, max_cache_entries, max_subscribers) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("pva-gateway-rs: failed to start: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "pva-gateway-rs: {} upstream client(s), {} downstream server(s) (Ctrl-C to stop)",
        gw.upstream_count(),
        gw.downstream_count()
    );
    if let Err(e) = tokio::signal::ctrl_c().await {
        eprintln!("pva-gateway-rs: signal wait failed: {e}");
    }
    gw.stop_all();
    ExitCode::SUCCESS
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

/// Single owner of `EPICS_PVA_GW_*` resolution, shared by both run modes.
///
/// Precedence is **type default < `EPICS_PVA_GW_*` env < explicit CLI
/// flag**. `base` is [`PvaGatewayConfig::default`] with [`with_env`]
/// applied, so it already carries env-resolved `max_cache_entries` /
/// `max_subscribers` / `control_prefix` / `read_only`; `cleanup` /
/// `connect` then layer the explicit CLI flag (the `Option` timeout
/// args) over the env value, so an explicit flag wins by construction
/// rather than by a runtime "was it the default?" check.
///
/// [`with_env`]: PvaGatewayConfig::with_env
struct ResolvedConfig {
    base: PvaGatewayConfig,
    cleanup: Duration,
    connect: Duration,
}

impl ResolvedConfig {
    fn from_args(args: &Args) -> Self {
        let base = PvaGatewayConfig::default().with_env();
        let cleanup = args
            .cleanup_interval_secs
            .map(Duration::from_secs)
            .unwrap_or(base.cleanup_interval);
        let connect = args
            .connect_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(base.connect_timeout);
        Self {
            base,
            cleanup,
            connect,
        }
    }

    /// Flag-mode single-gateway config: the env-resolved base with the
    /// CLI-derived downstream `server_config` and the env/CLI timeouts.
    /// `server_config` keeps `..PvaServerConfig::default()` (not the
    /// base's server config) so the existing flag-mode server defaults
    /// are unchanged by this resolution.
    fn into_flag_config(self, args: &Args) -> PvaGatewayConfig {
        // epics-base PR #205 IPv6 Stage 1: `PvaServerConfig::bind_ip` is
        // `IpAddr` so v4 and v6 bind addresses pass through unchanged.
        let server_config = PvaServerConfig {
            tcp_port: args.tcp_port,
            udp_port: args.udp_port,
            bind_ip: args.bind,
            ..PvaServerConfig::default()
        };
        PvaGatewayConfig {
            server_config,
            cleanup_interval: self.cleanup,
            connect_timeout: self.connect,
            ..self.base
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    init_tracing(args.verbose);

    // Single owner of EPICS_PVA_GW_* resolution (type default < env <
    // explicit CLI flag), shared by both run modes.
    let resolved = ResolvedConfig::from_args(&args);

    // Config mode (pva2pva-compatible JSON) takes precedence over the
    // single-gateway flags when a <CONFIG> path is supplied. The JSON
    // `readOnly` / `control_prefix` stay file-authoritative (pva2pva
    // gwmain.cpp has no env layer); only the gateway-rs knobs with no
    // pva2pva schema field are taken from the env layer here.
    if let Some(path) = args.config.as_ref() {
        return run_from_config(
            path,
            args.check_config,
            resolved.cleanup,
            resolved.connect,
            resolved.base.max_cache_entries,
            resolved.base.max_subscribers,
        )
        .await;
    }
    if args.check_config {
        eprintln!("pva-gateway-rs: --check-config requires a <CONFIG> file");
        return ExitCode::FAILURE;
    }

    // Flag mode: one upstream client, one downstream server.
    let cfg = resolved.into_flag_config(&args);

    let gateway = match PvaGateway::start(cfg) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("pva-gateway-rs: failed to start: {e}");
            return ExitCode::FAILURE;
        }
    };

    if !args.prefetch.is_empty() {
        let names: Vec<&str> = args.prefetch.iter().map(String::as_str).collect();
        tracing::info!(count = names.len(), "pre-warming gateway cache");
        gateway.prefetch(&names).await;
    }

    let report = gateway.report();
    eprintln!(
        "pva-gateway-rs listening tcp/{} udp/{} (Ctrl-C to stop)",
        report.tcp_port, report.udp_port
    );

    match gateway.run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pva-gateway-rs: stopped with error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The loopback.conf shipped with pva2pva — the canonical example.
    const LOOPBACK: &str = r#"{
        "version":1,
        "readOnly":false,
        "clients":[
            {"name":"theclient","provider":"pva","addrlist":"127.0.0.1",
             "autoaddrlist":false,"serverport":5085,"bcastport":5086}
        ],
        "servers":[
            {"name":"theserver","clients":["theclient"],"interface":"127.0.0.1",
             "addrlist":"127.255.255.255","autoaddrlist":false,
             "serverport":5075,"bcastport":5076}
        ]
    }"#;

    #[test]
    fn loopback_config_parses_and_validates() {
        let cfg = parse_config(LOOPBACK).expect("parse");
        assert_eq!(cfg.version, 1);
        assert!(!cfg.read_only);
        assert_eq!(cfg.clients.len(), 1);
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.clients[0].serverport, 5085);
        assert_eq!(cfg.servers[0].clients, vec!["theclient".to_string()]);
        validate(&cfg).expect("loopback.conf must validate");
    }

    #[test]
    fn missing_clients_is_rejected() {
        let cfg = parse_config(r#"{"version":1,"servers":[]}"#).expect("parse");
        assert_eq!(validate(&cfg).unwrap_err(), "No clients configured");
    }

    #[test]
    fn missing_servers_is_rejected() {
        let cfg =
            parse_config(r#"{"version":1,"clients":[{"name":"c","provider":"pva"}],"servers":[]}"#)
                .expect("parse");
        assert_eq!(validate(&cfg).unwrap_err(), "No servers configured");
    }

    #[test]
    fn server_referencing_unknown_client_is_rejected() {
        let cfg = parse_config(
            r#"{"version":1,
                "clients":[{"name":"c1","provider":"pva"}],
                "servers":[{"name":"s1","clients":["nope"]}]}"#,
        )
        .expect("parse");
        assert_eq!(
            validate(&cfg).unwrap_err(),
            "server 's1' references non-existent client 'nope'"
        );
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let cfg = parse_config(
            r#"{"version":2,"clients":[{"name":"c","provider":"pva"}],
                "servers":[{"name":"s","clients":["c"]}]}"#,
        )
        .expect("parse");
        assert_eq!(
            validate(&cfg).unwrap_err(),
            "config file version mis-match: expect 1, found 2"
        );
    }

    #[test]
    fn non_pva_provider_is_rejected() {
        let cfg = parse_config(
            r#"{"version":1,"clients":[{"name":"c","provider":"ca"}],
                "servers":[{"name":"s","clients":["c"]}]}"#,
        )
        .expect("parse");
        assert!(
            validate(&cfg)
                .unwrap_err()
                .contains("provider 'ca' is not supported")
        );
    }

    #[test]
    fn duplicate_client_names_rejected() {
        let cfg = parse_config(
            r#"{"version":1,
                "clients":[{"name":"c","provider":"pva"},{"name":"c","provider":"pva"}],
                "servers":[{"name":"s","clients":["c"]}]}"#,
        )
        .expect("parse");
        assert_eq!(validate(&cfg).unwrap_err(), "duplicate client name 'c'");
    }

    #[test]
    fn empty_client_name_rejected() {
        // Regression R0604-BRPVAGW-CONFIG-EMPTY-NAMES-1: C gwmain.cpp:298-299
        // rejects an empty client name before the duplicate check.
        let cfg = parse_config(
            r#"{"version":1,"clients":[{"name":"","provider":"pva"}],
                "servers":[{"name":"s","clients":[""]}]}"#,
        )
        .expect("parse");
        assert_eq!(
            validate(&cfg).unwrap_err(),
            "Client with empty name not allowed"
        );
    }

    #[test]
    fn empty_server_name_rejected() {
        // Regression R0604-BRPVAGW-CONFIG-EMPTY-NAMES-1: C gwmain.cpp:314-315
        // rejects an empty server name before the duplicate check.
        let cfg = parse_config(
            r#"{"version":1,"clients":[{"name":"c","provider":"pva"}],
                "servers":[{"name":"","clients":["c"]}]}"#,
        )
        .expect("parse");
        assert_eq!(
            validate(&cfg).unwrap_err(),
            "Server with empty name not allowed"
        );
    }

    #[test]
    fn server_with_no_clients_rejected() {
        let cfg = parse_config(
            r#"{"version":1,"clients":[{"name":"c","provider":"pva"}],
                "servers":[{"name":"s","clients":[]}]}"#,
        )
        .expect("parse");
        assert_eq!(
            validate(&cfg).unwrap_err(),
            "server 's' must reference at least one client"
        );
    }

    #[test]
    fn missing_version_warns_but_validates() {
        // version key absent → defaults to 0 → caller warns + assumes 1.
        let cfg = parse_config(
            r#"{"clients":[{"name":"c","provider":"pva"}],
                "servers":[{"name":"s","clients":["c"]}]}"#,
        )
        .expect("parse");
        assert_eq!(cfg.version, 0);
        validate(&cfg).expect("version 0 (missing) is accepted as 1");
    }

    #[test]
    fn flag_config_applies_env_with_cli_precedence() {
        // The defect this guards: flag mode used to build the config with
        // `..PvaGatewayConfig::default()`, so the documented EPICS_PVA_GW_*
        // knobs (incl. EPICS_PVA_GW_READONLY -> ReadOnlyLayer) had no effect.
        //
        // SAFETY: nextest runs each test in its own process; within this bin
        // test binary no sibling test reads/writes EPICS_PVA_GW_*, so these
        // mutations cannot race another test. We restore them before return.
        unsafe {
            std::env::set_var("EPICS_PVA_GW_READONLY", "YES");
            std::env::set_var("EPICS_PVA_GW_MAX_SUBSCRIBERS", "7");
            std::env::set_var("EPICS_PVA_GW_MAX_CACHE_ENTRIES", "13");
            std::env::set_var("EPICS_PVA_GW_CLEANUP_INTERVAL", "11");
        }

        // No --cleanup-interval-secs flag: env value applies.
        // Explicit --connect-timeout-secs: CLI overrides env/default.
        let args = Args::parse_from(["pva-gateway-rs", "--connect-timeout-secs", "9"]);
        let resolved = ResolvedConfig::from_args(&args);
        let cfg = resolved.into_flag_config(&args);

        assert!(
            cfg.read_only,
            "EPICS_PVA_GW_READONLY=YES must enable the ReadOnlyLayer"
        );
        assert_eq!(
            cfg.max_subscribers, 7,
            "EPICS_PVA_GW_MAX_SUBSCRIBERS applied"
        );
        assert_eq!(
            cfg.max_cache_entries, 13,
            "EPICS_PVA_GW_MAX_CACHE_ENTRIES applied"
        );
        assert_eq!(
            cfg.cleanup_interval,
            Duration::from_secs(11),
            "env cleanup interval applies when the flag is omitted"
        );
        assert_eq!(
            cfg.connect_timeout,
            Duration::from_secs(9),
            "explicit --connect-timeout-secs overrides EPICS_PVA_GW_CONNECT_TMO"
        );

        unsafe {
            std::env::remove_var("EPICS_PVA_GW_READONLY");
            std::env::remove_var("EPICS_PVA_GW_MAX_SUBSCRIBERS");
            std::env::remove_var("EPICS_PVA_GW_MAX_CACHE_ENTRIES");
            std::env::remove_var("EPICS_PVA_GW_CLEANUP_INTERVAL");
        }
    }

    #[test]
    fn addr_list_parsing_combines_bare_ip_with_default_port() {
        let got = parse_addr_list("127.0.0.1 10.0.0.2:9999", 5085).expect("parse");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], "127.0.0.1:5085".parse().unwrap());
        assert_eq!(got[1], "10.0.0.2:9999".parse().unwrap());
        assert!(parse_addr_list("", 5085).unwrap().is_empty());
        assert!(parse_addr_list("not-an-ip", 5085).is_err());
    }
}
