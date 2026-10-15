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
// On `exec_backend` this program's `main` refuses instead of running, so
// everything below it is unreachable in that configuration by construction.
// The lint is reporting the intent, not dead code: the default build still
// lints this file in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_app::{GroupLoadRequest, IocApplication};
#[cfg(tokio_backend)]
use epics_bridge_rs::qsrv::run_qsrv_ioc_app;
#[cfg(tokio_backend)]
use epics_pva_rs::server::PvaServer;
#[cfg(tokio_backend)]
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

    let args = Args::parse();

    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("qsrv-rs: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(tokio_backend)]
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
            iocsh_escape(&path.to_string_lossy()),
            iocsh_escape(&macros.join(","))
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

/// An argument as iocsh text that splits back into exactly its bytes. Inside
/// a quote C's `split` takes a backslash as data and escapes nothing
/// (`iocsh.cpp:252-371`); outside one a backslash makes the next byte data
/// for both `macDefExpand` and `split`. The same rule as softioc-rs's.
#[cfg(tokio_backend)]
fn iocsh_escape(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len());
    for c in arg.chars() {
        if c.is_ascii_whitespace() || "\\\"'(),$<>".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The `exec_backend` arm. QSRV serves its groups over the async PVA front-end
/// (`server_native::runtime`), which is `tokio_backend`-only; `realtime-pva-ioc`
/// is the entry point for this execution model.
#[cfg(exec_backend)]
fn main() -> ExitCode {
    epics_base_rs::runtime::task::refuse_backend(
        "qsrv-rs: this build selects the reactor-free execution backend \
         (EPICS_RS_BUILD_EXEC_BACKEND=thread), and the PVA server front-end needs \
         a tokio reactor. Unset that variable and rebuild.",
    )
}
