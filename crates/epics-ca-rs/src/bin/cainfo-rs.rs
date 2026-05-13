use clap::Parser;
use epics_ca_rs::DbFieldType;
use epics_ca_rs::client::CaClient;

const VERSION_INFO: &str = concat!(
    "\nEPICS Version epics-rs ",
    env!("CARGO_PKG_VERSION"),
    ", CA Protocol version 4.13"
);

#[derive(Parser)]
#[command(
    name = "cainfo-rs",
    about = "Show EPICS PV channel information and client diagnostics",
    disable_version_flag = true
)]
struct Args {
    #[arg(short = 'V', long, hide = true)]
    version: bool,

    /// CA timeout in seconds. Mirrors C `tool_lib.c:use_ca_timeout_env`.
    #[arg(short = 'w', long = "wait")]
    timeout: Option<f64>,

    /// `ca_client_status` interest level (0-10). Accepted for parity;
    /// today the Rust client surfaces the equivalent diagnostics via
    /// `--diag` instead.
    #[arg(short = 's', long = "stat-level", value_name = "LEVEL")]
    stat_level: Option<u8>,

    /// CA priority (0-99). Accepted for parity.
    #[arg(short = 'p', long)]
    priority: Option<u8>,

    /// Show client diagnostic counters and event history (Rust-only,
    /// no C analogue).
    #[arg(short = 'd', long)]
    diag: bool,

    /// PV names to query (omit for diagnostics only).
    pv_names: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.version {
        println!("{VERSION_INFO}");
        return;
    }

    if args.priority.is_some() {
        eprintln!("cainfo-rs: -p (priority) is accepted for parity but not yet honoured");
    }
    if args.stat_level.is_some() {
        eprintln!(
            "cainfo-rs: -s (interest level) is accepted for parity; use --diag for the Rust client's diagnostics"
        );
    }

    let client = CaClient::new().await.expect("failed to create CA client");
    let timeout = epics_ca_rs::cli::timeout_duration(
        args.timeout
            .unwrap_or_else(epics_ca_rs::cli::env_default_timeout),
    );

    let mut failed = false;
    for pv_name in &args.pv_names {
        let ch = client.create_channel(pv_name);
        match ch.wait_connected(timeout).await {
            Ok(()) => match ch.info().await {
                Ok(info) => {
                    // C `cainfo.c::printResult`: PV name on its own
                    // line, then five indented lines using a
                    // fixed-column key layout. Mirror it exactly so
                    // existing operator workflows that grep on
                    // `State:` / `Host:` etc. keep working.
                    let read_prefix = if info.access_rights.read { "" } else { "no " };
                    let write_prefix = if info.access_rights.write { "" } else { "no " };
                    println!(
                        "{name}\n    \
                         State:            connected\n    \
                         Host:             {host}\n    \
                         Access:           {rp}read, {wp}write\n    \
                         Native data type: {dbf}\n    \
                         Request type:     {dbr}\n    \
                         Element count:    {n}",
                        name = info.pv_name,
                        host = info.server_addr,
                        rp = read_prefix,
                        wp = write_prefix,
                        dbf = dbf_name(info.native_type),
                        dbr = dbr_name(info.native_type),
                        n = info.element_count,
                    );
                }
                Err(e) => {
                    eprintln!("{pv_name}: {e}");
                    failed = true;
                }
            },
            Err(_) => {
                // C `cainfo` prints "never connected" on connect-fail
                // (since `ca_state(chid)` returns the never-connected
                // state when the search hasn't resolved). Mirror that
                // shape — operators that pipe to grep / awk expect
                // the same key prefixes regardless of state.
                println!(
                    "{pv_name}\n    \
                     State:            never connected"
                );
                failed = true;
            }
        }
    }

    if args.diag || args.pv_names.is_empty() {
        if !args.pv_names.is_empty() {
            println!();
        }
        println!("--- Client Diagnostics ---");
        println!("{}", client.diagnostics());
    }

    if failed {
        std::process::exit(1);
    }
}

/// `dbf_type_to_text` parity. Maps our native field type to the
/// `DBF_*` mnemonic the C tool prints.
fn dbf_name(t: DbFieldType) -> &'static str {
    match t {
        DbFieldType::String => "DBF_STRING",
        DbFieldType::Short => "DBF_SHORT",
        DbFieldType::Float => "DBF_FLOAT",
        DbFieldType::Enum => "DBF_ENUM",
        DbFieldType::Char => "DBF_CHAR",
        DbFieldType::Long => "DBF_LONG",
        DbFieldType::Double => "DBF_DOUBLE",
        DbFieldType::Int64 => "DBF_INT64",
    }
}

/// `dbr_type_to_text(dbf_type_to_DBR(t))` parity. The default request
/// type for a native DBF is the matching DBR.
fn dbr_name(t: DbFieldType) -> &'static str {
    match t {
        DbFieldType::String => "DBR_STRING",
        DbFieldType::Short => "DBR_SHORT",
        DbFieldType::Float => "DBR_FLOAT",
        DbFieldType::Enum => "DBR_ENUM",
        DbFieldType::Char => "DBR_CHAR",
        DbFieldType::Long => "DBR_LONG",
        DbFieldType::Double => "DBR_DOUBLE",
        DbFieldType::Int64 => "DBR_DOUBLE", // Int64 has no CA wire type; appears as Double
    }
}
