use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use chrono::{DateTime, Local};
use clap::Parser;
use epics_ca_rs::cli::{
    FloatFormat, FloatStyle, IntStyle, PV_NAME_WIDTH, ValueFormat, format_value,
};
use epics_ca_rs::client::{CaChannel, CaClient, ConnectionEvent};

const VERSION_INFO: &str = concat!(
    "\nEPICS Version epics-rs ",
    env!("CARGO_PKG_VERSION"),
    ", CA Protocol version 4.13"
);

/// Mirror of C `camonitor` flags. The flag set is mostly the same as
/// `caget` minus `-t`/`-a`/`-d` and plus `-m`/`-t<key>`. We model the
/// CLI to match — including the parity-only flags so existing scripts
/// don't break.
#[derive(Parser)]
#[command(
    name = "camonitor-rs",
    about = "Monitor EPICS PVs for changes",
    disable_version_flag = true
)]
struct Args {
    #[arg(short = 'V', long, hide = true)]
    version: bool,

    /// CA timeout in seconds (initial connection wait). Mirrors C
    /// `tool_lib.c:use_ca_timeout_env` (commit 1d056c6).
    #[arg(short = 'w', long = "wait")]
    timeout: Option<f64>,

    /// CA event mask `<msk>`: any combination of `v` (value), `a`
    /// (alarm), `l` (log/archive), `p` (property). The subscription is
    /// issued with the resulting DBE_* mask; absent → value+log+alarm.
    #[arg(short = 'm', long, value_name = "MASK")]
    event_mask: Option<String>,

    /// CA priority (0-99). Accepted for parity.
    #[arg(short = 'p', long)]
    priority: Option<u8>,

    /// Timestamp rendering: `s`=server (default), `n`=none,
    /// `r`=relative since program start, `i`=incremental across all
    /// channels, `I`=incremental per channel.
    #[arg(short = 't', long = "timestamp", value_name = "KEY")]
    timestamp_key: Option<String>,

    #[arg(short = 'n', long = "num-enum")]
    enum_as_number: bool,

    #[arg(short = '#', long = "max-elements", value_name = "COUNT")]
    max_elements: Option<usize>,

    #[arg(short = 'S', long = "char-as-string")]
    char_array_as_string: bool,

    #[arg(short = 'e', long = "format-e", value_name = "PRECISION")]
    fmt_e: Option<u32>,
    #[arg(short = 'f', long = "format-f", value_name = "PRECISION")]
    fmt_f: Option<u32>,
    #[arg(short = 'g', long = "format-g", value_name = "PRECISION")]
    fmt_g: Option<u32>,

    #[arg(short = 's', long = "string-format")]
    string_format: bool,

    #[arg(long = "lx", conflicts_with_all = ["lo_flag", "lb_flag", "ix_flag", "io_flag", "ib_flag"])]
    lx_flag: bool,
    #[arg(long = "lo", conflicts_with_all = ["lx_flag", "lb_flag", "ix_flag", "io_flag", "ib_flag"])]
    lo_flag: bool,
    #[arg(long = "lb", conflicts_with_all = ["lx_flag", "lo_flag", "ix_flag", "io_flag", "ib_flag"])]
    lb_flag: bool,
    #[arg(long = "0x", conflicts_with_all = ["io_flag", "ib_flag"])]
    ix_flag: bool,
    #[arg(long = "0o", conflicts_with_all = ["ix_flag", "ib_flag"])]
    io_flag: bool,
    #[arg(long = "0b", conflicts_with_all = ["ix_flag", "io_flag"])]
    ib_flag: bool,

    /// Alternate output field separator. Defaults to a single space.
    /// Mirrors C `camonitor.c:342` (`case 'F'`).
    #[arg(short = 'F', long = "field-separator", value_name = "OFS")]
    field_separator: Option<char>,

    /// PV names to monitor.
    #[arg(required_unless_present_any = ["version"])]
    pv_names: Vec<String>,
}

impl Args {
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

    if args.priority.is_some() {
        eprintln!("camonitor-rs: -p (priority) is accepted for parity but not yet honoured");
    }
    if args.string_format {
        eprintln!("camonitor-rs: -s (string format) is accepted for parity but not yet honoured");
    }

    let client = CaClient::new().await.expect("failed to create CA client");

