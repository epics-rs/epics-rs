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
    // caput has no `-#` element-count flag, so req_elems is always false:
    // an array count prefix is emitted only when the array length itself
    // exceeds 1 (matches C `caget.c` / `tool_lib.c` PRN_TIME_VAL_STS gating).
    let val = format_value(&snap.value, fmt, enum_strings, false);
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

    /// CA priority (0-99). Opens the channel on the matching priority
    /// virtual circuit (libca `ca_create_channel` priority parameter).
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

    // -n / -s steer ENUM scalar handling below (force_numeric =
    // interpret as index; force_string = always send DBR_STRING for
    // server-side menu resolution). For non-ENUM channels they have
    // no effect — matching C `caput`, where enumAsNr / enumAsString
    // only gate the DBR_ENUM branch.

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

    // CA-FR-3/CA-FR-4: -p selects the priority virtual circuit.
    let ch = client.create_channel_with_priority(&pv_name, args.priority.unwrap_or(0));
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

    // CA-FR-4: build the value to write in C's precedence order — `-S`
    // (long string) resolved before any native-type parse. See
    // `build_write_value`.
    let parsed_value = match build_write_value(
        &args.values,
        native_type,
        args.force_numeric,
        args.force_string,
        args.long_string,
        args.array_mode,
    ) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let result = match &parsed_value {
        WriteValue::Typed(v) => {
            if args.callback {
                ch.put_with_timeout(v, timeout).await
            } else {
                ch.put_nowait(v).await
            }
        }
        WriteValue::EnumString(s) => {
            // Always write ENUM-by-name through the DBR_STRING put so
            // the server resolves the menu string.
            if args.callback {
                ch.put_string(s).await
            } else {
                ch.put_string_nowait(s).await
            }
        }
        WriteValue::EnumStringArray(v) => {
            // ENUM waveform by name — DBR_STRING array, server resolves
            // each element.
            if args.callback {
                ch.put_string_array(v).await
            } else {
                ch.put_string_array_nowait(v).await
            }
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    // Re-read for echoing to stdout (matches C caput which always
    // reads the PV back after the put). `echo_fallback` is the value
    // shown if the post-put read fails — just what was written.
    let echo_fallback = parsed_value.echo_fallback();
    let (new_value, new_snap) = if args.long_mode {
        match ch.get_with_metadata(DbrClass::Time).await {
            Ok(snap) => (snap.value.clone(), Some(snap)),
            Err(_) => (echo_fallback.clone(), None),
        }
    } else {
        (
            match ch.get_with_timeout(timeout).await {
                Ok((_, val)) => val,
                Err(_) => echo_fallback.clone(),
            },
            None,
        )
    };

    let mut fmt = ValueFormat::default();
    if let Some(c) = args.field_separator {
        fmt.field_separator = c;
    }
    let sep = fmt.field_separator;
    let old_rendered = format_value(&old_value, &fmt, None, false);
    let new_rendered = format_value(&new_value, &fmt, None, false);
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

/// What `caput-rs` will write — either a value typed against the
/// channel's native DBR type, or a string to be sent as `DBR_STRING`
/// for server-side resolution (the ENUM-by-name path).
enum WriteValue {
    Typed(epics_ca_rs::EpicsValue),
    /// A scalar ENUM value written by name. The server resolves it
    /// against the record's menu — see `CaChannel::put_string`.
    EnumString(String),
    /// An ENUM waveform written by name — each element a `DBR_STRING`
    /// the server resolves against the record's menu. See
    /// `CaChannel::put_string_array`.
    EnumStringArray(Vec<String>),
}

impl WriteValue {
    /// Value used to echo `New :` if the post-put read-back fails.
    fn echo_fallback(&self) -> epics_ca_rs::EpicsValue {
        match self {
            WriteValue::Typed(v) => v.clone(),
            WriteValue::EnumString(s) => epics_ca_rs::EpicsValue::String(s.clone()),
            WriteValue::EnumStringArray(v) => epics_ca_rs::EpicsValue::StringArray(v.clone()),
        }
    }
}

/// Build the value to write, in C `caput`'s precedence order. The single
/// owner of that precedence — `-S` (long string / charArrAsStr) is
/// resolved FIRST, before any native-type parse, mirroring C handling
/// charArrAsStr ahead of normal string conversion (`caput.c:514`).
/// Otherwise `caput -S PV hello` against a native DBF_CHAR would die
/// parsing "hello" as a numeric char: the old code parsed the native type
/// up front and exited on the parse error before ever applying `-S`.
/// Non-fatal `-a` count-token mismatches warn on stderr; fatal cases
/// return `Err` with the full message for the caller to print.
fn build_write_value(
    values: &[String],
    native_type: epics_ca_rs::DbFieldType,
    force_numeric: bool,
    force_string: bool,
    long_string: bool,
    array_mode: bool,
) -> Result<WriteValue, String> {
    // `-S` long-string put: bytes as a NUL-terminated DBR_CHAR array.
    // Highest precedence — no native-type parse on this path.
    if long_string {
        let joined = values.join(" ");
        let mut bytes: Vec<u8> = joined.into_bytes();
        bytes.push(0);
        return Ok(WriteValue::Typed(epics_ca_rs::EpicsValue::CharArray(bytes)));
    }

    if array_mode {
        // C `caput -a` (caput.c:413-418): after the PV name it skips the
        // count token (`optind++`) and uses ALL remaining values — the
        // count number is informational only. `values[0]` is the count
        // token, `[1..]` the actual values.
        let tokens = &values[1..];
        if let Ok(want) = values[0].parse::<usize>() {
            if want != tokens.len() {
                eprintln!(
                    "caput-rs: warning: -a count {} differs from {} values supplied; \
                     using all {} (C-parity)",
                    want,
                    tokens.len(),
                    tokens.len()
                );
            }
        } else {
            // C does not parse the count token at all — a non-numeric
            // token is silently skipped. Mirror that, no hard error.
            eprintln!(
                "caput-rs: warning: -a count token '{}' is not a number; \
                 ignored (C-parity)",
                values[0]
            );
        }
        if tokens.is_empty() {
            return Err("caput-rs: -a requires at least one value after the count token".into());
        }
        // ENUM waveform special-casing, parallel to the scalar path:
        // unless `-n` forces numeric, route to a DBR_STRING array for
        // server-side menu resolution when `-s` is set or any element is
        // not a plain integer index (the same documented divergence as
        // the scalar path).
        let enum_by_name = native_type == epics_ca_rs::DbFieldType::Enum
            && !force_numeric
            && (force_string || tokens.iter().any(|t| parse_plain_integer(t).is_none()));
        if enum_by_name {
            return Ok(WriteValue::EnumStringArray(tokens.to_vec()));
        }
        return parse_array(native_type, tokens)
            .map(WriteValue::Typed)
            .map_err(|e| format!("error: {e}"));
    }

    // Scalar: C `caput` joins extra positionals with single spaces.
    let joined = values.join(" ");
    // ENUM special treatment (C `caput.c:455-510`, not a strict mirror —
    // we don't fetch the menu, we let the server classify):
    // * `-n` (force_numeric): interpret as a number.
    // * `-s` (force_string): always DBR_STRING; server resolves the menu.
    // * default: a plain integer index goes numeric; anything else is
    //   sent as DBR_STRING for server-side menu resolution.
    let is_plain_integer = parse_plain_integer(&joined).is_some();
    if native_type == epics_ca_rs::DbFieldType::Enum
        && !force_numeric
        && (force_string || !is_plain_integer)
    {
        return Ok(WriteValue::EnumString(joined));
    }
    epics_ca_rs::EpicsValue::parse(native_type, &joined)
        .map(WriteValue::Typed)
        .map_err(|e| format!("error: {e}"))
}

/// Parse a plain integer index — decimal, optionally signed, no radix
/// prefix and no surrounding garbage. C `caput` treats anything that
/// is not a clean number as an ENUM menu string. Returns `Some` only
/// for a strict integer literal.
fn parse_plain_integer(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok()
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
        DT::UInt64 => {
            let mut arr = Vec::with_capacity(tokens.len());
            for t in tokens {
                arr.push(t.parse::<u64>().map_err(|e| e.to_string())?);
            }
            Ok(EpicsValue::UInt64Array(arr))
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

#[cfg(test)]
mod tests {
    use super::{WriteValue, build_write_value};
    use epics_ca_rs::{DbFieldType, EpicsValue};

    fn vals(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn long_string_takes_precedence_over_char_parse() {
        // `caput -S PV hello` against a native DBF_CHAR must send the
        // bytes as a NUL-terminated DBR_CHAR array, NOT parse "hello" as a
        // numeric char. The old order parsed first and exited on the parse
        // error before `-S` was ever applied.
        let r = build_write_value(
            &vals(&["hello"]),
            DbFieldType::Char,
            false,
            false,
            true,
            false,
        );
        match r {
            Ok(WriteValue::Typed(EpicsValue::CharArray(bytes))) => {
                assert_eq!(bytes, b"hello\0", "NUL-terminated long string bytes");
            }
            _ => panic!("expected Ok(CharArray) for -S on a CHAR PV"),
        }
    }

    #[test]
    fn long_string_precedence_holds_for_every_native_type() {
        // `-S` is type-independent: it never reaches the native-type
        // parse, so no native type can make it fail.
        for nt in [
            DbFieldType::Char,
            DbFieldType::Double,
            DbFieldType::Enum,
            DbFieldType::Long,
            DbFieldType::String,
        ] {
            let r = build_write_value(&vals(&["not a number"]), nt, false, false, true, false);
            assert!(
                matches!(r, Ok(WriteValue::Typed(EpicsValue::CharArray(_)))),
                "-S must yield CharArray for native type {nt:?}"
            );
        }
    }

    #[test]
    fn scalar_char_without_long_string_still_parses_numeric() {
        // Without `-S`, a native CHAR scalar parses the token as a number;
        // a non-numeric token is an error, exactly as before the fix.
        assert!(
            build_write_value(
                &vals(&["65"]),
                DbFieldType::Char,
                false,
                false,
                false,
                false
            )
            .is_ok()
        );
        assert!(
            build_write_value(
                &vals(&["hello"]),
                DbFieldType::Char,
                false,
                false,
                false,
                false
            )
            .is_err()
        );
    }
}
