// On `exec_backend` this program's `main` refuses instead of running, so
// everything below it is unreachable in that configuration by construction.
// The lint is reporting the intent, not dead code: the default build still
// lints this file in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

use clap::{CommandFactory, FromArgMatches, Parser};
use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::log::{ANSI_ESC_BLUE, ANSI_ESC_BOLD, ANSI_ESC_RESET, ERL_ERROR};
use epics_base_rs::server::ioc_app::{IocInitDecision, IocRunFailure};
use epics_base_rs::server::iocsh::use_ansi_color;
use epics_base_rs::server::records::{
    ai::AiRecord, ao::AoRecord, bi::BiRecord, bo::BoRecord, longin::LonginRecord,
    longout::LongoutRecord, mbbi::MbbiRecord, mbbo::MbboRecord, stringin::StringinRecord,
    stringout::StringoutRecord,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_ca_rs::copt;

/// A simple soft IOC that hosts PVs over Channel Access.
///
/// Example: softioc-rs --pv TEMP:double:25.0 --record ai:TEMP_REC:25.0 --db test.db
#[derive(Parser)]
#[command(name = "softioc", disable_help_flag = true)]
struct Args {
    /// Print this message and exit
    ///
    /// C `-h` (`softMain.cpp:165-168`) prints C's own usage block, which is
    /// what this prints: a script that scraped `softIoc -h` reads the same
    /// bytes here. `--help` is not C's flag and keeps clap's listing, which
    /// is where the long options this binary adds stay discoverable.
    #[arg(short = 'h', action = clap::ArgAction::SetTrue)]
    c_help: bool,

    /// List every option, including the ones C softIoc does not have
    #[arg(long = "help", action = clap::ArgAction::Help)]
    long_help: Option<bool>,

    /// PV definitions in the format NAME:TYPE:VALUE
    /// Supported types: string, short, float, enum, char, long, double
    #[arg(long = "pv")]
    pvs: Vec<String>,

    /// Record definitions in the format RECORD_TYPE:NAME:VALUE
    /// Supported record types: ai, ao, bi, bo, stringin, stringout, longin, longout, mbbi, mbbo
    #[arg(long = "record")]
    records: Vec<String>,

    /// Load records from a database file. C `-d`
    /// (`softMain.cpp:189-198`) is `dbLoadRecords(file, macros)` with the
    /// macro string current at that point in argv.
    #[arg(long = "db", short = 'd', value_name = "file.db")]
    db_files: Vec<String>,

    /// Set or REPLACE the macros used by the `-d` and `-a` that follow.
    /// C `-m` (`softMain.cpp:199-201`) assigns — `macros = optarg` — so
    /// each occurrence discards the previous set, which is what C's usage
    /// text means by "Each later -m option causes earlier macros to be
    /// discarded". The string is never parsed here for the same reason C
    /// never parses it: `dbLoadRecords` is the one that reads it.
    #[arg(long = "macro", short = 'm', value_name = "MAC=value,...")]
    macros: Vec<String>,

    /// Access-security configuration file. C `-a`
    /// (`softMain.cpp:174-185`): `asSetSubstitutions(macros)` when macros
    /// are set, then `asSetFilename(acf)` — both recorders, with the load
    /// itself deferred to `asInit`.
    #[arg(long = "acf", short = 'a', value_name = "ascf")]
    acf: Vec<String>,

    /// Load the exit database under this prefix, giving `<prefix>:exit`
    /// and `<prefix>:BaseVersion`. C `-x` (`softMain.cpp:212-220`).
    #[arg(long = "exit-prefix", short = 'x', value_name = "prefix")]
    exit_prefix: Vec<String>,

    /// `.dbd` file to read before the first database. C `-D`
    /// (`softMain.cpp:186-188`), which refuses one given after the
    /// database has already been loaded.
    #[arg(long = "dbd", short = 'D', value_name = "softIoc.dbd")]
    dbd: Vec<String>,

    /// Port to listen on. Omit to take `EPICS_CAS_SERVER_PORT`, then
    /// `EPICS_CA_SERVER_PORT`, then 5064 (C `caservertask.c:492-500`).
    /// An explicit `--port 0` binds an ephemeral port.
    #[arg(long)]
    port: Option<u16>,

    /// Do not start an interactive shell. C `-S`
    /// (`softMain.cpp:202-203`); C's default is interactive
    /// (`interactive = true`, `:137`), and so is this binary's.
    #[arg(long = "no-shell", short = 'S')]
    no_shell: bool,

    /// Accepted and ignored, exactly as C's `-s` is
    /// (`softMain.cpp:204-205`, "Previously caused a shell to be started").
    #[arg(short = 's', hide = true)]
    historical_shell: bool,

    /// Display the steps taken during startup. C `-v`
    /// (`softMain.cpp:206-207`, `verbose_out`).
    #[arg(long = "verbose", short = 'v')]
    verbose: bool,

    /// EXPERIMENTAL Rust-only TLS: server certificate chain (PEM).
    /// Both --tls-cert and --tls-key are required to enable TLS.
    /// Falls back to EPICS_CAS_TLS_CERT_FILE env var if not set.
    /// Setting these makes the IOC unreachable from C tools.
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

    /// Startup script run BEFORE iocInit, as C softIoc runs its
    /// positional `st.cmd` argument (`softMain.cpp:222-241`: the script
    /// executes first, and `iocInit()` follows it). The script owns the
    /// database load — `dbLoadDatabase`, `dbLoadRecords` and every other
    /// command that C refuses once `iocInit` has run are only reachable
    /// from here, which is what C's own usage text means by "to perform
    /// iocsh commands before iocInit, all database loading must be
    /// performed by the script itself".
    #[arg(value_name = "st.cmd")]
    startup_script: Option<String>,

    /// The `-D`/`-m`/`-d`/`-a`/`-x` sequence in the order argv spelled it,
    /// recovered by [`Args::from_argv`]. Skipped by clap because the
    /// per-flag `Vec`s above have already lost the interleaving and only
    /// [`clap::ArgMatches::indices_of`] still knows it.
    #[arg(skip)]
    steps: Vec<Step>,
}

/// One turn of C's getopt loop, in the order argv spelled it.
///
/// These five options are ORDER-DEPENDENT, and a per-flag list cannot say
/// so. C keeps one `macros` string that `-m` ASSIGNS (`softMain.cpp:200`)
/// and that the following `-d`/`-a` read at the instant they run, so
/// `-m A=1 -d x.db -m B=2 -d y.db` loads `x.db` under `A=1` and `y.db`
/// under `B=2` alone — never both. Collapsing the `-m`s into one joined
/// string, which is what a `Vec<String>` invites, gives every file every
/// macro and silently changes what the records are named.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// `-m` — replaces the macro string for every later step.
    Macros(String),
    /// `-d`
    Db(String),
    /// `-a`
    Acf(String),
    /// `-x`
    ExitDb(String),
    /// `-D`
    Dbd(String),
}

/// Rebuild C's argv order from clap's per-flag lists.
///
/// `indices_of` numbers each VALUE by its position in argv, so zipping the
/// indices with the values and sorting the union restores the sequence
/// getopt would have walked.
fn steps_in_argv_order(matches: &clap::ArgMatches) -> Vec<Step> {
    let mut ordered: Vec<(usize, Step)> = Vec::new();
    for (id, make) in [
        ("macros", Step::Macros as fn(String) -> Step),
        ("db_files", Step::Db as fn(String) -> Step),
        ("acf", Step::Acf as fn(String) -> Step),
        ("exit_prefix", Step::ExitDb as fn(String) -> Step),
        ("dbd", Step::Dbd as fn(String) -> Step),
    ] {
        let (Some(indices), Some(values)) =
            (matches.indices_of(id), matches.get_many::<String>(id))
        else {
            continue;
        };
        for (index, value) in indices.zip(values) {
            ordered.push((index, make(value.clone())));
        }
    }
    ordered.sort_by_key(|(index, _)| *index);
    ordered.into_iter().map(|(_, step)| step).collect()
}

