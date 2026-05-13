use chrono::{DateTime, Local};
use clap::Parser;
use epics_base_rs::server::snapshot::{DbrClass, Snapshot};
use epics_ca_rs::CaError;
use epics_ca_rs::cli::{PV_NAME_WIDTH, ValueFormat, format_value};
use epics_ca_rs::client::CaClient;
use std::time::SystemTime;

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

/// Print one `Old : ...` / `New : ...` line in long-mode shape:
///   `<prefix>{name-padded}<sep><ts>{sep}<value>{sep}{stat?}{sep}{sevr?}`
/// Mirrors `tool_lib.c::print_time_val_sts` — when alarm is
/// (NO_ALARM, NO_ALARM) the trailing two fields are emitted empty.
fn print_long_line(prefix: &str, name_col: &str, sep: char, snap: &Snapshot, fmt: &ValueFormat) {
    let enum_strings = snap.enums.as_ref().map(|e| e.strings.as_slice());
    let val = format_value(&snap.value, fmt, enum_strings);
    let ts = format_server_timestamp(snap.timestamp);
    let stat = snap.alarm.status;
    let sevr = snap.alarm.severity;
    if stat == 0 && sevr == 0 {
        println!("{prefix}{name_col}{sep}{ts}{sep}{val}{sep}{sep}");
    } else {
        println!(
            "{prefix}{name_col}{sep}{ts}{sep}{val}{sep}{stat_str}{sep}{sevr_str}",
            stat_str = stat_to_str(stat),
            sevr_str = sevr_to_str(sevr),
        );
    }
}

const VERSION_INFO: &str = concat!(
    "\nEPICS Version epics-rs ",
    env!("CARGO_PKG_VERSION"),
    ", CA Protocol version 4.13"
);

/// Mirror of C `caput` flag set.
///
/// Note: positional grammar differs in array mode (`-a`):
///
/// * scalar (default):  `caput-rs <PV name> <value> [more values]`
/// * array (`-a`):      `caput-rs -a <PV name> <count> <v0> <v1> ...`
///
/// `value_count` is the parsed `<count>` token that prefixes the
/// values when `-a` is present.
#[derive(Parser)]
#[command(
    name = "caput-rs",
    about = "Write a value to an EPICS PV",
    disable_version_flag = true
)]
struct Args {
    #[arg(short = 'V', long, hide = true)]
    version: bool,

    /// CA timeout in seconds. Mirrors C `tool_lib.c:use_ca_timeout_env`.
    #[arg(short = 'w', long = "timeout")]
    timeout: Option<f64>,

    /// Wait for completion callback (`ca_put_callback`).
    #[arg(short = 'c', long = "callback")]
    callback: bool,

    /// CA priority (0-99). Accepted for parity.
    #[arg(short = 'p', long)]
    priority: Option<u8>,

    /// Terse output: print only the new value (no `Old :`/`New :`
    /// prefix, no PV name).
    #[arg(short = 't', long)]
    terse: bool,

    /// Long mode: post-write read prints `name timestamp value stat
    /// sevr` like `caget -a`.
    #[arg(short = 'l', long = "long")]
    long_mode: bool,

    /// Force interpretation of values as numbers (overrides ENUM
    /// auto-string-resolution).
    #[arg(short = 'n', long = "num-enum")]
    force_numeric: bool,

    /// Force interpretation of values as strings (overrides numeric
    /// parse for ENUM).
    #[arg(short = 's', long = "string-enum", conflicts_with = "force_numeric")]
    force_string: bool,

    /// Put long string as an array of chars (long-string convention).
    #[arg(short = 'S', long = "long-string")]
    long_string: bool,

    /// Put as array. The remaining positionals are
    /// `<count> <v0> <v1> ...`.
    #[arg(short = 'a', long = "array")]
    array_mode: bool,

    /// Alternate output field separator.
    #[arg(short = 'F', long = "field-separator", value_name = "OFS")]
    field_separator: Option<char>,

