use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use clap::{CommandFactory, FromArgMatches, Parser};
use epics_base_rs::types::WallTime;
use epics_ca_rs::cli::{
    CountPrefix, FloatFormat, FloatStyle, PV_NAME_WIDTH, ValueFormat, format_time,
    format_value_segment, sevr_to_str, stat_to_str,
};
use epics_ca_rs::client::{CaChannel, CaClient, ConnectionEvent, EnumReadback};
use epics_ca_rs::copt::{self, CTool};

/// Owner of every C-scanned option argument in this binary (see
/// [`epics_ca_rs::copt`]).
const TOOL: CTool = CTool::new("camonitor");

/// The getopt cases that `return` from C's `main` (`camonitor.c:225-231`), by
/// clap id. `copt::Scan::finish` performs the FIRST one on the command line,
/// after replaying the warnings the loop raised on its way there (R13-26).
const TERMINALS: &[(&str, copt::Terminal)] = &[
    ("help", copt::Terminal::Usage(0)),
    ("version", copt::Terminal::Version),
];

/// C `camonitor.c:45-92` `usage()`, byte for byte. C substitutes two
/// COMPILE-TIME constants, not the running configuration: `%f` of
/// `DEFAULT_TIMEOUT` and `%u` of `CA_PRIORITY_MAX`, so `camonitor -w 5 -h` still
/// advertises the 1.000000 s default. Written to stderr by
/// `copt::Scan::finish` on the `Terminal::Usage` arm (`camonitor.c:226-228`).
fn usage() -> String {
    format!(
        r#"
Usage: camonitor [options] <PV name> ...

  -h:       Help: Print this message
  -V:       Version: Show EPICS and CA versions
Channel Access options:
  -w <sec>: Wait time, specifies CA timeout, default is {timeout:.6} second(s)
  -m <msk>: Specify CA event mask to use.  <msk> is any combination of
            'v' (value), 'a' (alarm), 'l' (log/archive), 'p' (property).
            Default event mask is 'va'
  -p <pri>: CA priority (0-{max}, default 0=lowest)
Timestamps:
  Default:  Print absolute timestamps (as reported by CA server)
  -t <key>: Specify timestamp source(s) and type, with <key> containing
            's' = CA server (remote) timestamps
            'c' = CA client (local) timestamps (shown in '()'s)
            'n' = no timestamps
            'r' = relative timestamps (time elapsed since start of program)
            'i' = incremental timestamps (time elapsed since last update)
            'I' = incremental timestamps (time since last update, by channel)
            'r', 'i' or 'I' require 's' or 'c' to select the time source
Enum format:
  -n:       Print DBF_ENUM values as number (default is enum string)
Array values: Print number of elements, then list of values
  Default:  Request and print all elements (dynamic arrays supported)
  -# <num>: Request and print up to <num> elements
  -S:       Print arrays of char as a string (long string)
Floating point format:
  Default:  Use %g format
  -e <num>: Use %e format, with a precision of <num> digits
  -f <num>: Use %f format, with a precision of <num> digits
  -g <num>: Use %g format, with a precision of <num> digits
  -s:       Get value as string (honors server-side precision)
  -lx:      Round to long integer and print as hex number
  -lo:      Round to long integer and print as octal number
  -lb:      Round to long integer and print as binary number
Integer number format:
  Default:  Print as decimal number
  -0x:      Print as hex number
  -0o:      Print as octal number
  -0b:      Print as binary number
Alternate output field separator:
  -F <ofs>: Use <ofs> to separate fields in output

Example: camonitor -f8 my_channel another_channel
  (doubles are printed as %f with precision of 8)

"#,
        timeout = epics_ca_rs::cli::DEFAULT_CLI_TIMEOUT_SECS,
        max = epics_ca_rs::copt::CA_PRIORITY_MAX,
    )
}

