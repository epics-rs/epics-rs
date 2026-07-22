use clap::{CommandFactory, FromArgMatches, Parser};
use epics_base_rs::server::snapshot::{DbrClass, Snapshot};
use epics_base_rs::types::{DBR_CLASS_NAME, DBR_LONG};
use epics_ca_rs::cli::{
    CountPrefix, FloatFormat, FloatStyle, NO_DATA_MARKER, PV_NAME_WIDTH, ValueFormat,
    ca_error_marker, dbr_value_field_type, format_c_g, format_time, format_value,
    format_value_segment, sevr_to_str, stat_to_str, zero_dbr_snapshot, zero_dbr_value,
};
use epics_ca_rs::client::{
    CaClient, ReqCount, enum_cli_readback_dbr, float_as_string_readback_dbr,
};
use epics_ca_rs::copt::{self, CTool, scan_i32};
use epics_ca_rs::protocol::ECA_DISCONN;
use epics_ca_rs::{CaError, DbFieldType, EpicsValue};
use std::time::SystemTime;

/// Owner of every C-scanned option argument in this binary (see
/// [`epics_ca_rs::copt`]). The name is what C stamps into its warnings.
const TOOL: CTool = CTool::new("caget");

/// The getopt cases that `return` from C's `main` (`caget.c:399-405`), by clap
/// id. `copt::Scan::finish` performs the FIRST one on the command line, after
/// replaying the warnings the loop raised on its way there (R13-26).
const TERMINALS: &[(&str, copt::Terminal)] = &[
    ("help", copt::Terminal::Usage(0)),
    ("version", copt::Terminal::Version),
];

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

/// The `-t` / `-a` / `-d` half of C's getopt loop, replayed in command-line
/// order — the ONE owner of both `format` and `type` (`caget.c:386-434`).
///
/// C's `complainIfNotPlainAndSet` (`caget.c:369-375`) warns whenever the
/// format is not still `plain`, so every occurrence past the first warns and
/// the LAST one wins. `-d` additionally scans its argument: an invalid type
/// warns AND reverts `format` to `plain` (`caget.c:430-434`), which is what
/// un-arms the next occurrence's mutual-exclusion warning — so the three
/// cannot be resolved independently of each other, or of `-d`'s validity.
///
/// All three repeat (R13-17): clap gives every `-d` occurrence its own index,
/// and for the two flags it records the count plus the index of the last one.
/// Replaying a flag's occurrences at that index is exact for the two things C
/// derives from them — which option wins (the highest index) and how many
/// mutual-exclusion warnings fire (one per occurrence past the first).
///
/// Returns C's `(format, type)` pair: `type` is the LAST `-d`'s scanned code,
/// `None` when that scan failed (C's `type == -1`).
fn resolve_format(scan: &mut copt::Scan) -> (OutputMode, Option<u16>) {
    #[derive(Clone)]
    enum Opt {
        Terse,
        All,
        Dbr(String),
    }
    let mut events: Vec<(usize, Opt)> = Vec::new();
    for (id, ev) in [("terse", Opt::Terse), ("wide", Opt::All)] {
        let n = scan.count(id);
        if let Some(i) = scan.last_index(id) {
            events.extend(std::iter::repeat_n((i, ev), usize::from(n)));
        }
    }
    events.extend(
        scan.occurrences("dbr_type")
            .into_iter()
            .map(|(i, v)| (i, Opt::Dbr(v.to_string()))),
    );
    events.sort_by_key(|&(i, _)| i);

    let mut format = OutputMode::Plain;
    let mut d_type: Option<u16> = None;
    for (at, opt) in events {
        if format != OutputMode::Plain {
            scan.warn(
                at,
                "Options t,d,a are mutually exclusive. ('caget -h' for help.)".to_string(),
            );
        }
        format = match opt {
            Opt::Terse => OutputMode::Terse,
            Opt::All => OutputMode::All,
            Opt::Dbr(arg) => {
                d_type = parse_dbr_type(&arg);
                if d_type.is_none() {
                    scan.warn(
                        at,
                        "Requested dbr type out of range or invalid - ignored. \
                         ('caget -h' for help.)"
                            .to_string(),
                    );
                    OutputMode::Plain
                } else {
                    OutputMode::SpecifiedDbr
                }
            }
        };
    }
    (format, d_type)
}