    let connected_flags: Vec<Arc<AtomicBool>> = args
        .pv_names
        .iter()
        .map(|_| Arc::new(AtomicBool::new(false)))
        .collect();

    let fmt = Arc::new(args.value_format());
    // C `tool_lib.c:486` (`PRN_TIME_VAL_STS`) gates the array count
    // prefix on `reqElems || nElems > 1`; `reqElems` is non-zero iff
    // the user passed `-#`.
    let req_elems_present = args.max_elements.is_some();
    // CA-FR-4: resolve the `-m <msk>` DBE_* mask + `-t` timestamp mode
    // once for all PVs. `prev_all`/`start` back the relative and
    // incremental timestamp renderings.
    let mask = parse_event_mask(args.event_mask.as_deref());
    let tmode = parse_timestamp_mode(args.timestamp_key.as_deref());
    let start = SystemTime::now();
    let prev_all = Arc::new(std::sync::Mutex::new(None::<SystemTime>));

    let mut handles = Vec::new();
    for (i, pv_name) in args.pv_names.iter().enumerate() {
        let channel = client.create_channel(pv_name);
        let pv = pv_name.clone();
        let flag = connected_flags[i].clone();
        let fmt = fmt.clone();
        let prev_all = prev_all.clone();
        handles.push(tokio::spawn(async move {
            monitor_pv(
                channel,
                pv,
                flag,
                fmt,
                req_elems_present,
                mask,
                tmode,
                start,
                prev_all,
            )
            .await;
        }));
    }

    // Initial connection wait (C: ca_pend_event(caTimeout))
    let timeout_secs = args
        .timeout
        .unwrap_or_else(epics_ca_rs::cli::env_default_timeout);
    tokio::time::sleep(epics_ca_rs::cli::timeout_duration(timeout_secs)).await;

    // Print "*** Not connected" for PVs that didn't connect within
    // the wait window. Mirrors `tool_lib.c::print_time_val_sts` line
    // 521 — "*** Not connected (PV not found)". Honor `-F`: emit the
    // field separator between the name and the message, and pad the
    // name to 30 only with the default space separator. C's full rule
    // also suppresses padding for an array PV (`nElems > 1`); a
    // not-connected PV carries no element count here, so we gate on
    // the separator alone — identical to C for the common scalar /
    // no-`-#` case.
    let sep = args.field_separator.unwrap_or(' ');
    for (i, pv_name) in args.pv_names.iter().enumerate() {
        if !connected_flags[i].load(Ordering::Acquire) {
            let name_col = if sep == ' ' {
                format!("{pv_name:<width$}", width = PV_NAME_WIDTH)
            } else {
                pv_name.clone()
            };
            println!("{name_col}{sep}*** Not connected (PV not found)");
        }
    }

    for handle in handles {
        let _ = handle.await;
    }
}

fn format_server_timestamp(ts: SystemTime) -> String {
    let dt: DateTime<Local> = ts.into();
    dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

/// Map alarm severity index → C `sevr_to_str` mnemonic. Out-of-range
/// values fall back to the integer rendering, matching libca which
/// returns "Illegal value" on overflow.
fn sevr_to_str(sevr: u16) -> &'static str {
    match sevr {
        0 => "NO_ALARM",
        1 => "MINOR",
        2 => "MAJOR",
        3 => "INVALID",
        _ => "Illegal value",
    }
}

/// Map alarm status index → C `stat_to_str` mnemonic. The full set
/// mirrors `libcom/src/misc/alarmString.h` (epics-base 7.0).
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