impl Args {
    /// [`clap::Parser::parse_from`] plus the argv order clap discards.
    ///
    /// The one thing overridden on the way in is what counts as an option's
    /// VALUE. getopt takes the next argv element whatever it looks like, so
    /// `softIoc -d -h` loads a file called `-h` rather than printing usage,
    /// and [`copt::getopt_cut`] walks argv that way; clap's default is to
    /// refuse a value that opens with `-`, which had the two disagree about
    /// where the options even were. [`copt::takes_a_value`] decides it once
    /// for both.
    fn from_argv<I, T>(argv: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let matches = Self::command()
            .mut_args(|a| {
                // The rule is getopt's `:`, so it is about an OPTION's
                // argument. A positional is a token getopt permuted past
                // because it did not open with `-`, and letting one swallow
                // `-q` would eat the refusal.
                let hyphens_are_text = !a.is_positional() && copt::takes_a_value(&a);
                a.allow_hyphen_values(hyphens_are_text)
            })
            .try_get_matches_from(argv)?;
        let mut args = Self::from_arg_matches(&matches)?;
        args.steps = steps_in_argv_order(&matches);
        Ok(args)
    }
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

/// C's `-d` as the iocsh line it already is: `softMain.cpp:194-197` prints
/// exactly this text under `-v`, and calls `dbLoadRecords(optarg, macros)`.
///
/// `-m`'s string is handed through as ONE argument because C never parses
/// it, and is omitted when empty for the same reason C's own spelling
/// omits it.
fn db_load_records_line(file: &str, macros: &str) -> String {
    if macros.is_empty() {
        format!("dbLoadRecords(\"{}\")", iocsh_quote(file))
    } else {
        format!(
            "dbLoadRecords(\"{}\", \"{}\")",
            iocsh_quote(file),
            iocsh_quote(macros)
        )
    }
}

/// C `verbose_out` (`softMain.cpp:56-60`): the step, painted, on stdout.
///
/// C emits the escapes unconditionally; this port routes every painted
/// line through the shell's own gate so `NO_COLOR` silences the binary as
/// a whole rather than half of it.
fn verbose_out(escape: &str, message: &str) {
    if use_ansi_color() {
        println!("{escape}{message}{ANSI_ESC_RESET}");
    } else {
        println!("{message}");
    }
}

/// C's getopt loop replayed as the startup this binary can run.
///
/// C interprets each option the moment it reads it; here the same options
/// become iocsh lines the startup shell runs in the same order, because
/// `-D`, `-d` and `-a` ARE iocsh commands (`dbLoadDatabase`,
/// `dbLoadRecords`, `asSetFilename`/`asSetSubstitutions`) and the shell is
/// the port's one route into the database.
#[derive(Debug, Default, PartialEq, Eq)]
struct StartupPlan {
    /// Command lines for the startup shell, in argv order.
    lines: Vec<PlannedCommand>,
    /// Whether any `-x` put a `dbLoadRecords` of the exit database into
    /// [`Self::lines`], which is what makes the file worth materialising.
    ///
    /// It says nothing about the `exit` subroutine: C registers that in
    /// `lazy_dbd` (`softMain.cpp:127`), which runs whatever the flags were.
    loads_exit_db: bool,
    /// C's `loadedDb` (`softMain.cpp:139`), which decides both whether
    /// `iocInit` runs and whether a non-interactive IOC has anything to do.
    loaded_db: bool,
}

/// An argv-derived command with C's own `errIf` message attached.
///
/// C guards every one of these calls with `errIf(ret, msg)` and the MESSAGE
/// is not decoration: the catch block prints it only when it is non-empty
/// (`softMain.cpp:274-276`), so `-d` — guarded with `""` — exits 2 saying
/// nothing beyond what `dbLoadRecords` already wrote, while a `.dbd` that
/// will not read adds `Failed to load DBD file: <f>` on top of the loader's
/// own two lines. Carrying the message with the line is what keeps that
/// distinction out of a match on the command's spelling.
#[derive(Debug, PartialEq, Eq)]
struct PlannedCommand {
    line: String,
    on_error: String,
}

/// Walk [`Args::steps`] the way C walks argv.
fn startup_plan(steps: &[Step], exit_db: &str) -> Result<StartupPlan, Failure> {
    let mut plan = StartupPlan::default();
    // C's `macros`, `dbd_file` and `lazy_dbd_loaded` (`:133-140`, `:115`).
    let mut macros = String::new();
    let mut dbd: Option<&str> = None;
    let mut dbd_loaded = false;

    // C `lazy_dbd` (`softMain.cpp:117-127`): the `.dbd` is read once, on
    // first demand, and reading it is what makes a later `-D` too late.
    // This port compiles its record types in, so there is nothing to read
    // unless argv named a file — but the ONE-SHOT and its ordering are C's
    // and are kept, or `-D` after `-d` would silently be accepted.
    fn lazy_dbd(plan: &mut StartupPlan, dbd: Option<&str>, loaded: &mut bool) {
        if *loaded {
            return;
        }
        *loaded = true;
        if let Some(file) = dbd {
            plan.lines.push(PlannedCommand {
                line: format!("dbLoadDatabase(\"{}\")", iocsh_quote(file)),
                on_error: format!("Failed to load DBD file: {file}"),
            });
        }
    }

    for step in steps {
        match step {
            Step::Macros(m) => macros = m.clone(),
            Step::Dbd(file) => {
                if dbd_loaded {
                    return Err(Failure::DbdTooLate);
                }
                dbd = Some(file);
            }
            Step::Db(file) => {
                lazy_dbd(&mut plan, dbd, &mut dbd_loaded);
                plan.lines.push(PlannedCommand {
                    line: db_load_records_line(file, &macros),
                    on_error: String::new(),
                });
                plan.loaded_db = true;
            }
            Step::ExitDb(prefix) => {
                // C `softMain.cpp:212-219`: `-x` is `dbLoadRecords` of the
                // installed `softIocExit.db` under `IOC=<prefix>`, not a
                // second way of creating records. Spelling it as the same
                // command is what gives `<prefix>:BaseVersion` its `DISP`,
                // `PINI` and `DTYP` — a hand-built pair carried none of
                // them, because those live on the database INSTANCE and
                // only the loader fills it.
                lazy_dbd(&mut plan, dbd, &mut dbd_loaded);
                plan.lines.push(PlannedCommand {
                    line: db_load_records_line(exit_db, &format!("IOC={prefix}")),
                    on_error: String::new(),
                });
                plan.loads_exit_db = true;
                plan.loaded_db = true;
            }
            Step::Acf(file) => {
                // C `softMain.cpp:175-184`, in this order and with the
                // macros as they stand HERE: a later `-m` cannot reach an
                // ACF whose substitutions were already recorded.
                if !macros.is_empty() {
                    plan.lines.push(PlannedCommand {
                        line: format!("asSetSubstitutions(\"{}\")", iocsh_quote(&macros)),
                        on_error: String::new(),
                    });
                }
                plan.lines.push(PlannedCommand {
                    line: format!("asSetFilename(\"{}\")", iocsh_quote(file)),
                    on_error: String::new(),
                });
                // No `asInit` line follows, as none follows in C: `-a` is
                // `asSetFilename` plus `asSetSubstitutions` and nothing more
                // (`softMain.cpp:174-185`), and `iocInit` is what reads the
                // file (`iocInit.c:187`). Queuing one here ran the load
                // BEFORE the startup script.
            }
        }
    }
    lazy_dbd(&mut plan, dbd, &mut dbd_loaded);

    Ok(plan)
}

/// C's `softIocExit.db`
/// (`modules/database/src/std/softIoc/softIocExit.db`), verbatim.
///
/// Kept as the FILE C loads rather than re-expressed as records: `DESC`,
/// `DISP`, `PINI` and `DTYP` live on the database instance, which only the
/// `.db` loader fills, so a hand-built pair silently dropped every one of
/// them and left `<prefix>:BaseVersion` writable where C's refuses.
const SOFT_IOC_EXIT_DB: &str = r#"# softIocExit.db

record(sub,"$(IOC):exit") {
    field(DESC,"Exit subroutine")
    field(SCAN,"Passive")
    field(SNAM,"exit")
}

record(stringin,"$(IOC):BaseVersion") {
    field(DESC,"EPICS Base Version")
    field(DTYP,"getenv")
    field(INP,"@EPICS_VERSION_FULL")
    field(PINI,"YES")
    field(DISP,1)
}
"#;

/// C's `exit_file` (`softMain.cpp:136`, `:158`) — the path `-x` hands to
/// `dbLoadRecords`.
///
/// C resolves it against its install tree, from `epicsGetExecDir`. This
/// binary has no install tree, so the file is written where the loader can
/// open it and taken away again on the way out. The pid is in the name
/// because two softIocs may run side by side, and each owns only its own.
struct ExitDatabaseFile(std::path::PathBuf);

impl ExitDatabaseFile {
    fn path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("softioc-rs-{}-softIocExit.db", std::process::id()))
    }

    fn write(path: &std::path::Path) -> Result<Self, Failure> {
        std::fs::write(path, SOFT_IOC_EXIT_DB).map_err(|e| {
            Failure::CommandLine(format!("Failed to write {}: {e}", path.display()))
        })?;
        // One finalizer per exit path, because the two do not overlap: a
        // `caput <prefix>:exit 0` leaves through `epicsExit`, which unwinds
        // nothing and so never runs `Drop`, while every other way out of
        // `main` returns and never runs the at-exit list.
        let owned = path.to_path_buf();
        epics_base_rs::runtime::exit::at_exit("softIoc-rs exit database", move || {
            let _ = std::fs::remove_file(&owned);
        });
        Ok(Self(path.to_path_buf()))
    }
}

