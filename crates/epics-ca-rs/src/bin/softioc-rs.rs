use clap::Parser;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::records::{
    ai::AiRecord, ao::AoRecord, bi::BiRecord, bo::BoRecord, longin::LonginRecord,
    longout::LongoutRecord, mbbi::MbbiRecord, mbbo::MbboRecord, stringin::StringinRecord,
    stringout::StringoutRecord,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_ca_rs::server::CaServer;
use std::collections::HashMap;

/// A simple soft IOC that hosts PVs over Channel Access.
///
/// Example: softioc-rs --pv TEMP:double:25.0 --record ai:TEMP_REC:25.0 --db test.db
#[derive(Parser)]
#[command(name = "softioc")]
struct Args {
    /// PV definitions in the format NAME:TYPE:VALUE
    /// Supported types: string, short, float, enum, char, long, double
    #[arg(long = "pv")]
    pvs: Vec<String>,

    /// Record definitions in the format RECORD_TYPE:NAME:VALUE
    /// Supported record types: ai, ao, bi, bo, stringin, stringout, longin, longout, mbbi, mbbo
    #[arg(long = "record")]
    records: Vec<String>,

    /// DB file paths to load
    #[arg(long = "db")]
    db_files: Vec<String>,

    /// Macro definitions for DB files in KEY=VALUE format
    #[arg(long = "macro", short = 'm')]
    macros: Vec<String>,

    /// Port to listen on. Omit to take `EPICS_CAS_SERVER_PORT`, then
    /// `EPICS_CA_SERVER_PORT`, then 5064 (C `caservertask.c:491-499`).
    /// An explicit `--port 0` binds an ephemeral port.
    #[arg(long)]
    port: Option<u16>,

    /// Start interactive iocsh shell
    #[arg(long, short = 'i')]
    shell: bool,

    /// EXPERIMENTAL Rust-only TLS: server certificate chain (PEM).
    /// Both --tls-cert and --tls-key are required to enable TLS.
    /// Falls back to EPICS_CAS_TLS_CERT_FILE env var if not set.
    /// Setting these makes the IOC unreachable from C tools — see
    /// doc/11-tls-design.md.
    #[arg(long = "tls-cert", value_name = "PEM_FILE")]
    tls_cert: Option<String>,

    /// EXPERIMENTAL Rust-only TLS: server private key (PEM).
    #[arg(long = "tls-key", value_name = "PEM_FILE")]
    tls_key: Option<String>,

    /// EXPERIMENTAL Rust-only TLS: client CA bundle (PEM). When set,
    /// the server requires mTLS — connections without a valid client
    /// cert from this trust pool are rejected.
    #[arg(long = "tls-client-ca", value_name = "PEM_FILE")]
    tls_client_ca: Option<String>,

    /// Announce this IOC via mDNS as `<INSTANCE>._epics-ca._tcp.local.`
    /// so clients on the same LAN can discover it without manual
    /// `EPICS_CA_ADDR_LIST` configuration. Requires building with
    /// --features discovery; otherwise the flag is rejected.
    #[arg(long = "mdns", value_name = "INSTANCE")]
    mdns: Option<String>,

    /// Repeatable: extra TXT key=value pair attached to the mDNS
    /// announce. Use for site metadata like `version=4.13` or
    /// `asg=BEAM`. Ignored unless --mdns is set.
    #[arg(long = "mdns-txt", value_name = "KEY=VALUE")]
    mdns_txt: Vec<String>,

    /// RFC 2136 Dynamic DNS UPDATE: address of the authoritative DNS
    /// server (e.g. `10.0.0.1:53`). When all of --dns-update-server,
    /// --dns-update-zone, and --dns-update-instance are set, the IOC
    /// self-registers a SRV+PTR+TXT triple in the zone on startup and
    /// removes them on graceful shutdown. Requires building with
    /// --features discovery-dns-update.
    #[arg(long = "dns-update-server", value_name = "HOST:PORT")]
    dns_update_server: Option<String>,

    /// DNS zone for RFC 2136 UPDATE (e.g. `facility.local.`).
    #[arg(long = "dns-update-zone", value_name = "ZONE")]
    dns_update_zone: Option<String>,

    /// Service-instance label used in the SRV record's owner name —
    /// becomes `<INSTANCE>._epics-ca._tcp.<ZONE>`.
    #[arg(long = "dns-update-instance", value_name = "NAME")]
    dns_update_instance: Option<String>,

    /// Hostname target written into the SRV record. Falls back to the
    /// system hostname. The host must already have an A/AAAA record in
    /// a resolvable zone.
    #[arg(long = "dns-update-host", value_name = "HOST")]
    dns_update_host: Option<String>,

    /// Path to a BIND-format TSIG key file (output of `tsig-keygen`).
    /// Without it the UPDATE is sent unsigned and most production DNS
    /// servers will reject it.
    #[arg(long = "dns-update-tsig-key", value_name = "FILE")]
    dns_update_tsig_key: Option<String>,

    /// TTL in seconds applied to every record we add (default: 60).
    #[arg(long = "dns-update-ttl", value_name = "SECONDS", default_value_t = 60)]
    dns_update_ttl: u64,

    /// Keepalive refresh interval in seconds (default: 30).
    #[arg(
        long = "dns-update-keepalive",
        value_name = "SECONDS",
        default_value_t = 30
    )]
    dns_update_keepalive: u64,
}