/// Mirror of C `camonitor` flags. The flag set is mostly the same as
/// `caget` minus `-t`/`-a`/`-d` and plus `-m`/`-t<key>`. We model the
/// CLI to match — including the parity-only flags so existing scripts
/// don't break.
#[derive(Parser)]
#[command(
    name = "camonitor-rs",
    about = "Monitor EPICS PVs for changes",
    disable_version_flag = true,
    disable_help_flag = true
)]
struct Args {
    // `-h` / `-V` are ordinary options, performed by `copt::Scan::finish` at
    // their place in the getopt order so the warnings C prints before them
    // survive (R13-26).
    //
    // Every option below is `Append` (value option) or `Count` (flag): C's
    // getopt loop accepts every option any number of times, last one winning
    // (R13-17, see `epics_ca_rs::copt`).
    //
    // Doc comments on these fields are the option's HELP TEXT, so the rationale
    // above stays a plain comment.
    /// Print this message
    #[arg(short = 'h', long, action = clap::ArgAction::Count)]
    help: u8,

    #[arg(short = 'V', long, hide = true, action = clap::ArgAction::Count)]
    version: u8,

    /// CA timeout in seconds (initial connection wait). `epicsScanDouble`;
    /// a bad value warns and keeps the default (`camonitor.c:263-272`). Raw
    /// `String`: every C-scanned option argument is resolved by
    /// [`epics_ca_rs::copt`], never by clap.
    #[arg(short = 'w', long = "wait", allow_hyphen_values = true, action = clap::ArgAction::Append)]
    timeout: Vec<String>,

    /// CA event mask `<msk>`: any combination of `v` (value), `a`
    /// (alarm), `l` (log/archive), `p` (property). The subscription is
    /// issued with the resulting DBE_* mask; absent → value+log+alarm.
    #[arg(
        short = 'm',
        long,
        value_name = "MASK",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    event_mask: Vec<String>,

    /// CA priority (`sscanf("%u")`, clamped to `CA_PRIORITY_MAX`; `-p -1`
    /// and `-p 500` both clamp to 99 in C, `camonitor.c:281-288`).
    #[arg(short = 'p', long, allow_hyphen_values = true, action = clap::ArgAction::Append)]
    priority: Vec<String>,

    /// Timestamp source(s) and kind. Sources: `s`=CA server/remote
    /// (default), `c`=CA client/local receive time (shown in `()`).
    /// Kind: `n`=none, `r`=relative since program start, `i`=incremental
    /// across all channels, `I`=incremental per channel. `r`/`i`/`I`
    /// require `s` or `c`. Sources combine, e.g. `-t sc`, `-t cr`.
    #[arg(
        short = 't',
        long = "timestamp",
        value_name = "KEY",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    timestamp_key: Vec<String>,

    #[arg(short = 'n', long = "num-enum", action = clap::ArgAction::Count)]
    enum_as_number: u8,

    /// C's `reqElems`, scanned with `sscanf("%lu")` — 64-bit, unlike
    /// `caget`'s `%d` (`camonitor.c:273-280`). `0` (no `-#`, `-# 0`, or an
    /// unscannable `-#`) is "not specified": the CA autosize request.
    #[arg(
        short = '#',
        long = "max-elements",
        value_name = "COUNT",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    max_elements: Vec<String>,

    #[arg(short = 'S', long = "char-as-string", action = clap::ArgAction::Count)]
    char_array_as_string: u8,

    /// `%e`/`%f`/`%g` float format with the given precision (`sscanf("%d")`
    /// plus the `0..=VALID_DOUBLE_DIGITS` gate; both failures warn and keep
    /// the default format — `camonitor.c:310-324`).
    #[arg(
        short = 'e',
        long = "format-e",
        value_name = "PRECISION",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    fmt_e: Vec<String>,
    #[arg(
        short = 'f',
        long = "format-f",
        value_name = "PRECISION",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    fmt_f: Vec<String>,
    #[arg(
        short = 'g',
        long = "format-g",
        value_name = "PRECISION",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    fmt_g: Vec<String>,

    #[arg(short = 's', long = "string-format", action = clap::ArgAction::Count)]
    string_format: u8,

    /// `-0<base>`: print integers in base `x`/`o`/`b`. C spells this as a
    /// getopt option TAKING AN ARGUMENT (`camonitor.c:224`
    /// `"...g:l:#:0:w:..."`), so it is `-0` with an attached or separate
    /// `<base>` — never a `--0x`-style flag, which no C script can pass.
    #[arg(
        short = '0',
        value_name = "BASE",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    int_base: Vec<String>,

