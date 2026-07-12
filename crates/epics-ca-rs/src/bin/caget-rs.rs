use chrono::{DateTime, Local};
use clap::{CommandFactory, FromArgMatches, Parser};
use epics_base_rs::server::snapshot::{DbrClass, Snapshot};
use epics_base_rs::types::{DBR_CLASS_NAME, WallTime};
use epics_ca_rs::cli::{
    FloatFormat, FloatStyle, NO_DATA_MARKER, PV_NAME_WIDTH, ValueFormat, base_style,
    ca_error_marker, dbr_value_field_type, format_c_g, format_value, sevr_to_str, stat_to_str,
    zero_dbr_snapshot, zero_dbr_value,
};
use epics_ca_rs::client::{
    CaClient, ReqCount, enum_cli_readback_dbr, float_as_string_readback_dbr,
};
use epics_ca_rs::protocol::ECA_DISCONN;
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
        // C `caget.c:485-497` writes exactly ONE of the two base globals per
        // flag: `-0<base>` sets `outTypeI` (integers), `-l<base>` sets
        // `outTypeF` (floats, via round-to-long). They never cross.
        fmt.int_style = base_style(self.ix_flag, self.io_flag, self.ib_flag);
        fmt.float_style = base_style(self.lx_flag, self.lo_flag, self.lb_flag);
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
#[derive(Debug)]
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