fn is_type_keyword(s: &str) -> bool {
    matches!(
        s,
        "string"
            | "str"
            | "short"
            | "int16"
            | "float"
            | "f32"
            | "enum"
            | "u16"
            | "char"
            | "u8"
            | "long"
            | "int32"
            | "double"
            | "f64"
    )
}

fn parse_pv_def(def: &str) -> CaResult<(String, EpicsValue)> {
    // Format is NAME:TYPE:VALUE, but NAME may contain colons (e.g. "SEQ:counter").
    // Find the type keyword by scanning the colon-separated segments from the right.
    let segments: Vec<&str> = def.split(':').collect();

    // We need at least 3 segments (name, type, value), with the type being a known keyword.
    // Scan from the end to find the type keyword — the segment after it is the value,
    // and everything before it is the name.
    let type_idx = segments
        .iter()
        .rposition(|s| is_type_keyword(&s.to_lowercase()));

    let type_idx = match type_idx {
        Some(idx) if idx > 0 && idx + 1 < segments.len() => idx,
        _ => {
            return Err(epics_base_rs::error::CaError::InvalidValue(format!(
                "expected NAME:TYPE:VALUE, got '{def}'"
            )));
        }
    };

    let name = segments[..type_idx].join(":");
    let type_str = segments[type_idx].to_lowercase();
    let value_str = segments[type_idx + 1..].join(":");

    let dbr_type = match type_str.as_str() {
        "string" | "str" => DbFieldType::String,
        "short" | "int16" => DbFieldType::Short,
        "float" | "f32" => DbFieldType::Float,
        "enum" | "u16" => DbFieldType::Enum,
        "char" | "u8" => DbFieldType::Char,
        "long" | "int32" => DbFieldType::Long,
        "double" | "f64" => DbFieldType::Double,
        _ => unreachable!(),
    };

    let value = EpicsValue::parse(dbr_type, &value_str)?;
    Ok((name, value))
}

