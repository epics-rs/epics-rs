//! qsrv-rs — Record ↔ pvAccess bridge daemon (Rust port of C++ QSRV).
//!
//! Loads EPICS records from a `.db` file and optional group PV definitions
//! from a JSON config, then exposes them over pvAccess using the native
//! PVA server via [`QsrvPvStore`].
//!
//! Usage:
//!
//! ```text
//! qsrv-rs --db-file records.db [--group-file groups.json] [--port 5075]
//!         [--macro KEY=VAL]...
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

use epics_bridge_rs::qsrv::{BridgeProvider, QsrvPvStore};
use epics_pva_rs::server::{PvaServer, PvaServerBuilder};
use epics_pva_rs::server_native::ChannelSource;

#[derive(Parser, Debug)]
#[command(
    name = "qsrv-rs",
    about = "Rust port of EPICS QSRV: serves records as pvAccess channels",
    version
)]
struct Args {
    /// Path to a `.db` file to load.
    #[arg(long)]
    db_file: Option<PathBuf>,

    /// Path to a group PV JSON config.
    #[arg(long)]
    group_file: Option<PathBuf>,

    /// Macro assignments applied to the `.db` file (repeatable, `KEY=VAL`).
    #[arg(long = "macro", value_parser = parse_macro)]
    macros: Vec<(String, String)>,

    /// TCP port for pvAccess (UDP is port + 1). Omit to take
    /// `EPICS_PVAS_SERVER_PORT`, then `EPICS_PVA_SERVER_PORT`, then 5075.
    /// `--port 0` binds an ephemeral port.
    #[arg(long)]
    port: Option<u16>,
}

fn parse_macro(raw: &str) -> Result<(String, String), String> {
    let (k, v) = raw
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VAL, got {raw:?}"))?;
    Ok((k.to_string(), v.to_string()))
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("qsrv-rs: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let mut builder: PvaServerBuilder = PvaServer::builder();
    if let Some(port) = args.port {
        builder = builder.port(port);
    }
    if let Some(path) = args.db_file.as_ref() {
        let macros: HashMap<String, String> = args.macros.iter().cloned().collect();
        builder = builder
            .db_file(path.to_string_lossy().as_ref(), &macros)
            .map_err(|e| format!("loading db file {}: {e}", path.display()))?;
    }
    let server = builder.build().await.map_err(|e| e.to_string())?;
    let db = server.database().clone();

    // C iocInit owns scan start (scanInit/initialProcess) — the PVA
    // front-end does not scan. Start the core-owned scan owner here,
    // where this binary's iocInit sequence ends; held to process end so
    // periodic SCAN fields stay live as long as the server serves.
    let _scan_owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());

    let provider = BridgeProvider::new(db);
    if let Some(path) = args.group_file.as_ref() {
        provider
            .load_group_file(path.to_string_lossy().as_ref())
            .map_err(|e| format!("loading group file {}: {e}", path.display()))?;
    }
    let store = Arc::new(QsrvPvStore::new(Arc::new(provider)));

    let pv_count = store.list_pvs().await.len();
    let group_count = store.provider().groups().len();
    tracing::info!(
        "qsrv-rs: serving {pv_count} PV(s) ({group_count} group) — starting PVA listener"
    );

    server
        .run_with_source(store)
        .await
        .map_err(|e| e.to_string())
}
