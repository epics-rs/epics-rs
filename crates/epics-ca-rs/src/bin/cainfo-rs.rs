use clap::{CommandFactory, FromArgMatches, Parser};
use epics_ca_rs::DbFieldType;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::copt::{self, CTool};

/// Owner of every C-scanned option argument in this binary (see
/// [`epics_ca_rs::copt`]).
const TOOL: CTool = CTool::new("cainfo");

/// The getopt cases that `return` from C's `main` (`cainfo.c:147-152`), by clap
/// id. `copt::Scan::finish` performs the FIRST one on the command line, after
/// replaying the warnings the loop raised on its way there (R13-26).
/// `-n` is the odd one (R13-25). It is in cainfo's optstring (`cainfo.c:146`
/// `":nhVw:s:p:"`) but has no `case` arm, so getopt hands it to the loop and it
/// falls straight into `default:` — `usage(); return 1` (`cainfo.c:194-196`).
/// That makes it a terminal like `-h`, only with the failure status; it is NOT
/// an unrecognized option (`case '?'`), which is what a letter *outside* the
/// optstring gets.
const TERMINALS: &[(&str, copt::Terminal)] = &[
    ("help", copt::Terminal::Usage(0)),
    ("version", copt::Terminal::Version),
    ("dead_n", copt::Terminal::Usage(1)),
];

#[derive(Parser)]
#[command(
    name = "cainfo-rs",
    about = "Show EPICS PV channel information and client diagnostics",
    disable_version_flag = true,
    disable_help_flag = true
)]
struct Args {
    // `-h` / `-V` are ordinary options, performed by `copt::Scan::finish` at
    // their place in the getopt order so the warnings C prints before them
    // survive (R13-26).
    //
    // Every option below is `Append` (value option) or `Count` (flag): C's
    // getopt loop accepts every option any number of times, last one winning
    // (R13-17, see `epics_ca_rs::copt`).
    //
    // Doc comments on these fields are the option's HELP TEXT, so the rationale
    // above stays a plain comment.
    /// Print this message
    #[arg(short = 'h', long, action = clap::ArgAction::Count)]
    help: u8,

    #[arg(short = 'V', long, hide = true, action = clap::ArgAction::Count)]
    version: u8,

    // `-n` must be DECLARED for the same reason C declares it in the optstring:
    // an option letter that is merely absent gets `case '?'` ("Unrecognized
    // option"), a different diagnostic and a different exit path from the
    // `default:` arm `-n` actually lands in (R13-25). It carries no value and is
    // never read — `TERMINALS` performs it.
    #[arg(short = 'n', hide = true, action = clap::ArgAction::Count)]
    dead_n: u8,

    /// CA timeout in seconds. Mirrors C `tool_lib.c:use_ca_timeout_env`.
    #[arg(short = 'w', long = "wait", allow_hyphen_values = true, action = clap::ArgAction::Append)]
    timeout: Vec<String>,

    /// `ca_client_status` interest level. A non-zero level prints the
    /// client status dump *instead of* per-PV info (C `cainfo.c:77-78`,
    /// `:202-205`); `-s 0` (and an unparseable value, C `:167-173`) is
    /// normal per-PV mode. Kept as a raw string so the C "invalid →
    /// ignored, reset to 0" rule is reproduced rather than clap erroring.
    #[arg(
        short = 's',
        long = "stat-level",
        value_name = "LEVEL",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    stat_level: Vec<String>,

    /// CA priority (`sscanf("%u")`, clamped to `CA_PRIORITY_MAX`; `-p -1`
    /// and `-p 500` both clamp to 99 in C, `cainfo.c:175-182`).
    #[arg(short = 'p', long, allow_hyphen_values = true, action = clap::ArgAction::Append)]
    priority: Vec<String>,

    /// Show client diagnostic counters and event history (Rust-only,
    /// no C analogue). Unlike `-s`, this is *additive*: per-PV info still
    /// prints, with the diagnostics appended.
    #[arg(short = 'd', long, action = clap::ArgAction::Count)]
    diag: u8,

    /// PV names to query.
    pv_names: Vec<String>,
}

