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

    /// CA TCP port. Default: 5064.
    #[arg(long, default_value_t = 5064)]
    ca_port: u16,

    /// PVA TCP port. Default: 5075.
    #[arg(long, default_value_t = 5075)]
    pva_port: u16,

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

#[tokio::main]
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

    let ca_handle = if args.no_ca {
        None
    } else {
        let server = CaServer::from_parts(db.clone(), args.ca_port, None, None, None, None);
        Some(tokio::spawn(async move {
            tracing::info!(port = args.ca_port, "CA listener starting");
            server.run().await
        }))
    };

    let pva_handle = if args.no_pva {
        None
    } else {
        let server = PvaServer::from_parts(db.clone(), args.pva_port, None, None, None);
        Some(tokio::spawn(async move {
            tracing::info!(port = args.pva_port, "PVA listener starting");
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
