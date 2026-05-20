use chrono::{DateTime, Local};
use clap::Parser;
use epics_base_rs::server::snapshot::DbrClass;
use epics_ca_rs::CaError;
use epics_ca_rs::cli::{
    FloatFormat, FloatStyle, IntStyle, PV_NAME_WIDTH, ValueFormat, format_value,
};
use epics_ca_rs::client::CaClient;
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

    /// CA priority (0-99). Accepted for parity; not yet plumbed into
    /// channel creation.
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
    if args.priority.is_some() {
        eprintln!("caget-rs: -p (priority) is accepted for parity but not yet honoured");
    }
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

    let channels: Vec<_> = args
        .pv_names
        .iter()
        .map(|name| (name.clone(), client.create_channel(name)))
        .collect();

    // Connect + read all PVs in parallel within single timeout window
    // (C: connect_pvs → ca_pend_io → ca_array_get → ca_pend_io).
    let want_time = args.wide;
    let mut handles = Vec::new();
    for (name, ch) in &channels {
        let name = name.clone();
        let t = timeout;
        let ch = ch.clone();
        let dbr_class = args.dbr_type.clone();
        handles.push(tokio::spawn(async move {
            let connect = ch.wait_connected(t).await;
            if connect.is_err() {
                return (name, Err("not connected".to_string()));
            }
            // For `-a` (wide / DBR_TIME) we need timestamp + alarm,
            // so route through `get_with_metadata` and wrap the
            // response in the same `Ok` variant. The plain path stays
            // on `get_with_timeout` because it doesn't pay for the
            // bigger DBR_TIME response.
            // CA-FR-4: `-d <type>` selects the DBR request class
            // (STS/TIME/GR/CTRL or plain value); `-a` forces TIME.
            // Routing the GET through the chosen class makes the wire
            // request type honour `-d` instead of always using the
            // channel default.
            let req_class = if want_time {
                Some(DbrClass::Time)
            } else {
                dbr_class.as_deref().and_then(parse_dbr_class)
            };
            let outcome = match req_class {
                Some(DbrClass::Time) => {
                    match tokio::time::timeout(t, ch.get_with_metadata(DbrClass::Time)).await {
                        Ok(Ok(snap)) => Ok(GetResult::Time(Box::new(snap))),
                        Ok(Err(CaError::Timeout)) => Err("timeout".to_string()),
                        Ok(Err(e)) => Err(format!("{e}")),
                        Err(_) => Err("timeout".to_string()),
                    }
                }
                Some(class) => match tokio::time::timeout(t, ch.get_with_metadata(class)).await {
                    Ok(Ok(snap)) => Ok(GetResult::Plain(snap.value)),
                    Ok(Err(CaError::Timeout)) => Err("timeout".to_string()),
                    Ok(Err(e)) => Err(format!("{e}")),
                    Err(_) => Err("timeout".to_string()),
                },
                None => match ch.get_with_timeout(t).await {
                    Ok((_dbr, value)) => Ok(GetResult::Plain(value)),
                    Err(CaError::Timeout) => Err("timeout".to_string()),
                    Err(e) => Err(format!("{e}")),
                },
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

/// CA-FR-4: map a `caget -d <type>` request to the DBR metadata class
/// to fetch. Recognises `DBR_`-prefixed or bare family names; a plain
/// value type (e.g. `DOUBLE`, `STRING`) or numeric id requests the
/// `Plain` value class. Returns `None` for an unrecognised token so the
/// caller falls back to the channel-default GET. Mirrors the C
/// `caget -d` family selection (`caget.c:175-187`).
fn parse_dbr_class(s: &str) -> Option<DbrClass> {
    let up = s.trim().to_ascii_uppercase();
    let fam = up.strip_prefix("DBR_").unwrap_or(&up);
    if fam.starts_with("CTRL") {
        Some(DbrClass::Ctrl)
    } else if fam.starts_with("GR") {
        Some(DbrClass::Gr)
    } else if fam.starts_with("TIME") {
        Some(DbrClass::Time)
    } else if fam.starts_with("STS") {
        Some(DbrClass::Sts)
    } else if fam.is_empty() {
        None
    } else {
        // Plain value type (STRING/SHORT/INT/FLOAT/ENUM/CHAR/LONG/DOUBLE)
        // or a numeric DBR id — request the plain value class.
        Some(DbrClass::Plain)
    }
}

#[cfg(test)]
mod tests {
    use super::{DbrClass, parse_dbr_class};

    #[test]
    fn dbr_class_families() {
        assert!(matches!(
            parse_dbr_class("DBR_TIME_DOUBLE"),
            Some(DbrClass::Time)
        ));
        assert!(matches!(
            parse_dbr_class("ctrl_double"),
            Some(DbrClass::Ctrl)
        ));
        assert!(matches!(parse_dbr_class("DBR_GR_LONG"), Some(DbrClass::Gr)));
        assert!(matches!(parse_dbr_class("STS_INT"), Some(DbrClass::Sts)));
        assert!(matches!(parse_dbr_class("DOUBLE"), Some(DbrClass::Plain)));
        assert!(matches!(parse_dbr_class("6"), Some(DbrClass::Plain)));
        assert!(parse_dbr_class("").is_none());
    }
}