#[tokio::main]
async fn main() {
    let cmd = Args::command();
    let parsed = TOOL.get_matches(cmd.clone());
    let matches = parsed.matches();
    let args = Args::from_arg_matches(matches).expect("clap validated the arguments");

    // C's ENTIRE getopt loop runs before the `nPvs < 1` check
    // (`cainfo.c:146-198`, then `:200`), so every option argument is scanned —
    // and every warning raised — even when no PV name follows.
    //
    // `statLevel` non-zero selects `ca_client_status` mode, which prints the
    // client status dump *instead of* per-PV info and does not require PV
    // names. Zero (or an unparseable `-s`) is normal per-PV mode. `--diag` is
    // the Rust-only additive flag.
    let mut scan = parsed.scan();
    let stat_level = scan.stat_level("stat_level");
    let stat_mode = stat_level != 0;
    // C `-w`: `epicsScanDouble` overwrites the env-loaded `caTimeout` only on
    // a successful scan; a bad value warns and the query still runs.
    let ca_timeout = scan.timeout("timeout", epics_ca_rs::cli::env_default_timeout());
    // -p selects the priority virtual circuit.
    let priority = scan.priority("priority");
    // End of C's getopt loop: warnings out in command-line order, then `-h` /
    // `-V` if the loop reached one (R13-26).
    scan.finish(&cmd, &epics_ca_rs::protocol::version_info(), TERMINALS);

    // C `cainfo.c:202-205`: a missing PV list is an error unless a
    // non-zero `-s` level was selected. `--diag` (Rust-only) is an
    // explicit diagnostics request, so it likewise exempts the error.
    let diag = args.diag > 0;
    if !stat_mode && !diag && args.pv_names.is_empty() {
        TOOL.no_pv_name();
    }

    let client = CaClient::new().await.expect("failed to create CA client");

    // C `cainfo.c:77-78`: in `ca_client_status` mode, print only the
    // client status dump (the Rust equivalent is `diagnostics()`) and
    // skip the per-PV block entirely.
    if stat_mode {
        println!("--- Client Diagnostics ---");
        println!("{}", client.diagnostics());
        return;
    }

    let timeout = epics_ca_rs::cli::timeout_duration(ca_timeout);
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
                // C `cainfo.c:101` prints `ca_host_name(chid)` — the
                // reverse-resolved name, not the dotted IP (W10-B5). The
                // resolution lives behind `Channel::host_name`, the
                // `ca_host_name` analog; `info.server_addr` is the raw peer
                // address and must not reach this line.
                let host = ch.host_name().await.unwrap_or_default();
                println!(
                    "{name}\n    \
                     State:            connected\n    \
                     Host:             {host}\n    \
                     Access:           {rp}read, {wp}write\n    \
                     Native data type: {dbf}\n    \
                     Request type:     {dbr}\n    \
                     Element count:    {n}",
                    name = info.pv_name,
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
    if diag {
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

    /// `cainfo -s <arg>`, resolved the way `main` resolves it — through the
    /// ordered scan over a real command line, since that is the only way a
    /// resolver can be reached (and the only way its warning gets a position).
    fn stat_level(arg: &str) -> u32 {
        let matches = Args::command()
            .no_binary_name(true)
            .get_matches_from(["-s", arg]);
        TOOL.scan(&matches).stat_level("stat_level")
    }

    /// The `-s` scan now lives in the shared owner ([`copt::Scan::stat_level`]),
    /// which every C-scanned option argument in every tool goes through.
    /// These boundaries are cainfo's stake in it.
    ///
    /// C `cainfo.c:167-173` `sscanf("%u")` semantics by boundary:
    /// valid → that level; `0` → 0 (normal mode, NOT diagnostics);
    /// unparseable → reset to 0 (ignored).
    #[test]
    fn stat_level_parses_like_sscanf_u() {
        assert_eq!(stat_level("0"), 0);
        assert_eq!(stat_level("1"), 1);
        assert_eq!(stat_level("10"), 10);
        // Leading digits with trailing junk: sscanf("%u") stops at junk.
        assert_eq!(stat_level("3abc"), 3);
        // No leading digits → ignored (reset to 0).
        assert_eq!(stat_level("abc"), 0);
        assert_eq!(stat_level(""), 0);
    }

    // C `%u` accepts an
    // optional sign before the digit run, with unsigned wrapping. The
    // earlier digit-only parser returned 0 for these and wrongly chose
    // normal per-PV mode where C selects ca_client_status mode.
    #[test]
    fn stat_level_accepts_signed_unsigned_prefix() {
        // `sscanf("-1","%u")` -> 4294967295 (probe-confirmed).
        assert_eq!(stat_level("-1"), 4_294_967_295);
        // `sscanf("+3abc","%u")` -> 3.
        assert_eq!(stat_level("+3abc"), 3);
        // Leading whitespace is skipped before the sign.
        assert_eq!(stat_level("  -5"), 5u32.wrapping_neg());
        assert_eq!(stat_level("+7"), 7);
        // A sign with no following digit is not a match → reset to 0.
        assert_eq!(stat_level("-"), 0);
        assert_eq!(stat_level("+"), 0);
        // `-0` converts to 0 → normal per-PV mode (not diagnostics).
        assert_eq!(stat_level("-0"), 0);
        // Overflow truncates mod 2^32 (NOT saturate): probe-confirmed
        // `sscanf("99999999999","%u") == 1215752191`.
        assert_eq!(stat_level("99999999999"), 1_215_752_191);
    }

    // The decisive mode-selector property: any non-zero `parse_stat_level`
    // selects ca_client_status mode (`stat_level != 0` in `main`), so a
    // signed `-s -1` enters status mode and is exempt from the missing-PV
    // error — matching C `cainfo.c:202` `if (!statLevel && nPvs < 1)`.
    #[test]
    fn signed_stat_level_selects_status_mode() {
        assert_ne!(stat_level("-1"), 0, "-s -1 must enter status mode");
        assert_ne!(stat_level("+3abc"), 0, "-s +3abc must enter status mode");
    }
}
