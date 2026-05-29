use chrono::{DateTime, Local};
use clap::{CommandFactory, FromArgMatches, Parser};
use epics_base_rs::server::snapshot::{DbrClass, Snapshot};
use epics_base_rs::types::DBR_CLASS_NAME;
use epics_ca_rs::cli::{
    FloatFormat, FloatStyle, IntStyle, PV_NAME_WIDTH, ValueFormat, format_value,
};
use epics_ca_rs::client::{CaClient, enum_string_readback_dbr, float_as_string_readback_dbr};
use epics_ca_rs::{CaError, DbFieldType, EpicsValue};
use std::time::SystemTime;

/// C `caget` output format (`caget.c:45`, `typedef enum { plain, terse,
/// all, specifiedDbr }`). The request DBR type and the output format are
/// independent: only `specifiedDbr` carries the `-d` type to the wire;
/// every other format requests the native TIME-derived type and prints
/// the value through the ordinary formatter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OutputMode {
    Plain,
    Terse,
    All,
    SpecifiedDbr,
}

/// C `caget.c:369-375` `complainIfNotPlainAndSet`: `-t`, `-a`, `-d` are
/// mutually exclusive output formats applied in command-line order — the
/// second one warns and the later option wins. clap collapses the three
/// into independent fields, so the order is recovered from the parsed
/// argument indices.
fn resolve_output_mode(matches: &clap::ArgMatches) -> OutputMode {
    use clap::parser::ValueSource;
    // `index_of` returns a bogus index for an arg left at its default,
    // so only an option actually supplied on the command line counts —
    // gate on `ValueSource::CommandLine`, then use its index for order.
    let mut opts: Vec<(usize, OutputMode)> = Vec::new();
    for (id, m) in [
        ("terse", OutputMode::Terse),
        ("wide", OutputMode::All),
        ("dbr_type", OutputMode::SpecifiedDbr),
    ] {
        if matches.value_source(id) == Some(ValueSource::CommandLine)
            && let Some(i) = matches.index_of(id)
        {
            opts.push((i, m));
        }
    }
    opts.sort_by_key(|&(i, _)| i);
    let mut format = OutputMode::Plain;
    for (_, requested) in opts {
        if format != OutputMode::Plain {
            eprintln!("Options t,d,a are mutually exclusive. ('caget -h' for help.)");
        }
        format = requested;
    }
    format
}

// C `caget -V` prints a blank line then
//   "EPICS Version EPICS 7.0.10.1-DEV, CA Protocol version 4.13"
// We mirror the same line shape but stamp our own crate version into
// the "EPICS Version" slot so operators can tell at a glance which
// implementation answered.
const VERSION_INFO: &str = concat!(
    "\nEPICS Version epics-rs ",
    env!("CARGO_PKG_VERSION"),
    ", CA Protocol version 4.13"
);

/// Mirror of C `caget` flags. Where the C flag is a value-printing
/// modifier we forward into [`epics_ca_rs::cli::ValueFormat`].
#[derive(Parser)]
#[command(
    name = "caget-rs",
    about = "Read EPICS PV values",
    disable_version_flag = true
)]
struct Args {
    /// Help / version are short-circuited in `parse_argv` before clap.
    #[arg(short = 'V', long, hide = true)]
    version: bool,

    /// CA timeout in seconds.
    /// C ref: `tool_lib.c:use_ca_timeout_env` (commit 1d056c6).
    #[arg(short = 'w', long = "wait")]
    timeout: Option<f64>,

    /// Asynchronous get (`ca_get_callback`); waits for completion.
    /// Today the Rust client always waits via the GET response, so
    /// this flag is accepted for parity but does not change behaviour.
    #[arg(short = 'c', long)]
    callback: bool,

    /// CA priority (0-99). Opens the channel on the matching priority
    /// virtual circuit (libca `ca_create_channel` priority parameter).
    #[arg(short = 'p', long)]
    priority: Option<u8>,

    /// Terse: print only the value (no PV name column).
    #[arg(short = 't', long)]
    terse: bool,

    /// Wide: print `name timestamp value stat sevr` (DBR_TIME_xxx).
    #[arg(short = 'a', long)]
    wide: bool,

    /// Request a specific DBR type by name (e.g. `DOUBLE`,
    /// `DBR_TIME_DOUBLE`) or numeric DBR id. The named family selects
    /// the GET request class (STS/TIME/GR/CTRL or plain value).
    #[arg(short = 'd', long = "dbr-type")]
    dbr_type: Option<String>,

    /// Print enums as numeric index (default is enum string when
    /// the server returns one).
    #[arg(short = 'n', long = "num-enum")]
    enum_as_number: bool,

    /// Print at most this many array elements (count prefix in the
    /// output stays the actual array length).
    #[arg(short = '#', long = "max-elements", value_name = "COUNT")]
    max_elements: Option<usize>,

    /// Render `DBR_CHAR` arrays as a NUL-terminated string.
    #[arg(short = 'S', long = "char-as-string")]
    char_array_as_string: bool,

    /// `%e` float format with the given precision.
    #[arg(short = 'e', long = "format-e", value_name = "PRECISION")]
    fmt_e: Option<u32>,

    /// `%f` float format with the given precision.
    #[arg(short = 'f', long = "format-f", value_name = "PRECISION")]
    fmt_f: Option<u32>,

