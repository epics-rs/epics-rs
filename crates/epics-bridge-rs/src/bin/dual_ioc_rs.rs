//! dual-ioc-rs — single process serving the same PV database over both
//! Channel Access and pvAccess.
//!
//! Targets sites in transition: tooling that still speaks CA
//! (`caget`, EDM, MEDM, CSS-via-CA) and tooling that's moved to PVA
//! (Phoebus, p4p) both reach the same records, no duplicate IOCs to
//! keep in sync. The PV database is `Arc<PvDatabase>` shared between
//! the two server tasks; writes through either channel see each
//! other immediately.
//!
//! Usage:
//! ```bash
//! dual-ioc-rs --pv MOTOR:VAL:double:0.0 \
//!             --ca-port 5064 --pva-port 5075
//! dual-ioc-rs --db records.db -m P=BL13:
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServer;
use epics_pva_rs::server::PvaServer;

#[derive(Parser, Debug)]
#[command(
    name = "dual-ioc-rs",
    about = "Single-process IOC serving the same PV DB over CA and PVA",
    version
)]
struct Args {
    /// PV definitions in `NAME:TYPE:VALUE` form. Repeatable.
    #[arg(long = "pv")]
    pvs: Vec<String>,

    /// `.db` file(s) to load. Repeatable.
    #[arg(long = "db")]
    db_files: Vec<PathBuf>,

    /// Macro substitutions for db files (`KEY=VAL`). Repeatable.
    #[arg(long = "macro", short = 'm')]
    macros: Vec<String>,

    /// CA TCP port. Omit to take `EPICS_CAS_SERVER_PORT`, then
    /// `EPICS_CA_SERVER_PORT`, then 5064. `--ca-port 0` binds ephemeral.
    #[arg(long)]
    ca_port: Option<u16>,

    /// PVA TCP port. Omit to take `EPICS_PVAS_SERVER_PORT`, then
    /// `EPICS_PVA_SERVER_PORT`, then 5075. `--pva-port 0` binds ephemeral.
    #[arg(long)]
    pva_port: Option<u16>,

    /// Disable CA serving (PVA only).
    #[arg(long)]
    no_ca: bool,

    /// Disable PVA serving (CA only).
    #[arg(long)]
    no_pva: bool,
}

fn parse_macros(raw: &[String]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for kv in raw {
        if let Some((k, v)) = kv.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().to_string());
        } else {
            eprintln!("warning: --macro expects KEY=VAL, got {kv:?}; skipping");
        }
    }
    out
}

fn parse_pv(def: &str) -> CaResult<(String, EpicsValue)> {
    // Format `NAME:TYPE:VALUE` — name can contain colons, type is one
    // recognized keyword. Reuse the same logic as softioc-rs but
    // simplified for the common types used in dual-IOC demos.
    let segments: Vec<&str> = def.split(':').collect();
    let known_types = ["string", "short", "float", "long", "double", "int", "char"];
    let type_idx = segments
        .iter()
        .rposition(|s| known_types.contains(&s.to_lowercase().as_str()))
        .ok_or_else(|| {
            epics_base_rs::error::CaError::InvalidValue(format!(
                "expected NAME:TYPE:VALUE, got {def:?}"
            ))
        })?;
    if type_idx == 0 || type_idx + 1 >= segments.len() {
        return Err(epics_base_rs::error::CaError::InvalidValue(format!(
            "bad PV def {def:?}"
        )));
    }
    let name = segments[..type_idx].join(":");
    let type_str = segments[type_idx].to_ascii_lowercase();
    let value_str = segments[type_idx + 1..].join(":");
    let dbf = match type_str.as_str() {
        "string" => epics_base_rs::types::DbFieldType::String,
        "short" | "int" => epics_base_rs::types::DbFieldType::Short,
        "float" => epics_base_rs::types::DbFieldType::Float,
        "long" => epics_base_rs::types::DbFieldType::Long,
        "double" => epics_base_rs::types::DbFieldType::Double,
        "char" => epics_base_rs::types::DbFieldType::Char,
        _ => unreachable!(),
    };
    let value = EpicsValue::parse(dbf, &value_str)?;
    Ok((name, value))
}