#[allow(clippy::too_many_arguments)]
async fn monitor_pv(
    channel: CaChannel,
    pv_name: String,
    connected_flag: Arc<AtomicBool>,
    fmt: Arc<ValueFormat>,
    req_elems_present: bool,
    mask: u16,
    tmode: TimestampMode,
    start: SystemTime,
    prev_all: Arc<std::sync::Mutex<Option<SystemTime>>>,
) {
    // Per-channel previous timestamp for `-t I`.
    let mut prev_chan: Option<SystemTime> = None;
    let mut conn_rx = channel.connection_events();
    let pv = pv_name.clone();
    let flag = connected_flag.clone();
    let sep = fmt.field_separator;
    tokio::spawn(async move {
        while let Ok(evt) = conn_rx.recv().await {
            match evt {
                ConnectionEvent::Connected => {
                    flag.store(true, Ordering::Release);
                }
                ConnectionEvent::Disconnected => {
                    let now = Local::now().format("%Y-%m-%d %H:%M:%S%.6f");
                    // C `tool_lib.c::print_time_val_sts` ECA_DISCONN
                    // branch: `name <sep> ts *** disconnected`. Pad the
                    // name to 30 only with the default space separator.
                    // C also suppresses padding for an array PV; the
                    // disconnect event carries no element count, so we
                    // gate on the separator alone — identical to C for
                    // the common scalar case.
                    let name_col = if sep == ' ' {
                        format!("{pv:<width$}", width = PV_NAME_WIDTH)
                    } else {
                        pv.clone()
                    };
                    println!("{name_col}{sep}{now} *** disconnected");
                }
                _ => {}
            }
        }
    });

    // CA-FR-4: honour `-m <msk>` via the caller-resolved DBE_* mask.
    let Ok(mut monitor) = channel.subscribe_with_mask(0.0, mask).await else {
        return;
    };
    while let Some(result) = monitor.recv().await {
        match result {
            Ok(snap) => {
                // CA-FR-4: `-t` selects the timestamp column rendering.
                // `IncrAll` shares state across channels; `IncrChan`/the
                // others use the per-channel slot.
                let time_seg = if tmode == TimestampMode::IncrAll {
                    let mut p = prev_all.lock().unwrap();
                    render_timestamp(tmode, snap.timestamp, start, &mut p)
                } else {
                    render_timestamp(tmode, snap.timestamp, start, &mut prev_chan)
                }
                .map(|ts| format!("{ts}{sep}"))
                .unwrap_or_default();
                let enum_strings = snap.enums.as_ref().map(|e| e.strings.as_slice());
                let rendered = format_value(&snap.value, &fmt, enum_strings, req_elems_present);
                let is_scalar = snap.value.count() == 1;
                let name_col = if is_scalar && sep == ' ' {
                    format!("{pv_name:<width$}", width = PV_NAME_WIDTH)
                } else {
                    pv_name.clone()
                };
                let stat = snap.alarm.status;
                let sevr = snap.alarm.severity;
                if stat == 0 && sevr == 0 {
                    // C `tool_lib.c` line 500: print `<sep><sep>` —
                    // two empty alarm fields trailing the value.
                    println!("{name_col}{sep}{time_seg}{rendered}{sep}{sep}");
                } else {
                    println!(
                        "{name_col}{sep}{time_seg}{rendered}{sep}{stat_str}{sep}{sevr_str}",
                        stat_str = stat_to_str(stat),
                        sevr_str = sevr_to_str(sevr),
                    );
                }
            }
            Err(e) => {
                eprintln!("{pv_name}: {e}");
            }
        }
    }
}

/// CA-FR-4: parse a `camonitor -m <msk>` mask string into `DBE_*` bits.
/// Letters: `v` value, `a` alarm, `l` log/archive, `p` property
/// (epics-base `camonitor.c:285-303`). `None`/empty/unrecognised → the
/// default value+log+alarm mask (matches the pre-`-m` subscription).
fn parse_event_mask(m: Option<&str>) -> u16 {
    const DBE_VALUE: u16 = 1;
    const DBE_LOG: u16 = 2;
    const DBE_ALARM: u16 = 4;
    const DBE_PROPERTY: u16 = 8;
    const DEFAULT: u16 = DBE_VALUE | DBE_LOG | DBE_ALARM;
    let Some(s) = m else { return DEFAULT };
    let mut mask = 0u16;
    for c in s.chars() {
        match c {
            'v' => mask |= DBE_VALUE,
            'a' => mask |= DBE_ALARM,
            'l' => mask |= DBE_LOG,
            'p' => mask |= DBE_PROPERTY,
            _ => {}
        }
    }
    if mask == 0 { DEFAULT } else { mask }
}

/// CA-FR-4: `camonitor -t <key>` timestamp rendering mode
/// (`camonitor.c:235-253`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TimestampMode {
    /// Server timestamp (default).
    Server,
    /// `n` — no timestamp column.
    None,
    /// `r` — seconds relative to program start.
    Relative,
    /// `i` — seconds since the previous event on ANY channel.
    IncrAll,
    /// `I` — seconds since the previous event on the SAME channel.
    IncrChan,
}

