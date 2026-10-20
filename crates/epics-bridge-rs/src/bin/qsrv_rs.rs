//! qsrv-rs — Record ↔ pvAccess bridge daemon (Rust port of C++ QSRV).
//!
//! Loads EPICS records from a `.db` file and optional group PV definitions
//! from a JSON config, then exposes them over pvAccess using the native
//! PVA server via [`QsrvPvStore`](epics_bridge_rs::qsrv::pva_adapter::QsrvPvStore).
//!
//! Usage:
//!
//! ```text
//! qsrv-rs --db-file records.db [--group-file groups.json] [--port 5075]
//!         [--macro KEY=VAL]...
//! ```
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_app::{GroupLoadRequest, IocApplication};
use epics_base_rs::server::iocsh::registry::escape_arg;
use epics_bridge_rs::qsrv::run_qsrv_ioc_app;
use epics_pva_rs::server::PvaServer;
use epics_pva_rs::server_native::CompositeSource;

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

    /// TCP port for pvAccess; the search (UDP) port is
    /// `EPICS_PVAS_BROADCAST_PORT`'s whatever this is. Omit to take
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

// One worker, reactor-style — the shape pvxs serves from (a single event
// loop). With the default per-CPU pool, the mostly-serial serving work
// migrates across idle workers, costing ~35 µs of extra CPU per put on a
// 96-core host. Multi-thread flavor is kept: current_thread would refuse
// `block_on_sync` from runtime tasks.
#[cfg(tokio_backend)]
#[tokio::main(worker_threads = 1)]
async fn main() -> ExitCode {
    // softIocPVX's console is errlog's stderr; an application subscriber
    // would keep errlog lines as its own records on stdout instead.
    epics_base_rs::runtime::log::install_console_subscriber();

    exit(run(Args::parse()).await)
}

/// The reactor-free arm: the same `run`, parked on the main thread.
#[cfg(exec_backend)]
fn main() -> ExitCode {
    epics_base_rs::runtime::log::install_console_subscriber();
    let result = epics_base_rs::runtime::task::block_on_sync(run(Args::parse()))
        .unwrap_or_else(|e| Err(e.to_string()));
    exit(result)
}

fn exit(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            epics_base_rs::stderr_println!("qsrv-rs: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let mut app = IocApplication::new();
    if let Some(path) = args.db_file.as_ref() {
        let macros: Vec<String> = args
            .macros
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        app = app.startup_line(&format!(
            "dbLoadRecords({}, {})",
            escape_arg(&path.to_string_lossy()),
            escape_arg(&macros.join(","))
        ));
    }
    let group_files: Vec<GroupLoadRequest> = args
        .group_file
        .iter()
        .map(|path| GroupLoadRequest {
            filename: path.to_string_lossy().into_owned(),
            macros: String::new(),
        })
        .collect();
    let port = args.port;
    run_qsrv_ioc_app(app, group_files, move |config, mount| async move {
        let composite = CompositeSource::new();
        mount
            .install_channel_sources(&config.db, &composite)
            .map_err(CaError::InvalidValue)?;
        PvaServer::from_parts(
            config.db,
            port,
            config.acf,
            config.autosave_config,
            config.autosave_manager,
        )
        .run_with_source(composite)
        .await
        .map_err(|e| CaError::InvalidValue(e.to_string()))
    })
    .await
    .map_err(|e| e.to_string())
}
