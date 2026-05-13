//! PVA-to-PVA gateway daemon binary.
//!
//! Usage:
//!
//! ```text
//! pva-gateway-rs [--bind 0.0.0.0] [--tcp-port 5075] [--udp-port 5076]
//!                [--connect-timeout-secs 5] [--cleanup-interval-secs 30]
//!                [--prefetch PV1 PV2 ...]
//! ```
//!
//! Mirrors `pva2pva/p2pApp/gwmain.cpp` at the runtime level: a single
//! process that holds an upstream `PvaClient` and a downstream
//! `PvaServer`, routing every search/get/put/monitor through a shared
//! cache.

use std::net::IpAddr;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

use epics_bridge_rs::pva_gateway::{PvaGateway, PvaGatewayConfig};
use epics_pva_rs::server_native::PvaServerConfig;

#[derive(Parser, Debug)]
#[command(
    name = "pva-gateway-rs",
    about = "Pure Rust PVA-to-PVA gateway (mirrors pva2pva)",
    version
)]
struct Args {
    /// Bind IP for the downstream TCP listener.
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,

    /// Downstream TCP port (default 5075).
    #[arg(long, default_value_t = 5075)]
    tcp_port: u16,

    /// Downstream UDP search port (default 5076).
    #[arg(long, default_value_t = 5076)]
    udp_port: u16,

    /// Per-PV upstream connect timeout in seconds.
    #[arg(long, default_value_t = 5)]
    connect_timeout_secs: u64,

    /// Cache cleanup interval in seconds (idle entries dropped after
    /// one full tick with zero downstream subscribers).
    #[arg(long, default_value_t = 30)]
    cleanup_interval_secs: u64,

    /// Pre-warm the cache with these PV names. Useful when you know
    /// the workload ahead of time and want the first downstream
    /// search to hit the fast path.
    #[arg(long = "prefetch", num_args = 1.., value_delimiter = ',')]
    prefetch: Vec<String>,

    /// Bump tracing verbosity. Repeat for more (`-v` info, `-vv`
    /// debug, `-vvv` trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
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

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    init_tracing(args.verbose);

    // epics-base PR #205 IPv6 Stage 1: `PvaServerConfig::bind_ip` is
    // `IpAddr` so v4 and v6 bind addresses pass through unchanged.
    let server_config = PvaServerConfig {
        tcp_port: args.tcp_port,
        udp_port: args.udp_port,
        bind_ip: args.bind,
        ..PvaServerConfig::default()
    };

    let cfg = PvaGatewayConfig {
        upstream_client: None,
        server_config,
        cleanup_interval: Duration::from_secs(args.cleanup_interval_secs),
        connect_timeout: Duration::from_secs(args.connect_timeout_secs),
        // Inherit from the type's defaults — operators tune these via
        // EPICS_PVA_GW_MAX_CACHE_ENTRIES / EPICS_PVA_GW_MAX_SUBSCRIBERS
        // in PvaGatewayConfig::with_env, or via PvaGatewayConfig::default().
        ..PvaGatewayConfig::default()
    };

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