/// Parse a `-t <key>` value. The recognised mode is the last source
/// letter present (`n`/`r`/`i`/`I`); anything else (incl. server `s`)
/// keeps the server timestamp. `None` → server default.
fn parse_timestamp_mode(k: Option<&str>) -> TimestampMode {
    let Some(k) = k else {
        return TimestampMode::Server;
    };
    let mut mode = TimestampMode::Server;
    for c in k.chars() {
        match c {
            'n' => mode = TimestampMode::None,
            'r' => mode = TimestampMode::Relative,
            'i' => mode = TimestampMode::IncrAll,
            'I' => mode = TimestampMode::IncrChan,
            _ => {}
        }
    }
    mode
}

/// Render the timestamp column for one event under `mode`. `prev` holds
/// the previous event timestamp for the incremental modes (shared for
/// `i`, per-channel for `I`); it is updated in place. Returns `None`
/// when no time column should be printed (`-t n`).
fn render_timestamp(
    mode: TimestampMode,
    ts: SystemTime,
    start: SystemTime,
    prev: &mut Option<SystemTime>,
) -> Option<String> {
    let secs_between = |a: SystemTime, b: SystemTime| -> f64 {
        a.duration_since(b)
            .or_else(|_| b.duration_since(a))
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    };
    match mode {
        TimestampMode::Server => Some(format_server_timestamp(ts)),
        TimestampMode::None => Option::None,
        TimestampMode::Relative => Some(format!("{:.6}", secs_between(ts, start))),
        TimestampMode::IncrAll | TimestampMode::IncrChan => {
            let delta = prev.map(|p| secs_between(ts, p)).unwrap_or(0.0);
            *prev = Some(ts);
            Some(format!("{delta:.6}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TimestampMode, parse_event_mask, parse_timestamp_mode, render_timestamp};
    use std::time::{Duration, SystemTime};

    #[test]
    fn mask_defaults_when_absent_or_unrecognised() {
        assert_eq!(parse_event_mask(None), 1 | 2 | 4);
        assert_eq!(parse_event_mask(Some("")), 1 | 2 | 4);
        assert_eq!(parse_event_mask(Some("xyz")), 1 | 2 | 4);
    }

    #[test]
    fn mask_parses_dbe_letters() {
        assert_eq!(parse_event_mask(Some("a")), 4, "alarm-only");
        assert_eq!(parse_event_mask(Some("v")), 1, "value-only");
        assert_eq!(parse_event_mask(Some("p")), 8, "property-only");
        assert_eq!(parse_event_mask(Some("val")), 1 | 4 | 2, "value+alarm+log");
    }

    #[test]
    fn timestamp_mode_parses_keys() {
        assert!(matches!(parse_timestamp_mode(None), TimestampMode::Server));
        assert!(matches!(
            parse_timestamp_mode(Some("s")),
            TimestampMode::Server
        ));
        assert!(matches!(
            parse_timestamp_mode(Some("n")),
            TimestampMode::None
        ));
        assert!(matches!(
            parse_timestamp_mode(Some("r")),
            TimestampMode::Relative
        ));
        assert!(matches!(
            parse_timestamp_mode(Some("i")),
            TimestampMode::IncrAll
        ));
        assert!(matches!(
            parse_timestamp_mode(Some("I")),
            TimestampMode::IncrChan
        ));
    }

    #[test]
    fn timestamp_render_modes() {
        let start = SystemTime::UNIX_EPOCH;
        let t1 = start + Duration::from_secs(10);
        let t2 = start + Duration::from_secs(13);
        // None → no column.
        let mut p = None;
        assert!(render_timestamp(TimestampMode::None, t1, start, &mut p).is_none());
        // Relative → seconds since start.
        let mut p = None;
        assert_eq!(
            render_timestamp(TimestampMode::Relative, t1, start, &mut p).as_deref(),
            Some("10.000000")
        );
        // Incremental → first is 0, then the delta; prev advances.
        let mut p = None;
        assert_eq!(
            render_timestamp(TimestampMode::IncrAll, t1, start, &mut p).as_deref(),
            Some("0.000000")
        );
        assert_eq!(
            render_timestamp(TimestampMode::IncrAll, t2, start, &mut p).as_deref(),
            Some("3.000000")
        );
    }
}