impl Drop for ExitDatabaseFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// C `registryFunctionAdd("exit", exitSubroutine)` (`softMain.cpp:127`),
/// which is what makes `SNAM "exit"` resolve — in the exit database `-x`
/// loads, and equally in any `.db` of the user's own.
fn register_exit_subroutine(
    app: epics_base_rs::server::ioc_app::IocApplication,
) -> epics_base_rs::server::ioc_app::IocApplication {
    // C `exitSubroutine` (`softMain.cpp:62-64`): field A decides the
    // status, and the exit is deferred to another thread so the put that
    // triggered it can still be answered — `epicsExitLater`, whose whole
    // point is that it returns.
    app.register_subroutine(
        "exit",
        |record: &mut dyn epics_base_rs::server::record::Record| {
            let a = match record.get_field("A") {
                Some(EpicsValue::Double(v)) => v,
                _ => 0.0,
            };
            let status = if a == 0.0 { 0 } else { 1 };
            std::thread::spawn(move || epics_base_rs::runtime::exit::exit(status));
            Ok(0)
        },
    )
}

/// C `usage` (`softMain.cpp:66-106`), verbatim through the last paragraph.
///
/// Verbatim because it is the answer to `-h`, to an unrecognised option and
/// to `Nothing to do!`, and on all three C writes it to STDOUT — a script
/// that greps it for `-x` must not have to know which binary it ran. clap's
/// listing used to stand in for it, so every one of those paths differed
/// from C in its whole body while agreeing on the exit status.
///
/// The trailer is the one part that cannot be verbatim: C names the
/// `softIoc.dbd` its build installed, resolved against `epicsGetExecDir`,
/// and there is no such file here — the record types are linked in. It says
/// so rather than naming a path that does not exist.
fn usage() -> String {
    let arg0 = std::env::args()
        .next()
        .unwrap_or_else(|| "softioc-rs".into());
    format!(
        "Usage: {arg0} [-D softIoc.dbd] [-h] [-S] [-s] [-v] [-a ascf]
[-m macro=value,macro2=value2] [-d file.db]
[-x prefix] [st.cmd]

    -D <dbd>  If used, must come first. Specify the path to the softIoc.dbdfile.        The compile-time install location is saved in the binary as a default.

    -h  Print this message and exit.

    -S  Prevents an interactive shell being started.

    -s  Previously caused a shell to be started.  Now accepted and ignored.

    -v  Verbose, display steps taken during startup.

    -a <acf>  Access Security configuration file.  Macro substitution is
        performed.

    -m <MAC>=<value>,... Set/replace macro definitions used by subsequent -d and
        -a.

    -d <db>  Load records from file (dbLoadRecords).  Macro substitution is
        performed.

    -x <prefix>  Load softIocExit.db.  Provides a record \"<prefix>:exit\".
        Put 0 to exit with success, or non-zero to exit with an error.

Any number of -m and -d arguments can be interspersed; the macros are applied
to the following .db files.  Each later -m option causes earlier macros to be
discarded.

A st.cmd file is optional.  If any databases were loaded the st.cmd file will
be run *after* iocInit.  To perform iocsh commands before iocInit, all database
loading must be performed by the script itself, or by the user from the
interactive IOC shell.

There is no compiled-in path to softIoc.dbd: the record types are linked into
this binary, and -D reads a .dbd on top of them.

--help lists the options this binary adds to C softIoc's.
"
    )
}

/// C's answer to an option its getopt loop would not take
/// (`softMain.cpp:169-173`): getopt's own line on stderr, the usage block on
/// stdout, then `Unknown argument: -?` — `?` literally, because `opt` holds
/// getopt's return rather than the offending letter.
///
/// clap's error text replaces getopt's only where C's would be wrong: C has
/// no long options, so a mistyped `--pv` cannot be reported as C reports
/// `'-'`.
fn report_argv_error(e: clap::Error) -> std::process::ExitCode {
    use clap::error::ErrorKind;
    if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
        e.exit();
    }
    let arg0 = std::env::args()
        .next()
        .unwrap_or_else(|| "softioc-rs".into());
    let what = if e.kind() == ErrorKind::UnknownArgument {
        "invalid option"
    } else {
        "option requires an argument"
    };
    eprintln!("{arg0}: {what} -- '{}'", blamed_option(&e));
    print!("{}", usage());
    eprintln!("Unknown argument: -?");
    std::process::ExitCode::from(2)
}

/// How C's getopt names the option it refused.
///
/// clap names a value option in its long form (`--db <file.db>`) whatever the
/// user typed; C names the letter `optopt` held. Resolving it back through the
/// spec is what makes `-d` with no value blame `d` rather than `--db`. An
/// option the spec does not have has no letter to resolve, so a short one is
/// unwrapped to its letter and a long one — which C has none of — is echoed as
/// typed, saying more than C's `'-'`.
fn blamed_option(e: &clap::Error) -> String {
    let blamed = e
        .get(clap::error::ContextKind::InvalidArg)
        .map(|v| v.to_string())
        .unwrap_or_default();
    let token = blamed.split_whitespace().next().unwrap_or(&blamed);
    let long = token.trim_start_matches('-');
    Args::command()
        .get_arguments()
        .find(|a| a.get_long() == Some(long) || a.get_id().as_str() == long)
        .and_then(clap::Arg::get_short)
        .map_or_else(
            || match token.strip_prefix('-') {
                Some(letter) if letter.chars().count() == 1 => letter.to_string(),
                _ => token.to_string(),
            },
            |c| c.to_string(),
        )
}

