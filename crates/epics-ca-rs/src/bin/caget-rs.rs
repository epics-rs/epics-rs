use chrono::{DateTime, Local};
use clap::Parser;
use epics_base_rs::server::snapshot::DbrClass;
use epics_ca_rs::CaError;
use epics_ca_rs::cli::{
    FloatFormat, FloatStyle, IntStyle, PV_NAME_WIDTH, ValueFormat, format_value,
};
use epics_ca_rs::client::{CaClient, enum_string_readback_dbr};
use std::time::SystemTime;

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
    Plain(epics_ca_rs::EpicsValue),
    // Boxed to keep the enum variants size-balanced after Snapshot
    // gained a class_name: Option<String> field for DBR_CLASS_NAME.
    Time(Box<epics_base_rs::server::snapshot::Snapshot>),
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

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.version {
        println!("{VERSION_INFO}");
        return;
    }

    // Acknowledge parity-only flags so the user knows we accepted but
    // are no-oping. Routed via stderr to avoid corrupting tool output
    // pipelines.
    if args.string_format {
        eprintln!("caget-rs: -s (string format) is accepted for parity but not yet honoured");
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
    let want_time = args.wide;
    // `-n`: render ENUM fields as the numeric index. Without it the
    // default readback requests the STRING form (state label), see the
    // per-PV get below (C `caget.c:178-181`).
    let enum_as_number = args.enum_as_number;
    // resolve `-d <type>` ONCE here, mirroring C `caget.c`'s
    // getopt-time resolution (`caget.c:416-434`). The "out of range or
    // invalid" diagnostic prints exactly once (not per PV), and an
    // unresolved/invalid token reverts to the plain native GET, exactly
    // as C sets `format = plain`. `-a` (wide) forces the DBR_TIME class
    // regardless of `-d`, matching C's `complainIfNotPlainAndSet`
    // format precedence.
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
            // C `caget.c:177-182`: with no `-d`, an ENUM field is read
            // back in its STRING form so the value renders as the state
            // label, unless `-n` keeps the numeric index. `native_field_type`
            // is libca `ca_field_type`, valid now that the channel is
            // connected. The substitution does not apply when `-d` set an
            // explicit type (C `format == specifiedDbr`).
            let enum_dbr = if req_dbr_type.is_none() {
                ch.native_field_type()
                    .ok()
                    .and_then(|nt| enum_string_readback_dbr(nt, want_time, !enum_as_number))
            } else {
                None
            };
            // For `-a` (wide / DBR_TIME) we need timestamp + alarm, so
            // route through the DBR_TIME class (native value type) and
            // wrap the response in the `Time` variant. `-d <type>`
            // requests the EXACT DBR type verbatim — C keeps the chosen
            // `dbrType` without re-deriving it from the native type
            // (`caget.c:172`), so `-d DBR_TIME_FLOAT` on a DOUBLE PV
            // requests a float, and `-d 38`/`DBR_CLASS_NAME` reaches the
            // record-class introspection type. The plain path stays on
            // the cheap `get_with_timeout` (no metadata payload).
            let outcome = if let Some(rt) = enum_dbr {
                // ENUM default → request the STRING form. Under `-a` the
                // TIME-class string (DBR_TIME_STRING) still carries
                // timestamp + alarm, so wrap it as `Time`.
                match tokio::time::timeout(t, ch.get_with_dbr_type(rt, 0)).await {
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
                match tokio::time::timeout(t, ch.get_with_metadata(DbrClass::Time)).await {
                    Ok(Ok(snap)) => Ok(GetResult::Time(Box::new(snap))),
                    Ok(Err(CaError::Timeout)) => Err("timeout".to_string()),
                    Ok(Err(e)) => Err(format!("{e}")),
                    Err(_) => Err("timeout".to_string()),
                }
            } else if let Some(rt) = req_dbr_type {
                match tokio::time::timeout(t, ch.get_with_dbr_type(rt, 0)).await {
                    Ok(Ok(snap)) => Ok(GetResult::Plain(snap.value)),
                    Ok(Err(CaError::Timeout)) => Err("timeout".to_string()),
                    Ok(Err(e)) => Err(format!("{e}")),
                    Err(_) => Err("timeout".to_string()),
                }
            } else {
                match ch.get_with_timeout(t).await {
                    Ok((_dbr, value)) => Ok(GetResult::Plain(value)),
                    Err(CaError::Timeout) => Err("timeout".to_string()),
                    Err(e) => Err(format!("{e}")),
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
                if args.terse {
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
                if args.terse {
                    println!("{rendered}");
                } else if stat == 0 && sevr == 0 {
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
            Err(e) if e.contains("not connected") || e.contains("isconnect") => {
                // C prints two different strings: plain/terse mode
                // (caget.c:265) prints lowercase `*** not connected`;
                // only `-a`/wide mode (print_time_val_sts,
                // tool_lib.c:521) prints `*** Not connected (PV not
                // found)`.
                if args.wide {
                    println!(
                        "{}{}*** Not connected (PV not found)",
                        pad_name(true, pv_name),
                        sep
                    );
                } else if args.terse {
                    println!("*** not connected");
                } else {
                    println!("{}{}*** not connected", pad_name(true, pv_name), sep);
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
                if args.terse {
                    println!("*** no data available (timeout)");
                } else {
                    println!(
                        "{}{}*** no data available (timeout)",
                        pad_name(true, pv_name),
                        sep
                    );
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
    use super::{parse_dbr_type, scan_leading_i64};
    use epics_base_rs::types::{
        DBR_CLASS_NAME, DBR_DOUBLE, DBR_STRING, DBR_STSACK_STRING, DBR_TIME_DOUBLE, DBR_TIME_FLOAT,
    };

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
