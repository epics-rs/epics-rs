use clap::Parser;
use epics_ca_rs::CaError;
use epics_ca_rs::cli::{
    FloatFormat, FloatStyle, IntStyle, PV_NAME_WIDTH, ValueFormat, format_value,
};
use epics_ca_rs::client::CaClient;
use std::time::Duration;

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
    /// `DBR_TIME_DOUBLE`) or numeric DBR id. Currently accepted for
    /// parity; the request is delegated to the channel default.
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
    if args.dbr_type.is_some() {
        eprintln!("caget-rs: -d (dbr type) is accepted for parity but not yet honoured");
    }
    if args.string_format {
        eprintln!("caget-rs: -s (string format) is accepted for parity but not yet honoured");
    }
    if args.callback {
        // GET already waits for the response — note silently.
    }

    let client = CaClient::new().await.expect("failed to create CA client");
    let timeout = Duration::from_secs_f64(
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
            match ch.get_with_timeout(t).await {
                Ok((_dbr, value)) => (name, Ok(value)),
                Err(CaError::Timeout) => (name, Err("timeout".to_string())),
                Err(e) => (name, Err(format!("{e}"))),
            }
        }));
    }

    // Collect results preserving PV order.
    let mut results = Vec::with_capacity(handles.len());
    for h in handles {
        results.push(h.await.unwrap());
    }

    let fmt = args.value_format();
    let sep = fmt.field_separator;
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
            Ok(value) => {
                let rendered = format_value(value, &fmt, None);
                let is_scalar = value.count() == 1;
                if args.terse {
                    // C `caget -t`: value only (no name, no leading sep).
                    println!("{rendered}");
                } else if args.wide {
                    // C `-a` shape (`tool_lib.c::print_time_val_sts`):
                    //   `<name-or-padded><sep><timestamp><sep><value>`
                    // followed by either `<sep><stat><sep><sevr>` when
                    // status||severity, or `<sep><sep>` (two empty
                    // fields) on NO_ALARM. We don't yet plumb the
                    // alarm pair through the channel API, so always
                    // emit the NO_ALARM tail. Timestamp placeholder
                    // is `*` until the GET response surfaces it.
                    println!(
                        "{name}{sep}*{sep}{val}{sep}{sep}",
                        name = pad_name(is_scalar, pv_name),
                        sep = sep,
                        val = rendered,
                    );
                } else {
                    println!("{}{}{}", pad_name(is_scalar, pv_name), sep, rendered);
                }
            }
            Err(e) if e.contains("not connected") || e.contains("isconnect") => {
                println!(
                    "{}{}*** Not connected (PV not found)",
                    pad_name(true, pv_name),
                    sep
                );
                failed = true;
            }
            Err(e) if e.contains("timeout") => {
                println!(
                    "{}{}*** no data available (timeout)",
                    pad_name(true, pv_name),
                    sep
                );
                failed = true;
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
