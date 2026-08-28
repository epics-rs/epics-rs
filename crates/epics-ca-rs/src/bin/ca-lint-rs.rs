//! Static configuration linter for EPICS Channel Access deployments.
//!
//! Catches the kinds of mistakes that traditionally land at the
//! operator at 3 a.m.: a typo'd `EPICS_CAS_ATUO_BEACON_ADDR_LIST`, a
//! `.db` with two records named `MOTOR:VAL`, an `.acf` referencing a
//! HAG that was renamed in the previous PR. None of these are syntax
//! errors libca catches; they happen quietly and surface as
//! mysterious connection failures hours later.
//!
//! Usage:
//! ```bash
//! ca-lint-rs --env                                # check EPICS_CA*
//! ca-lint-rs --db path/motor.db --db path/bpm.db  # check DB files
//! ca-lint-rs --acf path/site.acf                  # check ACF
//! ca-lint-rs --env --db x.db --acf y.acf          # everything
//! ```
//!
//! Exit code: 0 on no issues, 1 on warnings, 2 on errors.

use clap::Parser;
use std::collections::{HashMap, HashSet};

#[derive(Parser)]
#[command(name = "ca-lint-rs")]
struct Args {
    /// Check `EPICS_CA*` / `EPICS_CAS*` environment variables.
    #[arg(long)]
    env: bool,

    /// One or more .db files to lint.
    #[arg(long = "db", value_name = "FILE")]
    db: Vec<String>,

    /// One or more .acf files to lint.
    #[arg(long = "acf", value_name = "FILE")]
    acf: Vec<String>,

    /// Macro substitution `KEY=VALUE` for DB parsing. Repeatable.
    #[arg(long = "macro", short = 'm', value_name = "KEY=VALUE")]
    macros: Vec<String>,
}

#[derive(Debug)]
enum Issue {
    Warn(String),
    Error(String),
}

impl Issue {
    fn print(&self) {
        match self {
            Issue::Warn(s) => println!("warning: {s}"),
            Issue::Error(s) => println!("error: {s}"),
        }
    }
}

/// Whitelist of recognized EPICS_CA* / EPICS_CAS* env vars. Anything
/// outside this set in the user's environment is flagged as a likely typo.
const KNOWN_ENV: &[&str] = &[
    "EPICS_CA_ADDR_LIST",
    "EPICS_CA_AUTO_ADDR_LIST",
    "EPICS_CA_SERVER_PORT",
    "EPICS_CA_REPEATER_PORT",
    "EPICS_CA_CONN_TMO",
    "EPICS_CA_MAX_ARRAY_BYTES",
    "EPICS_CA_MAX_SEARCH_PERIOD",
    "EPICS_CA_PUT_TIMEOUT",
    "EPICS_CA_NAME_SERVERS",
    "EPICS_CA_MONITOR_QUEUE",
    "EPICS_CA_USE_SHELL_VARS",
    "EPICS_CA_DISCOVERY",
    "EPICS_CA_MCAST_TTL",
    // Rust-only client-side IP quarantine. The C
    // EPICS_IOC_IGNORE_SERVERS lives on the server side and operates
    // on server names (rsrv/qsrv) — different semantics, intentionally
    // not surfaced here as a CA env to avoid the previous confusion.
    "EPICS_RS_CLIENT_IGNORE",
    "EPICS_CAS_INTF_ADDR_LIST",
    "EPICS_CAS_BEACON_ADDR_LIST",
    "EPICS_CAS_AUTO_BEACON_ADDR_LIST",
    "EPICS_CAS_IGNORE_ADDR_LIST",
    "EPICS_CAS_BEACON_PERIOD",
    "EPICS_CAS_BEACON_PORT",
    "EPICS_CAS_SERVER_PORT",
    "EPICS_CAS_INACTIVITY_TMO",
    "EPICS_CAS_MAX_CHANNELS",
    "EPICS_CAS_MAX_SUBS_PER_CHAN",
    "EPICS_CAS_AUDIT_FILE",
    "EPICS_CAS_AUDIT",
    "EPICS_CAS_RATE_LIMIT_MSGS_PER_SEC",
    "EPICS_CAS_RATE_LIMIT_BURST",
    "EPICS_CAS_RATE_LIMIT_STRIKES",
    "EPICS_CAS_INTROSPECTION_ADDR",
    "EPICS_CAS_TLS_CERT_FILE",
    "EPICS_CAS_TLS_KEY_FILE",
    "EPICS_CAS_TLS_CLIENT_CA_FILE",
    "EPICS_CA_TLS_ROOTS_FILE",
];