/// Escape what the iocsh tokenizer would otherwise read as structure
/// (`iocsh.cpp:362-371`, and the port's own `registry` splitter, both of
/// which unescape `\\` and `\"` inside a quoted argument).
///
/// In C a filename and a substitution string are function arguments and
/// never text; here they travel as text for the length of one line, so
/// they have to survive the trip unchanged.
fn iocsh_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(tokio_backend)]
/// The IOC, however argv described it: command lines first, then the
/// optional `st.cmd`, then `iocInit`, then the protocol server.
///
/// One route, because C has one. `-d` in C is literally
/// `dbLoadRecords(optarg, macros)` (`softMain.cpp:192-198`) — the same
/// function the script's own `dbLoadRecords` line reaches — so the file it
/// cannot open is reported by the loader, twice and in the loader's words:
/// `Can't open file '<f>'` from `dbLexRoutines.c:284-285` and then
/// `Failed to load '<f>'` from `dbAccess.c:808`. A second loader reachable
/// only from argv said neither, and printed its own Rust `io::Error`
/// instead; there is no second loader now.
///
/// `--pv`/`--record` are inline sources the application adds before the
/// first command line, and the server-side options reach the builder
/// through [`ServerOptions::apply`].
async fn run_ioc_app(args: Args) -> Result<(), Failure> {
    // Argv's own correctness, decided before the boot as C's getopt loop
    // decides it — inside the `try` that exits 2, not in the serving phase
    // this route's builder is constructed from.
    let server_options = ServerOptions::from_args(&args).map_err(Failure::Startup)?;

    let mut app = epics_base_rs::server::ioc_app::IocApplication::new();
    // Before the script, not with the server: this binary links RSRV, so C
    // runs `rsrvRegistrar` out of `dbLoadDatabase`'s `.dbd` expansion, and
    // both halves of it — the `registrar(rsrvRegistrar)` line
    // `dbDumpRegistrar` reports and the `casr` command `iocshRegister` adds —
    // are in place from the first line the script runs (measured on R7.0.10,
    // whose list is the same ten entries in stdin and startup-script mode).
    // The CA server itself is `run_with_shell`'s, which `IocApplication`
    // reaches only after the script is done, so `casr` answers for the absent
    // server the way C's does: it prints nothing.
    //
    // The argv check above cannot observe either half — it exits 2 without
    // running a script or opening a shell — so declaring them here rather
    // than at the function head costs nothing and keeps one call site.
    app = epics_ca_rs::server::iocsh::register_rsrv_commands(app);
    if let Some(port) = args.port {
        app = app.port(port);
    }
    for pv_def in &args.pvs {
        let (name, value) = parse_pv_def(pv_def).map_err(Failure::Startup)?;
        app = app.pv(&name, value);
    }
    for rec_def in &args.records {
        let (name, record) = parse_record_def(rec_def).map_err(Failure::Startup)?;
        app = app.record_boxed(&name, record);
    }
    // C `softMain.cpp:163-222`: `-D`/`-d`/`-a`/`-x` ARE iocsh commands, run
    // before the script and in argv order, with `-m`'s string handed
    // straight through. This route spells them the same way rather than
    // reaching a second loader that only the command line can call.
    let exit_db = ExitDatabaseFile::path();
    let plan = startup_plan(&args.steps, &exit_db.to_string_lossy())?;
    // C's `loadedDb` (`softMain.cpp:139`), computed ONCE. It decides two
    // separate things — whether `iocInit()` runs (`:239`) and whether a
    // non-interactive IOC has anything to do (`:262`) — and this port
    // derived it twice, from the plan here and from argv in the caller, so
    // one spelling of C's single variable could drift from the other.
    //
    // `--pv`/`--record` count with it: an inline source is a record in the
    // database by the time the shell opens, exactly as a `-d` file is.
    let loaded_db = plan.loaded_db || !args.pvs.is_empty() || !args.records.is_empty();
    // C `softMain.cpp:262-271` refuses only a NON-interactive IOC that
    // loaded nothing and ran no script; an interactive one always has
    // something to do, because the shell it is about to start is the thing
    // to do, and a script on its own is enough as well (`ranScript`, `:236`).
    if args.no_shell && !loaded_db && args.startup_script.is_none() {
        return Err(Failure::NothingToDo);
    }
    // Held for the life of the IOC: the loader opens it during the startup
    // phase, and removing it any earlier would be a race with that read.
    let _exit_db = if plan.loads_exit_db {
        Some(ExitDatabaseFile::write(&exit_db)?)
    } else {
        None
    };
    for command in &plan.lines {
        if args.verbose {
            verbose_out(ANSI_ESC_BOLD, &command.line);
        }
        app = app.startup_line(&command.line);
    }
    // Unconditional, as C's is: `registryFunctionAdd("exit", …)` sits inside
    // `lazy_dbd` (`softMain.cpp:127`), which `main` calls at `:224` no matter
    // what the flags were. So `SNAM "exit"` resolves in a user's own `.db`
    // without `-x` — measured against `softIoc @R7.0.10`, which exits 1 on a
    // PINI'd `record(sub, …) { field(SNAM, "exit") field(A, "1") }` loaded
    // with `-d` alone. Only the exit *database* is `-x`'s; the subroutine is
    // the binary's.
    app = register_exit_subroutine(app);

    if let Some(ref script) = args.startup_script {
        // C `softMain.cpp:230-234` brackets the script in blue remarks. The
        // closing `# End` is written from the `before_ioc_init` gate below,
        // which is the turn between the script and `iocInit` that this
        // binary did not used to have.
        if args.verbose {
            verbose_out(ANSI_ESC_BLUE, &format!("# Begin {script}"));
        }
        app = app.startup_script(script);
    }

    // C `softMain.cpp:137`: interactive by default, and `-S` is the only
    // thing that turns it off (`:202-203`).
    let interactive = !args.no_shell;
    // C `softMain.cpp:239`: `iocInit()` runs only when argv loaded a
    // database. Everything a running IOC has — the scan threads, PINI, and
    // RSRV, which starts inside `iocRun` — hangs off that call, so a
    // `softIoc` with no `-d`/`-x` reaches its prompt having built nothing
    // and having opened no port. This binary booted regardless, which meant
    // a bare `softioc-rs` bound 5064 and served an empty database.
    let verbose = args.verbose;
    let script = args.startup_script.clone();
    app = app.before_ioc_init(move || {
        // C `softMain.cpp:233`, the closing half of the pair opened above.
        // It could not be written at the opening's call site: the script
        // had not run yet there, and by the time `run_phased` returned the
        // serving phase was already over.
        if verbose && let Some(script) = script {
            verbose_out(ANSI_ESC_BLUE, &format!("# End {script}"));
        }
        if loaded_db {
            // C `softMain.cpp:240` — inside the gate, so the line is
            // written only on the arm that actually calls `iocInit()`.
            if verbose {
                verbose_out(ANSI_ESC_BOLD, "iocInit()");
            }
            IocInitDecision::run(interactive)
        } else {
            IocInitDecision::skip(interactive)
        }
    });
    app.run_phased(
        move |config: epics_base_rs::server::ioc_app::IocRunConfig| async move {
            let mut builder = epics_ca_rs::server::CaServerBuilder::serving(
                config.db,
                config.acf,
                config.autosave_config,
                config.autosave_manager,
            )
            .port(config.port);
            if let Some(tcp_port) = config.tcp_port {
                builder = builder.tcp_port(tcp_port);
            }
            let server = server_options.apply(builder).build().await?;
            if interactive {
                server
                    .run_with_shell(move |shell| {
                        for cmd in config.shell_commands {
                            shell.register(cmd);
                        }
                    })
                    .await
            } else {
                server.run().await
            }
        },
    )
    .await
    .map_err(|failure| match failure {
        // C's `try` block covers the script and `iocInit`; its catch
        // exits 2. The protocol runner is past that block, where C's only
        // failure is `iocsh(NULL)` returning non-zero and exiting 1.
        IocRunFailure::StartupScript { path, .. } => Failure::Script(path),
        // C's catch prints `errIf`'s message and only when it is
        // non-empty, so the command carries its own — `""` for
        // `dbLoadRecords`, which has already reported itself in full.
        IocRunFailure::StartupCommand { line, .. } => Failure::CommandLine(
            plan.lines
                .iter()
                .find(|command| command.line == line)
                .map(|command| command.on_error.clone())
                .unwrap_or_default(),
        ),
        IocRunFailure::Startup(e) => Failure::Startup(e),
        IocRunFailure::Serving(e) => Failure::Serving(e),
    })
}

/// How C softIoc leaves the process when it will not run.
///
/// C has three exits and this is all of them: the catch block's
/// `epicsExit(2)` for anything thrown during setup (`softMain.cpp:274-278`),
/// `epicsExit(1)` when nothing was loaded and no script ran (`:268-271`),
/// and `epicsExit(1)` when the interactive `iocsh(NULL)` returns non-zero
/// (`:253-256`). Modelling them as one type is what keeps the status off
/// the failing sites: `main` returning `CaResult` handed every one of them
/// to Rust's `Termination`, which prints the error's `Debug` form and exits
/// 1 — so `-d bad.db` reported `DbLoadFailed("bad.db")` and status 1 where
/// C reports `Failed to load 'bad.db'` and status 2.
#[derive(Debug)]
enum Failure {
    /// C's catch block. `ERL_ERROR` is `ANSI_RED("ERROR")` (`errlog.h:305`),
    /// emitted whether or not stderr is a terminal, as C's is.
    Startup(epics_base_rs::error::CaError),
    /// C `softMain.cpp:231`: `errIf(iocsh(path), "Error in " + path)`, whose
    /// catch prints that sentence and exits 2. The path is the whole message
    /// — the shell has already said on its own line *why* the script failed,
    /// so carrying its reason here would print the failure twice.
    Script(String),
    /// C `softMain.cpp:262-267`: usage to stdout, `Nothing to do!` to
    /// stderr, exit 1.
    ///
    /// The usage block used to be withheld because this binary exited 2 on
    /// `-D`, `-S`, `-s`, `-v`, `-a` and `-x`, so printing C's text would
    /// have advertised options it rejected. It accepts all of them now, and
    /// the block is printed — clap's, not C's, because clap's lists the
    /// long options this binary also has and cannot fall out of step with
    /// what it parses.
    NothingToDo,
    /// C `softMain.cpp:180-182`: a `-D` that arrives after the database has
    /// been read is a `runtime_error`, so the catch block prints it and
    /// exits 2. The trailing newline is C's own — its message ends in one
    /// and the catch adds another.
    DbdTooLate,
    /// A pre-script command line from argv returned non-zero, carrying
    /// C's `errIf` message for that call ([`PlannedCommand::on_error`]).
    /// An EMPTY message is the point rather than an omission: C's catch
    /// prints nothing for it (`softMain.cpp:275`), so `-d` exits 2 with
    /// nothing beyond what `dbLoadRecords` itself already wrote.
    CommandLine(String),
    /// C `softMain.cpp:253-256`, which exits 1 silently. The reason is
    /// printed here because a server that stopped serving is worth saying
    /// out loud; the status is C's.
    Serving(epics_base_rs::error::CaError),
}

impl Failure {
    /// C's exit status for this failure. Separate from [`Self::report`]
    /// because the status is the assertable half and printing is not.
    fn status(&self) -> u8 {
        match self {
            Failure::Startup(_)
            | Failure::Script(_)
            | Failure::CommandLine(_)
            | Failure::DbdTooLate => 2,
            Failure::NothingToDo | Failure::Serving(_) => 1,
        }
    }

