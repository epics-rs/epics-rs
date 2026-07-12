use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use chrono::{DateTime, Local};
use clap::Parser;
use epics_base_rs::types::WallTime;
use epics_ca_rs::cli::{
    CountPrefix, FloatFormat, FloatStyle, PV_NAME_WIDTH, ValueFormat, format_value, sevr_to_str,
    stat_to_str,
};
use epics_ca_rs::client::{CaChannel, CaClient, ConnectionEvent, EnumReadback};
use epics_ca_rs::copt::CTool;

/// Owner of every C-scanned option argument in this binary (see
/// [`epics_ca_rs::copt`]).
const TOOL: CTool = CTool::new("camonitor");

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

    /// CA priority (0-99). Opens the channel on the matching priority
    /// virtual circuit (libca `ca_create_channel` priority parameter).
    #[arg(short = 'p', long)]
    priority: Option<u8>,

    /// Timestamp source(s) and kind. Sources: `s`=CA server/remote
    /// (default), `c`=CA client/local receive time (shown in `()`).
    /// Kind: `n`=none, `r`=relative since program start, `i`=incremental
    /// across all channels, `I`=incremental per channel. `r`/`i`/`I`
    /// require `s` or `c`. Sources combine, e.g. `-t sc`, `-t cr`.
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

    /// `-0<base>`: print integers in base `x`/`o`/`b`. C spells this as a
    /// getopt option TAKING AN ARGUMENT (`camonitor.c:224`
    /// `"...g:l:#:0:w:..."`), so it is `-0` with an attached or separate
    /// `<base>` — never a `--0x`-style flag, which no C script can pass.
    #[arg(short = '0', value_name = "BASE", action = clap::ArgAction::Append)]
    int_base: Vec<String>,

    /// `-l<base>`: round a float to a long and print it in base `x`/`o`/`b`
    /// (C `outTypeF`). Same option shape as `-0` (`camonitor.c:224`).
    /// Unlike `caget`, `camonitor` has no `-d`, so `-0` here forces no DBR
    /// type (`camonitor.c:337`).
    #[arg(short = 'l', value_name = "BASE", action = clap::ArgAction::Append)]
    float_base: Vec<String>,

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
        // C `camonitor.c:325-340` writes exactly ONE of the two base globals
        // per occurrence: `-0<base>` sets `outTypeI` (integers), `-l<base>`
        // sets `outTypeF` (floats, via round-to-long). They never cross.
        fmt.int_style = TOOL.base('0', &self.int_base);
        fmt.float_style = TOOL.base('l', &self.float_base);
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

    let client = CaClient::new().await.expect("failed to create CA client");
    // -p selects the priority virtual circuit.
    let priority = args.priority.unwrap_or(0);

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
    // resolve the `-m <msk>` DBE_* mask + `-t` timestamp mode
    // once for all PVs. `prev_all`/`start` back the relative and
    // incremental timestamp renderings.
    let mask = parse_event_mask(args.event_mask.as_deref());
    let spec = parse_timestamp_spec(args.timestamp_key.as_deref());
    // `-s` (`floatAsString`): request DBR_TIME_STRING for FLOAT/DOUBLE
    // fields so the server renders the value at record precision
    // (C `camonitor.c:162-166`).
    let float_as_string = args.string_format;
    // C `camonitor.c:168-169` applies the user's `-#` count to the
    // `ca_create_subscription` request count (clamped to the native element
    // count at connect); `None` (no `-#`) leaves `reqElems == 0`, the CA
    // autosize request, so the server reports each event at the record's
    // current element count. Carried into the subscription so each monitor
    // event transfers only the requested slice (or the autosized current
    // count) instead of the native capacity.
    let req_count = args.max_elements.map(|n| n as u32);
    let start = SystemTime::now();
    // `tsFirst` (`tool_lib.c:40`): the first SERVER stamp seen across all
    // channels, captured once — the server-relative (`-t sr`) baseline.
    let first_server = Arc::new(std::sync::Mutex::new(None::<SystemTime>));
    // `i` (incremental across ALL channels) shares the previous-event
    // time across PVs; one slot per source.
    let prev_all_server = Arc::new(std::sync::Mutex::new(None::<SystemTime>));
    let prev_all_client = Arc::new(std::sync::Mutex::new(None::<SystemTime>));

    let mut handles = Vec::new();
    for (i, pv_name) in args.pv_names.iter().enumerate() {
        let channel = client.create_channel_with_priority(pv_name, priority);
        let pv = pv_name.clone();
        let flag = connected_flags[i].clone();
        let fmt = fmt.clone();
        let first_server = first_server.clone();
        let prev_all_server = prev_all_server.clone();
        let prev_all_client = prev_all_client.clone();
        handles.push(tokio::spawn(async move {
            monitor_pv(
                channel,
                pv,
                flag,
                fmt,
                float_as_string,
                req_elems_present,
                req_count,
                mask,
                spec,
                start,
                first_server,
                prev_all_server,
                prev_all_client,
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

#[allow(clippy::too_many_arguments)]
async fn monitor_pv(
    channel: CaChannel,
    pv_name: String,
    connected_flag: Arc<AtomicBool>,
    fmt: Arc<ValueFormat>,
    float_as_string: bool,
    req_elems_present: bool,
    req_count: Option<u32>,
    mask: u16,
    spec: TimestampSpec,
    start: SystemTime,
    first_server: Arc<std::sync::Mutex<Option<SystemTime>>>,
    prev_all_server: Arc<std::sync::Mutex<Option<SystemTime>>>,
    prev_all_client: Arc<std::sync::Mutex<Option<SystemTime>>>,
) {
    // Per-channel previous timestamps for `-t I`, one per source.
    let mut prev_chan_server: Option<SystemTime> = None;
    let mut prev_chan_client: Option<SystemTime> = None;
    // Per-channel `firstStampPrinted` gate: the leading event of THIS
    // channel always prints an absolute stamp (C `tool_lib.c:414`).
    let mut first_printed = false;
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

    // honour `-m <msk>` via the caller-resolved DBE_* mask.
    // C `camonitor.c:155-162` ALWAYS substitutes an ENUM field's request
    // type (it never reads native DBR_TIME_ENUM): `-n` (`enumAsNr`) →
    // DBR_TIME_INT (the numeric index, `camonitor.c:158`), otherwise
    // DBR_TIME_STRING (the state label, `camonitor.c:156-160`) — the default,
    // so the monitor delivers labels. C `camonitor.c:162-166` requests
    // DBR_TIME_STRING for a FLOAT/DOUBLE field under `-s` so the server
    // renders it to a string. The ENUM case takes precedence over the float
    // case (C `if/else if`).
    let enum_readback = if fmt.enum_as_number {
        EnumReadback::Numeric
    } else {
        EnumReadback::Label
    };
    let Ok(mut monitor) = channel
        .subscribe_with_mask_readback_count(0.0, mask, enum_readback, float_as_string, req_count)
        .await
    else {
        return;
    };
    while let Some(result) = monitor.recv().await {
        match result {
            Ok(snap) => {
                // capture the client receive time as close to
                // arrival as possible — this is the `c` (client) source,
                // distinct from the server-supplied `snap.timestamp`.
                let recv_time = SystemTime::now();
                // `-t` selects source(s) + rendering kind. `tsFirst`
                // (the server-relative baseline) is global, so it is
                // always locked. `IncrAll` shares the previous-event
                // time across channels; `IncrChan`/the others use the
                // per-channel slots.
                let mut fs = first_server.lock().unwrap();
                let time_seg = if spec.kind == TimestampKind::IncrAll {
                    let mut ps = prev_all_server.lock().unwrap();
                    let mut pc = prev_all_client.lock().unwrap();
                    let mut st = TimestampState {
                        first_server: &mut fs,
                        first_printed: &mut first_printed,
                        prev_server: &mut ps,
                        prev_client: &mut pc,
                    };
                    render_timestamp(spec, snap.timestamp, recv_time, start, &mut st)
                } else {
                    let mut st = TimestampState {
                        first_server: &mut fs,
                        first_printed: &mut first_printed,
                        prev_server: &mut prev_chan_server,
                        prev_client: &mut prev_chan_client,
                    };
                    render_timestamp(spec, snap.timestamp, recv_time, start, &mut st)
                }
                .map(|ts| format!("{ts}{sep}"))
                .unwrap_or_default();
                drop(fs);
                let enum_strings = snap.enums.as_ref().map(|e| e.strings.as_slice());
                let rendered = format_value(
                    &snap.value,
                    &fmt,
                    enum_strings,
                    req_elems_present,
                    CountPrefix::IfRequestedOrArray,
                );
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

/// parse a `camonitor -m <msk>` mask string into `DBE_*` bits,
/// mirroring C `camonitor.c:40,285-301`.
///
/// With no `-m` the mask is the C default `DBE_VALUE | DBE_ALARM` (NOT
/// value+log+alarm). A `-m` argument resets the mask to 0 and ORs in
/// each recognised letter (`v` value, `a` alarm, `l` log/archive,
/// `p` property); the FIRST unrecognised letter prints the C diagnostic
/// to stderr, reverts the mask to `DBE_VALUE | DBE_ALARM`, and STOPS
/// scanning the rest of the argument (C sets `err = 1`). An empty
/// `-m ""` selects no events (mask 0), exactly as C's scan loop leaves
/// `eventMask` at 0.
fn parse_event_mask(m: Option<&str>) -> u16 {
    const DBE_VALUE: u16 = 1;
    const DBE_LOG: u16 = 2;
    const DBE_ALARM: u16 = 4;
    const DBE_PROPERTY: u16 = 8;
    const DEFAULT: u16 = DBE_VALUE | DBE_ALARM;
    let Some(s) = m else { return DEFAULT };
    let mut mask = 0u16;
    for c in s.chars() {
        match c {
            'v' => mask |= DBE_VALUE,
            'a' => mask |= DBE_ALARM,
            'l' => mask |= DBE_LOG,
            'p' => mask |= DBE_PROPERTY,
            _ => {
                eprintln!("Invalid argument '{s}' for option '-m' - ignored.");
                return DEFAULT;
            }
        }
    }
    mask
}

/// `camonitor -t <key>` rendering KIND — orthogonal to the
/// timestamp SOURCE (`camonitor.c:235-253`). C keys this off `tsType`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TimestampKind {
    /// Absolute wall-clock timestamp (default).
    Absolute,
    /// `r` — seconds relative to program start.
    Relative,
    /// `i` — seconds since the previous event on ANY channel.
    IncrAll,
    /// `I` — seconds since the previous event on the SAME channel.
    IncrChan,
}

/// a `-t` spec is two orthogonal axes — which SOURCE(s) to show
/// (`s` = CA server / remote stamp, `c` = CA client / local receive time,
/// shown in `()`), and the rendering KIND. C carries these as the
/// independent `tsSrcServer` / `tsSrcClient` flags plus `tsType`; the
/// earlier single-enum model dropped the source axis, so `-t cr` rendered
/// a relative time off the SERVER stamp instead of the receive time.
#[derive(Clone, Copy)]
struct TimestampSpec {
    server: bool,
    client: bool,
    kind: TimestampKind,
}

/// Parse a `-t <key>`. C `case 't'` resets both sources to 0 then sets
/// them from the letters; `s`/`c` pick source(s), `r`/`i`/`I` pick the
/// kind, `n` and unknown letters are no-ops (a kind with no source prints
/// nothing — the usage note "'r','i','I' require 's' or 'c'"). With no
/// `-t` at all the C globals default to server + absolute.
fn parse_timestamp_spec(k: Option<&str>) -> TimestampSpec {
    let Some(k) = k else {
        return TimestampSpec {
            server: true,
            client: false,
            kind: TimestampKind::Absolute,
        };
    };
    let mut spec = TimestampSpec {
        server: false,
        client: false,
        kind: TimestampKind::Absolute,
    };
    for c in k.chars() {
        match c {
            's' => spec.server = true,
            'c' => spec.client = true,
            'r' => spec.kind = TimestampKind::Relative,
            'i' => spec.kind = TimestampKind::IncrAll,
            'I' => spec.kind = TimestampKind::IncrChan,
            _ => {} // 'n' and unknown letters: no-op (matches C)
        }
    }
    spec
}

/// Mutable timestamp state threaded through [`render_timestamp`], one
/// bundle per event. Mirrors the C `tool_lib.c` globals/per-PV fields:
/// `tsFirst` (server-relative baseline), `firstStampPrinted`, and the
/// `tsPrevious{S,C}` incremental baselines.
struct TimestampState<'a> {
    /// `tsFirst` (`tool_lib.c:40`): the first SERVER stamp seen, captured
    /// once across all channels. The server-relative (`-t sr`) baseline.
    first_server: &'a mut Option<SystemTime>,
    /// `pv->firstStampPrinted` (`tool_lib.c:414`): this channel has
    /// already printed its absolute leading stamp.
    first_printed: &'a mut bool,
    /// `tsPrevious{S,C}` (`tool_lib.c:466-467`): the previous-event
    /// baselines for the incremental kinds — shared for `i`, per-channel
    /// for `I`.
    prev_server: &'a mut Option<SystemTime>,
    prev_client: &'a mut Option<SystemTime>,
}

/// Render the timestamp column for one event under `spec`. Returns
/// `None` when no time column should be printed (`-t n`).
///
/// Mirrors C `print_time_val_sts` (`tool_lib.c:407-467`): the FIRST
/// event of each channel always prints an ABSOLUTE stamp (C
/// `printAbs = !pv->firstStampPrinted`), even under `r`/`i`/`I`; only
/// later events render as diffs. The server-relative baseline is the
/// first SERVER stamp (`tsFirst`), NOT program start — program start
/// (`tsStart`) is the CLIENT-relative baseline.
fn render_timestamp(
    spec: TimestampSpec,
    server_ts: WallTime,
    client_ts: SystemTime,
    start: SystemTime,
    state: &mut TimestampState<'_>,
) -> Option<String> {
    // The server stamp is a `WallTime`; join it to the local-clock comparison
    // domain (client/start/baselines are `SystemTime`) for the µs-formatted
    // display and f64-seconds diffs below. The conversion is 100 ns-granular
    // on Windows, which neither the `%.6f` (µs) format nor the `%+12.6f`
    // (µs) diffs can observe.
    let server_ts: SystemTime = server_ts.into();
    // C `epicsTimeDiffInSeconds(pLeft, pRight)` is the SIGNED difference
    // `pLeft - pRight` (epicsTime.cpp:417-431) — a backward stamp step
    // (server clock correction, NTP step, device-support timestamp change,
    // reconnect to a different provider) yields a NEGATIVE delta. The old
    // `duration_since(a,b).or_else(b,a)` collapsed that to a positive
    // magnitude, so `-t sr/si/sI` reported a forward interval for exactly
    // the non-monotonic condition operators use those modes to detect.
    fn secs_between(a: SystemTime, b: SystemTime) -> f64 {
        match a.duration_since(b) {
            Ok(d) => d.as_secs_f64(),
            // `a < b`: SystemTimeError::duration() is the magnitude `b - a`;
            // negate it to recover the signed `a - b`.
            Err(e) => -e.duration().as_secs_f64(),
        }
    }
    // C `tool_lib.c:419-422`: latch the first SERVER stamp once; it is
    // the server-relative baseline (`tsFirst`).
    if state.first_server.is_none() {
        *state.first_server = Some(server_ts);
    }
    // C `tool_lib.c:414,449-452`: `printAbs = !pv->firstStampPrinted`.
    // The leading event for THIS channel renders absolute even in a
    // relative/incremental mode; absolute mode is always absolute.
    let print_abs = spec.kind == TimestampKind::Absolute || !*state.first_printed;
    // Server-relative baseline = `tsFirst`; client-relative = `tsStart`.
    let server_ref = state.first_server.unwrap_or(server_ts);
    // The inner value: an absolute stamp string, or — for a diff kind — the
    // C `%+12.6f` signed delta (12-wide, 6 decimals, forced sign), matching
    // `printf("%+12.6f", epicsTimeDiffInSeconds(...))` (tool_lib.c:455-457).
    let render_one = |ts: SystemTime, is_server: bool, prev: Option<SystemTime>| -> String {
        if print_abs {
            return format_server_timestamp(ts);
        }
        match spec.kind {
            // `print_abs` already covers the absolute kind above.
            TimestampKind::Absolute => format_server_timestamp(ts),
            TimestampKind::Relative => {
                let r = if is_server { server_ref } else { start };
                format!("{:+12.6}", secs_between(ts, r))
            }
            TimestampKind::IncrAll | TimestampKind::IncrChan => {
                format!("{:+12.6}", secs_between(ts, prev.unwrap_or(ts)))
            }
        }
    };
    // C `print_time_val_sts` prints the server stamp, then the client stamp
    // wrapped in `()`, back to back. Either may be absent. The diff (non-abs)
    // branches are preceded by a 14-space column prefix that sits OUTSIDE the
    // client parentheses (`printf("              (%+12.6f)", ...)`,
    // tool_lib.c:455-457); absolute stamps carry no such prefix.
    const DIFF_PREFIX: &str = "              "; // 14 spaces (C column shape)
    let prefix = if print_abs { "" } else { DIFF_PREFIX };
    let mut out = String::new();
    if spec.server {
        out.push_str(prefix);
        out.push_str(&render_one(server_ts, true, *state.prev_server));
    }
    if spec.client {
        out.push_str(prefix);
        out.push('(');
        out.push_str(&render_one(client_ts, false, *state.prev_client));
        out.push(')');
    }
    // C `tool_lib.c:461-467`: advance the incremental baselines on EVERY
    // event, and mark this channel's leading absolute stamp as printed.
    *state.prev_server = Some(server_ts);
    *state.prev_client = Some(client_ts);
    if print_abs {
        *state.first_printed = true;
    }
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::{
        TimestampKind, TimestampSpec, TimestampState, parse_event_mask, parse_timestamp_spec,
        render_timestamp,
    };
    use std::time::{Duration, SystemTime};

    #[test]
    fn mask_default_is_value_alarm() {
        // C `camonitor.c:40`: the no-`-m` default is VALUE|ALARM, NOT
        // value+log+alarm.
        assert_eq!(parse_event_mask(None), 1 | 4);
    }

    #[test]
    fn mask_invalid_letter_reverts_to_value_alarm() {
        // C `camonitor.c:298-300`: the first unrecognised letter reverts
        // to VALUE|ALARM and stops scanning (so a leading valid `v` is
        // discarded too).
        assert_eq!(parse_event_mask(Some("xyz")), 1 | 4);
        assert_eq!(parse_event_mask(Some("vx")), 1 | 4);
    }

    #[test]
    fn mask_empty_selects_no_events() {
        // C scan loop never runs on an empty arg → eventMask stays 0.
        assert_eq!(parse_event_mask(Some("")), 0);
    }

    #[test]
    fn mask_parses_dbe_letters() {
        assert_eq!(parse_event_mask(Some("a")), 4, "alarm-only");
        assert_eq!(parse_event_mask(Some("v")), 1, "value-only");
        assert_eq!(parse_event_mask(Some("p")), 8, "property-only");
        assert_eq!(parse_event_mask(Some("val")), 1 | 4 | 2, "value+alarm+log");
    }

    #[test]
    fn timestamp_spec_parses_keys() {
        let s = parse_timestamp_spec(None);
        assert!(s.server && !s.client && matches!(s.kind, TimestampKind::Absolute));
        let s = parse_timestamp_spec(Some("s"));
        assert!(s.server && !s.client);
        let s = parse_timestamp_spec(Some("c"));
        assert!(!s.server && s.client);
        let s = parse_timestamp_spec(Some("sc"));
        assert!(s.server && s.client);
        // 'n' (and unknown letters) select no source → no column.
        let s = parse_timestamp_spec(Some("n"));
        assert!(!s.server && !s.client, "n selects no source");
        let s = parse_timestamp_spec(Some("cr"));
        assert!(!s.server && s.client && matches!(s.kind, TimestampKind::Relative));
        assert!(matches!(
            parse_timestamp_spec(Some("i")).kind,
            TimestampKind::IncrAll
        ));
        assert!(matches!(
            parse_timestamp_spec(Some("I")).kind,
            TimestampKind::IncrChan
        ));
    }

    /// Build a fresh `TimestampState` over caller-owned slots so each
    /// test starts from C's initial state (`tsFirst` unset, this
    /// channel's leading stamp not yet printed).
    fn ts_state<'a>(
        first_server: &'a mut Option<SystemTime>,
        first_printed: &'a mut bool,
        prev_server: &'a mut Option<SystemTime>,
        prev_client: &'a mut Option<SystemTime>,
    ) -> TimestampState<'a> {
        TimestampState {
            first_server,
            first_printed,
            prev_server,
            prev_client,
        }
    }

    /// Reconstruct the C diff column shape exactly: a 14-space prefix then a
    /// `%+12.6f` signed, 12-wide, 6-decimal field (tool_lib.c:455). Used to
    /// pin sign + width + prefix without hand-counting spaces.
    fn srv_diff(d: f64) -> String {
        format!("              {d:+12.6}")
    }
    /// Client diff column: 14-space prefix then `(%+12.6f)` (tool_lib.c:457) —
    /// the prefix sits OUTSIDE the parentheses.
    fn cli_diff(d: f64) -> String {
        format!("              ({d:+12.6})")
    }

    #[test]
    fn timestamp_first_event_is_absolute_then_diffs() {
        // C `tool_lib.c:414`: the leading event of a channel prints an
        // ABSOLUTE stamp even under `-t sr`; later events diff against
        // the FIRST SERVER stamp (`tsFirst`), not program start.
        let start = SystemTime::UNIX_EPOCH;
        let t1 = start + Duration::from_secs(10);
        let t2 = start + Duration::from_secs(13);
        let srv = |kind| TimestampSpec {
            server: true,
            client: false,
            kind,
        };
        // No source → no column (state is irrelevant).
        let none = TimestampSpec {
            server: false,
            client: false,
            kind: TimestampKind::Absolute,
        };
        let (mut fsv, mut fp, mut ps, mut pc) = (None, false, None, None);
        let mut st = ts_state(&mut fsv, &mut fp, &mut ps, &mut pc);
        assert!(render_timestamp(none, t1.into(), t1, start, &mut st).is_none());

        // Server relative: first event ABSOLUTE (== absolute render of
        // t1), NOT "10.000000".
        let (mut fsv, mut fp, mut ps, mut pc) = (None, false, None, None);
        let mut st = ts_state(&mut fsv, &mut fp, &mut ps, &mut pc);
        let first = render_timestamp(srv(TimestampKind::Relative), t1.into(), t1, start, &mut st);
        assert_eq!(
            first.as_deref(),
            Some(super::format_server_timestamp(t1).as_str()),
            "first event must render the absolute server stamp"
        );
        // Second event: diff against the FIRST SERVER stamp (t1), so
        // t2 - t1 = 3s — NOT t2 - start (= 13s).
        let second = render_timestamp(srv(TimestampKind::Relative), t2.into(), t2, start, &mut st);
        assert_eq!(second.as_deref(), Some(srv_diff(3.0).as_str()));
    }

    #[test]
    fn timestamp_server_incremental_diffs_against_prev() {
        // First event absolute, then each event diffs against the prior.
        let start = SystemTime::UNIX_EPOCH;
        let t1 = start + Duration::from_secs(10);
        let t2 = start + Duration::from_secs(13);
        let srv = TimestampSpec {
            server: true,
            client: false,
            kind: TimestampKind::IncrAll,
        };
        let (mut fsv, mut fp, mut ps, mut pc) = (None, false, None, None);
        let mut st = ts_state(&mut fsv, &mut fp, &mut ps, &mut pc);
        assert_eq!(
            render_timestamp(srv, t1.into(), t1, start, &mut st).as_deref(),
            Some(super::format_server_timestamp(t1).as_str()),
            "leading incremental event is absolute"
        );
        assert_eq!(
            render_timestamp(srv, t2.into(), t2, start, &mut st).as_deref(),
            Some(srv_diff(3.0).as_str()),
            "second incremental event diffs against the prior stamp"
        );
    }

    /// A BACKWARD server-stamp step must render a
    /// NEGATIVE signed delta (C `epicsTimeDiffInSeconds` = pLeft - pRight,
    /// epicsTime.cpp:417-431). The previous magnitude-only formatting hid
    /// exactly the non-monotonic condition `-t si` is used to detect.
    #[test]
    fn timestamp_backward_step_renders_negative_delta() {
        let start = SystemTime::UNIX_EPOCH;
        let t1 = start + Duration::from_secs(10);
        let t2 = start + Duration::from_secs(7); // moved BACKWARD by 3s
        let srv = TimestampSpec {
            server: true,
            client: false,
            kind: TimestampKind::IncrAll,
        };
        let (mut fsv, mut fp, mut ps, mut pc) = (None, false, None, None);
        let mut st = ts_state(&mut fsv, &mut fp, &mut ps, &mut pc);
        // First event is absolute.
        render_timestamp(srv, t1.into(), t1, start, &mut st);
        // Second event: 7 - 10 = -3s, rendered with a leading '-'.
        let second = render_timestamp(srv, t2.into(), t2, start, &mut st);
        assert_eq!(
            second.as_deref(),
            Some(srv_diff(-3.0).as_str()),
            "backward step must render as a negative delta, not +3"
        );
        assert!(
            second.as_deref().unwrap().contains("-3.000000"),
            "delta carries a minus sign: {second:?}"
        );
    }

    #[test]
    fn timestamp_client_relative_uses_program_start_after_first() {
        // `-t cr`: the leading event is absolute (receive time in
        // parens); later events diff the CLIENT receive time against
        // program start (`tsStart`), independent of the server baseline.
        let start = SystemTime::UNIX_EPOCH;
        let c1 = start + Duration::from_secs(4);
        let c2 = start + Duration::from_secs(10);
        let cr = TimestampSpec {
            server: false,
            client: true,
            kind: TimestampKind::Relative,
        };
        let (mut fsv, mut fp, mut ps, mut pc) = (None, false, None, None);
        let mut st = ts_state(&mut fsv, &mut fp, &mut ps, &mut pc);
        // First: absolute client receive time in parens.
        let first = render_timestamp(cr, start.into(), c1, start, &mut st);
        assert_eq!(
            first.as_deref(),
            Some(format!("({})", super::format_server_timestamp(c1)).as_str())
        );
        // Second: client diff vs program start → 10s (NOT vs c1).
        let second = render_timestamp(cr, start.into(), c2, start, &mut st);
        assert_eq!(second.as_deref(), Some(cli_diff(10.0).as_str()));
    }

    #[test]
    fn timestamp_both_sources_render_independently_after_first() {
        // Both sources: server diffs against tsFirst, client against
        // tsStart. Drive past the absolute leading event first.
        let start = SystemTime::UNIX_EPOCH;
        let s1 = start + Duration::from_secs(5);
        let c1 = start + Duration::from_secs(4);
        let s2 = start + Duration::from_secs(8);
        let c2 = start + Duration::from_secs(10);
        let both = TimestampSpec {
            server: true,
            client: true,
            kind: TimestampKind::Relative,
        };
        let (mut fsv, mut fp, mut ps, mut pc) = (None, false, None, None);
        let mut st = ts_state(&mut fsv, &mut fp, &mut ps, &mut pc);
        // Leading event absolute: server stamp then (client stamp).
        let first = render_timestamp(both, s1.into(), c1, start, &mut st);
        assert_eq!(
            first.as_deref(),
            Some(
                format!(
                    "{}({})",
                    super::format_server_timestamp(s1),
                    super::format_server_timestamp(c1)
                )
                .as_str()
            )
        );
        // Second event: server (s2 - tsFirst=s1) = 3s, client
        // (c2 - tsStart=start) = 10s.
        let second = render_timestamp(both, s2.into(), c2, start, &mut st);
        assert_eq!(
            second.as_deref(),
            Some(format!("{}{}", srv_diff(3.0), cli_diff(10.0)).as_str())
        );
    }
}