    /// `-l<base>`: round a float to a long and print it in base `x`/`o`/`b`
    /// (C `outTypeF`). Same option shape as `-0` (`camonitor.c:224`).
    /// Unlike `caget`, `camonitor` has no `-d`, so `-0` here forces no DBR
    /// type (`camonitor.c:337`).
    #[arg(
        short = 'l',
        value_name = "BASE",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    float_base: Vec<String>,

    /// Alternate output field separator: C takes `(char) *optarg`, the FIRST
    /// character, discarding the rest (`camonitor.c:342`).
    #[arg(
        short = 'F',
        long = "field-separator",
        value_name = "OFS",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    field_separator: Vec<String>,

    /// PV names to monitor. NOT clap-`required` — see `caget-rs`; C checks
    /// `nPvs < 1` after the getopt loop (`camonitor.c:363-367`).
    pv_names: Vec<String>,
}

impl Args {
    fn value_format(&self, scan: &mut copt::Scan) -> ValueFormat {
        let mut fmt = ValueFormat::default();
        // W10-B2. `-e`/`-f`/`-g` are ONE getopt case writing ONE `dblFormatStr`
        // (`camonitor.c:310-324`), so the LAST VALID occurrence across the three letters wins
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
        // C `camonitor.c:325-340` writes exactly ONE of the two base globals
        // per occurrence: `-0<base>` sets `outTypeI` (integers), `-l<base>`
        // sets `outTypeF` (floats, via round-to-long). They never cross.
        // camonitor has no `-d`, so nothing races the `type` these also set.
        fmt.int_style = scan.base('0', "int_base").style;
        fmt.float_style = scan.base('l', "float_base").style;
        fmt.enum_as_number = self.enum_as_number > 0;
        fmt.char_array_as_string = self.char_array_as_string > 0;
        fmt.req_elems = scan.req_elems_ulong("max_elements");
        if let Some(c) = scan.field_separator("field_separator") {
            fmt.field_separator = c;
        }
        fmt
    }
}

