use chrono::{DateTime, Local};
use clap::Parser;
use epics_base_rs::server::snapshot::{DbrClass, Snapshot};
use epics_ca_rs::CaError;
use epics_ca_rs::cli::{PV_NAME_WIDTH, ValueFormat, format_value};
use epics_ca_rs::client::{CaChannel, CaClient, enum_string_readback_dbr};
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

    // -p selects the priority virtual circuit.
    let ch = client.create_channel_with_priority(&pv_name, args.priority.unwrap_or(0));
    if let Err(e) = ch.wait_connected(timeout).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    // The channel's native field type drives how the value to WRITE is
    // encoded (C `ca_field_type`, caput.c:143) — it must stay the real
    // native type even when the readback below is taken in STRING form.
    let native_type = match ch.native_field_type() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    // C caput.c:147-152: the readback (the `Old :`/`New :` display) is
    // requested in the STRING form for an ENUM field unless `-n`, so the
    // echoed value is the state label, not the index. `-l` keeps the
    // TIME-class string so the timestamp + alarm line is still populated.
    let enum_dbr = enum_string_readback_dbr(native_type, args.long_mode, !args.force_numeric);

    // Read the pre-put value for the `Old :` display. Long mode also
    // wants the server timestamp + alarm pair captured BEFORE the put so
    // the `Old :` line reflects the actual pre-put state — the regular
    // path stays on the cheaper plain GET.
    let long_mode = args.long_mode;
    let read_display = move |ch: CaChannel| async move {
        match (long_mode, enum_dbr) {
            // Long mode + ENUM default: DBR_TIME_STRING (label + ts/alarm).
            (true, Some(rt)) => ch
                .get_with_dbr_type(rt, 0)
                .await
                .map(|s| (s.value.clone(), Some(s))),
            // Long mode, other fields: native DBR_TIME class.
            (true, None) => ch
                .get_with_metadata(DbrClass::Time)
                .await
                .map(|s| (s.value.clone(), Some(s))),
            // Plain + ENUM default: DBR_STRING (label only).
            (false, Some(rt)) => ch.get_with_dbr_type(rt, 0).await.map(|s| (s.value, None)),
            // Plain, other fields: native plain GET.
            (false, None) => ch.get_with_timeout(timeout).await.map(|(_t, v)| (v, None)),
        }
    };
    // C caput.c:532-535 gates the pre-put "Old :" read+print on
    // `if (format != terse)`. Terse mode prints only the new value, so the
    // pre-put GET must NOT be issued: C never issues it, and a PV that is slow
    // to read, read-denied before a write-side access transition, or backed by
    // an expensive/side-effecting read path must still proceed to the write.
    // The post-put read below is kept in every mode (C still calls caget()
    // after the put, caput.c:583; terse only suppresses the `New :` label).
    let (old_value, old_snap) = if args.terse {
        (None, None)
    } else {
        match read_display(ch.clone()).await {
            Ok((v, s)) => (Some(v), s),
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

    // build the value to write in C's precedence order — `-S`
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
        WriteValue::Wire { dbr_type, value } => {
            // C-tool wire model: send the explicit DBR type (DBR_STRING /
            // DBR_CHAR) and let the server convert. The CLI `-w` timeout
            // owns the callback wait, matching caput.c:556-567.
            if args.callback {
                ch.put_as_dbr_with_timeout(*dbr_type, value, timeout).await
            } else {
                ch.put_as_dbr_nowait(*dbr_type, value).await
            }
        }
        WriteValue::EnumString(s) => {
            // ENUM-by-name → DBR_STRING; the server resolves the menu
            // string. Route through the same explicit-wire-type path as
            // the numeric/-S writes so the CLI `-w` timeout owns the
            // callback wait (caput.c:558-567 uses one caTimeout for every
            // dbrType), instead of put_string's EPICS_CA_PUT_TIMEOUT/30s
            // default which dropped `-w`.
            let v = epics_ca_rs::EpicsValue::String(s.clone());
            let dbr = epics_ca_rs::DbFieldType::String as u16;
            if args.callback {
                ch.put_as_dbr_with_timeout(dbr, &v, timeout).await
            } else {
                ch.put_as_dbr_nowait(dbr, &v).await
            }
        }
        WriteValue::EnumStringArray(v) => {
            // ENUM waveform by name — DBR_STRING array, server resolves
            // each element. Same single timeout owner as above.
            let arr = epics_ca_rs::EpicsValue::StringArray(v.clone());
            let dbr = epics_ca_rs::DbFieldType::String as u16;
            if args.callback {
                ch.put_as_dbr_with_timeout(dbr, &arr, timeout).await
            } else {
                ch.put_as_dbr_nowait(dbr, &arr).await
            }
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    // Re-read for echoing to stdout (matches C caput which always
    // reads the PV back after the put, caput.c:583). Same readback type
    // selection as the `Old :` read above, so an ENUM `New :` value also
    // echoes as the state label. `echo_fallback` is shown if the post-put
    // read fails — just what was written.
    let echo_fallback = parsed_value.echo_fallback();
    let (new_value, new_snap) = match read_display(ch.clone()).await {
        Ok(pair) => pair,
        Err(_) => (echo_fallback.clone(), None),
    };

    let mut fmt = ValueFormat::default();
    if let Some(c) = args.field_separator {
        fmt.field_separator = c;
    }
    let sep = fmt.field_separator;
    // In terse mode `old_value` is `None` (no pre-put read was issued) and the
    // rendered old value is unused; non-terse modes always carry `Some`.
    let old_rendered = old_value
        .as_ref()
        .map(|v| format_value(v, &fmt, None, false))
        .unwrap_or_default();
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
#[derive(Debug)]
enum WriteValue {
    /// A native-typed write sent as the channel's native DBR type via
    /// `CaChannel::put*`. Used only for ENUM numeric-index values, which
    /// C `caput` writes as the numeric type (`caput.c:474-481`,
    /// `enumAsNr`); every other CLI value is converted by the server from
    /// an explicit-wire-type [`WriteValue::Wire`].
    Typed(epics_ca_rs::EpicsValue),
    /// An explicit-wire-type write: the tool picks the DBR wire type and
    /// the server converts to the native field type. C `caput` sends a
    /// non-ENUM value as `DBR_STRING` (`caput.c:540-552`) and an `-S`
    /// long string as `DBR_CHAR` (`caput.c:531-538`), never the native
    /// binary type. Routed through `CaChannel::put_as_dbr_*`.
    Wire {
        dbr_type: u16,
        value: epics_ca_rs::EpicsValue,
    },
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
            WriteValue::Wire { value, .. } => value.clone(),
            WriteValue::EnumString(s) => epics_ca_rs::EpicsValue::String(s.clone()),
            WriteValue::EnumStringArray(v) => epics_ca_rs::EpicsValue::StringArray(v.clone()),
        }
    }
}

/// Port of EPICS `epicsStrnRawFromEscaped` (`libcom/.../epicsString.c`):
/// decode C escape sequences in `s` to their raw byte values.
///
/// `\a \b \f \n \r \t \v` → the control byte; `\\ \' \"` → the literal
/// char; `\0` → a NUL byte; `\xH`/`\xHH` → the hex byte (1-2 digits, a
/// following non-hex char is re-processed normally); any other `\c` → the
/// literal `c`. A trailing lone `\`, or a literal NUL in the input, stops
/// decoding. Note C does NOT decode multi-digit octal — only `\0`.
///
/// `caput` feeds string- and char-array-destined values through this so a
/// value like `'a\tb'` is sent with a real TAB byte, matching the C tool
/// (`caput.c:487,512,520`); the pre-fix Rust left `\n`/`\xNN` literal.
fn raw_from_escaped(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    'next: while i < b.len() {
        let mut c = b[i];
        i += 1;
        loop {
            if c == 0 {
                // Literal NUL in the input stops decoding (C `if (!c)`).
                return out;
            }
            if c != b'\\' {
                out.push(c);
                continue 'next;
            }
            // Backslash: a trailing lone `\` stops decoding.
            if i >= b.len() {
                return out;
            }
            c = b[i];
            i += 1;
            match c {
                b'a' => out.push(0x07),
                b'b' => out.push(0x08),
                b'f' => out.push(0x0C),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'v' => out.push(0x0B),
                b'\\' => out.push(b'\\'),
                b'\'' => out.push(b'\''),
                b'"' => out.push(b'"'),
                b'0' => out.push(0),
                b'x' => {
                    // 1-2 hex digits; a following non-hex char is
                    // re-processed normally (C `goto input`).
                    if i >= b.len() {
                        return out;
                    }
                    c = b[i];
                    i += 1;
                    let Some(hi) = (c as char).to_digit(16) else {
                        // Not a hex digit: re-process `c` from the top.
                        continue;
                    };
                    let u = hi as u8;
                    if i >= b.len() {
                        out.push(u);
                        return out;
                    }
                    c = b[i];
                    i += 1;
                    match (c as char).to_digit(16) {
                        Some(lo) => out.push(u << 4 | lo as u8),
                        None => {
                            // One hex digit, then re-process `c` (C `goto
                            // input`); a NUL `c` is caught at the loop top
                            // and stops decoding, matching C `goto done`.
                            out.push(u);
                            continue;
                        }
                    }
                }
                other => out.push(other),
            }
            continue 'next;
        }
    }
    out
}