/// C `caget.c` writes ONE `int type` that both `-d` and `-0<base>` assign,
/// in getopt order, and the last assignment wins (`caget.c:416-434` for
/// `-d`, `caget.c:493-495` for `-0`, which sets `type = DBR_LONG` whenever
/// the base scanned valid). Observed on the compiled C:
///
/// ```text
/// caget -d DBR_DOUBLE -0x TST:AO  →  Request type: DBR_LONG    Value: 0x1
/// caget -0x -d DBR_DOUBLE TST:AO  →  Request type: DBR_DOUBLE  Value: 1.5
/// ```
///
/// clap has no notion of that sequence, so the winner is recovered from the
/// argument indices — the same mechanism `resolve_format` uses.
///
/// Only a VALID `-0` enters the race (R13-16). C's assignment sits under
/// `if (outType != dec)` (`caget.c:497-503`), so `-0q` warns and touches
/// NOTHING: neither the base nor `type`. The position that raced `-d` is
/// therefore the last VALID occurrence, which is why the caller hands over a
/// [`copt::Base`] — the fold that decided validity — instead of a raw index:
///
/// ```text
/// caget -0x -d DBR_DOUBLE -0q TST:AO  →  Request type: DBR_DOUBLE
/// ```
///
/// (`-0q` is the last `-0`, but the last one that ASSIGNED is `-0x`, which
/// lost to `-d`.)
///
/// The type only reaches the wire under `specifiedDbr`
/// (`caget.c:175`), so `-0x` on its own still gets the native TIME type.
fn resolve_dbr_type(scan: &copt::Scan, int_base: copt::Base, dbr_type: Option<u16>) -> Option<u16> {
    // No valid `-0<base>`: C never ran the guard, so `type` was never forced.
    let Some(base) = int_base.valid_at else {
        return dbr_type;
    };
    // `-d` may repeat too, and EVERY occurrence assigns `type` in C (an
    // out-of-range one leaves it invalid and falls back to `plain`, where the
    // type is never used) — so the LAST `-d` is the one racing.
    match scan.last_index("dbr_type") {
        Some(d) if d > base => dbr_type,
        _ => Some(DBR_LONG),
    }
}

/// Mirror of C `caget` flags. Where the C flag is a value-printing
/// modifier we forward into [`epics_ca_rs::cli::ValueFormat`].
#[derive(Parser)]
#[command(
    name = "caget-rs",
    about = "Read EPICS PV values",
    disable_version_flag = true,
    disable_help_flag = true
)]
struct Args {
    // `-h` and `-V` are ORDINARY options, not clap's terminating Help/Version
    // actions: C's getopt loop reaches them in order, so the warnings from
    // options *before* them are already on stderr (R13-26). `copt::Scan`
    // performs them at their position — see `TERMINALS`.
    //
    // Every option below is declared `Append` (value option) or `Count` (flag)
    // because C's getopt loop accepts every option any number of times, last one
    // winning — see `epics_ca_rs::copt`, whose `get_matches` refuses to run a
    // spec that says otherwise.
    //
    // Doc comments on these fields are the option's HELP TEXT, so the rationale
    // above stays a plain comment.
    /// Print this message
    #[arg(short = 'h', long, action = clap::ArgAction::Count)]
    help: u8,

    #[arg(short = 'V', long, hide = true, action = clap::ArgAction::Count)]
    version: u8,

    /// CA timeout in seconds (`epicsScanDouble`; a bad value warns and keeps
    /// the `EPICS_CA_TIMEOUT` default). Raw `String`: every C-scanned option
    /// argument is resolved by [`epics_ca_rs::copt`], never by clap.
    /// C ref: `caget.c:437-443`, `tool_lib.c:use_ca_timeout_env`.
    #[arg(short = 'w', long = "wait", allow_hyphen_values = true, action = clap::ArgAction::Append)]
    timeout: Vec<String>,

