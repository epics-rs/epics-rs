//! qsrv-ioc — Dual-protocol IOC (Channel Access + pvAccess) with group PV.
//!
//! Loads records from a `.db` file and group PV definitions from JSON,
//! then serves everything over **both** CA and PVA simultaneously using
//! the same PvDatabase.
//!
//! # Usage
//!
//! ```text
//! cargo run --release -p qsrv-ioc --features ioc -- ioc/st.cmd
//! ```
//!
//! # Test
//!
//! ```text
//! # Channel Access (port 5064)
//! caget  DEMO:AI
//! caput  DEMO:AO 42.5
//! camonitor DEMO:AI
//!
//! # pvAccess (port 5075)
//! pvget  DEMO:AI              # NTScalar
//! pvget  DEMO:GROUP           # Group PV (PVA only)
//! pvmonitor DEMO:GROUP        # Live group updates
//! pvput  DEMO:AO 42.5
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::qsrv::{BridgeProvider, QsrvPvStore};
use epics_ca_rs::server::CaServer;
use epics_pva_rs::server::PvaServer;
use epics_pva_rs::server_native::ChannelSource;

#[epics_base_rs::epics_main]
async fn main() -> CaResult<()> {
    let args: Vec<String> = std::env::args().collect();

    // C `envGetInetPortConfigParam` semantics (5000 floor, sscanf leniency,
    // diagnostics) — a strict `parse()` here diverged from a C IOC.
    let ca_port: u16 = epics_base_rs::runtime::net::cas_server_port();

    // pvxs `PickOne` semantics (config.cpp:402-408): the server-specific
    // `EPICS_PVAS_SERVER_PORT` wins over the shared `EPICS_PVA_SERVER_PORT`,
    // and `0` means "bind an ephemeral port". Not the CA rule above — pvxs has
    // no 5000 floor and no out-of-range diagnostic.
    let pva_port: u16 = epics_pva_rs::config::env::pvas_server_port();

    epics_base_rs::runtime::env::set_default("QSRV_IOC", env!("CARGO_MANIFEST_DIR"));

    // Parse st.cmd or fall back to defaults
    let (db, group_file) = if args.len() > 1 && !args[1].starts_with('-') {
        let script = std::fs::read_to_string(&args[1])
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", args[1]));
        parse_and_build(&script).await?
    } else {
        let macros: HashMap<String, String> = HashMap::new();
        let db_path = format!("{}/db/qsrv_demo.db", env!("CARGO_MANIFEST_DIR"));
        let ioc = IocBuilder::new().db_file(&db_path, &macros)?;
        let (db, _) = ioc.build().await?;
        let group_path = format!("{}/db/group.json", env!("CARGO_MANIFEST_DIR"));
        (db, Some(group_path))
    };

    // ── QSRV provider (group PVs, PVA-only) ──
    let provider = BridgeProvider::new(db.clone());
    if let Some(ref path) = group_file {
        provider
            .load_group_file(path)
            .unwrap_or_else(|e| panic!("failed to load group config {path}: {e}"));
    }
    let provider = Arc::new(provider);
    let store = Arc::new(QsrvPvStore::new(provider));

    let pv_list = store.list_pvs().await;
    eprintln!("qsrv-ioc: dual-protocol IOC");
    eprintln!("  CA  port: {ca_port}");
    eprintln!("  PVA port: {pva_port}");
    eprintln!("  PVs ({}):", pv_list.len());
    for pv in &pv_list {
        eprintln!("    {pv}");
    }

    // ── Simulator ──
    spawn_simulator(db.clone());

    // ── Scan start (C iocInit owns it, the servers do not scan) ──
    // One core-owned scan owner covers both front-ends; held to the end
    // of main so periodic SCAN fields stay live while the IOC serves.
    let _scan_owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());

    // ── Build both servers from the same database ──
    let ca_server = CaServer::from_parts(db.clone(), ca_port, None, None, None, None).await?;
    let pva_server = PvaServer::from_parts(db, pva_port, None, None, None);

    // CA runs in background, PVA runs with iocsh (shell exit stops everything)
    let ca_handle = epics_base_rs::runtime::task::spawn(async move { ca_server.run().await });

    let result = pva_server
        .run_with_source_and_shell(store, |_shell| {
            // iocsh commands (dbl, dbgf, dbpf, etc.) are built-in
        })
        .await;

    ca_handle.abort();
    result
}