fn lint_env(issues: &mut Vec<Issue>) {
    let known: HashSet<&str> = KNOWN_ENV.iter().copied().collect();
    let mut seen_addr_list = false;
    let mut seen_auto_addr_list = false;
    for (k, v) in std::env::vars() {
        if !(k.starts_with("EPICS_CA") || k.starts_with("EPICS_CAS")) {
            continue;
        }
        if !known.contains(k.as_str()) {
            issues.push(Issue::Warn(format!(
                "{k} is not a recognized EPICS env var (typo? new since this lint?)",
            )));
            continue;
        }
        match k.as_str() {
            "EPICS_CA_ADDR_LIST" => {
                seen_addr_list = !v.trim().is_empty();
                for tok in v.split_whitespace() {
                    if !tok.contains('.') {
                        issues.push(Issue::Warn(format!(
                            "EPICS_CA_ADDR_LIST entry {tok:?} doesn't look like an IP address",
                        )));
                    }
                }
            }
            "EPICS_CA_AUTO_ADDR_LIST" => {
                seen_auto_addr_list = matches!(v.trim(), "NO" | "no" | "0" | "off");
            }
            "EPICS_CA_CONN_TMO" | "EPICS_CA_PUT_TIMEOUT" => {
                if v.parse::<f64>().is_err() {
                    issues.push(Issue::Error(format!("{k}={v:?} is not a number")));
                }
            }
            _ => {}
        }
    }
    if seen_addr_list && seen_auto_addr_list {
        issues.push(Issue::Warn(
            "EPICS_CA_ADDR_LIST is set with EPICS_CA_AUTO_ADDR_LIST=NO — make sure this is intentional".into(),
        ));
    }
}

fn lint_db_file(path: &str, macros: &HashMap<String, String>, issues: &mut Vec<Issue>) {
    use epics_base_rs::server::db_loader;
    let cfg = db_loader::DbLoadConfig::default();
    let recs = match db_loader::parse_db_file(std::path::Path::new(path), macros, &cfg) {
        Ok(r) => r,
        Err(e) => {
            issues.push(Issue::Error(format!("{path}: parse failed: {e}")));
            return;
        }
    };
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (i, r) in recs.iter().enumerate() {
        if let Some(prev) = seen.insert(r.name.clone(), i) {
            issues.push(Issue::Error(format!(
                "{path}: duplicate record name {:?} (records {prev} and {i})",
                r.name
            )));
        }
        if r.name.is_empty() {
            issues.push(Issue::Error(format!(
                "{path}: record {i} has an empty name"
            )));
        }
        // Distinguish "unsubstituted macro" (loud, common cause of bugs)
        // from generic odd-character warnings.
        if r.name.contains("$(") {
            issues.push(Issue::Warn(format!(
                "{path}: record name {:?} contains unsubstituted macro — pass macros via --macro KEY=VALUE",
                r.name
            )));
        } else {
            for c in r.name.chars() {
                if !(c.is_ascii_alphanumeric() || c == ':' || c == '_' || c == '-' || c == '.') {
                    issues.push(Issue::Warn(format!(
                        "{path}: record name {:?} contains unusual character {c:?}",
                        r.name
                    )));
                    break;
                }
            }
        }
    }
    println!("{path}: parsed {} record(s)", recs.len());
}