    /// C's line for this failure, split off [`Self::report`] for the same
    /// reason as [`Self::status`]: the bytes are the assertable half.
    /// `None` is C's empty `errIf` message — a failure whose whole report
    /// the failing command already wrote.
    fn rendered(&self) -> Option<String> {
        match self {
            Failure::Startup(e) | Failure::Serving(e) => Some(format!("{ERL_ERROR}: {e}")),
            Failure::Script(path) => Some(format!("{ERL_ERROR}: Error in {path}")),
            Failure::NothingToDo => Some("Nothing to do!".into()),
            Failure::DbdTooLate => Some(format!(
                "{ERL_ERROR}: -D specified too late, softIoc.dbd already loaded.\n"
            )),
            // C `softMain.cpp:275`: `if (e.what()[0] != '\0')`.
            Failure::CommandLine(message) if message.is_empty() => None,
            Failure::CommandLine(message) => Some(format!("{ERL_ERROR}: {message}")),
        }
    }

    fn report(self) -> std::process::ExitCode {
        // C `softMain.cpp:268-269` prints the usage block to STDOUT first,
        // and only then the refusal on stderr.
        if matches!(self, Failure::NothingToDo) {
            print!("{}", usage());
        }
        if let Some(line) = self.rendered() {
            eprintln!("{line}");
        }
        std::process::ExitCode::from(self.status())
    }
}

// One worker, reactor-style: the default per-CPU pool migrates the
// mostly-serial serving work across idle workers, costing ~35 µs of
// extra CPU per put on a 96-core host. Multi-thread flavor is kept so
// `block_on_sync` works from runtime tasks.
#[cfg(tokio_backend)]
#[tokio::main(worker_threads = 1)]
async fn main() -> std::process::ExitCode {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // C's `case 'h'` (`softMain.cpp:165-168`) is the one arm that returns
    // from the loop before clap could have judged the rest of the line.
    let cut = copt::getopt_cut(&Args::command(), &argv, &['h']).unwrap_or_else(|| argv.clone());
    let args = match Args::from_argv(cut) {
        Ok(args) => args,
        Err(e) => return report_argv_error(e),
    };
    // C `softMain.cpp:165-168`, which prints and leaves before it looks at
    // anything else it was given.
    if args.c_help {
        print!("{}", usage());
        return std::process::ExitCode::SUCCESS;
    }
    run_ioc_app(args)
        .await
        .map_or_else(Failure::report, |()| std::process::ExitCode::SUCCESS)
}

/// The SERVER-side half of the CLI: everything argv says about the server
/// rather than about its database.
///
/// A struct rather than a function on the builder because the two things
/// happen at different instants. Whether argv is WELL-FORMED is decided
/// while argv is being read — C's getopt loop, inside the `try` that exits
/// 2 — but the builder it configures cannot exist until the database does,
/// which on the startup-script route is after the script has run. Fusing
/// them made a mistyped `--dns-update-server` a SERVING failure (exit 1)
/// on one route and a setup failure (exit 2) on the other.
///
/// The same values reach the builder on both routes: a `--tls-cert` means
/// the same thing to an IOC whose records came from `--db` and to one whose
/// records came from an `st.cmd`. It used to be refused by name on the
/// second only because `CaServer::from_parts` was a second constructor that
/// never mentioned it.
///
/// `tokio_backend` with the front-end it configures: `apply` hands back a
/// `CaServerBuilder`, and `dns_update` names `epics_ca_rs::discovery`, both of
/// which are that backend's.
#[cfg(tokio_backend)]
struct ServerOptions {
    /// `None` leaves the builder's own EPICS-environment seeding alone.
    port: Option<u16>,
    #[cfg(feature = "experimental-rust-tls")]
    tls: Option<epics_ca_rs::tls::TlsConfig>,
    mdns: Option<String>,
    mdns_txt: Vec<(String, String)>,
    #[cfg(feature = "discovery-dns-update")]
    dns_update: Option<epics_ca_rs::discovery::DnsRegistration>,
}

#[cfg(tokio_backend)]
impl ServerOptions {
    /// Every way argv can be wrong about the server, decided here.
    fn from_args(args: &Args) -> CaResult<Self> {
        // CLI-supplied TLS overrides any EPICS_CAS_TLS_* env vars; if both
        // CLI flags are absent the server picks them up from the env at
        // run() time. Mismatched (only --tls-cert OR only --tls-key) is a
        // hard error.
        #[cfg(feature = "experimental-rust-tls")]
        let tls = match (&args.tls_cert, &args.tls_key) {
            (Some(cert_path), Some(key_path)) => {
                let chain = epics_ca_rs::tls::load_certs(cert_path)?;
                let key = epics_ca_rs::tls::load_private_key(key_path)?;
                Some(if let Some(ref ca_path) = args.tls_client_ca {
                    let roots = epics_ca_rs::tls::load_root_store(ca_path)?;
                    epics_ca_rs::tls::TlsConfig::server_mtls_from_pem(chain, key, roots).map_err(
                        |e| epics_base_rs::error::CaError::InvalidValue(format!("TLS: {e}")),
                    )?
                } else {
                    epics_ca_rs::tls::TlsConfig::server_from_pem(chain, key).map_err(|e| {
                        epics_base_rs::error::CaError::InvalidValue(format!("TLS: {e}"))
                    })?
                })
            }
            (None, None) => None, // env-based or plaintext
            _ => {
                return Err(epics_base_rs::error::CaError::InvalidValue(
                    "--tls-cert and --tls-key must both be set or both unset".into(),
                ));
            }
        };
        #[cfg(not(feature = "experimental-rust-tls"))]
        if args.tls_cert.is_some() || args.tls_key.is_some() || args.tls_client_ca.is_some() {
            return Err(epics_base_rs::error::CaError::InvalidValue(
                "TLS flags require building with --features experimental-rust-tls".into(),
            ));
        }

        // mDNS announce. The discovery feature is required to actually
        // emit packets; without it we keep the field for diagnostics and
        // the server logs a warning at startup.
        let mut mdns_txt = Vec::new();
        if args.mdns.is_some() {
            for kv in &args.mdns_txt {
                if let Some((k, v)) = kv.split_once('=') {
                    mdns_txt.push((k.to_string(), v.to_string()));
                } else {
                    eprintln!("warning: --mdns-txt expects KEY=VALUE, got {kv:?}; skipping");
                }
            }
        }

        // RFC 2136 Dynamic DNS UPDATE registration.
        #[cfg(feature = "discovery-dns-update")]
        let dns_update = {
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
                        epics_base_rs::error::CaError::InvalidValue(format!(
                            "--dns-update-server: {e}"
                        ))
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
                Some(epics_ca_rs::discovery::DnsRegistration {
                    server,
                    zone: args.dns_update_zone.clone().unwrap(),
                    instance: args.dns_update_instance.clone().unwrap(),
                    host,
                    // Advertise the port the server will actually bind: an
                    // omitted --port defers to the EPICS environment, same
                    // as the builder's own seeding.
                    port: args
                        .port
                        .unwrap_or_else(epics_base_rs::runtime::net::ca_server_port),
                    txt: Vec::new(),
                    ttl: std::time::Duration::from_secs(args.dns_update_ttl),
                    keepalive: std::time::Duration::from_secs(args.dns_update_keepalive),
                    tsig,
                })
            } else {
                None
            }
        };
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

        Ok(Self {
            port: args.port,
            #[cfg(feature = "experimental-rust-tls")]
            tls,
            mdns: args.mdns.clone(),
            mdns_txt,
            #[cfg(feature = "discovery-dns-update")]
            dns_update,
        })
    }

    /// Infallible by construction: [`Self::from_args`] already rejected
    /// everything rejectable, so applying these to a builder cannot fail
    /// and cannot be mistaken for a failure of the phase it runs in.
    fn apply(
        self,
        mut builder: epics_ca_rs::server::CaServerBuilder,
    ) -> epics_ca_rs::server::CaServerBuilder {
        // The builder already seeds `port` from the EPICS environment; only an
        // explicit `--port` may override it.
        if let Some(port) = self.port {
            builder = builder.port(port);
        }
        #[cfg(feature = "experimental-rust-tls")]
        if let Some(tls) = self.tls {
            builder = builder.with_tls(tls);
        }
        if let Some(ref instance) = self.mdns {
            builder = builder.announce_mdns(instance);
            for (k, v) in &self.mdns_txt {
                builder = builder.announce_txt(k, v);
            }
        }
        #[cfg(feature = "discovery-dns-update")]
        if let Some(reg) = self.dns_update {
            builder = builder.register_dns_update(reg);
        }
        builder
    }
}