    /// Asynchronous get (`ca_get_callback`); waits for completion.
    /// Today the Rust client always waits via the GET response, so
    /// this flag is accepted for parity but does not change behaviour.
    #[arg(short = 'c', long, action = clap::ArgAction::Count)]
    callback: u8,

    /// CA priority (`sscanf("%u")`, clamped to `CA_PRIORITY_MAX`). `-p -1`
    /// and `-p 500` are NOT errors in C — both clamp to 99 (`caget.c:455-462`).
    #[arg(short = 'p', long, allow_hyphen_values = true, action = clap::ArgAction::Append)]
    priority: Vec<String>,

    /// Terse: print only the value (no PV name column).
    #[arg(short = 't', long, action = clap::ArgAction::Count)]
    terse: u8,

    /// Wide: print `name timestamp value stat sevr` (DBR_TIME_xxx).
    #[arg(short = 'a', long, action = clap::ArgAction::Count)]
    wide: u8,

    /// Request a specific DBR type by name (e.g. `DOUBLE`,
    /// `DBR_TIME_DOUBLE`) or numeric DBR id. The named family selects
    /// the GET request class (STS/TIME/GR/CTRL or plain value).
    #[arg(
        short = 'd',
        long = "dbr-type",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    dbr_type: Vec<String>,

    /// Print enums as numeric index (default is enum string when
    /// the server returns one).
    #[arg(short = 'n', long = "num-enum", action = clap::ArgAction::Count)]
    enum_as_number: u8,

    /// C's `reqElems` (`sscanf("%d")`, `caget.c:447-453`). `0` — including
    /// `-# 0` and an unscannable `-#` — is C's "not specified", i.e. ALL
    /// elements. Resolved by [`epics_ca_rs::copt::Scan::req_elems_int`].
    #[arg(
        short = '#',
        long = "max-elements",
        value_name = "COUNT",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    max_elements: Vec<String>,

    /// Render `DBR_CHAR` arrays as a NUL-terminated string.
    #[arg(short = 'S', long = "char-as-string", action = clap::ArgAction::Count)]
    char_array_as_string: u8,

    /// `%e` float format with the given precision (`sscanf("%d")` + the
    /// `0..=VALID_DOUBLE_DIGITS` gate; both failures warn and keep the
    /// default format — `caget.c:470-484`).
    #[arg(
        short = 'e',
        long = "format-e",
        value_name = "PRECISION",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    fmt_e: Vec<String>,

    /// `%f` float format with the given precision.
    #[arg(
        short = 'f',
        long = "format-f",
        value_name = "PRECISION",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    fmt_f: Vec<String>,

    /// `%g` float format with the given precision (the default style).
    #[arg(
        short = 'g',
        long = "format-g",
        value_name = "PRECISION",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    fmt_g: Vec<String>,

    /// Get value as string (honors server-side precision).
    /// Accepted for parity; today returns the same as default since
    /// the server already serialises floats with its own precision.
    #[arg(short = 's', long = "string-format", action = clap::ArgAction::Count)]
    string_format: u8,

    /// `-0<base>`: print integers in base `x` (hex), `o` (octal) or `b`
    /// (binary), and request the value as `DBR_LONG`. C spells this as a
    /// getopt option TAKING AN ARGUMENT (`caget.c:398` `"...#:d:0:w:..."`),
    /// so it is `-0` with an attached or separate `<base>` — never a
    /// `--0x`-style flag, which no C script can pass. Repeats are folded by
    /// [`epics_ca_rs::copt::Scan::base`], which keeps C's "last VALID wins" rule.
    #[arg(
        short = '0',
        value_name = "BASE",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    int_base: Vec<String>,

    /// `-l<base>`: round a float to a long and print it in base `x`/`o`/`b`
    /// (C `outTypeF`). Same option shape as `-0` (`caget.c:398`).
    #[arg(
        short = 'l',
        value_name = "BASE",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    float_base: Vec<String>,

    /// Alternate output field separator: C takes `(char) *optarg`, the FIRST
    /// character, and discards the rest (`caget.c:505`).
    #[arg(
        short = 'F',
        long = "field-separator",
        value_name = "OFS",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    field_separator: Vec<String>,