fn parse_record_def(
    def: &str,
) -> CaResult<(String, Box<dyn epics_base_rs::server::record::Record>)> {
    // Split on first ':' to get record type; the remainder is NAME or NAME:...:VALUE.
    // PV names often contain colons (e.g. "SEQ:counter"), so we try to parse the
    // last ':'-separated segment as a value — if that fails, the whole remainder is the name.
    let (rec_type_str, remainder) = def.split_once(':').ok_or_else(|| {
        epics_base_rs::error::CaError::InvalidValue(format!(
            "expected RECORD_TYPE:NAME[:VALUE], got '{def}'"
        ))
    })?;

    let rec_type = rec_type_str.to_lowercase();

    // Try splitting off the last ':' segment as a candidate value.
    let (name, value_str) = if let Some((prefix, suffix)) = remainder.rsplit_once(':') {
        (prefix, suffix)
    } else {
        (remainder, "")
    };

    // Helper: attempt to parse the candidate value segment.
    //   - empty candidate (no `:VALUE`, or a trailing `:`)  -> `name` + default;
    //     `name` is the colon-stripped prefix, and with no colon at all
    //     `rsplit_once` failed so `name == remainder` anyway. Using `name`
    //     here (not `remainder`) keeps a trailing `:` out of the PV name,
    //     matching the stringin/stringout arms.
    //   - candidate parses                                  -> `name` + value.
    //   - candidate present but unparsable (a colon-bearing PV name whose
    //     last segment is not a number) -> `remainder` + default, so the
    //     name keeps its embedded colons.
    macro_rules! parse_or_default {
        ($type:ty, $default:expr) => {{
            if value_str.is_empty() {
                (name, $default)
            } else if let Ok(v) = value_str.parse::<$type>() {
                (name, v)
            } else {
                (remainder, $default)
            }
        }};
    }

    // Each arm yields a `(record_name, record)` tuple: the numeric arms
    // parse a typed value (falling back to a default — and keeping the
    // colon-bearing `remainder` as the name — when the candidate value
    // segment does not parse), the string arms take `name`/`value_str`
    // straight from the `rsplit_once` split, and the catch-all errors.
    let (record_name, record): (String, Box<dyn epics_base_rs::server::record::Record>) =
        match rec_type.as_str() {
            "ai" => {
                let (n, val) = parse_or_default!(f64, 0.0);
                (n.to_string(), Box::new(AiRecord::new(val)))
            }
            "ao" => {
                let (n, val) = parse_or_default!(f64, 0.0);
                (n.to_string(), Box::new(AoRecord::new(val)))
            }
            "bi" => {
                let (n, val) = parse_or_default!(u16, 0);
                (n.to_string(), Box::new(BiRecord::new(val)))
            }
            "bo" => {
                let (n, val) = parse_or_default!(u16, 0);
                (n.to_string(), Box::new(BoRecord::new(val)))
            }
            "longin" => {
                let (n, val) = parse_or_default!(i32, 0);
                (n.to_string(), Box::new(LonginRecord::new(val)))
            }
            "longout" => {
                let (n, val) = parse_or_default!(i32, 0);
                (n.to_string(), Box::new(LongoutRecord::new(val)))
            }
            "mbbi" => {
                let (n, val) = parse_or_default!(u16, 0);
                (n.to_string(), Box::new(MbbiRecord::new(val)))
            }
            "mbbo" => {
                let (n, val) = parse_or_default!(u16, 0);
                (n.to_string(), Box::new(MbboRecord::new(val)))
            }
            "stringin" | "stringout" => {
                // No parse step — any string is a valid value — so use
                // the `rsplit_once` split directly: with a `:VALUE`
                // segment, `name`/`value_str`; with no colon, `name` is
                // `remainder` and `value_str` is empty. A trailing colon
                // (`stringin:PV:`) is an explicit empty value and yields
                // name `PV`, not `PV:` — which the prior `value_str`-
                // empty branch got wrong by falling back to `remainder`.
                if rec_type == "stringin" {
                    (name.to_string(), Box::new(StringinRecord::new(value_str)))
                } else {
                    (name.to_string(), Box::new(StringoutRecord::new(value_str)))
                }
            }
            _ => {
                return Err(epics_base_rs::error::CaError::InvalidValue(format!(
                    "unknown record type '{rec_type}'"
                )));
            }
        };

    // A record with an empty or whitespace-only PV name (e.g. `ai:`,
    // `stringin::x`, `ai: `) is never valid and would otherwise register
    // an unaddressable record.
    if record_name.trim().is_empty() {
        return Err(epics_base_rs::error::CaError::InvalidValue(format!(
            "record definition '{def}' has an empty PV name"
        )));
    }
    Ok((record_name, record))
}

