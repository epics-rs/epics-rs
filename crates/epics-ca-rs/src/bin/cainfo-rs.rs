use clap::{CommandFactory, FromArgMatches, Parser};
use epics_ca_rs::DbFieldType;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::copt::CTool;

/// Owner of every C-scanned option argument in this binary (see
/// [`epics_ca_rs::copt`]).
const TOOL: CTool = CTool::new("cainfo");

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
    #[arg(short = 'w', long = "wait", allow_hyphen_values = true)]
    timeout: Option<String>,

    /// `ca_client_status` interest level. A non-zero level prints the
    /// client status dump *instead of* per-PV info (C `cainfo.c:77-79`,
    /// `:202-205`); `-s 0` (and an unparseable value, C `:167-173`) is
    /// normal per-PV mode. Kept as a raw string so the C "invalid →
    /// ignored, reset to 0" rule is reproduced rather than clap erroring.
    #[arg(short = 's', long = "stat-level", value_name = "LEVEL")]
    stat_level: Option<String>,

    /// CA priority (`sscanf("%u")`, clamped to `CA_PRIORITY_MAX`; `-p -1`
    /// and `-p 500` both clamp to 99 in C, `cainfo.c:175-182`).
    #[arg(short = 'p', long, allow_hyphen_values = true)]
    priority: Option<String>,

    /// Show client diagnostic counters and event history (Rust-only,
    /// no C analogue). Unlike `-s`, this is *additive*: per-PV info still
    /// prints, with the diagnostics appended.
    #[arg(short = 'd', long)]
    diag: bool,

    /// PV names to query.
    pv_names: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::from_arg_matches(&TOOL.get_matches(Args::command()))
        .expect("clap validated the arguments");

    if args.version {
        println!("{VERSION_INFO}");
        return;
    }

    // C `cainfo.c`: `statLevel` non-zero selects `ca_client_status`
    // mode, which prints the client status dump *instead of* per-PV
    // info and does not require PV names. Zero (or an unparseable `-s`)
    // is normal per-PV mode. `--diag` is the Rust-only additive flag.
    let stat_level = TOOL.stat_level(args.stat_level.as_deref());
    let stat_mode = stat_level != 0;

    // C `cainfo.c:202-205`: a missing PV list is an error unless a
    // non-zero `-s` level was selected. `--diag` (Rust-only) is an
    // explicit diagnostics request, so it likewise exempts the error.
    if !stat_mode && !args.diag && args.pv_names.is_empty() {
        TOOL.no_pv_name();
    }

    let client = CaClient::new().await.expect("failed to create CA client");

    // C `cainfo.c:77-79`: in `ca_client_status` mode, print only the
    // client status dump (the Rust equivalent is `diagnostics()`) and
    // skip the per-PV block entirely.
    if stat_mode {
        println!("--- Client Diagnostics ---");
        println!("{}", client.diagnostics());
        return;
    }

    // C `-w`: `epicsScanDouble` overwrites the env-loaded `caTimeout` only on
    // a successful scan; a bad value warns and the query still runs.
    let timeout = epics_ca_rs::cli::timeout_duration(TOOL.timeout(
        args.timeout.as_deref(),
        epics_ca_rs::cli::env_default_timeout(),
    ));

    // -p selects the priority virtual circuit.
    let priority = TOOL.priority(args.priority.as_deref());
    // C `cainfo.c:228-232`: `connect_pvs` gates the ENTIRE per-PV print
    // phase (`if (!result) result = cainfo(pvs, nPvs)`). One PV that fails
    // to connect inside the single `ca_pend_io` window aborts before
    // `cainfo()` runs — stdout stays empty and the tool exits 1. It never
    // prints a `State: never connected` block, because a connect failure
    // means the print phase never happens.
    let channels =
        match epics_ca_rs::cli::connect_pvs(&client, &args.pv_names, priority, timeout).await {
            Ok(channels) => channels,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };

    let mut failed = false;
    for (pv_name, ch) in args.pv_names.iter().zip(&channels) {
        match ch.info().await {
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
        }
    }

    // `--diag` (Rust-only) appends the client diagnostics after the
    // per-PV block. `-s` never reaches here — its non-zero mode returned
    // above, and `-s 0` is plain per-PV mode.
    if args.diag {
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
        DbFieldType::UInt64 => "DBF_UINT64",
        DbFieldType::UShort => "DBF_USHORT",
        DbFieldType::ULong => "DBF_ULONG",
        DbFieldType::UChar => "DBF_UCHAR",
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
        DbFieldType::UInt64 => "DBR_DOUBLE", // UInt64 has no CA wire type; appears as Double
        DbFieldType::UShort => "DBR_LONG",  // DBF_USHORT promotes to DBR_LONG (db_convert.h)
        DbFieldType::ULong => "DBR_DOUBLE", // DBF_ULONG promotes to DBR_DOUBLE like UInt64
        DbFieldType::UChar => "DBR_CHAR",   // DBF_UCHAR promotes to DBR_CHAR (db_convert.h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `-s` scan now lives in the shared owner ([`CTool::stat_level`]),
    /// which every C-scanned option argument in every tool goes through.
    /// These boundaries are cainfo's stake in it.
    ///
    /// C `cainfo.c:167-173` `sscanf("%u")` semantics by boundary:
    /// valid → that level; `0` → 0 (normal mode, NOT diagnostics);
    /// unparseable → reset to 0 (ignored).
    #[test]
    fn stat_level_parses_like_sscanf_u() {
        assert_eq!(TOOL.stat_level(Some("0")), 0);
        assert_eq!(TOOL.stat_level(Some("1")), 1);
        assert_eq!(TOOL.stat_level(Some("10")), 10);
        // Leading digits with trailing junk: sscanf("%u") stops at junk.
        assert_eq!(TOOL.stat_level(Some("3abc")), 3);
        // No leading digits → ignored (reset to 0).
        assert_eq!(TOOL.stat_level(Some("abc")), 0);
        assert_eq!(TOOL.stat_level(Some("")), 0);
    }

    // C `%u` accepts an
    // optional sign before the digit run, with unsigned wrapping. The
    // earlier digit-only parser returned 0 for these and wrongly chose
    // normal per-PV mode where C selects ca_client_status mode.
    #[test]
    fn stat_level_accepts_signed_unsigned_prefix() {
        // `sscanf("-1","%u")` -> 4294967295 (probe-confirmed).
        assert_eq!(TOOL.stat_level(Some("-1")), 4_294_967_295);
        // `sscanf("+3abc","%u")` -> 3.
        assert_eq!(TOOL.stat_level(Some("+3abc")), 3);
        // Leading whitespace is skipped before the sign.
        assert_eq!(TOOL.stat_level(Some("  -5")), 5u32.wrapping_neg());
        assert_eq!(TOOL.stat_level(Some("+7")), 7);
        // A sign with no following digit is not a match → reset to 0.
        assert_eq!(TOOL.stat_level(Some("-")), 0);
        assert_eq!(TOOL.stat_level(Some("+")), 0);
        // `-0` converts to 0 → normal per-PV mode (not diagnostics).
        assert_eq!(TOOL.stat_level(Some("-0")), 0);
        // Overflow truncates mod 2^32 (NOT saturate): probe-confirmed
        // `sscanf("99999999999","%u") == 1215752191`.
        assert_eq!(TOOL.stat_level(Some("99999999999")), 1_215_752_191);
    }

    // The decisive mode-selector property: any non-zero `parse_stat_level`
    // selects ca_client_status mode (`stat_level != 0` in `main`), so a
    // signed `-s -1` enters status mode and is exempt from the missing-PV
    // error — matching C `cainfo.c:202` `if (!statLevel && nPvs < 1)`.
    #[test]
    fn signed_stat_level_selects_status_mode() {
        assert_ne!(
            TOOL.stat_level(Some("-1")),
            0,
            "-s -1 must enter status mode"
        );
        assert_ne!(
            TOOL.stat_level(Some("+3abc")),
            0,
            "-s +3abc must enter status mode"
        );
    }
}