    /// `%g` float format with the given precision (the default style).
    #[arg(short = 'g', long = "format-g", value_name = "PRECISION")]
    fmt_g: Option<u32>,

    /// Get value as string (honors server-side precision).
    /// Accepted for parity; today returns the same as default since
    /// the server already serialises floats with its own precision.
    #[arg(short = 's', long = "string-format")]
    string_format: bool,

    /// Round float to integer and print in hex (`-lx`).
    #[arg(long = "lx", conflicts_with_all = ["lo_flag", "lb_flag", "ix_flag", "io_flag", "ib_flag"])]
    lx_flag: bool,
    /// Round float to integer and print in octal (`-lo`).
    #[arg(long = "lo", conflicts_with_all = ["lx_flag", "lb_flag", "ix_flag", "io_flag", "ib_flag"])]
    lo_flag: bool,
    /// Round float to integer and print in binary (`-lb`).
    #[arg(long = "lb", conflicts_with_all = ["lx_flag", "lo_flag", "ix_flag", "io_flag", "ib_flag"])]
    lb_flag: bool,

    /// Print integers in hex (`-0x`).
    #[arg(long = "0x", conflicts_with_all = ["io_flag", "ib_flag"])]
    ix_flag: bool,
    /// Print integers in octal (`-0o`).
    #[arg(long = "0o", conflicts_with_all = ["ix_flag", "ib_flag"])]
    io_flag: bool,
    /// Print integers in binary (`-0b`).
    #[arg(long = "0b", conflicts_with_all = ["ix_flag", "io_flag"])]
    ib_flag: bool,

    /// Alternate output field separator. Defaults to a single space.
    #[arg(short = 'F', long = "field-separator", value_name = "OFS")]
    field_separator: Option<char>,

    /// PV names to read.
    #[arg(required_unless_present_any = ["version"])]
    pv_names: Vec<String>,
}

impl Args {
    /// Build a [`ValueFormat`] from the CLI flags.
    fn value_format(&self) -> ValueFormat {
        let mut fmt = ValueFormat::default();
        if let Some(p) = self.fmt_e {
            fmt.float = FloatFormat {
                style: FloatStyle::E,
                precision: p,
            };
        } else if let Some(p) = self.fmt_f {
            fmt.float = FloatFormat {
                style: FloatStyle::F,
                precision: p,
            };
        } else if let Some(p) = self.fmt_g {
            fmt.float = FloatFormat {
                style: FloatStyle::G,
                precision: p,
            };
        }
        if self.ix_flag || self.lx_flag {
            fmt.int_style = IntStyle::Hex;
        } else if self.io_flag || self.lo_flag {
            fmt.int_style = IntStyle::Oct;
        } else if self.ib_flag || self.lb_flag {
            fmt.int_style = IntStyle::Bin;
        }
        fmt.float_as_int = self.lx_flag || self.lo_flag || self.lb_flag;
        fmt.enum_as_number = self.enum_as_number;
        fmt.char_array_as_string = self.char_array_as_string;
        fmt.max_elements = self.max_elements;
        if let Some(c) = self.field_separator {
            fmt.field_separator = c;
        }
        fmt
    }
}

/// Per-PV GET payload returned from the per-channel task.
/// `Plain` is the cheap typed-value path (no timestamp); `Time` is
/// the DBR_TIME variant produced by `-a` so the print loop can lift
/// the real server timestamp + alarm pair onto the wire.
enum GetResult {
    Plain(EpicsValue),
    // Boxed to keep the enum variants size-balanced after Snapshot
    // gained a class_name: Option<String> field for DBR_CLASS_NAME.
    Time(Box<Snapshot>),
    /// `caget.c:298-340` specifiedDbr: the full snapshot for the `-d`
    /// type plus the channel's native field type, so the report can
    /// print native/request type, class name or element-count/value, and
    /// the extended metadata block.
    Specified {
        native: Option<DbFieldType>,
        req_type: u16,
        snap: Box<Snapshot>,
    },
}

fn format_server_timestamp(ts: SystemTime) -> String {
    let dt: DateTime<Local> = ts.into();
    dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

fn sevr_to_str(sevr: u16) -> &'static str {
    match sevr {
        0 => "NO_ALARM",
        1 => "MINOR",
        2 => "MAJOR",
        3 => "INVALID",
        _ => "Illegal value",
    }
}

fn stat_to_str(stat: u16) -> &'static str {
    match stat {
        0 => "NO_ALARM",
        1 => "READ",
        2 => "WRITE",
        3 => "HIHI",
        4 => "HIGH",
        5 => "LOLO",
        6 => "LOW",
        7 => "STATE",
        8 => "COS",
        9 => "COMM",
        10 => "TIMEOUT",
        11 => "HW_LIMIT",
        12 => "CALC",
        13 => "SCAN",
        14 => "LINK",
        15 => "SOFT",
        16 => "BAD_SUB",
        17 => "UDF",
        18 => "DISABLE",
        19 => "SIMM",
        20 => "READ_ACCESS",
        21 => "WRITE_ACCESS",
        _ => "Illegal value",
    }
}

/// C `dbf_type_to_text`: native field type → `DBF_*` mnemonic.
fn dbf_text(t: DbFieldType) -> &'static str {
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
    }
}