fn parse_macros(macro_strs: &[String]) -> HashMap<String, String> {
    let mut macros = HashMap::new();
    for m in macro_strs {
        if let Some((k, v)) = m.split_once('=') {
            macros.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    macros
}

// One worker, reactor-style: the default per-CPU pool migrates the
// mostly-serial serving work across idle workers, costing ~35 µs of
// extra CPU per put on a 96-core host (doc/qsrv-put-perf.md in the
// workspace root). Multi-thread flavor is kept so `block_on_sync`
// works from runtime tasks.
#[tokio::main(worker_threads = 1)]
async fn main() -> CaResult<()> {
    let args = Args::parse();

    if args.pvs.is_empty() && args.records.is_empty() && args.db_files.is_empty() {
        eprintln!("Error: at least one --pv, --record, or --db is required");
        std::process::exit(1);
    }

    // The builder already seeds `port` from the EPICS environment; only an
    // explicit `--port` may override it.
    let mut builder = CaServer::builder();
    if let Some(port) = args.port {
        builder = builder.port(port);
    }

    for pv_def in &args.pvs {
        let (name, value) = parse_pv_def(pv_def)?;
        eprintln!("  PV: {name} = {value} ({})", value.dbr_type() as u16);
        builder = builder.pv(&name, value);
    }

    for rec_def in &args.records {
        let (name, record) = parse_record_def(rec_def)?;
        eprintln!("  Record: {name} ({})", record.record_type());
        builder = builder.record_boxed(&name, record);
    }

    let macros = parse_macros(&args.macros);
    for db_file in &args.db_files {
        eprintln!("  Loading DB: {db_file}");
        builder = builder.db_file(db_file, &macros)?;
    }

    // CLI-supplied TLS overrides any EPICS_CAS_TLS_* env vars; if both
    // CLI flags are absent the server picks them up from the env at
    // run() time. Mismatched (only --tls-cert OR only --tls-key) is a
    // hard error.
    #[cfg(feature = "experimental-rust-tls")]
    {
        match (&args.tls_cert, &args.tls_key) {
            (Some(cert_path), Some(key_path)) => {
                let chain = epics_ca_rs::tls::load_certs(cert_path)?;
                let key = epics_ca_rs::tls::load_private_key(key_path)?;
                let tls = if let Some(ref ca_path) = args.tls_client_ca {
                    let roots = epics_ca_rs::tls::load_root_store(ca_path)?;
                    epics_ca_rs::tls::TlsConfig::server_mtls_from_pem(chain, key, roots).map_err(
                        |e| epics_base_rs::error::CaError::InvalidValue(format!("TLS: {e}")),
                    )?
                } else {
                    epics_ca_rs::tls::TlsConfig::server_from_pem(chain, key).map_err(|e| {
                        epics_base_rs::error::CaError::InvalidValue(format!("TLS: {e}"))
                    })?
                };
                builder = builder.with_tls(tls);
            }
            (None, None) => {} // env-based or plaintext
            _ => {
                return Err(epics_base_rs::error::CaError::InvalidValue(
                    "--tls-cert and --tls-key must both be set or both unset".into(),
                ));
            }
        }
    }
    #[cfg(not(feature = "experimental-rust-tls"))]
    if args.tls_cert.is_some() || args.tls_key.is_some() || args.tls_client_ca.is_some() {
        return Err(epics_base_rs::error::CaError::InvalidValue(
            "TLS flags require building with --features experimental-rust-tls".into(),
        ));
    }

    // mDNS announce. The discovery feature is required to actually
    // emit packets; without it we keep the field for diagnostics and
    // the server logs a warning at startup.
    if let Some(ref instance) = args.mdns {
        builder = builder.announce_mdns(instance);
        for kv in &args.mdns_txt {
            if let Some((k, v)) = kv.split_once('=') {
                builder = builder.announce_txt(k, v);
            } else {
                eprintln!("warning: --mdns-txt expects KEY=VALUE, got {kv:?}; skipping");
            }
        }
    }

    // RFC 2136 Dynamic DNS UPDATE registration.
    #[cfg(feature = "discovery-dns-update")]
    {
        let any_dns_flag = args.dns_update_server.is_some()
            || args.dns_update_zone.is_some()
            || args.dns_update_instance.is_some();
        let all_required = args.dns_update_server.is_some()
            && args.dns_update_zone.is_some()
            && args.dns_update_instance.is_some();
        if any_dns_flag && !all_required {
            return Err(epics_base_rs::error::CaError::InvalidValue(
                "--dns-update-server, --dns-update-zone, --dns-update-instance must all be set together".into(),
            ));
        }
        if all_required {
            let server: std::net::SocketAddr = args
                .dns_update_server
                .as_ref()
                .unwrap()
                .parse()
                .map_err(|e| {
                    epics_base_rs::error::CaError::InvalidValue(format!("--dns-update-server: {e}"))
                })?;
            let host = args.dns_update_host.clone().unwrap_or_else(|| {
                // Fallback: $HOSTNAME env var, then /etc/hostname, then "localhost".
                // We avoid pulling in a `hostname` crate just for this; users with
                // exotic hostname sources can pass --dns-update-host explicitly.
                std::env::var("HOSTNAME")
                    .ok()
                    .or_else(|| {
                        std::fs::read_to_string("/etc/hostname")
                            .ok()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or_else(|| "localhost".to_string())
            });
            let tsig = match args.dns_update_tsig_key.as_ref() {
                Some(path) => Some(
                    epics_ca_rs::discovery::TsigKey::from_bind_file(path).map_err(|e| {
                        epics_base_rs::error::CaError::InvalidValue(format!(
                            "--dns-update-tsig-key: {e}"
                        ))
                    })?,
                ),
                None => None,
            };
            let reg = epics_ca_rs::discovery::DnsRegistration {
                server,
                zone: args.dns_update_zone.clone().unwrap(),
                instance: args.dns_update_instance.clone().unwrap(),
                host,
                // Advertise the port the server will actually bind: an
                // omitted --port defers to the EPICS environment, same
                // as the builder's own seeding above.
                port: args
                    .port
                    .unwrap_or_else(epics_base_rs::runtime::net::ca_server_port),
                txt: Vec::new(),
                ttl: std::time::Duration::from_secs(args.dns_update_ttl),
                keepalive: std::time::Duration::from_secs(args.dns_update_keepalive),
                tsig,
            };
            builder = builder.register_dns_update(reg);
        }
    }
    #[cfg(not(feature = "discovery-dns-update"))]
    if args.dns_update_server.is_some()
        || args.dns_update_zone.is_some()
        || args.dns_update_instance.is_some()
        || args.dns_update_tsig_key.is_some()
    {
        return Err(epics_base_rs::error::CaError::InvalidValue(
            "--dns-update-* flags require building with --features discovery-dns-update".into(),
        ));
    }

    let server = builder.build().await?;

    // C iocInit owns scanInit/initialProcess — not RSRV. Start the
    // core-owned scan owner (periodic scan-%g threads + the PINI=YES
    // pass) here, where this binary's iocInit sequence ends; the CA
    // server itself never scans. Held to process end so the scan
    // threads live as long as the IOC serves.
    let _scan_owner = epics_base_rs::server::scan::ScanOwner::start(server.database().clone());

    if args.shell {
        server.run_with_shell(|_shell| {}).await
    } else {
        server.run().await
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, parse_record_def};
    use clap::Parser;

    /// R19-22 boundary: an omitted `--port` must reach the builder as
    /// `None`, so `CaServerBuilder`'s env-seeded default
    /// (`EPICS_CAS_SERVER_PORT` > `EPICS_CA_SERVER_PORT` > 5064) survives.
    /// A clap literal here silently shadows the environment and pins the
    /// IOC to the production port.
    #[test]
    fn omitted_port_defers_to_the_epics_environment() {
        let args = Args::parse_from(["softioc-rs", "--db", "x.db"]);
        assert_eq!(args.port, None);
    }

    /// The flag still wins over the environment, and `--port 0` is a
    /// distinct, representable request for an ephemeral bind — the two
    /// meanings the old `default_value_t = 5064` conflated.
    #[test]
    fn explicit_port_overrides_and_zero_means_ephemeral() {
        let args = Args::parse_from(["softioc-rs", "--db", "x.db", "--port", "15070"]);
        assert_eq!(args.port, Some(15070));
        let args = Args::parse_from(["softioc-rs", "--db", "x.db", "--port", "0"]);
        assert_eq!(args.port, Some(0));
    }

    fn name_of(def: &str) -> String {
        parse_record_def(def).expect("valid record def").0
    }

    #[test]
    fn numeric_record_splits_value_from_name() {
        assert_eq!(name_of("ai:TEMP"), "TEMP");
        assert_eq!(name_of("ai:TEMP:42.5"), "TEMP");
        // Non-numeric trailing segment: the whole remainder is the name.
        assert_eq!(name_of("ai:SEQ:counter"), "SEQ:counter");
        // Trailing colon = explicit empty/default value; the numeric arms
        // must strip it from the name, same as the string arms.
        assert_eq!(name_of("ai:TEMP:"), "TEMP");
        assert_eq!(name_of("longout:SEQ:counter:"), "SEQ:counter");
    }

    #[test]
    fn whitespace_only_pv_name_is_rejected() {
        assert!(parse_record_def("ai: ").is_err());
        assert!(parse_record_def("stringin:  ").is_err());
    }

    #[test]
    fn string_record_handles_colons_and_trailing_colon() {
        // Plain name, no value.
        assert_eq!(name_of("stringin:MYPV"), "MYPV");
        // NAME:VALUE.
        assert_eq!(name_of("stringout:MYPV:hello"), "MYPV");
        // Trailing colon = explicit empty value; name must NOT keep the
        // colon (the bug this fix addresses).
        assert_eq!(name_of("stringin:MYPV:"), "MYPV");
    }

    #[test]
    fn empty_pv_name_is_rejected() {
        assert!(parse_record_def("ai:").is_err());
        assert!(parse_record_def("stringin::hello").is_err());
    }

    #[test]
    fn unknown_record_type_is_rejected() {
        assert!(parse_record_def("bogus:PV").is_err());
        assert!(parse_record_def("noseparator").is_err());
    }
}