    /// Positional PV name.
    #[arg(required_unless_present_any = ["version"])]
    pv_name: Option<String>,

    /// Positional values. In `-a` mode the first element is the
    /// count, the rest are the values. Negative numeric values are
    /// allowed via `--`.
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    values: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.version {
        println!("{VERSION_INFO}");
        return;
    }

    if args.priority.is_some() {
        eprintln!("caput-rs: -p (priority) is accepted for parity but not yet honoured");
    }
    if args.long_string {
        eprintln!("caput-rs: -S (long-string put) is accepted for parity but not yet honoured");
    }
    if args.force_numeric || args.force_string {
        // Today the channel's native type drives parse(); -n/-s have
        // no observable effect until a typed-write API is exposed.
        // No-op silently to avoid noisy stderr in scripts.
    }

    let pv_name = args.pv_name.expect("clap enforces required");

    if args.values.is_empty() {
        eprintln!("caput-rs: missing value");
        std::process::exit(1);
    }

    let client = CaClient::new().await.expect("failed to create CA client");
    let timeout = epics_ca_rs::cli::timeout_duration(
        args.timeout
            .unwrap_or_else(epics_ca_rs::cli::env_default_timeout),
    );

    let ch = client.create_channel(&pv_name);
    if let Err(e) = ch.wait_connected(timeout).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    // Determine the channel's native type. Long mode also wants the
    // server timestamp + alarm pair captured BEFORE the put so the
    // `Old :` line reflects the actual pre-put state — the regular
    // path stays on the cheaper plain GET.
    let (native_type, old_value, old_snap) = if args.long_mode {
        match ch.get_with_metadata(DbrClass::Time).await {
            Ok(snap) => (snap.value.dbr_type(), snap.value.clone(), Some(snap)),
            Err(CaError::Timeout) => {
                eprintln!("Read operation timed out: PV data was not read.");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    } else {
        match ch.get_with_timeout(timeout).await {
            Ok((t, v)) => (t, v, None),
            Err(CaError::Timeout) => {
                eprintln!("Read operation timed out: PV data was not read.");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    };

    // Parse the value(s) according to the array/scalar mode.
    let parsed_value = if args.array_mode {
        let want = match args.values[0].parse::<usize>() {
            Ok(n) => n,
            Err(e) => {
                eprintln!("caput-rs: -a count must be a non-negative integer ({e})");
                std::process::exit(1);
            }
        };
        if args.values.len() < 1 + want {
            eprintln!(
                "caput-rs: -a count {} but only {} values provided",
                want,
                args.values.len() - 1
            );
            std::process::exit(1);
        }
        let tokens = &args.values[1..1 + want];
        match parse_array(native_type, tokens) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    } else {
        // Scalar: C `caput` joins extra positionals with single
        // spaces (legacy convention). Modern usage is one value but
        // operators occasionally write `caput PV "alpha beta"`.
        let joined = args.values.join(" ");
        match epics_ca_rs::EpicsValue::parse(native_type, &joined) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    };

    let result = if args.callback {
        ch.put_with_timeout(&parsed_value, timeout).await
    } else {
        ch.put_nowait(&parsed_value).await
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    // Re-read for echoing to stdout (matches C caput which always
    // reads the PV back after the put).
    let (new_value, new_snap) = if args.long_mode {
        match ch.get_with_metadata(DbrClass::Time).await {
            Ok(snap) => (snap.value.clone(), Some(snap)),
            Err(_) => (parsed_value.clone(), None),
        }
    } else {
        (
            match ch.get_with_timeout(timeout).await {
                Ok((_, val)) => val,
                Err(_) => parsed_value.clone(),
            },
            None,
        )
    };

    let mut fmt = ValueFormat::default();
    if let Some(c) = args.field_separator {
        fmt.field_separator = c;
    }
    let sep = fmt.field_separator;
    let old_rendered = format_value(&old_value, &fmt, None);
    let new_rendered = format_value(&new_value, &fmt, None);
    let is_scalar = new_value.count() == 1;
    let pad = |name: &str| -> String {
        if is_scalar && sep == ' ' {
            format!("{name:<width$}", width = PV_NAME_WIDTH)
        } else {
            name.to_string()
        }
    };

    if args.terse {
        // C `caput -t`: only the new value (no name, no Old/New).
        println!("{new_rendered}");
    } else if args.long_mode {
        // C `caput -l`: same shape as `caget -a` for both lines, using
        // the DBR_TIME snapshots captured around the put.
        let name_col = pad(&pv_name);
        match &old_snap {
            Some(s) => print_long_line("Old : ", &name_col, sep, s, &fmt),
            None => println!("Old : {name_col}{sep}*{sep}{old_rendered}{sep}{sep}"),
        }
        match &new_snap {
            Some(s) => print_long_line("New : ", &name_col, sep, s, &fmt),
            None => println!("New : {name_col}{sep}*{sep}{new_rendered}{sep}{sep}"),
        }
    } else {
        // Default: `Old : <name-padded><sep><value>` and likewise for
        // New. Mirrors C `caput.c::main` post-put echo.
        println!(
            "Old : {name}{sep}{val}",
            name = pad(&pv_name),
            val = old_rendered
        );
        println!(
            "New : {name}{sep}{val}",
            name = pad(&pv_name),
            val = new_rendered
        );
    }
}

fn parse_array(
    native_type: epics_ca_rs::DbFieldType,
    tokens: &[String],
) -> Result<epics_ca_rs::EpicsValue, String> {
    use epics_ca_rs::DbFieldType as DT;
    use epics_ca_rs::EpicsValue;
    match native_type {
        DT::Short => {
            let mut arr = Vec::with_capacity(tokens.len());
            for t in tokens {
                arr.push(
                    EpicsValue::parse(DT::Short, t)
                        .and_then(|v| match v {
                            EpicsValue::Short(n) => Ok(n),
                            _ => Err(epics_ca_rs::CaError::InvalidValue("not short".into())),
                        })
                        .map_err(|e| e.to_string())?,
                );
            }
            Ok(EpicsValue::ShortArray(arr))
        }
        DT::Float => {
            let mut arr = Vec::with_capacity(tokens.len());
            for t in tokens {
                arr.push(t.parse::<f32>().map_err(|e| e.to_string())?);
            }
            Ok(EpicsValue::FloatArray(arr))
        }
        DT::Double => {
            let mut arr = Vec::with_capacity(tokens.len());
            for t in tokens {
                arr.push(t.parse::<f64>().map_err(|e| e.to_string())?);
            }
            Ok(EpicsValue::DoubleArray(arr))
        }
        DT::Long => {
            let mut arr = Vec::with_capacity(tokens.len());
            for t in tokens {
                arr.push(t.parse::<i32>().map_err(|e| e.to_string())?);
            }
            Ok(EpicsValue::LongArray(arr))
        }
        DT::Enum => {
            let mut arr = Vec::with_capacity(tokens.len());
            for t in tokens {
                arr.push(t.parse::<u16>().map_err(|e| e.to_string())?);
            }
            Ok(EpicsValue::EnumArray(arr))
        }
        DT::Int64 => {
            let mut arr = Vec::with_capacity(tokens.len());
            for t in tokens {
                arr.push(t.parse::<i64>().map_err(|e| e.to_string())?);
            }
            Ok(EpicsValue::Int64Array(arr))
        }
        DT::Char => Ok(EpicsValue::CharArray(
            tokens
                .iter()
                .map(|t| t.parse::<u8>().unwrap_or(0))
                .collect(),
        )),
        DT::String => Ok(EpicsValue::StringArray(tokens.to_vec())),
    }
}