// One worker, reactor-style — see qsrv_rs.rs: the default per-CPU pool
// migrates the mostly-serial serving work across idle workers, costing
// ~35 µs of extra CPU per put on a 96-core host (doc/qsrv-put-perf.md).
// Multi-thread flavor is kept so `block_on_sync` works from runtime tasks.
#[tokio::main(worker_threads = 1)]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    if args.no_ca && args.no_pva {
        eprintln!("error: --no-ca and --no-pva are mutually exclusive");
        return ExitCode::from(2);
    }

    if args.pvs.is_empty() && args.db_files.is_empty() {
        eprintln!("error: at least one --pv or --db is required");
        return ExitCode::from(2);
    }

    // Build a shared PvDatabase via IocBuilder. Both server tasks
    // receive `Arc<PvDatabase>` clones — a caput on the CA side is
    // immediately visible through PVA monitors and vice versa.
    let macros = parse_macros(&args.macros);
    let mut builder = IocBuilder::new();

    for pv_def in &args.pvs {
        match parse_pv(pv_def) {
            Ok((name, value)) => {
                eprintln!("  PV: {name}");
                builder = builder.pv(&name, value);
            }
            Err(e) => {
                eprintln!("error parsing --pv {pv_def:?}: {e}");
                return ExitCode::from(2);
            }
        }
    }

    for db_path in &args.db_files {
        eprintln!("  Loading DB: {}", db_path.display());
        let path_str = db_path.to_string_lossy().to_string();
        builder = match builder.db_file(&path_str, &macros) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("error loading {}: {e}", db_path.display());
                return ExitCode::from(2);
            }
        };
    }

    let (db, autosave_cfg) = match builder.build().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error building IOC: {e}");
            return ExitCode::from(2);
        }
    };
    let _ = autosave_cfg;

    // C iocInit owns scan start (scanInit/scanRun) — neither the CA nor
    // the PVA front-end scans. One core-owned scan owner covers whichever
    // subset of the two servers is enabled; `try_claim_scan_start` keeps
    // any redundant start parked, so the shared database never
    // double-scans. Held to process end.
    let _scan_owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());

    // Flag > EPICS environment > compiled default. `from_parts` takes a
    // literal port, so the environment fallback is resolved here.
    let ca_port = args
        .ca_port
        .unwrap_or_else(epics_base_rs::runtime::net::cas_server_port);
    let pva_port = args
        .pva_port
        .unwrap_or_else(epics_pva_rs::config::env::pvas_server_port);

    let ca_handle = if args.no_ca {
        None
    } else {
        let server = match CaServer::from_parts(db.clone(), ca_port, None, None, None, None).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error binding CA server on port {ca_port}: {e}");
                return ExitCode::FAILURE;
            }
        };
        Some(tokio::spawn(async move {
            tracing::info!(port = ca_port, "CA listener starting");
            server.run().await
        }))
    };

    let pva_handle = if args.no_pva {
        None
    } else {
        let server = PvaServer::from_parts(db.clone(), pva_port, None, None, None);
        Some(tokio::spawn(async move {
            tracing::info!(port = pva_port, "PVA listener starting");
            server.run().await
        }))
    };

    let result = match (ca_handle, pva_handle) {
        (Some(ca), Some(pva)) => tokio::select! {
            r = ca => format_join("CA", r),
            r = pva => format_join("PVA", r),
        },
        (Some(ca), None) => format_join("CA", ca.await),
        (None, Some(pva)) => format_join("PVA", pva.await),
        (None, None) => Ok(()),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dual-ioc-rs: {e}");
            ExitCode::FAILURE
        }
    }
}

fn format_join(which: &str, r: Result<CaResult<()>, tokio::task::JoinError>) -> Result<(), String> {
    match r {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("{which} server exited: {e}")),
        Err(e) => Err(format!("{which} task panicked: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;

    /// R19-22 boundary: an omitted port flag must reach the resolver as
    /// `None`, so the EPICS environment decides the bind port. A clap
    /// literal (`default_value_t = 5064` / `5075`) shadows
    /// `EPICS_CAS_SERVER_PORT` / `EPICS_PVAS_SERVER_PORT` and pins the
    /// IOC to the production ports.
    #[test]
    fn omitted_ports_defer_to_the_epics_environment() {
        let args = Args::parse_from(["dual-ioc-rs", "--db", "x.db"]);
        assert_eq!(args.ca_port, None);
        assert_eq!(args.pva_port, None);
    }

    /// The flags still win, and `0` stays a representable request for an
    /// ephemeral bind — distinct from "unset".
    #[test]
    fn explicit_ports_override_and_zero_means_ephemeral() {
        let args = Args::parse_from([
            "dual-ioc-rs",
            "--db",
            "x.db",
            "--ca-port",
            "15064",
            "--pva-port",
            "0",
        ]);
        assert_eq!(args.ca_port, Some(15064));
        assert_eq!(args.pva_port, Some(0));
    }
}