/// Max raw payload of a CA `DBR_STRING` element: `MAX_STRING_SIZE` (40)
/// minus the trailing NUL. C `caput` decodes each string/ENUM-name value
/// into a fixed `EpicsStr[MAX_STRING_SIZE]` buffer, so at most 39 raw
/// bytes survive (`epicsStrnRawFromEscaped` writes while `--rem > 0` from
/// `rem = 40`, then NUL-terminates — epicsString.c:55-118).
const DBR_STRING_PAYLOAD_MAX: usize = 39;

/// `raw_from_escaped` decoded into a `String` for the DBR_STRING / ENUM-
/// by-name put paths. The common escapes (`\n`, `\t`, `\\`, …) decode to
/// ASCII; a high-byte `\xNN` that is not valid UTF-8 falls back to lossy
/// decoding (the Rust `EpicsValue::String` is UTF-8, whereas C carries a
/// raw byte buffer).
///
/// The decoded value is truncated to [`DBR_STRING_PAYLOAD_MAX`] bytes, the
/// way C `caput` decodes into its fixed `EpicsStr` buffer and forces a
/// trailing NUL (caput.c:484-489 for ENUM names, caput.c:523-528 for
/// native DBR_STRING): an overlong CLI value is written as its 39-byte
/// prefix, not rejected. The Rust client's `validate_put_strings` /
/// libca's `nciu::stringVerify` reject `>= 40`, so the cap must happen in
/// the CLI builder to keep C-tool parity. `-S` long strings take the
/// DBR_CHAR path (`raw_from_escaped`, not this helper) and stay uncapped.
/// The truncation lands on a UTF-8 char boundary — C's buffer is
/// byte-oriented, but for the printable/ASCII values this targets the two
/// coincide.
fn raw_from_escaped_string(s: &str) -> String {
    let mut decoded = String::from_utf8_lossy(&raw_from_escaped(s)).into_owned();
    if decoded.len() > DBR_STRING_PAYLOAD_MAX {
        let mut end = DBR_STRING_PAYLOAD_MAX;
        while !decoded.is_char_boundary(end) {
            end -= 1;
        }
        decoded.truncate(end);
    }
    decoded
}