fn lint_acf_file(path: &str, issues: &mut Vec<Issue>) {
    use epics_base_rs::server::access_security;
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            issues.push(Issue::Error(format!("{path}: cannot read: {e}")));
            return;
        }
    };
    let cfg = match access_security::parse_acf(&content) {
        Ok(c) => c,
        Err(e) => {
            issues.push(Issue::Error(format!("{path}: parse failed: {e}")));
            return;
        }
    };
    println!(
        "{path}: parsed {} UAG, {} HAG, {} ASG",
        cfg.uag.len(),
        cfg.hag.len(),
        cfg.asg.len(),
    );
    if !cfg.asg.contains_key("DEFAULT") {
        issues.push(Issue::Warn(format!(
            "{path}: no DEFAULT ASG — channels not matching any explicit ASG will fall through to ReadWrite",
        )));
    }
    // Cross-check: every UAG/HAG referenced in any rule must be defined.
    let known_uag: HashSet<&str> = cfg.uag.keys().map(|s| s.as_str()).collect();
    let known_hag: HashSet<&str> = cfg.hag.keys().map(|s| s.as_str()).collect();
    for (asg_name, asg) in &cfg.asg {
        for (i, rule) in asg.rules.iter().enumerate() {
            for u in &rule.uag {
                if !known_uag.contains(u.as_str()) {
                    issues.push(Issue::Error(format!(
                        "{path}: ASG {asg_name:?} rule {i} references undefined UAG {u:?}",
                    )));
                }
            }
            for h in &rule.hag {
                if !known_hag.contains(h.as_str()) {
                    issues.push(Issue::Error(format!(
                        "{path}: ASG {asg_name:?} rule {i} references undefined HAG {h:?}",
                    )));
                }
            }
        }
    }
    // Reverse: warn on UAG/HAG that no rule references (probably stale).
    let mut used_uag: HashSet<&str> = HashSet::new();
    let mut used_hag: HashSet<&str> = HashSet::new();
    for asg in cfg.asg.values() {
        for rule in &asg.rules {
            for u in &rule.uag {
                used_uag.insert(u.as_str());
            }
            for h in &rule.hag {
                used_hag.insert(h.as_str());
            }
        }
    }
    for u in known_uag.difference(&used_uag) {
        issues.push(Issue::Warn(format!("{path}: UAG {u:?} defined but unused")));
    }
    for h in known_hag.difference(&used_hag) {
        issues.push(Issue::Warn(format!("{path}: HAG {h:?} defined but unused")));
    }
}

fn main() {
    let args = Args::parse();
    let did_anything = args.env || !args.db.is_empty() || !args.acf.is_empty();
    if !did_anything {
        eprintln!(
            "ca-lint-rs: nothing to do — pass --env, --db FILE, and/or --acf FILE.\n\
             See `ca-lint-rs --help`."
        );
        std::process::exit(2);
    }
    let mut issues: Vec<Issue> = Vec::new();
    if args.env {
        lint_env(&mut issues);
    }
    let mut macros: HashMap<String, String> = HashMap::new();
    for kv in &args.macros {
        if let Some((k, v)) = kv.split_once('=') {
            macros.insert(k.trim().to_string(), v.trim().to_string());
        } else {
            eprintln!("warning: --macro expects KEY=VALUE, got {kv:?}; skipping");
        }
    }
    for db in &args.db {
        lint_db_file(db, &macros, &mut issues);
    }
    for acf in &args.acf {
        lint_acf_file(acf, &mut issues);
    }
    let mut warns = 0;
    let mut errs = 0;
    for i in &issues {
        i.print();
        match i {
            Issue::Warn(_) => warns += 1,
            Issue::Error(_) => errs += 1,
        }
    }
    println!("ca-lint-rs: {errs} error(s), {warns} warning(s)");
    if errs > 0 {
        std::process::exit(2);
    }
    if warns > 0 {
        std::process::exit(1);
    }
}