/// The `exec_backend` arm. `softioc-rs` serves through the async CA front-end,
/// which is `tokio_backend`-only, so on the reactor-free backend there is no
/// server to start — `realtime-ca-ioc` is the entry point that brings a CA IOC
/// up on this execution model, through the blocking thread-per-client driver.
/// Refusing here, rather than compiling the front-end and panicking inside a
/// background worker, makes a wrong build a startup message.
#[cfg(exec_backend)]
fn main() -> std::process::ExitCode {
    eprintln!(
        "softioc-rs: this build selects the reactor-free execution backend \
         (EPICS_RS_BUILD_EXEC_BACKEND=thread), and the async CA server front-end \
         needs a tokio reactor.\nRun `realtime-ca-ioc`, which serves CA through \
         the blocking driver on this backend, or unset that variable and rebuild \
         softioc-rs."
    );
    std::process::ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::{
        Args, CommandFactory, ERL_ERROR, Failure, SOFT_IOC_EXIT_DB, Step, blamed_option, copt,
        db_load_records_line, parse_record_def, startup_plan, usage,
    };
    #[cfg(tokio_backend)]
    use super::{ServerOptions, run_ioc_app};

    #[cfg(tokio_backend)]
    /// `-d`/`--db` on a file that is not there is a failing `dbLoadRecords`,
    /// which C guards with `errIf(..., "")` (`softMain.cpp:198`): the EMPTY
    /// message is what makes the catch block silent, so softIoc exits 2
    /// having printed only what the loader itself wrote —
    /// `ERROR: Can't open file '<f>'` (`dbLexRoutines.c:284-285`) and
    /// `ERROR: Failed to load '<f>'` (`dbAccess.c:808`), measured on
    /// R7.0.10 and byte-identical here.
    ///
    /// The classification is the assertion: `CommandLine` is the only
    /// failure that renders nothing, and reaching it proves the load went
    /// through the shell's `dbLoadRecords` rather than a second loader that
    /// argv alone could call and that reported a Rust `io::Error` instead.
    #[epics_macros_rs::epics_test]
    async fn a_missing_db_file_fails_as_the_loader_reported_it() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.db");
        let failed = run_ioc_app(
            Args::from_argv(["softioc-rs", "--db", missing.to_str().unwrap()]).unwrap(),
        )
        .await
        .expect_err("a database that is not there stops the IOC");
        assert!(
            matches!(failed, Failure::CommandLine(ref m) if m.is_empty()),
            "the loader owns the report, got {:?}",
            failed.rendered()
        );
        assert_eq!(
            failed.rendered(),
            None,
            "softioc adds no sentence of its own"
        );
        assert_eq!(failed.status(), 2);
    }

    #[cfg(tokio_backend)]
    /// C `softIoc` R7.0.10 prints the same ten `registrar()` lines whether it
    /// is reading stdin or running an `st.cmd`, because the list comes from
    /// the `.dbd` the binary linked and not from RSRV having started. This
    /// port announced `rsrvRegistrar` from `CaServer::run_with_shell`, which
    /// `IocApplication` reaches only once the script is finished, so a
    /// script's own `dbDumpRegistrar` was one line short.
    ///
    /// The script ends in a failing line under `on error break` so the
    /// startup phase is the only phase that runs — a successful script would
    /// go on to `iocInit` and bind a CA port.
    #[epics_macros_rs::epics_test]
    async fn a_startup_script_sees_the_ca_server_s_registrar() {
        let dir = tempfile::tempdir().unwrap();
        let dump = dir.path().join("registrars.txt");
        let script = dir.path().join("st.cmd");
        std::fs::write(
            &script,
            format!(
                "on error break\ndbDumpRegistrar pdbbase > {}\nnosuchcommand\n",
                dump.display()
            ),
        )
        .unwrap();
        let script = script.display().to_string();

        let failed = run_ioc_app(Args::from_argv(["softioc-rs", &script]).unwrap())
            .await
            .expect_err("`on error break` stops the IOC before it serves");
        assert!(
            matches!(failed, Failure::Script(_)),
            "the script phase failed"
        );

        let listed: Vec<String> = std::fs::read_to_string(&dump)
            .expect("dbDumpRegistrar wrote its redirect")
            .lines()
            .map(str::to_string)
            .collect();
        // C's ten minus `asSub`, which this workspace has no subroutine for
        // and deliberately does not declare.
        assert_eq!(
            listed,
            [
                "registrar(arrInitialize)",
                "registrar(dbndInitialize)",
                "registrar(decInitialize)",
                "registrar(dlloadRegistar)",
                "registrar(iocshSystemCommand)",
                "registrar(rsrvRegistrar)",
                "registrar(syncInitialize)",
                "registrar(tsInitialize)",
                "registrar(utagInitialize)",
            ]
        );
    }

    /// R19-22 boundary: an omitted `--port` must reach the builder as
    /// `None`, so `CaServerBuilder`'s env-seeded default
    /// (`EPICS_CAS_SERVER_PORT` > `EPICS_CA_SERVER_PORT` > 5064) survives.
    /// A clap literal here silently shadows the environment and pins the
    /// IOC to the production port.
    #[test]
    fn omitted_port_defers_to_the_epics_environment() {
        let args = Args::from_argv(["softioc-rs", "--db", "x.db"]).unwrap();
        assert_eq!(args.port, None);
    }

    /// The flag still wins over the environment, and `--port 0` is a
    /// distinct, representable request for an ephemeral bind — the two
    /// meanings the old `default_value_t = 5064` conflated.
    #[test]
    fn explicit_port_overrides_and_zero_means_ephemeral() {
        let args = Args::from_argv(["softioc-rs", "--db", "x.db", "--port", "15070"]).unwrap();
        assert_eq!(args.port, Some(15070));
        let args = Args::from_argv(["softioc-rs", "--db", "x.db", "--port", "0"]).unwrap();
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

    /// C's three exits, by status: everything thrown while `main` sets the
    /// IOC up is the catch block's 2 (`softMain.cpp:274-278`); nothing to do
    /// and a failed interactive shell are 1 (`:268-271`, `:253-256`).
    /// Measured on C softIoc R7.0.10: `-d bad.db` 2, `--nosuchflag` 2,
    /// `/nonexistent/st.cmd` 2, no arguments 1, `-h` 0.
    #[test]
    fn every_failure_carries_c_s_exit_status() {
        use epics_base_rs::error::CaError;
        assert_eq!(
            Failure::Startup(CaError::DbLoadFailed("bad.db".into())).status(),
            2
        );
        assert_eq!(
            Failure::Startup(CaError::InvalidValue("bad option".into())).status(),
            2
        );
        assert_eq!(Failure::NothingToDo.status(), 1);
        assert_eq!(Failure::Serving(CaError::Shutdown).status(), 1);
        assert_eq!(Failure::Script("/no/such/st.cmd".into()).status(), 2);
    }

    /// Byte-compared against `softIoc -S /no/such/st.cmd` at R7.0.10, whose
    /// second stderr line this is — the first belongs to iocsh, which has
    /// already said the file could not be opened. C builds the sentence in
    /// `errIf(iocsh(argv[optind]), std::string("Error in ")+argv[optind])`
    /// and prints it from the catch as `ERL_ERROR ": " << e.what()`.
    #[test]
    fn a_failed_script_renders_as_c_s_sentence() {
        assert_eq!(
            Failure::Script("/no/such/st.cmd".into()).rendered(),
            Some(format!("{ERL_ERROR}: Error in /no/such/st.cmd"))
        );
    }

    /// The load failure C reports as `Failed to load 'bad.db'` must reach
    /// stderr as a message, not as the error type's `Debug` form — which is
    /// what `main` returning `CaResult` produced (`DbLoadFailed("bad.db")`).
    #[test]
    fn the_load_failure_renders_as_c_s_sentence() {
        use epics_base_rs::error::CaError;
        assert_eq!(
            CaError::DbLoadFailed("bad.db".into()).to_string(),
            "Failed to load 'bad.db'"
        );
    }

    /// C takes the script as a bare positional argument
    /// (`softMain.cpp:67` usage: `[st.cmd]`), and it is the one input
    /// that on its own gives the IOC something to do.
    #[test]
    fn the_startup_script_is_a_positional_argument() {
        let args = Args::from_argv(["softioc", "st.cmd"]).unwrap();
        assert_eq!(args.startup_script.as_deref(), Some("st.cmd"));
        assert!(args.pvs.is_empty());
        assert!(args.db_files.is_empty());
    }

    /// C `softMain.cpp:194-197` writes the `-d` call as this exact text
    /// under `-v`, with `-m`'s string as ONE argument and omitted when
    /// empty.
    #[test]
    fn a_db_flag_is_c_s_db_load_records_call() {
        assert_eq!(db_load_records_line("x.db", ""), "dbLoadRecords(\"x.db\")");
        assert_eq!(
            db_load_records_line("x.db", "P=A,R=B"),
            "dbLoadRecords(\"x.db\", \"P=A,R=B\")"
        );
    }

    /// The line is text for exactly as long as it takes the shell to split
    /// it, so a filename C would have passed as a pointer must come out
    /// the other side unchanged.
    #[test]
    fn a_quote_in_a_filename_survives_the_line() {
        assert_eq!(
            db_load_records_line(r#"od"d\.db"#, ""),
            r#"dbLoadRecords("od\"d\\.db")"#
        );
    }

    #[cfg(tokio_backend)]
    /// C `softIoc -S -d good.db st.cmd` has the records in the database
    /// before the script's first line — measured on R7.0.10, where the
    /// script's own `dbl` lists them. This port refused the combination
    /// by name until `--db` became the `dbLoadRecords` call C's `-d`
    /// already is.
    #[epics_macros_rs::epics_test]
    async fn a_db_file_loads_before_the_startup_script() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("ab.db");
        std::fs::write(&db, "record(ai, \"AB:PRELOAD\") { field(VAL, \"3\") }\n")
            .expect("write db");
        let dump = dir.path().join("dbl.txt");
        let script = dir.path().join("st.cmd");
        std::fs::write(
            &script,
            format!("on error break\ndbl > {}\nnosuchcommand\n", dump.display()),
        )
        .expect("write script");

        let script = script.display().to_string();
        let failed = run_ioc_app(
            Args::from_argv([
                "softioc-rs",
                "--db",
                db.to_str().unwrap(),
                "--pv",
                "AB:INLINE:double:1.0",
                &script,
            ])
            .unwrap(),
        )
        .await
        .expect_err("`on error break` stops the IOC before it serves");
        assert!(
            matches!(failed, Failure::Script(_)),
            "the script phase failed"
        );

        // `dbl` is the RECORD list, so it shows the `.db` record only;
        // `--pv` lands in the simple-PV namespace on this route exactly as
        // it does on the builder's. Reaching the script phase at all is
        // what proves `--pv` is no longer refused beside a script.
        let listed: Vec<String> = std::fs::read_to_string(&dump)
            .expect("dbl wrote its redirect")
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(listed, ["AB:PRELOAD"]);
    }

    #[cfg(tokio_backend)]
    /// The server-side options reach the same struct whichever route
    /// builds the database, so a `--mdns` beside an `st.cmd` is carried
    /// rather than refused by name.
    #[test]
    fn the_server_options_survive_a_startup_script() {
        let opts = ServerOptions::from_args(
            &Args::from_argv([
                "softioc",
                "--mdns",
                "inst",
                "--mdns-txt",
                "a=b",
                "--port",
                "0",
                "st.cmd",
            ])
            .unwrap(),
        )
        .expect("well-formed argv");
        assert_eq!(opts.mdns.as_deref(), Some("inst"));
        assert_eq!(opts.mdns_txt, [("a".to_string(), "b".to_string())]);
        assert_eq!(opts.port, Some(0));
    }

    #[cfg(tokio_backend)]
    /// Whether argv is well-formed is C's getopt-loop question, inside the
    /// `try` that exits 2 — so a mismatched `--tls-cert` must stop the boot
    /// before the script, not surface from the protocol runner where the
    /// same typo would exit 1.
    #[epics_macros_rs::epics_test]
    async fn a_malformed_server_option_fails_before_the_script() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("ran.txt");
        let script = dir.path().join("st.cmd");
        std::fs::write(
            &script,
            format!("epicsEnvShow PATH > {}\n", marker.display()),
        )
        .expect("write script");
        let script = script.display().to_string();

        let failed = run_ioc_app(
            Args::from_argv(["softioc-rs", "--tls-cert", "no-such-cert.pem", &script]).unwrap(),
        )
        .await
        .expect_err("--tls-cert without --tls-key is argv's mistake");
        assert!(
            matches!(failed, Failure::Startup(_)),
            "a bad option is the setup phase, got {:?}",
            failed.rendered()
        );
        assert_eq!(failed.status(), 2);
        assert!(!marker.exists(), "the script must not have run");
    }

    /// C `-x` is `dbLoadRecords(exit_file, "IOC=<prefix>")` and nothing else
    /// (`softMain.cpp:212-219`), so the fields that only the loader can set —
    /// `DISP` above all, which is what makes `<prefix>:BaseVersion`
    /// read-only — arrive with the records rather than being lost with them.
    #[test]
    fn a_dash_x_is_c_s_db_load_records_of_the_exit_database() {
        let plan = startup_plan(
            &[Step::ExitDb("XT".into()), Step::ExitDb("YT".into())],
            "/x/softIocExit.db",
        )
        .unwrap();
        assert_eq!(
            plan.lines
                .iter()
                .map(|c| c.line.as_str())
                .collect::<Vec<_>>(),
            vec![
                r#"dbLoadRecords("/x/softIocExit.db", "IOC=XT")"#,
                r#"dbLoadRecords("/x/softIocExit.db", "IOC=YT")"#,
            ]
        );
        assert!(plan.loads_exit_db);
        assert!(plan.loaded_db, "C sets loadedDb at `:219`");
        // Every field C's file carries, including the three the hand-built
        // records could not express.
        for field in [
            r#"field(DESC,"Exit subroutine")"#,
            r#"field(SNAM,"exit")"#,
            r#"field(DTYP,"getenv")"#,
            r#"field(PINI,"YES")"#,
            "field(DISP,1)",
        ] {
            assert!(SOFT_IOC_EXIT_DB.contains(field), "{field}");
        }
    }

    /// The block C prints for `-h`, for an option it will not take and for
    /// `Nothing to do!` is its own, and the run-together line its string
    /// concatenation produces (`softMain.cpp:72`) is the tell that this is
    /// C's text and not a re-typing of it.
    #[test]
    fn the_usage_block_is_c_s_own_text() {
        let text = usage();
        assert!(
            text.starts_with(&format!(
                "Usage: {} [-D softIoc.dbd] [-h] [-S] [-s] [-v] [-a ascf]\n\
                 [-m macro=value,macro2=value2] [-d file.db]\n\
                 [-x prefix] [st.cmd]\n",
                std::env::args().next().unwrap()
            )),
            "{text}"
        );
        assert!(
            text.contains(
                "    -D <dbd>  If used, must come first. Specify the path to the \
                 softIoc.dbdfile.        The compile-time install location is saved \
                 in the binary as a default.\n"
            ),
            "{text}"
        );
        assert!(text.contains("\ninteractive IOC shell.\n"), "{text}");
        // The one paragraph that cannot be C's: there is no installed
        // `softIoc.dbd` here to name.
        assert!(
            !text.contains("Compiled-in path to softIoc.dbd is:"),
            "{text}"
        );
    }

    /// Every case here was measured against `softIoc` R7.0.10-146: what C
    /// read is what C's exit code proves it read. The boundaries are where a
    /// command line can stop — a terminating option before another option,
    /// after one, inside a cluster, after a permuted non-option, as the VALUE
    /// of an option, and after `--`, where it stops being an option at all.
    #[test]
    fn argv_is_cut_where_c_s_getopt_stopped_reading() {
        let read = |argv: &[&str]| -> Vec<String> {
            let owned: Vec<std::ffi::OsString> =
                argv.iter().map(std::ffi::OsString::from).collect();
            copt::getopt_cut(&Args::command(), &owned, &['h'])
                .unwrap_or_else(|| owned.clone())
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        // `-h` ends it where it stands; C exits 0 whatever follows.
        assert_eq!(read(&["softIoc", "-h", "-q"]), ["softIoc", "-h"]);
        assert_eq!(read(&["softIoc", "-h", "--nosuch"]), ["softIoc", "-h"]);
        assert_eq!(
            read(&["softIoc", "-h", "-d", "nosuch.db"]),
            ["softIoc", "-h"]
        );
        // ... including from inside a cluster, which is several options.
        assert_eq!(read(&["softIoc", "-hq"]), ["softIoc", "-h"]);
        assert_eq!(read(&["softIoc", "-vh"]), ["softIoc", "-v", "-h"]);
        // An option C's optstring has not ends it the same way, and the
        // letter C blames is the one it stopped on, not the whole cluster.
        assert_eq!(read(&["softIoc", "-q", "-h"]), ["softIoc", "-q"]);
        assert_eq!(read(&["softIoc", "-qh"]), ["softIoc", "-q"]);
        assert_eq!(
            read(&["softIoc", "--nosuch", "-h"]),
            ["softIoc", "--nosuch"]
        );
        assert_eq!(
            read(&["softIoc", "-m", "X", "-q"]),
            ["softIoc", "-m", "X", "-q"]
        );
        // A permuted non-option is skipped, not a stop: GNU getopt reads on.
        assert_eq!(
            read(&["softIoc", "good.db", "-h"]),
            ["softIoc", "good.db", "-h"]
        );
        // An option's value is a value even when it looks like an option, so
        // `-d -h` loads a file named `-h` rather than printing usage.
        assert_eq!(read(&["softIoc", "-d", "-h"]), ["softIoc", "-d", "-h"]);
        assert_eq!(read(&["softIoc", "-mX", "-h"]), ["softIoc", "-mX", "-h"]);
        // `--` ends option scanning, so the `-h` after it is the st.cmd name.
        assert_eq!(read(&["softIoc", "--", "-h"]), ["softIoc", "--", "-h"]);
        // Nothing terminating: the whole line reaches clap, missing value
        // and all.
        assert_eq!(read(&["softIoc", "-m"]), ["softIoc", "-m"]);
        assert_eq!(read(&["softIoc", "-S", "-v"]), ["softIoc", "-S", "-v"]);
    }

    /// The cut is only worth anything if what survives it parses to the
    /// verdict C reached: usage and 0 for the lines C exits 0 on, and the
    /// refusal naming C's letter for the ones it exits 2 on.
    #[test]
    fn a_cut_command_line_reaches_c_s_verdict() {
        let parse = |argv: &[&str]| {
            let owned: Vec<std::ffi::OsString> =
                argv.iter().map(std::ffi::OsString::from).collect();
            let cut =
                copt::getopt_cut(&Args::command(), &owned, &['h']).unwrap_or_else(|| owned.clone());
            Args::from_argv(cut)
        };
        for argv in [
            &["softIoc", "-h", "-q"][..],
            &["softIoc", "-hq"][..],
            &["softIoc", "-vh"][..],
            &["softIoc", "-h", "--nosuch"][..],
            &["softIoc", "good.db", "-h"][..],
            &["softIoc", "-S", "-h"][..],
        ] {
            let args = parse(argv).unwrap_or_else(|e| panic!("{argv:?} must parse: {e}"));
            assert!(args.c_help, "{argv:?}");
        }
        for argv in [&["softIoc", "-q", "-h"][..], &["softIoc", "-qh"][..]] {
            let Err(e) = parse(argv) else {
                panic!("{argv:?} must be refused");
            };
            assert_eq!(blamed_option(&e), "q", "{argv:?}");
        }
        // `--` puts the script back where C puts it.
        let Ok(script) = parse(&["softIoc", "--", "-h"]) else {
            panic!("`-h` after `--` is the st.cmd name, not a flag");
        };
        assert!(!script.c_help);
        assert_eq!(script.startup_script.as_deref(), Some("-h"));
    }

    /// getopt takes the next argv element as the value whatever it looks
    /// like, so a `-` in that position is text. Measured: `softIoc -d -h`
    /// answers `Can't open file '-h'`, not usage. The boundary the rule must
    /// NOT cross is the positional — `softIoc -q` has to stay a refusal
    /// rather than a startup script called `-q`.
    #[test]
    fn an_option_s_value_is_text_even_when_it_opens_with_a_hyphen() {
        let args = Args::from_argv(["softIoc", "-d", "-h"]).expect("`-h` is the file name");
        assert_eq!(args.db_files, ["-h"]);
        assert!(!args.c_help);
        assert_eq!(
            Args::from_argv(["softIoc", "-D", "-h"])
                .expect("`-h` is the dbd name")
                .dbd,
            ["-h"]
        );
        assert_eq!(
            Args::from_argv(["softIoc", "-m", "-h", "-S"])
                .expect("`-h` is the macro string")
                .macros,
            ["-h"]
        );
        assert!(
            Args::from_argv(["softIoc", "-q"]).is_err(),
            "a refused option must not become the st.cmd name"
        );
    }

    /// C's getopt names the OPTION LETTER in both of its diagnostics, and
    /// clap names whatever it likes. The boundaries are the three shapes a
    /// blamed token arrives in: a value option resolved to its long form, an
    /// unknown short, and a long option C has no concept of.
    #[test]
    fn a_refused_option_is_named_as_c_s_getopt_names_it() {
        let blame = |argv: &[&str]| match Args::from_argv(argv) {
            Err(e) => blamed_option(&e),
            Ok(_) => panic!("{argv:?} must not parse"),
        };
        assert_eq!(blame(&["softIoc", "-d"]), "d");
        assert_eq!(blame(&["softIoc", "-D"]), "D");
        assert_eq!(blame(&["softIoc", "-q"]), "q");
        assert_eq!(blame(&["softIoc", "--nosuch"]), "--nosuch");
    }

    /// C guards `-d` with `errIf(..., "")`, and the empty message is what
    /// makes its catch block silent (`softMain.cpp:274-278`):
    /// `dbLoadRecords` has already written its own two lines. `lazy_dbd`
    /// guards the `.dbd` with a message instead (`:121-122`), and C prints
    /// that one on top of the loader's.
    #[test]
    fn a_failed_command_line_prints_c_s_err_if_message_and_only_that() {
        assert_eq!(Failure::CommandLine(String::new()).status(), 2);
        assert_eq!(Failure::CommandLine(String::new()).rendered(), None);
        assert_eq!(
            Failure::CommandLine("Failed to load DBD file: x.dbd".into()).rendered(),
            Some(format!("{ERL_ERROR}: Failed to load DBD file: x.dbd"))
        );
    }

    /// C's getopt loop replayed: `-m` ASSIGNS, so the second `-d` sees only
    /// the second `-m`'s string (`softMain.cpp:199-201`). Measured against
    /// `softIoc -S -m P=A: -d both.db -m Q=B: -d both.db`, whose `dbl`
    /// lists `A:qREC` and `pB:REC` — never `A:B:REC`.
    #[test]
    fn a_later_macro_option_discards_the_earlier_one() {
        let plan = startup_plan(
            &[
                Step::Macros("P=A:".into()),
                Step::Db("both.db".into()),
                Step::Macros("Q=B:".into()),
                Step::Db("both.db".into()),
            ],
            "/x/softIocExit.db",
        )
        .expect("a plan without -D");
        let lines: Vec<&str> = plan.lines.iter().map(|c| c.line.as_str()).collect();
        assert_eq!(
            lines,
            [
                r#"dbLoadRecords("both.db", "P=A:")"#,
                r#"dbLoadRecords("both.db", "Q=B:")"#,
            ]
        );
        assert!(plan.loaded_db);
    }

    /// C `softMain.cpp:117-127`: the `.dbd` is read once, on first demand,
    /// and it is that read — not the `-D` option — which makes a later `-D`
    /// too late. Two `-D`s before the first `-d` are therefore fine and the
    /// LAST one wins, while `-d -D` is the `runtime_error`.
    #[test]
    fn the_dbd_is_read_once_and_only_before_the_first_database() {
        let plan = startup_plan(
            &[
                Step::Dbd("first.dbd".into()),
                Step::Dbd("second.dbd".into()),
                Step::Db("x.db".into()),
            ],
            "/x/softIocExit.db",
        )
        .expect("both -D options precede the first -d");
        let lines: Vec<&str> = plan.lines.iter().map(|c| c.line.as_str()).collect();
        assert_eq!(
            lines,
            [
                r#"dbLoadDatabase("second.dbd")"#,
                r#"dbLoadRecords("x.db")"#
            ]
        );

        let too_late = startup_plan(
            &[Step::Db("x.db".into()), Step::Dbd("late.dbd".into())],
            "/x/softIocExit.db",
        )
        .unwrap_err();
        assert!(matches!(too_late, Failure::DbdTooLate));
        assert_eq!(too_late.status(), 2);
    }

    /// C `softMain.cpp:174-185`: the substitutions recorded for an ACF are
    /// the macros as they stand AT the `-a`, so a `-m` that follows cannot
    /// reach it — and with no macros set, C emits no `asSetSubstitutions`
    /// at all.
    #[test]
    fn an_acf_records_the_macros_current_at_its_own_option() {
        let plan = startup_plan(
            &[
                Step::Acf("bare.acf".into()),
                Step::Macros("U=ops".into()),
                Step::Acf("subs.acf".into()),
                Step::Macros("U=late".into()),
            ],
            "/x/softIocExit.db",
        )
        .expect("a plan without -D");
        let lines: Vec<&str> = plan.lines.iter().map(|c| c.line.as_str()).collect();
        assert_eq!(
            lines,
            [
                r#"asSetFilename("bare.acf")"#,
                r#"asSetSubstitutions("U=ops")"#,
                r#"asSetFilename("subs.acf")"#,
            ]
        );
        // C's `-a` alone leaves `loadedDb` false, so `-S -a f.acf` is
        // "Nothing to do!" and exits 1.
        assert!(!plan.loaded_db);
    }
}