/// C `dbr_type_to_text`: DBR type code (0..=38) → `DBR_*` mnemonic
/// (db_access.c `dbr_text[]`). Out-of-range codes mirror C's
/// `"DBR_invalid"`.
fn dbr_text(code: u16) -> &'static str {
    const NAMES: [&str; 39] = [
        "DBR_STRING",
        "DBR_SHORT",
        "DBR_FLOAT",
        "DBR_ENUM",
        "DBR_CHAR",
        "DBR_LONG",
        "DBR_DOUBLE",
        "DBR_STS_STRING",
        "DBR_STS_SHORT",
        "DBR_STS_FLOAT",
        "DBR_STS_ENUM",
        "DBR_STS_CHAR",
        "DBR_STS_LONG",
        "DBR_STS_DOUBLE",
        "DBR_TIME_STRING",
        "DBR_TIME_SHORT",
        "DBR_TIME_FLOAT",
        "DBR_TIME_ENUM",
        "DBR_TIME_CHAR",
        "DBR_TIME_LONG",
        "DBR_TIME_DOUBLE",
        "DBR_GR_STRING",
        "DBR_GR_SHORT",
        "DBR_GR_FLOAT",
        "DBR_GR_ENUM",
        "DBR_GR_CHAR",
        "DBR_GR_LONG",
        "DBR_GR_DOUBLE",
        "DBR_CTRL_STRING",
        "DBR_CTRL_SHORT",
        "DBR_CTRL_FLOAT",
        "DBR_CTRL_ENUM",
        "DBR_CTRL_CHAR",
        "DBR_CTRL_LONG",
        "DBR_CTRL_DOUBLE",
        "DBR_PUT_ACKT",
        "DBR_PUT_ACKS",
        "DBR_STSACK_STRING",
        "DBR_CLASS_NAME",
    ];
    NAMES.get(code as usize).copied().unwrap_or("DBR_invalid")
}

/// C `dbr2str` (tool_lib.c:335): the extended-metadata block printed
/// after the value for a `specifiedDbr` response whose request type is
/// `> DBR_DOUBLE`. Returns an empty string for basic value types
/// (`DBR_STRING..DBR_DOUBLE`), which carry no extra info. The request
/// type code selects the metadata class; the snapshot supplies the
/// values. Each line is indented with four spaces and the block carries
/// no trailing newline.
///
/// Numeric limit formatting follows the C macros' spirit (`%8d` for
/// integer classes, `%g` for float/double) but exact `sprint_long`/`%g`
/// byte-parity is the separate concern of CA-RS-2026-05-28-11.
fn dbr_extended_str(req_type: u16, snap: &Snapshot) -> String {
    if req_type <= 6 {
        return String::new();
    }
    let stat = snap.alarm.status;
    let sevr = snap.alarm.severity;
    let sts = format!(
        "    Status:           {}\n    Severity:         {}",
        stat_to_str(stat),
        sevr_to_str(sevr)
    );
    match req_type {
        // STS_* (7..=13): status + severity only.
        7..=13 => sts,
        // TIME_* (14..=20): timestamp then status + severity.
        14..=20 => format!(
            "    Timestamp:        {}\n{sts}",
            format_server_timestamp(snap.timestamp)
        ),
        // GR_ENUM (24) / CTRL_ENUM (31): status/severity then the enum
        // state table (C `PRN_DBR_X_ENUM`).
        24 | 31 => {
            let labels = snap
                .enums
                .as_ref()
                .map(|e| e.strings.as_slice())
                .unwrap_or(&[]);
            let mut out = sts;
            out.push_str(&format!("\n    Enums:            ({:2})", labels.len()));
            for (i, label) in labels.iter().enumerate() {
                out.push_str(&format!("\n                      [{i:2}] {label}"));
            }
            out
        }
        // GR_* (21..=27) / CTRL_* (28..=34) numeric: status/severity,
        // units, [precision for float/double], 6 graphic limits, and
        // (CTRL only) 2 control limits.
        21..=34 => {
            let is_ctrl = req_type >= 28;
            let is_float = matches!(req_type, 23 | 27 | 30 | 34); // GR/CTRL FLOAT/DOUBLE
            let is_int = matches!(req_type, 22 | 25 | 26 | 29 | 32 | 33); // SHORT/CHAR/LONG
            let d = snap.display.clone().unwrap_or_default();
            let lim = |v: f64| -> String {
                if is_int {
                    format!("{:8}", v as i64)
                } else {
                    format!("{v}")
                }
            };
            let mut out = sts;
            out.push_str(&format!("\n    Units:            {}", d.units));
            if is_float {
                out.push_str(&format!("\n    Precision:        {}", d.precision));
            }
            out.push_str(&format!(
                "\n    Lo disp limit:    {}",
                lim(d.lower_disp_limit)
            ));
            out.push_str(&format!(
                "\n    Hi disp limit:    {}",
                lim(d.upper_disp_limit)
            ));
            out.push_str(&format!(
                "\n    Lo alarm limit:   {}",
                lim(d.lower_alarm_limit)
            ));
            out.push_str(&format!(
                "\n    Lo warn limit:    {}",
                lim(d.lower_warning_limit)
            ));
            out.push_str(&format!(
                "\n    Hi warn limit:    {}",
                lim(d.upper_warning_limit)
            ));
            out.push_str(&format!(
                "\n    Hi alarm limit:   {}",
                lim(d.upper_alarm_limit)
            ));
            if is_ctrl {
                let c = snap.control.clone().unwrap_or_default();
                out.push_str(&format!(
                    "\n    Lo ctrl limit:    {}",
                    lim(c.lower_ctrl_limit)
                ));
                out.push_str(&format!(
                    "\n    Hi ctrl limit:    {}",
                    lim(c.upper_ctrl_limit)
                ));
            }
            out
        }
        // STSACK_STRING (37): status/severity then the ack pair
        // (C `PRN_DBR_STSACK`).
        37 => {
            let ackt = snap.alarm.ackt.unwrap_or(0);
            let acks = snap.alarm.acks.unwrap_or(0);
            format!(
                "{sts}\n    Ack transient?:   {}\n    Ack severity:     {}",
                if ackt != 0 { "YES" } else { "NO" },
                sevr_to_str(acks)
            )
        }
        _ => String::new(),
    }
}