#[tokio::main]
async fn main() {
    // Parse via ArgMatches (not the plain derive) so the command-line order of
    // `-e`/`-f`/`-g` is recoverable for C's last-valid-wins rule (W10-B2).
    let parsed = TOOL.get_matches(Args::command());
    let matches = parsed.matches();
    let args = Args::from_arg_matches(matches).expect("clap validated the arguments");

    // C's ENTIRE getopt loop runs before the `nPvs < 1` check
    // (`camonitor.c:224-359`, then `:363`), so every option argument is
    // scanned — and every warning raised — even when no PV name follows.
    // `value_format` scans `-#`, `-e`/`-f`/`-g`, `-0`/`-l` and `-F`; the four
    // below cover the rest. `-m` and `-t` are bare re-scans: every occurrence
    // re-runs the case (so every bad one warns) and the last one wins.
    let mut scan = parsed.scan();
    let ca_timeout = scan.timeout("timeout", epics_ca_rs::cli::env_default_timeout());
    let priority = scan.priority("priority");
    let fmt = Arc::new(args.value_format(&mut scan));
    // The `-m <msk>` DBE_* mask + the `-t` timestamp mode, resolved once for
    // all PVs. `prev_all`/`start` back the relative and incremental timestamp
    // renderings.
    let mask = parse_event_mask(&mut scan, "event_mask");
    let spec = parse_timestamp_spec(&mut scan, "timestamp_key");
    // End of C's getopt loop: warnings out in command-line order, then `-h` /
    // `-V` if the loop reached one (R13-26).
    scan.finish(&usage(), &epics_ca_rs::protocol::version_info(), TERMINALS);

    if args.pv_names.is_empty() {
        TOOL.no_pv_name();
    }

    let client = CaClient::new().await.expect("failed to create CA client");

    let connected_flags: Vec<Arc<AtomicBool>> = args
        .pv_names
        .iter()
        .map(|_| Arc::new(AtomicBool::new(false)))
        .collect();
    // `-s` (`floatAsString`): request DBR_TIME_STRING for FLOAT/DOUBLE
    // fields so the server renders the value at record precision
    // (C `camonitor.c:162-166`).
    let float_as_string = args.string_format > 0;
    // C `camonitor.c:168-169` applies the user's `-#` count to the
    // `ca_create_subscription` request count (clamped to the native element
    // count at connect); `reqElems == 0` — no `-#`, `-# 0`, or an unscannable
    // `-#` — is the CA autosize request, so the server reports each event at
    // the record's current element count. `fmt` is the single carrier of C's
    // `reqElems`; a count that overflows `u32` clamps at the wire boundary,
    // which is where C's `ca_create_subscription(..., unsigned long)` narrows
    // it too.
    let req_count = match fmt.req_elems {
        0 => None,
        n => Some(u32::try_from(n).unwrap_or(u32::MAX)),
    };
    let start = SystemTime::now();
    // `tsFirst` (`tool_lib.c:40`): the first SERVER stamp seen across all
    // channels, captured once — the server-relative (`-t sr`) baseline.
    let first_server = Arc::new(std::sync::Mutex::new(None::<SystemTime>));
    // `i` (incremental across ALL channels) shares the previous-event
    // time across PVs; one slot per source.
    let prev_all_server = Arc::new(std::sync::Mutex::new(None::<SystemTime>));
    let prev_all_client = Arc::new(std::sync::Mutex::new(None::<SystemTime>));

    // C `camonitor.c:392-395` calls `create_pvs` itself — not the
    // `connect_pvs` barrier, because camonitor has no all-channels wait —
    // and returns its code before the event loop starts. The name gate is
    // the same one, so it lives in the same owner.
    let channels = match epics_ca_rs::cli::create_pvs(&client, &args.pv_names, priority) {
        Ok(channels) => channels,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let mut handles = Vec::new();
    for (i, (pv_name, channel)) in args.pv_names.iter().zip(channels).enumerate() {
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
    tokio::time::sleep(epics_ca_rs::cli::timeout_duration(ca_timeout)).await;

    // Print "*** Not connected" for PVs that didn't connect within
    // the wait window. Mirrors `tool_lib.c::print_time_val_sts` line
    // 521 — "*** Not connected (PV not found)". Honor `-F`: emit the
    // field separator between the name and the message, and pad the
    // name to 30 only with the default space separator. C's full rule
    // also suppresses padding for an array PV (`nElems > 1`); a
    // not-connected PV carries no element count here, so we gate on
    // the separator alone — identical to C for the common scalar /
    // no-`-#` case.
    // Taken off `fmt`, which is where `-F` was already resolved through the
    // owner — re-reading the raw argument here would be a second source for
    // one C global (`fieldSeparator`).
    let sep = fmt.field_separator;
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

/// camonitor's whole timestamp domain is `SystemTime` (it diffs client and
/// server stamps for `-t r`/`-t i`), so this adapts into the shared
/// `epicsTimeToStrftime` owner rather than re-implementing its rounding.
fn format_server_timestamp(ts: SystemTime) -> String {
    format_time(ts.into())
}

#[allow(clippy::too_many_arguments)]
async fn monitor_pv(
    channel: CaChannel,
    pv_name: String,
    connected_flag: Arc<AtomicBool>,
    fmt: Arc<ValueFormat>,
    float_as_string: bool,
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
                    // C `tool_lib.c:515` stamps this line with
                    // `epicsTimeGetCurrent` through the same formatter.
                    let now = format_server_timestamp(SystemTime::now());
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
                };
                drop(fs);
                let enum_strings = snap.enums.as_ref().map(|e| e.strings.as_slice());
                // C's separator is a PREFIX of the value's items, not a suffix
                // of the timestamp (`tool_lib.c:481-489`), so `-t n` — whose
                // timestamp is empty — still prints it.
                let value_seg = format_value_segment(
                    &snap.value,
                    &fmt,
                    enum_strings,
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
                    println!("{name_col}{sep}{time_seg}{value_seg}{sep}{sep}");
                } else {
                    println!(
                        "{name_col}{sep}{time_seg}{value_seg}{sep}{stat_str}{sep}{sevr_str}",
                        stat_str = stat_to_str(stat),
                        sevr_str = sevr_to_str(sevr),
                    );
                }
            }
            Err(_status) => {
                // C `camonitor.c:108-124` `event_handler` records the status
                // on the pv and prints ONLY when it is ECA_NORMAL — nothing in
                // the tool ever reads that status back, so a non-normal event
                // costs zero bytes of output. The port printed a line here,
                // which meant every IOC restart emitted a diagnostic C does
                // not emit (`TST:AI: server reported ECA status 0x00c0` once
                // the disconnect fan-out started landing here). The lost IOC
                // *is* reported — on stdout, as C's `*** disconnected` line,
                // from the connection callback, which is the only place C
                // reports it.
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
/// EVERY occurrence re-runs the case, so every bad one warns at its own
/// position and the last one still decides the mask — the scan folds them all
/// rather than looking only at the last (R13-26: a warning C prints is a
/// warning the port prints, at the place C prints it).
fn parse_event_mask(scan: &mut copt::Scan, id: &str) -> u16 {
    const DBE_VALUE: u16 = 1;
    const DBE_LOG: u16 = 2;
    const DBE_ALARM: u16 = 4;
    const DBE_PROPERTY: u16 = 8;
    const DEFAULT: u16 = DBE_VALUE | DBE_ALARM;

    let mut effective = DEFAULT;
    for (at, s) in scan
        .occurrences(id)
        .into_iter()
        .map(|(at, s)| (at, s.to_string()))
        .collect::<Vec<_>>()
    {
        let mut mask = 0u16;
        let mut bad = false;
        for c in s.chars() {
            match c {
                'v' => mask |= DBE_VALUE,
                'a' => mask |= DBE_ALARM,
                'l' => mask |= DBE_LOG,
                'p' => mask |= DBE_PROPERTY,
                // C sets `err = 1` here, so the rest of the argument is not
                // scanned and only ONE warning fires per occurrence.
                _ => {
                    scan.warn(
                        at,
                        format!("Invalid argument '{s}' for option '-m' - ignored."),
                    );
                    bad = true;
                    break;
                }
            }
        }
        effective = if bad { DEFAULT } else { mask };
    }
    effective
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

/// Parse a `-t <key>`. `s`/`c` pick the source(s), `r`/`i`/`I` the kind, `n` is
/// silent, an unknown letter warns (R13-18); a kind with no source prints
/// nothing — the usage note "'r','i','I' require 's' or 'c'".
///
/// The two axes have DIFFERENT lifetimes in C, because they are two separate
/// globals and `case 't'` resets only one pair (`camonitor.c:236-237`):
///
/// * `tsSrcServer`/`tsSrcClient` — zeroed at the top of EVERY `-t`, so only the
///   letters of the last occurrence choose the source(s);
/// * `tsType` — never reset, so a kind survives every later `-t` that does not
///   name a kind of its own. `camonitor -t r -t s` is a SERVER RELATIVE stamp
///   (`tool_lib.c:45-47` are the initial values: server, absolute).
fn parse_timestamp_spec(scan: &mut copt::Scan, id: &str) -> TimestampSpec {
    // C `tool_lib.c:45-47`, the globals as they stand before the getopt loop.
    let mut server = true;
    let mut client = false;
    // Declared OUTSIDE the occurrence loop, exactly as C declares it outside the
    // getopt loop: no `-t` resets it, so its value is sticky.
    let mut kind = TimestampKind::Absolute;

    for (at, k) in scan
        .occurrences(id)
        .into_iter()
        .map(|(at, s)| (at, s.to_string()))
        .collect::<Vec<_>>()
    {
        // The whole of `case 't'`'s reset (`camonitor.c:236-237`) — the sources,
        // and nothing else.
        server = false;
        client = false;
        for c in k.chars() {
            match c {
                's' => server = true,
                'c' => client = true,
                // `case 'n': break;` — the ONLY letter C accepts silently
                // without acting on it (`camonitor.c:246`).
                'n' => {}
                'r' => kind = TimestampKind::Relative,
                'i' => kind = TimestampKind::IncrAll,
                'I' => kind = TimestampKind::IncrChan,
                // C's `default:` inside the per-character switch
                // (`camonitor.c:249-251`): every letter it does not know warns
                // and is skipped — the scan continues, so `-t xsy` still selects
                // the server source and warns twice.
                _ => scan.warn(
                    at,
                    format!("Invalid argument '{c}' for option '-t' - ignored."),
                ),
            }
        }
    }
    TimestampSpec {
        server,
        client,
        kind,
    }
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

/// Render the timestamp column for one event under `spec` — the EMPTY string
/// under `-t n`, which prints no time column.
///
/// C has no "no timestamp" branch to special-case: `print_time_val_sts` prints
/// the name, then an unconditional separator, then whatever the timestamp
/// block emitted (nothing, when neither `tsSrcServer` nor `tsSrcClient` is
/// set), and the value brings its OWN separator (`tool_lib.c:517-519`). So the
/// empty column is just an empty string, and the caller needs no branch.
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
) -> String {
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
    out
}

#[cfg(test)]
mod tests {

    /// Measured against `camonitor -h` from EPICS 7.0.10.1-DEV: 2193 bytes on
    /// stderr, byte for byte. The length is pinned because the failure this
    /// guards against — rendering the block through clap instead — changes
    /// every line of it.
    #[test]
    fn the_usage_block_is_cs_text_not_claps() {
        let u = super::usage();
        assert!(u.starts_with("\nUsage: camonitor [options] <PV name> ...\n"));
        // C prints the COMPILE-TIME constants, not the running configuration.
        assert!(u.contains("default is 1.000000 second(s)"));
        assert!(u.contains("(0-99, default 0=lowest)"));
        assert!(u.ends_with("\n\n"), "C's block closes with a blank line");
        assert_eq!(u.len(), 2193);
    }
    use super::{
        Args, TOOL, TimestampKind, TimestampSpec, TimestampState, parse_event_mask,
        parse_timestamp_spec, render_timestamp,
    };
    use clap::CommandFactory;
    use std::time::{Duration, SystemTime};

    /// The real command line, parsed by the real spec — the only way to reach a
    /// resolver now, because a warning without its getopt position cannot be
    /// ordered against the rest of the loop (R13-26).
    fn matches_of(argv: &[&str]) -> clap::ArgMatches {
        Args::command().no_binary_name(true).get_matches_from(argv)
    }

    fn mask_of(argv: &[&str]) -> u16 {
        let m = matches_of(argv);
        parse_event_mask(&mut TOOL.scan(&m), "event_mask")
    }

    fn spec_of(argv: &[&str]) -> TimestampSpec {
        let m = matches_of(argv);
        parse_timestamp_spec(&mut TOOL.scan(&m), "timestamp_key")
    }

    #[test]
    fn mask_default_is_value_alarm() {
        // C `camonitor.c:40`: the no-`-m` default is VALUE|ALARM, NOT
        // value+log+alarm.
        assert_eq!(mask_of(&[]), 1 | 4);
    }

    #[test]
    fn mask_invalid_letter_reverts_to_value_alarm() {
        // C `camonitor.c:298-300`: the first unrecognised letter reverts
        // to VALUE|ALARM and stops scanning (so a leading valid `v` is
        // discarded too).
        assert_eq!(mask_of(&["-m", "xyz"]), 1 | 4);
        assert_eq!(mask_of(&["-m", "vx"]), 1 | 4);
    }

    #[test]
    fn mask_empty_selects_no_events() {
        // C scan loop never runs on an empty arg → eventMask stays 0.
        assert_eq!(mask_of(&["-m", ""]), 0);
    }

    #[test]
    fn mask_parses_dbe_letters() {
        assert_eq!(mask_of(&["-m", "a"]), 4, "alarm-only");
        assert_eq!(mask_of(&["-m", "v"]), 1, "value-only");
        assert_eq!(mask_of(&["-m", "p"]), 8, "property-only");
        assert_eq!(mask_of(&["-m", "val"]), 1 | 4 | 2, "value+alarm+log");
    }

    /// R13-17/R13-26: `case 'm'` re-runs whole per occurrence, so the last one
    /// decides the mask AND every bad one warns at its own position — the
    /// earlier fold looked only at the last occurrence, so an invalid `-m`
    /// followed by a valid one printed nothing where C prints its diagnostic.
    #[test]
    fn every_m_occurrence_is_scanned_and_the_last_decides() {
        assert_eq!(mask_of(&["-m", "v", "-m", "a"]), 4, "last -m wins");
        assert_eq!(
            mask_of(&["-m", "xyz", "-m", "v"]),
            1,
            "the bad one still loses to the later good one"
        );

        let m = matches_of(&["-m", "xyz", "-m", "v"]);
        let mut scan = TOOL.scan(&m);
        let _ = parse_event_mask(&mut scan, "event_mask");
        assert_eq!(
            scan.ordered_warnings(),
            vec!["Invalid argument 'xyz' for option '-m' - ignored."],
            "C warns for the bad occurrence even though a later one wins"
        );
    }

    #[test]
    fn timestamp_spec_parses_keys() {
        let s = spec_of(&[]);
        assert!(s.server && !s.client && matches!(s.kind, TimestampKind::Absolute));
        let s = spec_of(&["-t", "s"]);
        assert!(s.server && !s.client);
        let s = spec_of(&["-t", "c"]);
        assert!(!s.server && s.client);
        let s = spec_of(&["-t", "sc"]);
        assert!(s.server && s.client);
        // 'n' (and unknown letters) select no source → no column.
        let s = spec_of(&["-t", "n"]);
        assert!(!s.server && !s.client, "n selects no source");
        let s = spec_of(&["-t", "cr"]);
        assert!(!s.server && s.client && matches!(s.kind, TimestampKind::Relative));
        assert!(matches!(spec_of(&["-t", "i"]).kind, TimestampKind::IncrAll));
        assert!(matches!(
            spec_of(&["-t", "I"]).kind,
            TimestampKind::IncrChan
        ));
        // Every occurrence re-runs the case (both sources reset first), so the
        // last one decides — `-t c -t s` is server-only, not both.
        let s = spec_of(&["-t", "c", "-t", "s"]);
        assert!(s.server && !s.client, "the last -t resets and re-sets");
    }

    /// W11-B7: `case 't'` resets the two SOURCE globals and nothing else
    /// (`camonitor.c:236-237`). `tsType` is a third global that no `-t` ever
    /// zeroes (`tool_lib.c:45`), so a kind chosen by one occurrence survives
    /// every later occurrence that does not name a kind of its own — the two
    /// axes have different lifetimes, and the port used to reset them together.
    #[test]
    fn a_later_t_resets_the_source_but_never_the_kind() {
        // The kind survives a later `-t` that only names a source.
        let s = spec_of(&["-t", "r", "-t", "s"]);
        assert!(
            s.server && !s.client && matches!(s.kind, TimestampKind::Relative),
            "`-t r -t s` is a SERVER RELATIVE stamp: 's' reset the sources, \
             `tsType` was left alone"
        );

        // ... and so does an incremental kind, through two later occurrences.
        let s = spec_of(&["-t", "I", "-t", "c", "-t", "s"]);
        assert!(
            s.server && !s.client && matches!(s.kind, TimestampKind::IncrChan),
            "no `-t` resets `tsType`, however many follow"
        );

        // A later occurrence that DOES name a kind overwrites it, as its
        // `case 'r'`/`'i'`/`'I'` assignment would.
        let s = spec_of(&["-t", "r", "-t", "si"]);
        assert!(
            s.server && matches!(s.kind, TimestampKind::IncrAll),
            "the later kind wins where one is given"
        );

        // The sources, by contrast, ARE zeroed by every occurrence: the 'c' of
        // the first `-t` does not survive the second.
        let s = spec_of(&["-t", "c", "-t", "r"]);
        assert!(
            !s.server && !s.client && matches!(s.kind, TimestampKind::Relative),
            "`-t c -t r` leaves NO source selected (C prints no stamp column)"
        );
    }

    /// R13-18: `case 't'` switches on EVERY character of `optarg` and its
    /// `default:` warns for each letter it does not know
    /// (`camonitor.c:249-251`, `"Invalid argument '%c' for option '-t' -
    /// ignored."`). Only `n` is accepted silently (`case 'n': break;`,
    /// `camonitor.c:246`). The scan does not stop at a bad letter — unlike
    /// `-m`, which warns once with the whole string and gives up
    /// (`camonitor.c:296-300`) — so the good letters around it still apply.
    #[test]
    fn every_bad_t_character_warns_and_the_good_ones_still_apply() {
        fn warnings(argv: &[&str]) -> Vec<String> {
            let m = matches_of(argv);
            let mut scan = TOOL.scan(&m);
            let _ = parse_timestamp_spec(&mut scan, "timestamp_key");
            scan.ordered_warnings()
        }

        assert_eq!(
            warnings(&["-t", "x"]),
            vec!["Invalid argument 'x' for option '-t' - ignored."]
        );
        assert_eq!(
            warnings(&["-t", "xy"]),
            vec![
                "Invalid argument 'x' for option '-t' - ignored.",
                "Invalid argument 'y' for option '-t' - ignored.",
            ],
            "one warning per unknown character, in the order they appear"
        );
        assert!(
            warnings(&["-t", "n"]).is_empty(),
            "`case 'n'` is the one letter C accepts without a warning"
        );
        assert!(
            warnings(&["-t", "scriI"]).is_empty(),
            "every letter with a case of its own is silent"
        );

        // The bad letter is skipped, not fatal: 's' before it and 'r' after it
        // both still take effect.
        let s = spec_of(&["-t", "sxr"]);
        assert!(
            s.server && matches!(s.kind, TimestampKind::Relative),
            "C's per-character switch carries on past the `default:` arm"
        );
        assert_eq!(
            warnings(&["-t", "sxr"]),
            vec!["Invalid argument 'x' for option '-t' - ignored."]
        );

        // Each occurrence re-runs the case, so a bad letter in an occurrence
        // that later loses still warns at its own position (R13-26).
        assert_eq!(
            warnings(&["-t", "x", "-t", "s"]),
            vec!["Invalid argument 'x' for option '-t' - ignored."]
        );
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
        assert_eq!(
            render_timestamp(none, t1.into(), t1, start, &mut st),
            "",
            "`-t n` renders an EMPTY column, not an absent one — C has no \
             no-timestamp branch, it just prints nothing there"
        );

        // Server relative: first event ABSOLUTE (== absolute render of
        // t1), NOT "10.000000".
        let (mut fsv, mut fp, mut ps, mut pc) = (None, false, None, None);
        let mut st = ts_state(&mut fsv, &mut fp, &mut ps, &mut pc);
        let first = render_timestamp(srv(TimestampKind::Relative), t1.into(), t1, start, &mut st);
        assert_eq!(
            first,
            super::format_server_timestamp(t1),
            "first event must render the absolute server stamp"
        );
        // Second event: diff against the FIRST SERVER stamp (t1), so
        // t2 - t1 = 3s — NOT t2 - start (= 13s).
        let second = render_timestamp(srv(TimestampKind::Relative), t2.into(), t2, start, &mut st);
        assert_eq!(second, srv_diff(3.0));
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
            render_timestamp(srv, t1.into(), t1, start, &mut st),
            super::format_server_timestamp(t1),
            "leading incremental event is absolute"
        );
        assert_eq!(
            render_timestamp(srv, t2.into(), t2, start, &mut st),
            srv_diff(3.0),
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
            second,
            srv_diff(-3.0),
            "backward step must render as a negative delta, not +3"
        );
        assert!(
            second.contains("-3.000000"),
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
        assert_eq!(first, format!("({})", super::format_server_timestamp(c1)));
        // Second: client diff vs program start → 10s (NOT vs c1).
        let second = render_timestamp(cr, start.into(), c2, start, &mut st);
        assert_eq!(second, cli_diff(10.0));
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
            first,
            format!(
                "{}({})",
                super::format_server_timestamp(s1),
                super::format_server_timestamp(c1)
            )
        );
        // Second event: server (s2 - tsFirst=s1) = 3s, client
        // (c2 - tsStart=start) = 10s.
        let second = render_timestamp(both, s2.into(), c2, start, &mut st);
        assert_eq!(second, format!("{}{}", srv_diff(3.0), cli_diff(10.0)));
    }
}