    /// PV names to read. NOT clap-`required`: C's getopt loop has no
    /// required positional — it parses, then `main` checks `nPvs < 1` and
    /// reports C's own diagnostic (`CTool::no_pv_name`, caget.c:527-531).
    pv_names: Vec<String>,
}

impl Args {
    /// Build a [`ValueFormat`] from the CLI flags. Every C-scanned argument
    /// goes through [`TOOL`], which warns and falls back exactly like C's
    /// getopt loop — nothing here can fail the program.
    /// `int_base` is passed in already folded because C's `-0` case writes TWO
    /// things — the integer base AND `type` — and both callers must see the
    /// same fold (and its warnings, printed exactly once).
    fn value_format(&self, scan: &mut copt::Scan, int_base: copt::Base) -> ValueFormat {
        let mut fmt = ValueFormat::default();
        // W10-B2. `-e`/`-f`/`-g` are ONE getopt case writing ONE `dblFormatStr`
        // (`caget.c:470-484`), so the LAST VALID occurrence across the three letters wins
        // — in command-line order, not by an `e` > `f` > `g` precedence. The
        // order lives in the scan, which is why the three `Vec<String>` fields
        // cannot resolve it themselves. Every occurrence is still scanned, so
        // each malformed precision emits its own warning as C's loop does.
        if let Some((letter, precision)) =
            scan.float_precision(&[('e', "fmt_e"), ('f', "fmt_f"), ('g', "fmt_g")])
        {
            fmt.float = FloatFormat {
                style: match letter {
                    'e' => FloatStyle::E,
                    'f' => FloatStyle::F,
                    _ => FloatStyle::G,
                },
                precision,
            };
        }
        // C `caget.c:485-499` writes exactly ONE of the two base globals per
        // occurrence: `-0<base>` sets `outTypeI` (integers), `-l<base>` sets
        // `outTypeF` (floats, via round-to-long). They never cross.
        fmt.int_style = int_base.style;
        fmt.float_style = scan.base('l', "float_base").style;
        fmt.enum_as_number = self.enum_as_number > 0;
        fmt.char_array_as_string = self.char_array_as_string > 0;
        fmt.req_elems = scan.req_elems_int("max_elements");
        if let Some(c) = scan.field_separator("field_separator") {
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

/// C `dbr_type_to_text`: DBR type code (0..=38) → `DBR_*` mnemonic.
/// The table lives in `epics_base_rs` beside its inverse
/// (`dbr_text_to_type`); this is the only caller left in the tools.
fn dbr_text(code: u16) -> &'static str {
    epics_base_rs::types::dbr_type_to_text(code)
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
            format_time(snap.timestamp)
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
        // C `caget.c:328-334`: this block already printed `Element count:` on
        // its own line, so its Value loop joins the elements BARE — there is
        // no `printf("%lu%c", nElems, ...)` here, unlike the plain/terse loop
        // at `:286`. `reqElems` still reaches the `-S` long-string gate
        // (`caget.c:318`), so it is passed through unchanged.
        let rendered = format_value(&snap.value, fmt, enum_strings, CountPrefix::Never);
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
/// `req_elems` is C's `reqElems` — the resolved `-#` count, where `0` is
/// "not specified" (see [`ValueFormat::req_elems`]); `native` is the
/// connected channel's element count (libca `ca_element_count`). Both C
/// branches clamp with `reqElems > nElems ? nElems : reqElems`, and `0`
/// survives the clamp as `0`, so the two modes differ only in what they do
/// with a `0` afterwards.
fn caget_req_count(callback: bool, req_elems: u64, native: u32) -> ReqCount {
    let count = req_elems.min(u64::from(native)) as u32;
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
    format_time(SystemTime::now().into())
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
    // rule (`resolve_format`).
    let cmd = Args::command();
    let parsed = TOOL.get_matches(cmd.clone());
    let matches = parsed.matches();
    let args = Args::from_arg_matches(matches).expect("clap validated the arguments");

    // C's ENTIRE getopt loop runs before any post-loop check, so every option
    // argument is scanned — and every warning raised — while `nPvs` is still
    // unread (`caget.c:398-525`, then `:527`). `caget -w abc` with no PV name
    // therefore prints the timeout warning AND the missing-PV diagnostic; a
    // scan deferred until after the `nPvs` check would print neither.
    //
    // `value_format` is what scans `-#`, `-e`/`-f`/`-g`, `-0`/`-l` and `-F`,
    // and `resolve_format` is what scans `-t`/`-a`/`-d`; between them and the
    // two below, every C-scanned argument in this tool is resolved here, once.
    let mut scan = parsed.scan();
    let ca_timeout = scan.timeout("timeout", epics_ca_rs::cli::env_default_timeout());
    let priority = scan.priority("priority");
    // Folded ONCE: C's `-0` case scans the base and, on success only, forces
    // `type` — so the base fold and the `type` race must see the SAME result,
    // and the "Invalid argument" warnings must be raised exactly once.
    let int_base = scan.base('0', "int_base");
    let fmt = args.value_format(&mut scan, int_base);
    // Resolve `-t`/`-a`/`-d` in command-line order — C's getopt loop writes
    // `format` and `type` from the same three cases, and an invalid `-d`
    // reverts `format` to plain (`caget.c:369-375,416-434`). `-0<base>` never
    // sets `format` in C, so a `-0x` alongside an invalid `-d` must not
    // rescue `specifiedDbr`.
    let (mode, d_type) = resolve_format(&mut scan);
    // `-0<base>` assigns the SAME `int type` as `-d` (`caget.c:493`), racing it
    // in getopt order. The type only reaches the wire under `specifiedDbr`,
    // which is why `mode` is settled first. Resolved while the scan is still
    // alive — it is the scan that knows both options' positions.
    let req_dbr_type = resolve_dbr_type(&scan, int_base, d_type);

    // End of C's getopt loop: the warnings above go to stderr in command-line
    // order, and `-h` / `-V` — which C's loop `return`s from where it meets
    // them — run here, AFTER those warnings and never before (R13-26).
    scan.finish(&cmd, &epics_ca_rs::protocol::version_info(), TERMINALS);

    // C `caget.c:527-531`: the missing-PV check runs after the getopt loop,
    // so `-V` above still wins and a bad option argument has already warned.
    if args.pv_names.is_empty() {
        TOOL.no_pv_name();
    }

    let client = CaClient::new().await.expect("failed to create CA client");
    let timeout = epics_ca_rs::cli::timeout_duration(ca_timeout);
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
    let enum_as_number = args.enum_as_number > 0;
    // `-s` (C `floatAsString`): request a native FLOAT/DOUBLE field's value
    // in string form so the SERVER converts it (C `caget.c:183-187`).
    let float_as_string = args.string_format > 0;
    // Only `all` needs the DBR_TIME class for its native readback; the
    // enum/float substitutions below use `want_time` to pick the TIME
    // vs plain string form (C `caget.c:176-187`).
    let want_time = mode == OutputMode::All;
    // C `caget.c:200` clamps the user's `-#` count to the native element
    // count before the wire request (`reqElems > nElems ? nElems :
    // reqElems`); `0` (no `-#`, `-# 0`, or an unscannable `-#`) requests the
    // full count. Taken from `fmt`, the single carrier of C's `reqElems`.
    let req_elems = fmt.req_elems;
    // C `caget.c:197-218` resolves that count differently per request mode:
    // callback (`-c`) preserves a count-0 autosize request, the synchronous
    // path rewrites 0 → native. Captured as a Copy for the same reason.
    let callback = args.callback > 0;
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
            let req_count = caget_req_count(callback, req_elems, native);
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

    let sep = fmt.field_separator;
    // C's `reqElems` feeds BOTH the plain/terse count-prefix gate
    // (`caget.c:286`) and the `-S` long-string gate on every block
    // (`caget.c:273,318`); the specifiedDbr Value loop takes the second but
    // not the first (`CountPrefix::Never`). Both read it off `fmt`, which is
    // now its only carrier.
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
                let rendered = format_value(value, &fmt, None, CountPrefix::IfRequestedOrArray);
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
                // The separator before the value is C's per-item PREFIX
                // (`tool_lib.c:481-489`), not a suffix of the timestamp.
                let value_seg = format_value_segment(
                    &snap.value,
                    &fmt,
                    enum_strings,
                    CountPrefix::IfRequestedOrArray,
                );
                let is_scalar = snap.value.count() == 1;
                let ts = format_time(snap.timestamp);
                let stat = snap.alarm.status;
                let sevr = snap.alarm.severity;
                if stat == 0 && sevr == 0 {
                    println!(
                        "{name}{sep}{ts}{value_seg}{sep}{sep}",
                        name = pad_name(is_scalar, pv_name),
                        sep = sep,
                    );
                } else {
                    println!(
                        "{name}{sep}{ts}{value_seg}{sep}{stat_str}{sep}{sevr_str}",
                        name = pad_name(is_scalar, pv_name),
                        sep = sep,
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
    let resolved: Option<i64> = if let Some(n) = scan_i32(s) {
        Some(i64::from(n))
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
        Args, GetResult, OutputMode, PvRead, ReadError, ReqCount, TOOL, caget_req_count,
        dbr_extended_str, dbr_text, parse_dbr_type, read_error, read_timeout, resolve_dbr_type,
        resolve_format, specified_dbr_report,
    };
    use clap::{CommandFactory, FromArgMatches, Parser};
    use epics_base_rs::server::snapshot::{ControlInfo, DisplayInfo, EnumInfo, Snapshot};
    use epics_base_rs::types::WallTime;
    use epics_base_rs::types::{
        DBR_CLASS_NAME, DBR_CTRL_DOUBLE, DBR_CTRL_STRING, DBR_DOUBLE, DBR_GR_STRING, DBR_LONG,
        DBR_STRING, DBR_STSACK_STRING, DBR_TIME_DOUBLE, DBR_TIME_FLOAT,
    };
    use epics_ca_rs::cli::IntStyle;
    use epics_ca_rs::cli::{
        EPICS_EPOCH_UNIX_SECS, FloatFormat, FloatStyle, ValueFormat, zero_dbr_snapshot,
        zero_dbr_value,
    };
    use epics_ca_rs::copt::scan_i32;
    use epics_ca_rs::protocol::{ECA_NORDACCESS, eca_message};
    use epics_ca_rs::{CaError, DbFieldType, EpicsValue};
    use std::time::SystemTime;

    fn mode_of(argv: &[&str]) -> OutputMode {
        let m = Args::command().get_matches_from(argv);
        resolve_format(&mut TOOL.scan(&m)).0
    }

    /// R12-16. C declares the two base options as getopt options TAKING AN
    /// ARGUMENT (`caget.c:398`, `":taicnhsSVe:f:g:l:#:d:0:w:p:F:"`), so the
    /// C spelling is `-0x` / `-l x` — a single dash. The Rust port declared
    /// them as clap LONG flags (`--0x`), which made every one of these
    /// invocations `error: unexpected argument '-0' found`, exit 2: the base
    /// options were unreachable through the C CLI.
    ///
    /// Verified against the compiled C `caget` (EPICS 7.0.10.1-DEV):
    ///   `caget -0x TST:LO` → `TST:LO   0xC8`.
    #[test]
    fn base_options_take_a_single_dash_argument_like_c_getopt() {
        for argv in [
            vec!["caget", "-0x", "PV"],
            vec!["caget", "-0", "x", "PV"],
            vec!["caget", "-lb", "PV"],
            vec!["caget", "-l", "b", "PV"],
            vec!["caget", "-0x", "-lb", "PV"],
        ] {
            let a = Args::try_parse_from(&argv)
                .unwrap_or_else(|e| panic!("C spells this `{argv:?}`; clap rejected it: {e}"));
            assert_eq!(a.pv_names, ["PV"], "the base argument must not eat the PV");
        }

        let m = Args::command().get_matches_from(["caget", "-0x", "PV"]);
        let a = Args::from_arg_matches(&m).expect("parses");
        let mut scan = TOOL.scan(&m);
        let base = scan.base('0', "int_base");
        assert_eq!(a.value_format(&mut scan, base).int_style, IntStyle::Hex);
        let m = Args::command().get_matches_from(["caget", "-lb", "PV"]);
        let a = Args::from_arg_matches(&m).expect("parses");
        let mut scan = TOOL.scan(&m);
        let base = scan.base('0', "int_base");
        assert_eq!(a.value_format(&mut scan, base).float_style, IntStyle::Bin);
    }

    /// R12-20. C's `-0<base>` assigns the SAME `int type` as `-d`
    /// (`caget.c:493`, `type = DBR_LONG`), so the two race in getopt order
    /// and the last one wins. Observed on the compiled C:
    ///   `caget -d DBR_DOUBLE -0x TST:AO` → `Request type: DBR_LONG`, `0x1`
    ///   `caget -0x -d DBR_DOUBLE TST:AO` → `Request type: DBR_DOUBLE`, `1.5`
    /// An INVALID base never reaches the assignment (C guards it with
    /// `if (outType != dec)`), so `-d DBR_DOUBLE -0q` keeps DBR_DOUBLE.
    #[test]
    fn zero_base_forces_dbr_long_in_getopt_order() {
        let resolve = |argv: &[&str]| {
            let m = Args::command().get_matches_from(argv);
            let mut scan = TOOL.scan(&m);
            let base = scan.base('0', "int_base");
            let d = resolve_format(&mut scan).1;
            resolve_dbr_type(&scan, base, d)
        };
        assert_eq!(resolve(&["caget", "-0x", "PV"]), Some(DBR_LONG));
        assert_eq!(
            resolve(&["caget", "-d", "DBR_DOUBLE", "-0x", "PV"]),
            Some(DBR_LONG),
            "-0 came last, so it wins"
        );
        assert_eq!(
            resolve(&["caget", "-0x", "-d", "DBR_DOUBLE", "PV"]),
            Some(DBR_DOUBLE),
            "-d came last, so it wins"
        );
        assert_eq!(
            resolve(&["caget", "-d", "DBR_DOUBLE", "-0q", "PV"]),
            Some(DBR_DOUBLE),
            "an invalid base is guarded out of the `type` assignment"
        );
        assert_eq!(
            resolve(&["caget", "-d", "DBR_DOUBLE", "PV"]),
            Some(DBR_DOUBLE)
        );
        assert_eq!(resolve(&["caget", "PV"]), None);
        assert_eq!(
            resolve(&["caget", "-lx", "PV"]),
            None,
            "-l sets outTypeF only; it never touches `type`"
        );
    }

    /// R13-16. Only a VALID `-0` re-enters the `type` race: C's assignment is
    /// guarded by `if (outType != dec)` (`caget.c:497-503`), so a trailing
    /// invalid `-0` warns, assigns nothing, and CANNOT reclaim `type` from a
    /// `-d` that beat the last valid `-0`. Boundary cases of "which occurrence
    /// last assigned":
    #[test]
    fn only_a_valid_zero_base_re_enters_the_dbr_type_race() {
        let resolve = |argv: &[&str]| {
            let m = Args::command().get_matches_from(argv);
            let mut scan = TOOL.scan(&m);
            let base = scan.base('0', "int_base");
            let d = resolve_format(&mut scan).1;
            resolve_dbr_type(&scan, base, d)
        };
        // The invalid `-0q` is the LAST `-0` but not the last one to ASSIGN.
        assert_eq!(
            resolve(&["caget", "-0x", "-d", "DBR_DOUBLE", "-0q", "PV"]),
            Some(DBR_DOUBLE),
            "`-0q` never wrote `type`, so `-d` still holds it"
        );
        // ... and a valid one after `-d` does reclaim it.
        assert_eq!(
            resolve(&["caget", "-0x", "-d", "DBR_DOUBLE", "-0b", "PV"]),
            Some(DBR_LONG),
            "`-0b` assigned after `-d`"
        );
        // An invalid `-0` BEFORE the valid one changes nothing.
        assert_eq!(
            resolve(&["caget", "-0q", "-0x", "-d", "DBR_DOUBLE", "PV"]),
            Some(DBR_DOUBLE)
        );
        assert_eq!(
            resolve(&["caget", "-d", "DBR_DOUBLE", "-0q", "-0x", "PV"]),
            Some(DBR_LONG)
        );
        // No occurrence ever assigned: `type` is untouched by `-0` entirely.
        assert_eq!(resolve(&["caget", "-0q", "PV"]), None);
        assert_eq!(
            resolve(&["caget", "-0q", "-d", "DBR_DOUBLE", "PV"]),
            Some(DBR_DOUBLE)
        );
        // `-d` repeats too, and the LAST `-d` is the one racing.
        assert_eq!(
            resolve(&["caget", "-d", "DBR_DOUBLE", "-0x", "-d", "DBR_STRING", "PV"]),
            Some(0),
            "DBR_STRING == 0, assigned after the valid `-0`"
        );
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
            |callback, req_elems: u64| caget_req_count(callback, req_elems, native).resolve(native);

        // Synchronous (no -c): a count-0 request becomes the native count.
        // `-# 0` and no `-#` are the SAME value in C (`reqElems == 0` is the
        // single "not specified" encoding, caget.c:386), so there is no
        // Some(0)/None distinction left to test.
        assert_eq!(wire(false, 0), native, "sync, no -# / -# 0");
        assert_eq!(wire(false, 3), 3, "sync, -# 3");
        assert_eq!(wire(false, 9), native, "sync, -# > native clamps");

        // Callback (-c): no positive -# preserves the count-0 autosize wire
        // request; a positive -# clamps to native exactly like the sync path.
        assert_eq!(wire(true, 0), 0, "callback, no -# / -# 0 => autosize 0");
        assert_eq!(wire(true, 3), 3, "callback, -# 3");
        assert_eq!(wire(true, 9), native, "callback, -# > native clamps");

        // The request-mode variant itself: no-positive-`-#` callback is the
        // only case that constructs an Autosize request.
        assert_eq!(caget_req_count(true, 0, native), ReqCount::Autosize(0));
        assert_eq!(caget_req_count(false, 0, native), ReqCount::Fixed(0));
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

    /// C `caget.c:317-335`: the specifiedDbr block prints `Element count:` on
    /// its own line and then a BARE value loop — unlike the plain loop at
    /// `:286` it carries no `printf("%lu%c", nElems, sep)` count prefix.
    ///
    /// Pre-fix, `format_value`'s array renderers prefixed the count whenever
    /// `total > 1`, so `caget -d DBR_LONG` on a 3-element array printed
    /// `Value:            3 10 20 30` where C prints `Value:            10 20
    /// 30`. The bare-ness holds for EVERY `-#` value, so the report passes
    /// `CountPrefix::Never` rather than inspecting `fmt.req_elems`.
    #[test]
    fn specified_report_value_line_has_no_count_prefix() {
        let snap = Snapshot::new(
            EpicsValue::LongArray(vec![10, 20, 30]),
            0,
            0,
            SystemTime::UNIX_EPOCH,
        );
        for req_elems in [0u64, 3] {
            let fmt = ValueFormat {
                req_elems,
                ..ValueFormat::default()
            };
            let out = specified_dbr_report("wf:x", Some(DbFieldType::Long), DBR_LONG, &snap, &fmt);
            assert!(out.contains("    Element count:    3\n"), "{out}");
            assert!(
                out.contains("    Value:            10 20 30\n"),
                "the -d Value loop is bare (req_elems={req_elems}): {out}"
            );
        }
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
        e.enums = Some(EnumInfo::new(vec!["OFF".into(), "ON".into()]));
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

    // `-d` takes a numeric DBR code or a mnemonic; the numeric branch is
    // C `sscanf(optarg, "%d", &type)` (caget.c:454), which is now the shared
    // owner `copt::scan_i32`.
    #[test]
    fn dbr_type_number_scan_matches_sscanf_d() {
        assert_eq!(scan_i32("16"), Some(16));
        assert_eq!(scan_i32("  20  "), Some(20));
        assert_eq!(scan_i32("-5"), Some(-5));
        assert_eq!(scan_i32("16x"), Some(16));
        assert_eq!(scan_i32("0x10"), Some(0));
        assert_eq!(scan_i32("DBR_TIME_FLOAT"), None);
        assert_eq!(scan_i32(""), None);
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