/// Build the value to write, in C `caput`'s precedence order
/// (`caput.c:454-530`): the ENUM field type is handled FIRST, then `-S`
/// (charArrAsStr → DBR_CHAR) for a NON-ENUM PV, then the DBR_STRING /
/// numeric paths. `-a` (array) resets charArrAsStr in C (`caput.c:318`),
/// so the array path takes precedence over `-S`. String- and char-array-
/// destined values are escape-decoded via [`raw_from_escaped`]; numeric
/// values are parsed from the raw token (C runs `epicsStrtod` on the
/// original argv). In `-a` mode the leading count token is skipped
/// without parsing (C `caput.c:413-418`); a per-element parse failure
/// returns `Err` with the full message for the caller to print.
fn build_write_value(
    values: &[String],
    native_type: epics_ca_rs::DbFieldType,
    force_numeric: bool,
    force_string: bool,
    long_string: bool,
    array_mode: bool,
) -> Result<WriteValue, String> {
    if array_mode {
        // C `caput -a` (caput.c:413-418): after the PV name it skips the
        // count token (`optind++`) WITHOUT parsing it, then derives the
        // real count from `argc - optind`. The token is purely
        // informational positional-compatibility syntax — C never
        // validates it against the supplied values, never errors on a
        // non-numeric token, and reaches the write path with `count == 0`
        // when no values follow. Mirror that: skip `values[0]` silently
        // and let `values[1..]` (possibly empty) flow to the write, so a
        // zero-count put is decided by the server/libca, not by CLI
        // argument parsing.
        let tokens = &values[1..];
        // ENUM waveform special-casing, parallel to the scalar path:
        // unless `-n` forces numeric, route to a DBR_STRING array for
        // server-side menu resolution when `-s` is set or any element is
        // not a plain integer index (the same documented divergence as
        // the scalar path). C escapes each enum-name element (caput.c:487).
        let enum_by_name = native_type == epics_ca_rs::DbFieldType::Enum
            && !force_numeric
            && (force_string || tokens.iter().any(|t| parse_plain_integer(t).is_none()));
        if enum_by_name {
            let escaped = tokens.iter().map(|t| raw_from_escaped_string(t)).collect();
            return Ok(WriteValue::EnumStringArray(escaped));
        }
        // ENUM numeric-index waveform stays native — the server takes the
        // index directly. This is the documented ENUM divergence, not the
        // non-ENUM string-conversion path below.
        if native_type == epics_ca_rs::DbFieldType::Enum {
            return parse_array(native_type, tokens)
                .map(WriteValue::Typed)
                .map_err(|e| format!("error: {e}"));
        }
        // Non-ENUM array: C sends every element as a DBR_STRING after
        // epicsStrnRawFromEscaped (caput.c:540-552), regardless of the
        // native numeric or string field type — the server converts each.
        let escaped: Vec<String> = tokens.iter().map(|t| raw_from_escaped_string(t)).collect();
        return Ok(WriteValue::Wire {
            dbr_type: epics_ca_rs::DbFieldType::String as u16,
            value: epics_ca_rs::EpicsValue::StringArray(escaped),
        });
    }

    // Scalar: C `caput` joins extra positionals with single spaces.
    let joined = values.join(" ");

    // (1) ENUM field type is handled FIRST (caput.c:455), BEFORE `-S` —
    // charArrAsStr never applies to an ENUM PV. We don't fetch the menu;
    // we let the server classify:
    // * `-n` (force_numeric): interpret as a number.
    // * `-s` (force_string): always DBR_STRING; server resolves the menu.
    // * default: a plain integer index goes numeric; anything else is
    //   sent as DBR_STRING for server-side menu resolution (escaped, as
    //   C runs epicsStrnRawFromEscaped on the menu name, caput.c:487).
    if native_type == epics_ca_rs::DbFieldType::Enum {
        let is_plain_integer = parse_plain_integer(&joined).is_some();
        if !force_numeric && (force_string || !is_plain_integer) {
            return Ok(WriteValue::EnumString(raw_from_escaped_string(&joined)));
        }
        return epics_ca_rs::EpicsValue::parse(native_type, &joined)
            .map(WriteValue::Typed)
            .map_err(|e| format!("error: {e}"));
    }

    // (2) `-S` (charArrAsStr) on a NON-ENUM PV → NUL-terminated DBR_CHAR
    // array built from the escape-decoded bytes, sent with the explicit
    // DBR_CHAR wire type and count = nbytes + NUL (caput.c:531-538:
    // `dbrType = DBR_CHAR; count = epicsStrnRawFromEscaped(...) + 1`).
    // The wire type must be DBR_CHAR regardless of the channel native
    // type — sending these char bytes under the native header
    // (DBR_STRING/DBR_DOUBLE/…) is a malformed/rejected write.
    if long_string {
        let mut bytes = raw_from_escaped(&joined);
        bytes.push(0);
        return Ok(WriteValue::Wire {
            dbr_type: epics_ca_rs::DbFieldType::Char as u16,
            value: epics_ca_rs::EpicsValue::CharArray(bytes),
        });
    }

    // (3) Non-ENUM, non-`-S`: C sends the value as DBR_STRING after
    // epicsStrnRawFromEscaped (caput.c:540-552), regardless of the native
    // numeric or string field type — the server/IOC performs the
    // string->native conversion. This matches C `caput`'s wire model; the
    // programmatic native-typed write stays on `CaChannel::put`.
    Ok(WriteValue::Wire {
        dbr_type: epics_ca_rs::DbFieldType::String as u16,
        value: epics_ca_rs::EpicsValue::String(raw_from_escaped_string(&joined)),
    })
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
    use super::{WriteValue, build_write_value, raw_from_escaped};
    use epics_ca_rs::{DbFieldType, EpicsValue};

    fn vals(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn raw_from_escaped_matches_epics_strn_raw_from_escaped() {
        // Per-boundary cases against the C `epicsStrnRawFromEscaped`:
        // control escapes, literal escapes, the lone `\0` octal, `\xHH`
        // hex with 1 and 2 digits, `\x` followed by a non-hex char
        // (re-processed), an unknown `\c` (literal `c`), a trailing lone
        // backslash (stops), and a literal NUL in the input (stops).
        assert_eq!(
            raw_from_escaped("\\a\\b\\f\\n\\r\\t\\v"),
            vec![0x07, 0x08, 0x0C, 0x0A, 0x0D, 0x09, 0x0B]
        );
        assert_eq!(raw_from_escaped("\\\\\\'\\\""), vec![b'\\', b'\'', b'"']);
        assert_eq!(raw_from_escaped("\\0"), vec![0]);
        assert_eq!(raw_from_escaped("\\x41"), vec![0x41]); // two hex digits
        assert_eq!(raw_from_escaped("\\xA"), vec![0x0A]); // single trailing hex
        assert_eq!(raw_from_escaped("\\xG"), vec![b'G']); // non-hex re-processed
        assert_eq!(raw_from_escaped("\\q"), vec![b'q']); // unknown escape → literal
        assert_eq!(raw_from_escaped("a\\"), vec![b'a']); // trailing lone backslash stops
        assert_eq!(raw_from_escaped("a\0b"), vec![b'a']); // literal NUL stops decoding
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
            Ok(WriteValue::Wire {
                dbr_type,
                value: EpicsValue::CharArray(bytes),
            }) => {
                assert_eq!(dbr_type, DbFieldType::Char as u16, "DBR_CHAR wire type");
                assert_eq!(bytes, b"hello\0", "NUL-terminated long string bytes");
            }
            _ => panic!("expected Ok(DBR_CHAR Wire CharArray) for -S on a CHAR PV"),
        }
    }

    #[test]
    fn long_string_applies_to_every_non_enum_native_type() {
        // For a NON-ENUM PV `-S` never reaches the native-type parse, so no
        // such native type can make it fail — it always yields a
        // NUL-terminated DBR_CHAR array.
        for nt in [
            DbFieldType::Char,
            DbFieldType::Double,
            DbFieldType::Long,
            DbFieldType::String,
        ] {
            let r = build_write_value(&vals(&["not a number"]), nt, false, false, true, false);
            assert!(
                matches!(
                    r,
                    Ok(WriteValue::Wire {
                        dbr_type,
                        value: EpicsValue::CharArray(_),
                    }) if dbr_type == DbFieldType::Char as u16
                ),
                "-S must yield a DBR_CHAR Wire CharArray for non-ENUM native type {nt:?}"
            );
        }
    }

    #[test]
    fn enum_field_type_wins_over_long_string() {
        // C checks the ENUM field type FIRST (`caput.c:455`), so `-S`
        // (charArrAsStr) never applies to an ENUM PV: a non-integer token
        // routes to DBR_STRING for server-side menu resolution, NOT a
        // DBR_CHAR array. Pre-fix the top-level `-S` block hijacked this.
        let r = build_write_value(
            &vals(&["not a number"]),
            DbFieldType::Enum,
            false,
            false,
            true,
            false,
        );
        match r {
            Ok(WriteValue::EnumString(s)) => assert_eq!(s, "not a number"),
            other => panic!("-S on an ENUM PV must yield EnumString, got {other:?}"),
        }
        // A plain integer index on an ENUM PV still goes numeric even with
        // `-S` set — ENUM precedence, then the default index path.
        let idx = build_write_value(&vals(&["2"]), DbFieldType::Enum, false, false, true, false);
        assert!(
            matches!(idx, Ok(WriteValue::Typed(_))),
            "integer index on ENUM PV stays numeric, got {idx:?}"
        );
    }

    #[test]
    fn long_string_decodes_c_escapes_to_raw_bytes() {
        // `-S` feeds the value through `epicsStrnRawFromEscaped`, so
        // `a\tb\n` becomes real TAB/LF bytes (then the NUL terminator), not
        // the literal backslash sequences the pre-fix `into_bytes()` left.
        let r = build_write_value(
            &vals(&["a\\tb\\n"]),
            DbFieldType::Char,
            false,
            false,
            true,
            false,
        );
        match r {
            Ok(WriteValue::Wire {
                dbr_type,
                value: EpicsValue::CharArray(bytes),
            }) => {
                assert_eq!(dbr_type, DbFieldType::Char as u16, "DBR_CHAR wire type");
                assert_eq!(bytes, vec![b'a', 0x09, b'b', 0x0A, 0x00]);
            }
            other => panic!("expected escape-decoded DBR_CHAR Wire CharArray, got {other:?}"),
        }
    }

    #[test]
    fn native_string_scalar_decodes_c_escapes() {
        // A non-ENUM scalar is escape-decoded and sent as DBR_STRING
        // (`caput.c:540-552`): `\x41` → 'A', `\\` → one backslash, `\q`
        // (unknown) → 'q'. The wire type is DBR_STRING regardless of the
        // native field type — the server converts.
        let r = build_write_value(
            &vals(&["\\x41\\\\\\q"]),
            DbFieldType::String,
            false,
            false,
            false,
            false,
        );
        match r {
            Ok(WriteValue::Wire {
                dbr_type,
                value: EpicsValue::String(s),
            }) => {
                assert_eq!(dbr_type, DbFieldType::String as u16, "DBR_STRING wire type");
                assert_eq!(s, "A\\q");
            }
            other => panic!("expected escape-decoded DBR_STRING Wire, got {other:?}"),
        }
    }

    #[test]
    fn non_enum_numeric_scalar_and_array_send_dbr_string() {
        // CA-RS parity: C `caput` sends every non-ENUM, non-`-S` value as
        // DBR_STRING (`caput.c:540-552`), NOT the native numeric type; the
        // server/IOC performs the string->native conversion. A numeric
        // scalar against a DBF_DOUBLE PV must therefore yield a DBR_STRING
        // Wire carrying the original token, not a parsed EpicsValue::Double.
        match build_write_value(
            &vals(&["1.5"]),
            DbFieldType::Double,
            false,
            false,
            false,
            false,
        ) {
            Ok(WriteValue::Wire {
                dbr_type,
                value: EpicsValue::String(s),
            }) => {
                assert_eq!(dbr_type, DbFieldType::String as u16);
                assert_eq!(s, "1.5");
            }
            other => panic!("numeric scalar must be a DBR_STRING Wire, got {other:?}"),
        }
        // A numeric array (`-a`) sends DBR_STRING[] with count == nvalues,
        // each element the raw token; no native LongArray parse.
        match build_write_value(
            &vals(&["3", "10", "20", "30"]),
            DbFieldType::Long,
            false,
            false,
            false,
            true,
        ) {
            Ok(WriteValue::Wire {
                dbr_type,
                value: EpicsValue::StringArray(a),
            }) => {
                assert_eq!(dbr_type, DbFieldType::String as u16);
                assert_eq!(a, vec!["10", "20", "30"]);
            }
            other => panic!("numeric array must be a DBR_STRING[] Wire, got {other:?}"),
        }
    }

    #[test]
    fn array_count_token_is_skipped_without_parsing_or_error() {
        // C `caput -a` (caput.c:413-418) skips the count token without
        // parsing it: the value is taken from `argc - optind`. A count
        // that disagrees with the supplied values, a non-numeric count,
        // and a count of zero must all reach the write path silently —
        // no warning, no CLI-side rejection.
        // The non-ENUM array path now sends DBR_STRING[] (caput.c:540-552);
        // these assertions check the count-token skip via the element list.
        // `-a PV 999 1 2`: count 999 vs 2 values → use both, no error.
        match build_write_value(
            &vals(&["999", "1", "2"]),
            DbFieldType::Long,
            false,
            false,
            false,
            true,
        ) {
            Ok(WriteValue::Wire {
                value: EpicsValue::StringArray(a),
                ..
            }) => assert_eq!(a, vec!["1", "2"]),
            other => panic!("count mismatch must use all values, got {other:?}"),
        }
        // `-a PV not-a-count 1 2`: non-numeric count token skipped.
        match build_write_value(
            &vals(&["not-a-count", "1", "2"]),
            DbFieldType::Long,
            false,
            false,
            false,
            true,
        ) {
            Ok(WriteValue::Wire {
                value: EpicsValue::StringArray(a),
                ..
            }) => assert_eq!(a, vec!["1", "2"]),
            other => panic!("non-numeric count token must be ignored, got {other:?}"),
        }
        // `-a PV 0`: zero trailing values reaches the write path as an
        // empty array (count == 0), decided by the server — not a CLI error.
        match build_write_value(&vals(&["0"]), DbFieldType::Long, false, false, false, true) {
            Ok(WriteValue::Wire {
                value: EpicsValue::StringArray(a),
                ..
            }) => assert!(a.is_empty()),
            other => panic!("zero-count -a must reach the write path empty, got {other:?}"),
        }
    }

    #[test]
    fn scalar_char_without_long_string_sends_dbr_string() {
        // Without `-S`, a non-ENUM CHAR scalar is sent as DBR_STRING
        // (caput.c:540-552), NOT parsed locally as a number — so a
        // non-numeric token is no longer a CLI-side error (the server
        // performs the conversion and any rejection). This is distinct
        // from `-S`, which sends a DBR_CHAR byte array.
        for tok in ["65", "hello"] {
            match build_write_value(&vals(&[tok]), DbFieldType::Char, false, false, false, false) {
                Ok(WriteValue::Wire {
                    dbr_type,
                    value: EpicsValue::String(s),
                }) => {
                    assert_eq!(dbr_type, DbFieldType::String as u16);
                    assert_eq!(s, tok);
                }
                other => panic!("CHAR scalar without -S must be a DBR_STRING Wire, got {other:?}"),
            }
        }
    }

    #[test]
    fn overlong_dbr_string_values_truncate_to_39_bytes() {
        // C `caput` decodes each DBR_STRING / ENUM-name value into a fixed
        // EpicsStr[40] buffer and keeps 39 raw bytes (caput.c:484-489,
        // 523-528); an overlong value is TRUNCATED before libca, not
        // rejected. The Rust client rejects >= 40, so caput-rs must cap.
        let long = "a".repeat(50); // 50 ASCII bytes
        // Non-ENUM string scalar -> DBR_STRING capped to 39.
        match build_write_value(
            &vals(&[long.as_str()]),
            DbFieldType::String,
            false,
            false,
            false,
            false,
        ) {
            Ok(WriteValue::Wire {
                value: EpicsValue::String(s),
                ..
            }) => {
                assert_eq!(s.len(), 39, "scalar string truncated to 39 bytes");
                assert_eq!(s, "a".repeat(39));
            }
            other => panic!("expected truncated DBR_STRING Wire, got {other:?}"),
        }
        // Non-ENUM string array element -> each capped to 39 (the leading
        // token is the skipped -a count).
        match build_write_value(
            &vals(&["2", long.as_str(), "short"]),
            DbFieldType::String,
            false,
            false,
            false,
            true,
        ) {
            Ok(WriteValue::Wire {
                value: EpicsValue::StringArray(a),
                ..
            }) => {
                assert_eq!(a[0].len(), 39, "array element truncated to 39 bytes");
                assert_eq!(a[1], "short");
            }
            other => panic!("expected truncated DBR_STRING[] Wire, got {other:?}"),
        }
        // ENUM-by-name scalar (`-s`) -> EnumString capped to 39.
        match build_write_value(
            &vals(&[long.as_str()]),
            DbFieldType::Enum,
            false,
            true,
            false,
            false,
        ) {
            Ok(WriteValue::EnumString(s)) => {
                assert_eq!(s.len(), 39, "enum-by-name value truncated to 39 bytes")
            }
            other => panic!("expected truncated EnumString, got {other:?}"),
        }
        // `-S` long strings take the DBR_CHAR path and stay UNCAPPED — all
        // 50 bytes + NUL survive (finding: -S is not 39-byte limited).
        match build_write_value(
            &vals(&[long.as_str()]),
            DbFieldType::Char,
            false,
            false,
            true,
            false,
        ) {
            Ok(WriteValue::Wire {
                dbr_type,
                value: EpicsValue::CharArray(b),
            }) => {
                assert_eq!(dbr_type, DbFieldType::Char as u16);
                assert_eq!(b.len(), 51, "-S keeps all 50 bytes + NUL, uncapped");
            }
            other => panic!("expected uncapped DBR_CHAR Wire, got {other:?}"),
        }
    }
}