/// Parse a simplified st.cmd — returns shared PvDatabase + optional group file.
async fn parse_and_build(script: &str) -> CaResult<(Arc<PvDatabase>, Option<String>)> {
    let mut macros: HashMap<String, String> = HashMap::new();
    // Seed QSRV_IOC so $(QSRV_IOC) expands in st.cmd
    macros.insert("QSRV_IOC".into(), env!("CARGO_MANIFEST_DIR").into());
    let mut ioc = IocBuilder::new();
    let mut group_file: Option<String> = None;

    for raw_line in script.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with("epicsEnvSet") {
            if let Some((key, val)) = parse_env_set(line) {
                let expanded = expand_macros(&val, &macros);
                macros.insert(key, expanded);
            }
            continue;
        }

        if line.starts_with("dbLoadRecords") {
            if let Some((path, mac_str)) = parse_db_load(line) {
                let path = expand_macros(&path, &macros);
                let mut db_macros = macros.clone();
                for pair in mac_str.split(',') {
                    if let Some((k, v)) = pair.split_once('=') {
                        db_macros.insert(k.trim().to_string(), expand_macros(v.trim(), &macros));
                    }
                }
                ioc = ioc.db_file(&path, &db_macros)?;
            }
            continue;
        }

        if line.starts_with("qsrvGroupLoadConfig") {
            if let Some(path) = parse_single_arg(line) {
                group_file = Some(expand_macros(&path, &macros));
            }
            continue;
        }
    }

    let (db, _) = ioc.build().await?;
    Ok((db, group_file))
}

fn spawn_simulator(db: Arc<PvDatabase>) {
    epics_base_rs::runtime::task::spawn(async move {
        let mut tick = 0.0_f64;
        let mut interval = epics_base_rs::runtime::task::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            tick += 0.1;
            let temp = 22.0 + (tick * 0.5).sin();
            let _ = db
                .put_record_field_from_ca("DEMO:AI", "VAL", EpicsValue::Double(temp))
                .await;
            // Toggle every 5 seconds (tick += 0.1 per 500ms → 0.2/sec)
            let bi_val = if (tick as u64).is_multiple_of(2) {
                0
            } else {
                1
            };
            let _ = db
                .put_record_field_from_ca("DEMO:BI", "VAL", EpicsValue::Enum(bi_val))
                .await;
        }
    });
}

// ── Minimal st.cmd parser helpers ──────────────────────────────────────

fn parse_env_set(line: &str) -> Option<(String, String)> {
    let inner = line.strip_prefix("epicsEnvSet(")?.strip_suffix(')')?;
    let (k, v) = inner.split_once(',')?;
    Some((unquote(k.trim()), unquote(v.trim())))
}

fn parse_db_load(line: &str) -> Option<(String, String)> {
    let inner = line.strip_prefix("dbLoadRecords(")?.strip_suffix(')')?;
    let (path, mac) = inner.split_once(',')?;
    Some((unquote(path.trim()), unquote(mac.trim())))
}

fn parse_single_arg(line: &str) -> Option<String> {
    let start = line.find('(')?;
    let inner = line[start + 1..].strip_suffix(')')?;
    Some(unquote(inner.trim()))
}

fn unquote(s: &str) -> String {
    s.trim_matches('"').to_string()
}

fn expand_macros(s: &str, macros: &HashMap<String, String>) -> String {
    let mut result = s.to_string();
    for _ in 0..10 {
        let prev = result.clone();
        for (key, val) in macros {
            result = result.replace(&format!("$({key})"), val);
        }
        if result == prev {
            break;
        }
    }
    result
}