/// C `caget.c:298-340` `specifiedDbr`: PV name on its own line, then the
/// indented native/request-type lines, then either the `Class Name:`
/// line (for `DBR_CLASS_NAME`) or `Element count:` + `Value:` plus the
/// extended-metadata block (for any type `> DBR_DOUBLE`). Returns the
/// full block including its trailing newline.
fn specified_dbr_report(
    pv_name: &str,
    native: Option<DbFieldType>,
    req_type: u16,
    snap: &Snapshot,
    fmt: &ValueFormat,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "{pv_name}");
    let _ = writeln!(
        out,
        "    Native data type: {}",
        native.map(dbf_text).unwrap_or("DBF_NO_ACCESS")
    );
    let _ = writeln!(out, "    Request type:     {}", dbr_text(req_type));
    if req_type == DBR_CLASS_NAME {
        let cn = snap
            .class_name
            .clone()
            .or_else(|| match &snap.value {
                EpicsValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let _ = writeln!(out, "    Class Name:       {cn}");
    } else {
        let enum_strings = snap.enums.as_ref().map(|e| e.strings.as_slice());
        // C's specifiedDbr Value line joins elements WITHOUT a leading
        // count (unlike plain mode), so render with req_elems_present=false;
        // a scalar therefore renders as the bare value.
        let rendered = format_value(&snap.value, fmt, enum_strings, false);
        let _ = writeln!(out, "    Element count:    {}", snap.value.count());
        let _ = writeln!(out, "    Value:            {rendered}");
        let ext = dbr_extended_str(req_type, snap);
        if !ext.is_empty() {
            let _ = writeln!(out, "{ext}");
        }
    }
    out
}

#[tokio::main]
async fn main() {
    // Parse via ArgMatches (not the plain derive) so the command-line
    // order of `-t`/`-a`/`-d` is recoverable for the C mutual-exclusion
    // rule (`resolve_output_mode`).
    let matches = Args::command().get_matches();
    let args = Args::from_arg_matches(&matches).expect("clap validated the arguments");

    if args.version {
        println!("{VERSION_INFO}");
        return;
    }

    if args.callback {
        // GET already waits for the response — note silently.
    }

    let client = CaClient::new().await.expect("failed to create CA client");
    let timeout = epics_ca_rs::cli::timeout_duration(
        args.timeout
            .unwrap_or_else(epics_ca_rs::cli::env_default_timeout),
    );

    // Route -p into the priority circuit (libca
    // `tool_lib.c` passes `caPriority` to `ca_create_channel`).
    let priority = args.priority.unwrap_or(0);
    let channels: Vec<_> = args
        .pv_names
        .iter()
        .map(|name| {
            (
                name.clone(),
                client.create_channel_with_priority(name, priority),
            )
        })
        .collect();

    // Connect + read all PVs in parallel within single timeout window
    // (C: connect_pvs → ca_pend_io → ca_array_get → ca_pend_io).
    // `-n`: render ENUM fields as the numeric index. Without it the
    // default readback requests the STRING form (state label), see the
    // per-PV get below (C `caget.c:178-181`).
    let enum_as_number = args.enum_as_number;
    // `-s` (C `floatAsString`): request a native FLOAT/DOUBLE field's value
    // in string form so the SERVER converts it (C `caget.c:183-187`).
    let float_as_string = args.string_format;
    // resolve `-d <type>` ONCE here, mirroring C `caget.c`'s
    // getopt-time resolution (`caget.c:416-434`). The "out of range or
    // invalid" diagnostic prints exactly once (not per PV).
    let req_dbr_type: Option<u16> = match args.dbr_type.as_deref() {
        Some(s) => {
            let t = parse_dbr_type(s);
            if t.is_none() {
                eprintln!(
                    "Requested dbr type out of range or invalid - ignored. \
                     ('caget -h' for help.)"
                );
            }
            t
        }
        None => None,
    };
    // Resolve the output format from `-t`/`-a`/`-d` (command-line order,
    // mutual-exclusion warning). C `caget.c:430-434`: an invalid `-d`
    // type reverts `format` to plain.
    let mut mode = resolve_output_mode(&matches);
    if mode == OutputMode::SpecifiedDbr && req_dbr_type.is_none() {
        mode = OutputMode::Plain;
    }
    // Only `all` needs the DBR_TIME class for its native readback; the
    // enum/float substitutions below use `want_time` to pick the TIME
    // vs plain string form (C `caget.c:176-187`).
    let want_time = mode == OutputMode::All;
    // C `caget.c:200` clamps the user's `-#` count to the native element
    // count before the wire request (`reqElems > nElems ? nElems :
    // reqElems`); `None` (no `-#`) requests the full count. Captured as a
    // Copy so each spawned task owns it without moving `args`.
    let max_elements = args.max_elements;
    let mut handles = Vec::new();
    for (name, ch) in &channels {
        let name = name.clone();
        let t = timeout;
        let ch = ch.clone();
        handles.push(tokio::spawn(async move {
            let connect = ch.wait_connected(t).await;
            if connect.is_err() {
                return (name, Err("not connected".to_string()));
            }
            // Bound the CA payload at the request boundary: clamp `-# N` to
            // the connected channel's native element count (C `caget.c:200`).
            // `0` requests the full count, which the count-aware GET paths
            // translate to the native element count.
            let req_count: u32 = match max_elements {
                Some(n) => (n as u32).min(ch.element_count().unwrap_or(0)),
                None => 0,
            };
            // C `caget.c:172-187`: the request DBR type depends on the
            // output format. `specifiedDbr` carries the `-d` type verbatim
            // (`pvs[n].dbrType = dbrType`) and keeps the full snapshot for
            // the report; EVERY other format re-derives the native
            // TIME-class type and applies the ENUM (`-n`) / float (`-s`)
            // substitutions, discarding any `-d` type. `native_field_type`
            // is libca `ca_field_type`, valid now that the channel is
            // connected.
            let outcome = if mode == OutputMode::SpecifiedDbr {
                let rt = req_dbr_type
                    .expect("specifiedDbr mode implies a resolved -d type (else reverts to plain)");
                let native = ch.native_field_type().ok();
                match tokio::time::timeout(t, ch.get_with_dbr_type(rt, req_count)).await {
                    Ok(Ok(snap)) => Ok(GetResult::Specified {
                        native,
                        req_type: rt,
                        snap: Box::new(snap),
                    }),
                    Ok(Err(CaError::Timeout)) => Err("timeout".to_string()),
                    Ok(Err(e)) => Err(format!("{e}")),
                    Err(_) => Err("timeout".to_string()),
                }
            } else {
                // C `caget.c:177-187` readback substitution, in C's
                // precedence: an ENUM field is read back in its STRING form
                // (state label unless `-n` keeps the index), ELSE a `-s`
                // request on a native FLOAT/DOUBLE field is read back as
                // DBR_TIME_STRING so the SERVER converts it.
                let nt = ch.native_field_type().ok();
                let sub_dbr = nt
                    .and_then(|nt| enum_string_readback_dbr(nt, want_time, !enum_as_number))
                    .or_else(|| {
                        float_as_string
                            .then(|| nt.and_then(float_as_string_readback_dbr))
                            .flatten()
                    });
                if let Some(rt) = sub_dbr {
                    // Under `-a` the TIME-class string (DBR_TIME_STRING)
                    // still carries timestamp + alarm, so wrap it as `Time`.
                    match tokio::time::timeout(t, ch.get_with_dbr_type(rt, req_count)).await {
                        Ok(Ok(snap)) => Ok(if want_time {
                            GetResult::Time(Box::new(snap))
                        } else {
                            GetResult::Plain(snap.value)
                        }),
                        Ok(Err(CaError::Timeout)) => Err("timeout".to_string()),
                        Ok(Err(e)) => Err(format!("{e}")),
                        Err(_) => Err("timeout".to_string()),
                    }
                } else if want_time {
                    match tokio::time::timeout(
                        t,
                        ch.get_with_metadata_count(DbrClass::Time, req_count),
                    )
                    .await
                    {
                        Ok(Ok(snap)) => Ok(GetResult::Time(Box::new(snap))),
                        Ok(Err(CaError::Timeout)) => Err("timeout".to_string()),
                        Ok(Err(e)) => Err(format!("{e}")),
                        Err(_) => Err("timeout".to_string()),
                    }
                } else {
                    // plain / terse: cheap typed value, no metadata payload.
                    match ch.get_with_timeout_count(t, req_count).await {
                        Ok((_dbr, value)) => Ok(GetResult::Plain(value)),
                        Err(CaError::Timeout) => Err("timeout".to_string()),
                        Err(e) => Err(format!("{e}")),
                    }
                }
            };
            (name, outcome)
        }));
    }

    // Collect results preserving PV order.
    let mut results = Vec::with_capacity(handles.len());
    for h in handles {
        results.push(h.await.unwrap());
    }

    let fmt = args.value_format();
    let sep = fmt.field_separator;
    // C `caget.c:286` gates the array count prefix on
    // `reqElems || nElems > 1`. `reqElems` is non-zero iff the user
    // passed `-#` on the command line.
    let req_elems_present = args.max_elements.is_some();
    // Mirror C `caget.c::main` (line 260): pad the PV name column to
    // 30 characters only when the value is a scalar AND the field
    // separator is the default space. Custom `-F` separator and
    // arrays both fall back to the bare PV name + sep + value shape.
    let pad_name = |is_scalar: bool, name: &str| -> String {
        if is_scalar && sep == ' ' {
            format!("{name:<width$}", width = PV_NAME_WIDTH)
        } else {
            name.to_string()
        }
    };
    let mut failed = false;
    for (pv_name, result) in &results {
        match result {
            Ok(GetResult::Plain(value)) => {
                let rendered = format_value(value, &fmt, None, req_elems_present);
                let is_scalar = value.count() == 1;
                if mode == OutputMode::Terse {
                    println!("{rendered}");
                } else {
                    println!("{}{}{}", pad_name(is_scalar, pv_name), sep, rendered);
                }
            }
            Ok(GetResult::Time(snap)) => {
                // C `-a` shape (`tool_lib.c::print_time_val_sts`):
                //   `<name-or-padded><sep><timestamp><sep><value>`
                // then either `<sep><stat><sep><sevr>` when status or
                // severity is non-zero, or `<sep><sep>` (two empty
                // fields) on NO_ALARM. Mirror that exactly using the
                // alarm pair the DBR_TIME response carried.
                let enum_strings = snap.enums.as_ref().map(|e| e.strings.as_slice());
                let rendered = format_value(&snap.value, &fmt, enum_strings, req_elems_present);
                let is_scalar = snap.value.count() == 1;
                let ts = format_server_timestamp(snap.timestamp);
                let stat = snap.alarm.status;
                let sevr = snap.alarm.severity;
                if stat == 0 && sevr == 0 {
                    println!(
                        "{name}{sep}{ts}{sep}{val}{sep}{sep}",
                        name = pad_name(is_scalar, pv_name),
                        sep = sep,
                        val = rendered,
                    );
                } else {
                    println!(
                        "{name}{sep}{ts}{sep}{val}{sep}{stat_str}{sep}{sevr_str}",
                        name = pad_name(is_scalar, pv_name),
                        sep = sep,
                        val = rendered,
                        stat_str = stat_to_str(stat),
                        sevr_str = sevr_to_str(sevr),
                    );
                }
            }
            Ok(GetResult::Specified {
                native,
                req_type,
                snap,
            }) => {
                print!(
                    "{}",
                    specified_dbr_report(pv_name, *native, *req_type, snap, &fmt)
                );
            }
            Err(e) if e.contains("not connected") || e.contains("isconnect") => {
                // C prints different strings per format: plain/terse
                // (caget.c:265) print lowercase `*** not connected`;
                // `-a`/wide (print_time_val_sts, tool_lib.c:521) prints
                // `*** Not connected (PV not found)`; specifiedDbr
                // (caget.c:301) prints the name line then an indented
                // `    *** not connected`.
                match mode {
                    OutputMode::All => println!(
                        "{}{}*** Not connected (PV not found)",
                        pad_name(true, pv_name),
                        sep
                    ),
                    OutputMode::Terse => println!("*** not connected"),
                    OutputMode::SpecifiedDbr => println!("{pv_name}\n    *** not connected"),
                    OutputMode::Plain => {
                        println!("{}{}*** not connected", pad_name(true, pv_name), sep)
                    }
                }
                failed = true;
            }
            Err(e) if e.contains("timeout") => {
                // C `caget`: `connect_pvs` returns 1 only on a
                // `ca_pend_io` connect timeout; the data-read function
                // (`caget.c:348`) always returns 0. A CONNECTED PV
                // whose GET times out therefore does NOT change the
                // exit code — print the timeout line but leave
                // `failed` untouched.
                match mode {
                    OutputMode::Terse => println!("*** no data available (timeout)"),
                    OutputMode::SpecifiedDbr => {
                        println!("{pv_name}\n    *** no data available (timeout)")
                    }
                    _ => println!(
                        "{}{}*** no data available (timeout)",
                        pad_name(true, pv_name),
                        sep
                    ),
                }
            }
            Err(e) => {
                println!(
                    "{}{}*** no data available ({e})",
                    pad_name(true, pv_name),
                    sep
                );
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}

/// C `sscanf(optarg, "%d", &type)` semantics: skip leading whitespace,
/// accept an optional sign, then take the leading run of decimal digits
/// — trailing junk is ignored (`"16x"` → `16`, `"0x10"` → `0`). Returns
/// `None` when no digit leads (C's `sscanf` returns 0, so `caget` falls
/// through to the textual `dbr_text_to_type` lookup).
fn scan_leading_i64(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut neg = false;
    if let Some(&c) = bytes.first()
        && (c == b'+' || c == b'-')
    {
        neg = c == b'-';
        i = 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    s[start..i]
        .parse::<i64>()
        .ok()
        .map(|n| if neg { -n } else { n })
}

/// resolve a `caget -d <type>` token to an EXACT DBR type code,
/// mirroring C `caget.c:416-434`. The token is resolved as:
///
/// 1. `sscanf("%d")` — a leading integer is the type code verbatim.
/// 2. else `dbr_text_to_type(token)` — exact, case-sensitive name.
/// 3. else retry with a `DBR_` prefix (so `TIME_FLOAT` resolves).
///
/// The code is then validated against C's range
/// `DBR_STRING(0) ..= DBR_CLASS_NAME(38)`, excluding `DBR_PUT_ACKT(35)`
/// and `DBR_PUT_ACKS(36)`. An out-of-range or unresolved token yields
/// `None`; the caller warns once and reverts to the plain native GET,
/// exactly as C sets `format = plain`.
///
/// Unlike the pre-fix `parse_dbr_class`, the resolved code is carried
/// verbatim to the wire — `-d DBR_TIME_FLOAT` on a DOUBLE PV requests
/// `DBR_TIME_FLOAT` (16), not the native-derived `DBR_TIME_DOUBLE`, and
/// `-d 37`/`-d 38` reach `DBR_STSACK_STRING`/`DBR_CLASS_NAME`.
fn parse_dbr_type(s: &str) -> Option<u16> {
    let s = s.trim();
    let resolved: Option<i64> = if let Some(n) = scan_leading_i64(s) {
        Some(n)
    } else {
        epics_base_rs::types::dbr_text_to_type(s)
            .or_else(|| epics_base_rs::types::dbr_text_to_type(&format!("DBR_{s}")))
            .map(i64::from)
    };
    match resolved {
        Some(t) if (0..=38).contains(&t) && t != 35 && t != 36 => Some(t as u16),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Args, OutputMode, dbr_extended_str, dbr_text, parse_dbr_type, resolve_output_mode,
        scan_leading_i64, specified_dbr_report,
    };
    use clap::CommandFactory;
    use epics_base_rs::server::snapshot::{ControlInfo, DisplayInfo, EnumInfo, Snapshot};
    use epics_base_rs::types::{
        DBR_CLASS_NAME, DBR_CTRL_DOUBLE, DBR_DOUBLE, DBR_STRING, DBR_STSACK_STRING,
        DBR_TIME_DOUBLE, DBR_TIME_FLOAT,
    };
    use epics_ca_rs::EpicsValue;
    use epics_ca_rs::cli::ValueFormat;
    use std::time::SystemTime;

    fn mode_of(argv: &[&str]) -> OutputMode {
        let m = Args::command().get_matches_from(argv);
        resolve_output_mode(&m)
    }

    // C `complainIfNotPlainAndSet` (caget.c:369): -t/-a/-d are mutually
    // exclusive output formats, applied in command-line order — the LATER
    // option wins. Boundary: each single option, and both orderings of the
    // -a/-d pair the finding called out.
    #[test]
    fn output_mode_resolves_in_command_line_order() {
        assert_eq!(mode_of(&["caget", "PV"]), OutputMode::Plain);
        assert_eq!(mode_of(&["caget", "-t", "PV"]), OutputMode::Terse);
        assert_eq!(mode_of(&["caget", "-a", "PV"]), OutputMode::All);
        assert_eq!(
            mode_of(&["caget", "-d", "DBR_TIME_DOUBLE", "PV"]),
            OutputMode::SpecifiedDbr
        );
        // `-a -d X`: -d is later → specifiedDbr wins (the finding's case).
        assert_eq!(
            mode_of(&["caget", "-a", "-d", "DBR_TIME_DOUBLE", "PV"]),
            OutputMode::SpecifiedDbr
        );
        // `-d X -a`: -a is later → all wins.
        assert_eq!(
            mode_of(&["caget", "-d", "DBR_TIME_DOUBLE", "-a", "PV"]),
            OutputMode::All
        );
    }

    fn ctrl_double_snap() -> Snapshot {
        let mut s = Snapshot::new(EpicsValue::Double(1.5), 0, 0, SystemTime::UNIX_EPOCH);
        s.display = Some(DisplayInfo {
            units: "mm".to_string(),
            precision: 3,
            upper_disp_limit: 10.0,
            lower_disp_limit: -10.0,
            upper_alarm_limit: 8.0,
            upper_warning_limit: 6.0,
            lower_warning_limit: -6.0,
            lower_alarm_limit: -8.0,
            ..Default::default()
        });
        s.control = Some(ControlInfo {
            upper_ctrl_limit: 9.0,
            lower_ctrl_limit: -9.0,
        });
        s
    }

    // C `caget.c:307-338` specifiedDbr for a CTRL_DOUBLE: name, native /
    // request type, element count + value, then the dbr2str CTRL block
    // (status, severity, units, precision, 6 graphic + 2 control limits).
    #[test]
    fn specified_report_ctrl_double_has_full_metadata_block() {
        let snap = ctrl_double_snap();
        let out = specified_dbr_report(
            "ai:temp",
            Some(DbFieldType::Double),
            DBR_CTRL_DOUBLE,
            &snap,
            &ValueFormat::default(),
        );
        assert!(out.starts_with("ai:temp\n"), "{out}");
        assert!(out.contains("    Native data type: DBF_DOUBLE\n"), "{out}");
        assert!(
            out.contains("    Request type:     DBR_CTRL_DOUBLE\n"),
            "{out}"
        );
        assert!(out.contains("    Element count:    1\n"), "{out}");
        assert!(out.contains("    Value:            1.5\n"), "{out}");
        assert!(out.contains("    Units:            mm\n"), "{out}");
        assert!(out.contains("    Precision:        3\n"), "{out}");
        assert!(out.contains("    Lo ctrl limit:    -9\n"), "{out}");
        assert!(out.contains("    Hi ctrl limit:    9\n"), "{out}");
    }

    // C `caget.c:312-316`: DBR_CLASS_NAME prints only the Class Name line
    // (no element count / value / extended block).
    #[test]
    fn specified_report_class_name_prints_class_line_only() {
        let mut snap = Snapshot::new(
            EpicsValue::String("ai".to_string()),
            0,
            0,
            SystemTime::UNIX_EPOCH,
        );
        snap.class_name = Some("ai".to_string());
        let out = specified_dbr_report(
            "ai:temp",
            Some(DbFieldType::Double),
            DBR_CLASS_NAME,
            &snap,
            &ValueFormat::default(),
        );
        assert!(
            out.contains("    Request type:     DBR_CLASS_NAME\n"),
            "{out}"
        );
        assert!(out.contains("    Class Name:       ai\n"), "{out}");
        assert!(!out.contains("Element count"), "{out}");
        assert!(!out.contains("Value:"), "{out}");
    }

    // C dbr2str: basic value types carry no extended block; TIME adds a
    // Timestamp line; GR_ENUM lists the enum states.
    #[test]
    fn extended_block_by_class_boundary() {
        let basic = Snapshot::new(EpicsValue::Double(1.0), 0, 0, SystemTime::UNIX_EPOCH);
        assert_eq!(dbr_extended_str(DBR_DOUBLE, &basic), "");
        // TIME_DOUBLE (20): timestamp + status + severity.
        let t = dbr_extended_str(DBR_TIME_DOUBLE, &basic);
        assert!(t.contains("    Timestamp:        "), "{t}");
        assert!(t.contains("    Status:           NO_ALARM"), "{t}");
        // GR_ENUM (24): status/severity then the enum table.
        let mut e = Snapshot::new(EpicsValue::Enum(1), 0, 0, SystemTime::UNIX_EPOCH);
        e.enums = Some(EnumInfo {
            strings: vec!["OFF".to_string(), "ON".to_string()],
        });
        let es = dbr_extended_str(24, &e);
        assert!(es.contains("    Enums:            ( 2)"), "{es}");
        assert!(es.contains("[ 0] OFF"), "{es}");
        assert!(es.contains("[ 1] ON"), "{es}");
    }

    // C db_access dbr_text[]: code → mnemonic, out-of-range → invalid.
    #[test]
    fn dbr_text_maps_codes() {
        assert_eq!(dbr_text(DBR_DOUBLE), "DBR_DOUBLE");
        assert_eq!(dbr_text(DBR_CTRL_DOUBLE), "DBR_CTRL_DOUBLE");
        assert_eq!(dbr_text(DBR_CLASS_NAME), "DBR_CLASS_NAME");
        assert_eq!(dbr_text(99), "DBR_invalid");
    }

    use epics_ca_rs::DbFieldType;

    #[test]
    fn scan_leading_i64_matches_sscanf_d() {
        assert_eq!(scan_leading_i64("16"), Some(16));
        assert_eq!(scan_leading_i64("  20  "), Some(20));
        assert_eq!(scan_leading_i64("-5"), Some(-5));
        assert_eq!(scan_leading_i64("16x"), Some(16));
        assert_eq!(scan_leading_i64("0x10"), Some(0));
        assert_eq!(scan_leading_i64("DBR_TIME_FLOAT"), None);
        assert_eq!(scan_leading_i64(""), None);
    }

    #[test]
    fn numeric_tokens_pass_through_verbatim() {
        // The exact code is preserved — no collapse to a metadata band.
        assert_eq!(parse_dbr_type("0"), Some(DBR_STRING));
        assert_eq!(parse_dbr_type("6"), Some(DBR_DOUBLE));
        assert_eq!(parse_dbr_type("16"), Some(DBR_TIME_FLOAT));
        assert_eq!(parse_dbr_type("20"), Some(DBR_TIME_DOUBLE));
        // 37/38 (STSACK / CLASS_NAME) are valid and reachable.
        assert_eq!(parse_dbr_type("37"), Some(DBR_STSACK_STRING));
        assert_eq!(parse_dbr_type("38"), Some(DBR_CLASS_NAME));
    }

    #[test]
    fn invalid_codes_revert_to_plain() {
        // C: type < 0 || > 38 || == 35 || == 36 → revert to plain.
        assert_eq!(parse_dbr_type("-1"), None);
        assert_eq!(parse_dbr_type("35"), None); // DBR_PUT_ACKT
        assert_eq!(parse_dbr_type("36"), None); // DBR_PUT_ACKS
        assert_eq!(parse_dbr_type("39"), None);
        assert_eq!(parse_dbr_type("999"), None);
    }

    #[test]
    fn named_types_resolve_exactly() {
        // Full `DBR_`-prefixed name and the bare-family `DBR_` retry
        // both resolve to the exact code (C `dbr_text_to_type`).
        assert_eq!(parse_dbr_type("DBR_TIME_FLOAT"), Some(DBR_TIME_FLOAT));
        assert_eq!(parse_dbr_type("TIME_FLOAT"), Some(DBR_TIME_FLOAT));
        assert_eq!(parse_dbr_type("DBR_DOUBLE"), Some(DBR_DOUBLE));
        assert_eq!(parse_dbr_type("DOUBLE"), Some(DBR_DOUBLE));
        assert_eq!(parse_dbr_type("DBR_CLASS_NAME"), Some(DBR_CLASS_NAME));
    }

    #[test]
    fn case_sensitive_and_unknown_revert_to_plain() {
        // C `strcmp` is case-sensitive; lowercase reverts to plain.
        assert_eq!(parse_dbr_type("dbr_time_float"), None);
        assert_eq!(parse_dbr_type("double"), None);
        assert_eq!(parse_dbr_type("NONSENSE"), None);
        assert_eq!(parse_dbr_type(""), None);
    }
}