fn format_server_timestamp(ts: WallTime) -> String {
    // Display only, to microseconds (`%.6f`), so converting through
    // `SystemTime` (100 ns-granular on Windows) loses nothing visible.
    let dt: DateTime<Local> = SystemTime::from(ts).into();
    dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
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
        DbFieldType::UShort => "DBF_USHORT",
        DbFieldType::ULong => "DBF_ULONG",
        DbFieldType::UChar => "DBF_UCHAR",
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
/// Numeric limits take the conversion the C macro embeds: `%8d` for the
/// integer classes and a hardcoded `%g` for FLOAT/DOUBLE, rendered by
/// [`epics_ca_rs::cli::format_c_g`] — NOT the `-e`/`-f`/`-g` value format.
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
        // STS_* (7..=13), plus the two "not implemented" string special-DBRs
        // GR_STRING (21) and CTRL_STRING (28): status + severity only. C
        // `tool_lib.c:350-352` routes DBR_GR_STRING and DBR_CTRL_STRING
        // through the DBR_STS_STRING arm (`PRN_DBR_STS`), so they carry no
        // units, precision, or display/control limits — unlike the numeric
        // GR/CTRL types below.
        7..=13 | 21 | 28 => sts,
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
        // GR numeric (22 SHORT/INT, 23 FLOAT, 25 CHAR, 26 LONG, 27 DOUBLE)
        // and CTRL numeric (29 SHORT/INT, 30 FLOAT, 32 CHAR, 33 LONG,
        // 34 DOUBLE): status/severity, units, [precision for float/double],
        // 6 graphic limits, and (CTRL only) 2 control limits. The string
        // (21/28) and enum (24/31) members of the GR/CTRL range are handled
        // by the arms above, so this arm lists only the numeric members,
        // mirroring the C `dbr2str` switch's per-type cases instead of a
        // broad `21..=34` range that fabricated a limit block for the two
        // string types.
        22 | 23 | 25 | 26 | 27 | 29 | 30 | 32 | 33 | 34 => {
            let is_ctrl = req_type >= 28;
            let is_float = matches!(req_type, 23 | 27 | 30 | 34); // GR/CTRL FLOAT/DOUBLE
            let is_int = matches!(req_type, 22 | 25 | 26 | 29 | 32 | 33); // SHORT/CHAR/LONG
            let d = snap.display.clone().unwrap_or_default();
            // C renders each limit straight from the `FMT_GR` / `FMT_CTRL`
            // macro's embedded conversion — `%8d` for the integer classes,
            // a hardcoded `%g` for FLOAT/DOUBLE (`tool_lib.c:248-254`). The
            // `-e`/`-f`/`-g` flags only rewrite `dblFormatStr`, which is
            // read by `val2str` (the Value line) and never by `dbr2str`, so
            // `fmt` deliberately does not participate here.
            let lim = |v: f64| -> String {
                if is_int {
                    format!("{:8}", v as i64)
                } else {
                    format_c_g(v)
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
                EpicsValue::String(s) => Some(s.as_str_lossy().into_owned()),
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

/// Resolve the CA READ_NOTIFY element count for `caget`'s two request
/// modes, mirroring C `caget.c:197-218`:
///
/// - **Callback** (`-c`, `ca_array_get_callback`, `caget.c:200`): the
///   user's `-#` count is clamped to the native count
///   (`reqElems > nElems ? nElems : reqElems`), but no positive `-#` (no
///   `-#`, or `-# 0`) is sent as the CA autosize request (count 0), so a
///   dynamic waveform returns only its current `NORD` elements →
///   [`ReqCount::Autosize`].
/// - **Synchronous** (no `-c`, `ca_array_get`, `caget.c:208`): the same
///   clamp applies, but a 0 (no `-#`/`-# 0`) means the full native count
///   (`reqElems && reqElems < nElems ? reqElems : nElems`) →
///   [`ReqCount::Fixed`] (which resolves `0` to `native`).
///
/// `max_elements` is the user's `-#` argument (`None` = not given);
/// `native` is the connected channel's element count (libca
/// `ca_element_count`).
fn caget_req_count(callback: bool, max_elements: Option<usize>, native: u32) -> ReqCount {
    let count = max_elements.map_or(0, |n| (n as u32).min(native));
    if callback {
        ReqCount::Autosize(count)
    } else {
        ReqCount::Fixed(count)
    }
}

/// One PV's read outcome, plus whether its read timed out. C warns ONCE on
/// stderr for a timed-out read phase (`caget.c:224-226` for `ca_pend_io`,
/// `:238-239` for the callback wait) and then still runs the print loop, so
/// the flag has to survive alongside the (possibly successful-looking)
/// outcome.
struct PvRead {
    name: String,
    outcome: Result<GetResult, ReadError>,
    timed_out: bool,
}

/// C `pvs[n].status` after the read phase — the value the print loop
/// switches on (`caget.c:262-268`, `tool_lib.c:520-529`).
///
/// Exactly ONE of these is fatal, and only collectively: C counts the PVs
/// that were still connected when it issued the read (`nConn`) and returns 1
/// only when that count is zero (`caget.c:227`). Past that gate the read
/// function returns 0 unconditionally (`caget.c:348`), so a read-access
/// denial, a CA error, or a read timeout on SOME PV prints its marker and
/// leaves the exit status at 0.
#[derive(Debug)]
enum ReadError {
    /// `ca_state != cs_conn` when the read was issued (`caget.c:220`): C
    /// never sends the get and leaves the PV OUT of `nConn`.
    Disconnected,
    /// The `-c` callback never delivered, so `pvs[n].value` is still NULL
    /// (`caget.c:130,268`). Unreachable on the synchronous path — see
    /// [`read_timeout`].
    CallbackTimeout,
    /// A CA failure carrying an ECA status: `ECA_NORDACCESS` is C's
    /// read-access denial, anything else is its generic `ca_message` error.
    Ca(u32),
}

impl ReadError {
    /// C's `*** ...` marker for this status (`caget.c:262-268` — the same
    /// four strings the `terse`, `plain` and `specifiedDbr` formats share,
    /// and that `print_time_val_sts` repeats for `-a`).
    fn marker(&self) -> String {
        match self {
            ReadError::Disconnected => ca_error_marker(ECA_DISCONN),
            ReadError::CallbackTimeout => NO_DATA_MARKER.to_string(),
            ReadError::Ca(s) => ca_error_marker(*s),
        }
    }
}

/// Map one failed get onto C's PV status. A `Timeout` is C's `ca_pend_io`
/// expiring, which only the CALLBACK path can render as a marker; the caller
/// resolves that through [`read_timeout`] before reaching here.
fn read_error(e: &CaError) -> ReadError {
    match e {
        CaError::Disconnected | CaError::Shutdown => ReadError::Disconnected,
        other => ReadError::Ca(other.to_eca_status()),
    }
}

/// C `print_time_val_sts` stamps its error lines with the CLIENT's current
/// time (`epicsTimeGetCurrent`, `tool_lib.c:514-515`), not with a server
/// timestamp — there is no server response to take one from.
fn format_client_timestamp() -> String {
    format_server_timestamp(SystemTime::now().into())
}

/// Print one PV's error line in the shape its output format uses.
///
/// * `terse` (`caget.c:264-269`): the bare marker.
/// * `plain` (`caget.c:260-269`): the padded name column, then the marker.
/// * `specifiedDbr` (`caget.c:299-305`): the name line, then the marker
///   indented four spaces.
/// * `all` (`tool_lib.c:517-529`): the padded name column, then either the
///   `!onceConnected` line — reached exactly when the channel was NOT
///   connected at read time, so it carries no timestamp — or the client's
///   current time, a literal space, and the marker.
fn print_read_error(mode: OutputMode, name_col: &str, pv_name: &str, sep: char, e: &ReadError) {
    let marker = e.marker();
    match mode {
        OutputMode::Terse => println!("{marker}"),
        OutputMode::SpecifiedDbr => println!("{pv_name}\n    {marker}"),
        OutputMode::Plain => println!("{name_col}{sep}{marker}"),
        OutputMode::All => match e {
            ReadError::Disconnected => {
                println!("{name_col}{sep}*** Not connected (PV not found)")
            }
            _ => println!(
                "{name_col}{sep}{ts} {marker}",
                ts = format_client_timestamp()
            ),
        },
    }
}

/// C's read-timeout contract for one PV — the single owner of what a
/// timed-out `caget` read RENDERS.
///
/// The synchronous get (`ca_array_get`, the default) callocs its readback
/// buffer BEFORE the wire request (`caget.c:207-215`) and `ca_pend_io`
/// timing out neither frees it nor touches `pvs[n].status`. The print loop
/// therefore sees `status == ECA_NORMAL` and `value != 0` and renders the
/// still-ZEROED buffer (`caget.c:262-293`) — `0` for a scalar, an empty
/// string for a string/ENUM-label readback, `count` zeros for an array, and
/// under `-a` the EPICS-epoch stamp with NO_ALARM/NO_ALARM.
///
/// ONLY the callback get (`-c`, `ca_array_get_callback`) allocates lazily,
/// inside its event handler (`caget.c:130`): a callback that never arrives
/// leaves `value == NULL`, which is the sole way to reach C's
/// `*** no data available (timeout)` branch (`caget.c:268`).
///
/// `base` is the value carrier of the DBR type actually requested; `None`
/// means `ca_field_type` failed, i.e. the channel dropped after the connect
/// barrier — which C reports as `ECA_DISCONN` (`caget.c:219-221`), not as a
/// timeout.
fn read_timeout(
    callback: bool,
    base: Option<DbFieldType>,
    elems: u32,
    zeroed: impl FnOnce(DbFieldType, u32) -> GetResult,
) -> Result<GetResult, ReadError> {
    if callback {
        return Err(ReadError::CallbackTimeout);
    }
    match base {
        Some(b) => Ok(zeroed(b, elems)),
        None => Err(ReadError::Disconnected),
    }
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
    // C `caget.c:553-556`: `connect_pvs` gates the ENTIRE get+print phase.
    // Any PV that fails to connect inside the one `ca_pend_io` window
    // aborts before `caget()` runs, so stdout carries zero value lines and
    // the tool exits 1 — a connected PV's value is NEVER printed alongside
    // a missing PV's marker.
    let channels =
        match epics_ca_rs::cli::connect_pvs(&client, &args.pv_names, priority, timeout).await {
            Ok(channels) => args
                .pv_names
                .iter()
                .cloned()
                .zip(channels)
                .collect::<Vec<_>>(),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };

    // Read all PVs in parallel within a single timeout window
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
    // C `caget.c:197-218` resolves that count differently per request mode:
    // callback (`-c`) preserves a count-0 autosize request, the synchronous
    // path rewrites 0 → native. Captured as a Copy for the same reason.
    let callback = args.callback;
    let mut handles = Vec::new();
    for (name, ch) in &channels {
        let name = name.clone();
        let t = timeout;
        let ch = ch.clone();
        handles.push(tokio::spawn(async move {
            // Every channel is connected here — `connect_pvs` above is the
            // barrier. A channel can still drop between the barrier and the
            // GET (C: `ca_state != cs_conn` → `ECA_DISCONN`,
            // `caget.c:219-221`), which surfaces below as a Disconnected
            // read error and prints the same `*** not connected` marker.
            //
            // Bound the CA payload at the request boundary and pick the
            // request-mode count contract (C `caget.c:197-218`): the
            // callback path (`-c`) sends a count-0 autosize request so a
            // dynamic waveform returns its current NORD, while the
            // synchronous path requests the full native count.
            let native = ch.element_count().unwrap_or(0);
            let req_count = caget_req_count(callback, max_elements, native);
            // C sizes the readback buffer `dbr_size_n(dbrType, nElems)` with
            // the SAME clamped count it puts on the wire (`caget.c:207-215`).
            let elems = req_count.resolve(native);
            // C `caget.c:172-187`: the request DBR type depends on the
            // output format. `specifiedDbr` carries the `-d` type verbatim
            // (`pvs[n].dbrType = dbrType`) and keeps the full snapshot for
            // the report; EVERY other format re-derives the native
            // TIME-class type and applies the ENUM (`-n`) / float (`-s`)
            // substitutions, discarding any `-d` type. `native_field_type`
            // is libca `ca_field_type`, valid now that the channel is
            // connected.
            let mut timed_out = false;
            let outcome = if mode == OutputMode::SpecifiedDbr {
                let rt = req_dbr_type
                    .expect("specifiedDbr mode implies a resolved -d type (else reverts to plain)");
                let native = ch.native_field_type().ok();
                // The `-d` code passed `parse_dbr_type` (0..=38), so it always
                // has a value carrier.
                let base = dbr_value_field_type(rt);
                let on_timeout = || {
                    read_timeout(callback, base, elems, |b, n| GetResult::Specified {
                        native,
                        req_type: rt,
                        snap: Box::new(zero_dbr_snapshot(b, n)),
                    })
                };
                match tokio::time::timeout(t, ch.get_with_dbr_type(rt, req_count)).await {
                    Ok(Ok(snap)) => Ok(GetResult::Specified {
                        native,
                        req_type: rt,
                        snap: Box::new(snap),
                    }),
                    Ok(Err(CaError::Timeout)) | Err(_) => {
                        timed_out = true;
                        on_timeout()
                    }
                    Ok(Err(e)) => Err(read_error(&e)),
                }
            } else {
                // C `caget.c:177-187` readback substitution, in C's
                // precedence: an ENUM field is ALWAYS substituted —
                // `-n` (`enumAsNr`) → DBR_TIME_INT (numeric index), otherwise
                // DBR_TIME_STRING (state label) — it is never read back as
                // native DBR_TIME_ENUM. ELSE a `-s` request on a native
                // FLOAT/DOUBLE field is read back as DBR_TIME_STRING so the
                // SERVER converts it. Both substitutions are TIME class; the
                // value-only output modes below just take `snap.value`.
                let nt = ch.native_field_type().ok();
                let sub_dbr = nt
                    .and_then(|nt| enum_cli_readback_dbr(nt, enum_as_number))
                    .or_else(|| {
                        float_as_string
                            .then(|| nt.and_then(float_as_string_readback_dbr))
                            .flatten()
                    });
                // The zeroed buffer takes the shape of the type REQUESTED: a
                // substituted ENUM-label readback is a DBR_*_STRING get, so
                // its zeroed buffer is an empty string, not a `0` index.
                let base = match sub_dbr {
                    Some(rt) => dbr_value_field_type(rt),
                    None => nt,
                };
                if let Some(rt) = sub_dbr {
                    // Under `-a` the TIME-class string (DBR_TIME_STRING)
                    // still carries timestamp + alarm, so wrap it as `Time`.
                    let on_timeout = || {
                        read_timeout(callback, base, elems, |b, n| {
                            if want_time {
                                GetResult::Time(Box::new(zero_dbr_snapshot(b, n)))
                            } else {
                                GetResult::Plain(zero_dbr_value(b, n))
                            }
                        })
                    };
                    match tokio::time::timeout(t, ch.get_with_dbr_type(rt, req_count)).await {
                        Ok(Ok(snap)) => Ok(if want_time {
                            GetResult::Time(Box::new(snap))
                        } else {
                            GetResult::Plain(snap.value)
                        }),
                        Ok(Err(CaError::Timeout)) | Err(_) => {
                            timed_out = true;
                            on_timeout()
                        }
                        Ok(Err(e)) => Err(read_error(&e)),
                    }
                } else if want_time {
                    let on_timeout = || {
                        read_timeout(callback, base, elems, |b, n| {
                            GetResult::Time(Box::new(zero_dbr_snapshot(b, n)))
                        })
                    };
                    match tokio::time::timeout(
                        t,
                        ch.get_with_metadata_count(DbrClass::Time, req_count),
                    )
                    .await
                    {
                        Ok(Ok(snap)) => Ok(GetResult::Time(Box::new(snap))),
                        Ok(Err(CaError::Timeout)) | Err(_) => {
                            timed_out = true;
                            on_timeout()
                        }
                        Ok(Err(e)) => Err(read_error(&e)),
                    }
                } else {
                    // plain / terse: cheap typed value, no metadata payload.
                    match ch.get_with_timeout_count(t, req_count).await {
                        Ok((_dbr, value)) => Ok(GetResult::Plain(value)),
                        Err(CaError::Timeout) => {
                            timed_out = true;
                            read_timeout(callback, base, elems, |b, n| {
                                GetResult::Plain(zero_dbr_value(b, n))
                            })
                        }
                        Err(e) => Err(read_error(&e)),
                    }
                }
            };
            PvRead {
                name,
                outcome,
                timed_out,
            }
        }));
    }

    // Collect results preserving PV order.
    let mut results = Vec::with_capacity(handles.len());
    for h in handles {
        results.push(h.await.unwrap());
    }
    // C's ONE stderr warning for a timed-out read phase (`caget.c:224-226`,
    // `:238-239`) — it does not name the PV and does not stop the print loop.
    if results.iter().any(|r| r.timed_out) {
        eprintln!("Read operation timed out: some PV data was not read.");
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
    for PvRead {
        name: pv_name,
        outcome: result,
        ..
    } in &results
    {
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
            Err(e) => {
                // A failed PV never carries a value, so C's name column takes
                // the scalar padding (`nElems == 0 <= 1`).
                print_read_error(mode, &pad_name(true, pv_name), pv_name, sep, e);
            }
        }
    }
    // C `caget.c:227`: `if (!nConn) return 1` — `nConn` counts the PVs that
    // were still connected when the read was issued, and it is the ONLY thing
    // that can make the read phase fail. Past that gate `caget()` returns 0
    // unconditionally (`caget.c:348`), so a read-access denial, a CA error, or
    // a read timeout on some PV prints its marker and leaves the exit status
    // at 0 — as does a disconnect, as long as ANY other PV was still
    // connected.
    let n_conn = results
        .iter()
        .filter(|r| !matches!(r.outcome, Err(ReadError::Disconnected)))
        .count();
    if n_conn == 0 {
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
        Args, GetResult, OutputMode, PvRead, ReadError, ReqCount, caget_req_count,
        dbr_extended_str, dbr_text, parse_dbr_type, read_error, read_timeout, resolve_output_mode,
        scan_leading_i64, specified_dbr_report,
    };
    use clap::CommandFactory;
    use epics_base_rs::server::snapshot::{ControlInfo, DisplayInfo, EnumInfo, Snapshot};
    use epics_base_rs::types::WallTime;
    use epics_base_rs::types::{
        DBR_CLASS_NAME, DBR_CTRL_DOUBLE, DBR_CTRL_STRING, DBR_DOUBLE, DBR_GR_STRING, DBR_STRING,
        DBR_STSACK_STRING, DBR_TIME_DOUBLE, DBR_TIME_FLOAT,
    };
    use epics_ca_rs::cli::{
        EPICS_EPOCH_UNIX_SECS, FloatFormat, FloatStyle, ValueFormat, zero_dbr_snapshot,
        zero_dbr_value,
    };
    use epics_ca_rs::protocol::{ECA_NORDACCESS, eca_message};
    use epics_ca_rs::{CaError, DbFieldType, EpicsValue};
    use std::time::SystemTime;

    fn mode_of(argv: &[&str]) -> OutputMode {
        let m = Args::command().get_matches_from(argv);
        resolve_output_mode(&m)
    }

    /// The DEFAULT (synchronous) read path callocs its buffer BEFORE the wire
    /// request (`caget.c:207-215`), so a `ca_pend_io` timeout leaves the PV at
    /// `status == ECA_NORMAL` with a non-NULL, still-ZEROED buffer, and the
    /// print loop renders those zeroes (`caget.c:262-293`). Only `-c` can reach
    /// `*** no data available (timeout)`, because only its event handler
    /// allocates (`caget.c:130`).
    ///
    /// Pre-fix caget-rs printed the timeout marker on BOTH paths.
    #[test]
    fn synchronous_read_timeout_renders_the_zeroed_buffer() {
        let plain = |callback, base, elems| {
            read_timeout(callback, base, elems, |b, n| {
                GetResult::Plain(zero_dbr_value(b, n))
            })
        };

        // Synchronous scalar DOUBLE: C prints "0".
        match plain(false, Some(DbFieldType::Double), 1) {
            Ok(GetResult::Plain(v)) => assert_eq!(v, EpicsValue::Double(0.0)),
            other => panic!("sync timeout must yield the zeroed buffer, got {other:?}"),
        }
        // Synchronous array: C zeroes every one of the nElems it sized with.
        match plain(false, Some(DbFieldType::Long), 3) {
            Ok(GetResult::Plain(v)) => assert_eq!(v, EpicsValue::LongArray(vec![0, 0, 0])),
            other => panic!("sync array timeout must zero every element, got {other:?}"),
        }
        // An ENUM substituted to its label form is a DBR_*_STRING get, so its
        // zeroed buffer is an empty string, not a 0 index.
        match plain(false, Some(DbFieldType::String), 1) {
            Ok(GetResult::Plain(v)) => assert_eq!(v, EpicsValue::String("".into())),
            other => panic!("string readback timeout must yield \"\", got {other:?}"),
        }

        // Callback (`-c`) is the ONLY path with no buffer: it alone reaches
        // C's `*** no data available (timeout)` marker.
        match plain(true, Some(DbFieldType::Double), 1) {
            Err(e @ ReadError::CallbackTimeout) => {
                assert_eq!(e.marker(), "*** no data available (timeout)")
            }
            other => panic!("-c has no calloc'd buffer to print, got {other:?}"),
        }
    }

    /// `-a` / `-d` render the zeroed `dbr_time_*` header too: a zeroed
    /// `epicsTimeStamp` is the EPICS epoch and the alarm pair is
    /// NO_ALARM / NO_ALARM (`tool_lib.c::print_time_val_sts` then prints two
    /// empty trailing fields).
    #[test]
    fn synchronous_read_timeout_carries_the_epics_epoch_stamp() {
        let r = read_timeout(false, Some(DbFieldType::Double), 1, |b, n| {
            GetResult::Time(Box::new(zero_dbr_snapshot(b, n)))
        });
        match r {
            Ok(GetResult::Time(snap)) => {
                assert_eq!(snap.value, EpicsValue::Double(0.0));
                assert_eq!(snap.alarm.status, 0);
                assert_eq!(snap.alarm.severity, 0);
                assert_eq!(
                    snap.timestamp,
                    WallTime::from_unix(EPICS_EPOCH_UNIX_SECS, 0),
                    "a zeroed epicsTimeStamp is 1990-01-01T00:00:00Z"
                );
            }
            other => panic!("-a sync timeout must yield a zeroed TIME snapshot, got {other:?}"),
        }
    }

    /// `ca_field_type` failing means the channel dropped after the connect
    /// barrier — C reports that as `ECA_DISCONN` (`caget.c:219-221`), not as a
    /// timeout, so there is no buffer to zero.
    #[test]
    fn read_timeout_without_a_field_type_is_a_disconnect() {
        let r = read_timeout(false, None, 1, |b, n| {
            GetResult::Plain(zero_dbr_value(b, n))
        });
        match r {
            Err(ReadError::Disconnected) => {}
            other => panic!("a dropped channel is not a timeout, got {other:?}"),
        }
    }

    /// C `caget.c:262-268`: a post-gate read failure prints a `*** ...`
    /// marker whose text comes straight from the PV's ECA status —
    /// `ECA_NORDACCESS` is spelled out, every other status goes through
    /// `ca_message`. The port printed an invented
    /// `*** no data available (<Rust error Display>)` for all of them.
    #[test]
    fn read_error_markers_match_c() {
        assert_eq!(
            read_error(&CaError::ServerError(ECA_NORDACCESS)).marker(),
            "*** no read access"
        );
        assert_eq!(
            read_error(&CaError::Disconnected).marker(),
            "*** not connected"
        );
        assert_eq!(
            read_error(&CaError::Shutdown).marker(),
            "*** not connected",
            "a client shutdown is C's ECA_DISCONN too"
        );
        // Any other ECA status renders C's `ca_message` text.
        let e = CaError::Protocol("bad frame".into());
        let status = e.to_eca_status();
        assert_eq!(
            read_error(&e).marker(),
            format!("*** CA error {}", eca_message(status)),
            "generic CA failures print ca_message"
        );
    }

    /// C `caget.c:227,348`: `if (!nConn) return 1` is the ONLY non-zero
    /// return of the read phase — `nConn` counts the PVs still connected when
    /// the read was issued. A read-access denial, a CA error or a read timeout
    /// on a CONNECTED PV therefore exits 0, and so does a disconnect as long
    /// as ANY other PV was connected.
    ///
    /// Pre-fix caget-rs set `failed = true` on the `not connected` branch and
    /// on every generic error, exiting 1 for all of them.
    #[test]
    fn exit_status_is_c_n_conn() {
        let read = |outcome| PvRead {
            name: "PV".into(),
            outcome,
            timed_out: false,
        };
        let n_conn = |reads: &[PvRead]| {
            reads
                .iter()
                .filter(|r| !matches!(r.outcome, Err(ReadError::Disconnected)))
                .count()
        };

        // Read-access denied on the only PV: it IS connected → nConn == 1 → 0.
        assert_eq!(
            n_conn(&[read(Err(ReadError::Ca(ECA_NORDACCESS)))]),
            1,
            "an ECA_NORDACCESS PV is still connected"
        );
        // A callback read timeout: connected → exit 0.
        assert_eq!(
            n_conn(&[read(Err(ReadError::CallbackTimeout))]),
            1,
            "a timed-out read is still connected"
        );
        // One disconnected PV among connected ones → nConn > 0 → exit 0.
        assert_eq!(
            n_conn(&[
                read(Err(ReadError::Disconnected)),
                read(Ok(GetResult::Plain(EpicsValue::Double(1.0)))),
            ]),
            1,
            "one live PV keeps nConn non-zero"
        );
        // EVERY PV disconnected → nConn == 0 → the sole exit-1 case.
        assert_eq!(
            n_conn(&[
                read(Err(ReadError::Disconnected)),
                read(Err(ReadError::Disconnected)),
            ]),
            0,
            "!nConn is C's only read-phase failure"
        );
    }

    /// `caget -c` (callback,
    /// `ca_array_get_callback`) must send the CA autosize request (count 0)
    /// when no positive `-#` is given, so a dynamic waveform returns its
    /// current `NORD`; the synchronous path (no `-c`, `ca_array_get`)
    /// instead requests the full native element count. A positive `-# N`
    /// clamps to the native count in both modes (C `caget.c:197-218`).
    ///
    /// One assertion per (mode × `-#` boundary); the asserted value is the
    /// resolved on-the-wire READ_NOTIFY count (`ReqCount::resolve`):
    /// callback → 0 / 0 / N, synchronous → native / native / N. Before the
    /// fix the callback flag was a no-op, so both modes resolved 0 → native
    /// and the autosize request was lost.
    #[test]
    fn caget_req_count_callback_preserves_autosize() {
        let native = 5u32;
        let wire =
            |callback, max: Option<usize>| caget_req_count(callback, max, native).resolve(native);

        // Synchronous (no -c): a count-0 request becomes the native count.
        assert_eq!(wire(false, None), native, "sync, no -#");
        assert_eq!(wire(false, Some(0)), native, "sync, -# 0");
        assert_eq!(wire(false, Some(3)), 3, "sync, -# 3");
        assert_eq!(wire(false, Some(9)), native, "sync, -# > native clamps");

        // Callback (-c): no positive -# preserves the count-0 autosize wire
        // request; a positive -# clamps to native exactly like the sync path.
        assert_eq!(wire(true, None), 0, "callback, no -# => autosize 0");
        assert_eq!(wire(true, Some(0)), 0, "callback, -# 0 => autosize 0");
        assert_eq!(wire(true, Some(3)), 3, "callback, -# 3");
        assert_eq!(wire(true, Some(9)), native, "callback, -# > native clamps");

        // The request-mode variant itself: no-positive-`-#` callback is the
        // only case that constructs an Autosize request.
        assert_eq!(caget_req_count(true, None, native), ReqCount::Autosize(0));
        assert_eq!(caget_req_count(false, None, native), ReqCount::Fixed(0));
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
            units: "mm".into(),
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

    /// C's `FMT_GR` / `FMT_CTRL` macros embed a hardcoded `%g` for the
    /// FLOAT/DOUBLE limit classes (`tool_lib.c:248-254,375-386`), so every
    /// graphic/control limit prints at printf's default 6 significant digits.
    /// The `-e`/`-f`/`-g` flags rewrite `dblFormatStr`, which only `val2str`
    /// reads — they must NOT reach a limit line.
    ///
    /// Pre-fix the `lim` closure used Rust's `Display`, printing the full f64
    /// (`3.14159265`, `1000000`) where C prints `3.14159` / `1e+06`.
    #[test]
    fn gr_ctrl_float_limits_use_c_hardcoded_g() {
        let mut snap = ctrl_double_snap();
        let d = snap
            .display
            .as_mut()
            .expect("ctrl_double_snap sets display");
        // Verified against `printf("%g", ...)`: 8.76543 / 1e+06 / -0.00123457.
        d.lower_disp_limit = 8.765_432_19;
        d.upper_disp_limit = 1e6;
        d.lower_alarm_limit = -0.001234567;
        let c = snap
            .control
            .as_mut()
            .expect("ctrl_double_snap sets control");
        c.upper_ctrl_limit = 123456789.0;

        // The limit block must be identical under EVERY float style, because
        // C's limits never consult `dblFormatStr`.
        for float in [
            FloatFormat::default(),
            FloatFormat {
                style: FloatStyle::F,
                precision: 9,
            },
            FloatFormat {
                style: FloatStyle::E,
                precision: 2,
            },
            FloatFormat {
                style: FloatStyle::G,
                precision: 12,
            },
        ] {
            let fmt = ValueFormat {
                float,
                ..ValueFormat::default()
            };
            let out = specified_dbr_report(
                "ai:temp",
                Some(DbFieldType::Double),
                DBR_CTRL_DOUBLE,
                &snap,
                &fmt,
            );
            assert!(
                out.contains("    Lo disp limit:    8.76543\n"),
                "%g truncates to 6 significant digits: {out}"
            );
            assert!(
                out.contains("    Hi disp limit:    1e+06\n"),
                "%g switches to scientific at exp >= precision: {out}"
            );
            assert!(out.contains("    Lo alarm limit:   -0.00123457\n"), "{out}");
            assert!(out.contains("    Hi ctrl limit:    1.23457e+08\n"), "{out}");
        }
        // Negative control: the VALUE line DOES follow `-f 9` (C `val2str`
        // reads `dblFormatStr`), so the two renderings are genuinely distinct.
        let f9 = ValueFormat {
            float: FloatFormat {
                style: FloatStyle::F,
                precision: 9,
            },
            ..ValueFormat::default()
        };
        let out = specified_dbr_report(
            "ai:temp",
            Some(DbFieldType::Double),
            DBR_CTRL_DOUBLE,
            &snap,
            &f9,
        );
        assert!(
            out.contains("    Value:            1.500000000\n"),
            "-f 9 must still reach the Value line: {out}"
        );
    }

    /// C `caget.c:312-316`: DBR_CLASS_NAME prints only the Class Name line
    /// (no element count / value / extended block).
    #[test]
    fn specified_report_class_name_prints_class_line_only() {
        let mut snap = Snapshot::new(
            EpicsValue::String("ai".into()),
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
            strings: vec!["OFF".into(), "ON".into()],
        });
        let es = dbr_extended_str(24, &e);
        assert!(es.contains("    Enums:            ( 2)"), "{es}");
        assert!(es.contains("[ 0] OFF"), "{es}");
        assert!(es.contains("[ 1] ON"), "{es}");
    }

    // C `tool_lib.c:350-352`
    // marks DBR_GR_STRING (21) and DBR_CTRL_STRING (28) "not implemented" and
    // routes them through the DBR_STS_STRING arm (`PRN_DBR_STS`), so
    // `caget -d DBR_GR_STRING` / `-d DBR_CTRL_STRING` print only Status +
    // Severity — never Units, precision, or display/control limits. The
    // earlier `21..=34` range fabricated a numeric GR/CTRL limit block for
    // these two string types.
    #[test]
    fn gr_ctrl_string_extended_block_is_status_severity_only() {
        // A snapshot carrying full display + control metadata: if the buggy
        // numeric arm were still taken, Units/limits/precision would leak in.
        let snap = ctrl_double_snap();
        for req in [DBR_GR_STRING, DBR_CTRL_STRING] {
            let ext = dbr_extended_str(req, &snap);
            assert!(
                ext.contains("    Status:           NO_ALARM"),
                "req {req} must print Status: {ext}"
            );
            assert!(
                ext.contains("    Severity:         NO_ALARM"),
                "req {req} must print Severity: {ext}"
            );
            assert!(!ext.contains("Units"), "req {req} must omit Units: {ext}");
            assert!(
                !ext.contains("disp limit"),
                "req {req} must omit display limits: {ext}"
            );
            assert!(
                !ext.contains("alarm limit"),
                "req {req} must omit alarm limits: {ext}"
            );
            assert!(
                !ext.contains("warn limit"),
                "req {req} must omit warn limits: {ext}"
            );
            assert!(
                !ext.contains("ctrl limit"),
                "req {req} must omit control limits: {ext}"
            );
            assert!(
                !ext.contains("Precision"),
                "req {req} must omit Precision: {ext}"
            );
        }
    }

    // C db_access dbr_text[]: code → mnemonic, out-of-range → invalid.
    #[test]
    fn dbr_text_maps_codes() {
        assert_eq!(dbr_text(DBR_DOUBLE), "DBR_DOUBLE");
        assert_eq!(dbr_text(DBR_CTRL_DOUBLE), "DBR_CTRL_DOUBLE");
        assert_eq!(dbr_text(DBR_CLASS_NAME), "DBR_CLASS_NAME");
        assert_eq!(dbr_text(99), "DBR_invalid");
    }

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
