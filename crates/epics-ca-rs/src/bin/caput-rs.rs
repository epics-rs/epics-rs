use clap::{CommandFactory, FromArgMatches, Parser};
use epics_base_rs::server::snapshot::{DbrClass, Snapshot};
use epics_ca_rs::cli::{
    CountPrefix, PV_NAME_WIDTH, ValueFormat, ca_error_marker, format_time, format_value,
    format_value_segment, sevr_to_str, stat_to_str, zero_dbr_snapshot, zero_dbr_value,
};
use epics_ca_rs::client::{CaChannel, CaClient, enum_string_readback_dbr};
use epics_ca_rs::copt::{self, CTool};
use epics_ca_rs::{CaError, DbFieldType, EpicsValue};
use std::time::SystemTime;

/// One `Old : ...` / `New : ...` line in long-mode shape:
///   `{name-padded}<sep><ts>{sep}<value>{sep}{stat?}{sep}{sevr?}`
/// Mirrors `tool_lib.c::print_time_val_sts` — when alarm is
/// (NO_ALARM, NO_ALARM) the trailing two fields are emitted empty.
fn long_line(name_col: &str, sep: char, snap: &Snapshot, fmt: &ValueFormat) -> String {
    let enum_strings = snap.enums.as_ref().map(|e| e.strings.as_slice());
    // C `caput.c:535,583` calls its `caget()` with `reqElems = 0`, so the
    // count prefix reduces to `nElems > 1` and the `-S` long-string gate to
    // `reqElems(0) || reqElems_pv > 1`. C's readback call is literally
    // `caget(pvs, nPvs, format, 0, 0)` (`caput.c:537`) — `reqElems` is a
    // hardcoded 0 — which is why `caput` never sets `ValueFormat::req_elems`
    // even though `-#` is accepted on the command line (its value is
    // overwritten before the put, `caput.c:418,441`).
    // The separator before the value is C's per-item PREFIX
    // (`tool_lib.c:481-489`), not a suffix of the timestamp.
    let val = format_value_segment(
        &snap.value,
        fmt,
        enum_strings,
        CountPrefix::IfRequestedOrArray,
    );
    let ts = format_time(snap.timestamp);
    let stat = snap.alarm.status;
    let sevr = snap.alarm.severity;
    if stat == 0 && sevr == 0 {
        format!("{name_col}{sep}{ts}{val}{sep}{sep}")
    } else {
        format!(
            "{name_col}{sep}{ts}{val}{sep}{stat_str}{sep}{sevr_str}",
            stat_str = stat_to_str(stat),
            sevr_str = sevr_to_str(sevr),
        )
    }
}

/// C `print_time_val_sts` stamps its error lines with the CLIENT's current
/// time (`epicsTimeGetCurrent`, `tool_lib.c:514-515`), not a server timestamp
/// — a failed read carries no server response to take one from.
fn format_client_timestamp() -> String {
    format_time(SystemTime::now().into())
}

/// The SINGLE owner of what a `caput` readback may write to STDERR.
///
/// C's `caget()` (`caput.c:130-240`) has exactly ONE `fprintf(stderr, ...)`:
/// the `ca_pend_io` timeout warning (`caput.c:186-188`). Every other outcome
/// is silent on stderr —
///
/// * a CA error or read denial renders its `*** ...` marker on STDOUT inside
///   the print loop (`caput.c:200-206`) and `caget()` still returns 0;
/// * `!nConn` returns 1 from `caput.c:181` BEFORE the print loop, so C emits
///   nothing on EITHER stream for that PV.
///
/// Routing both readback sites through one exhaustive match is what keeps a
/// caller from inventing a diagnostic C never prints: `caput-rs` used to
/// `eprintln!("error: {e}")` on the `New :` disconnect path, which has no C
/// counterpart (the exit code already matched).
fn readback_stderr(rb: &Readback) -> Option<&'static str> {
    match rb {
        Readback::TimedOut(..) => Some("Read operation timed out: PV data was not read."),
        Readback::Value(..) | Readback::Disconnected | Readback::Other(_) => None,
    }
}

/// What C's `caget()` (`caput.c:130-240`) prints on STDOUT for one readback,
/// WITHOUT the `Old : ` / `New : ` prefix its caller already emitted.
///
/// `None` means C returned from `if (!nConn) return 1` (`caput.c:181`) BEFORE
/// reaching its print loop — it printed nothing at all, not even a newline,
/// leaving the bare prefix on stdout.
///
/// Whether that non-zero return MATTERS is the caller's business, and the two
/// call sites differ: the `Old :` read's return is DISCARDED (`caput.c:535`)
/// so the put runs regardless, while the `New :` read's return IS caput's
/// exit status (`caput.c:583,589`).
fn readback_line(
    rb: &Readback,
    name_col: &str,
    sep: char,
    fmt: &ValueFormat,
    terse: bool,
    long_mode: bool,
) -> Option<String> {
    match rb {
        // C treats a timed-out read exactly like a successful one here: the
        // zeroed buffer is still a buffer (see `zero_readback`).
        Readback::Value(v, snap) | Readback::TimedOut(v, snap) => {
            let rendered = format_value(v, fmt, None, CountPrefix::IfRequestedOrArray);
            if terse {
                return Some(rendered);
            }
            Some(match (long_mode, snap) {
                (true, Some(s)) => long_line(name_col, sep, s, fmt),
                (true, None) => format!("{name_col}{sep}*{sep}{rendered}{sep}{sep}"),
                (false, _) => format!("{name_col}{sep}{rendered}"),
            })
        }
        Readback::Disconnected => None,
        Readback::Other(e) => {
            let marker = ca_error_marker(e.to_eca_status());
            Some(if terse {
                marker
            } else if long_mode {
                format!(
                    "{name_col}{sep}{ts} {marker}",
                    ts = format_client_timestamp()
                )
            } else {
                format!("{name_col}{sep}{marker}")
            })
        }
    }
}

/// Owner of every C-scanned option argument in this binary (see
/// [`epics_ca_rs::copt`]).
const TOOL: CTool = CTool::new("caput");

/// The getopt cases that `return` from C's `main` (`caput.c:291-297`), by clap
/// id. `copt::Scan::finish` performs the FIRST one on the command line, after
/// replaying the warnings the loop raised on its way there (R13-26).
const TERMINALS: &[(&str, copt::Terminal)] = &[
    ("help", copt::Terminal::Usage(0)),
    ("version", copt::Terminal::Version),
];

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

    /// CA timeout in seconds (`epicsScanDouble`; a bad value warns and keeps
    /// the default — `caput.c:323-332`). Raw `String`: every C-scanned option
    /// argument is resolved by [`epics_ca_rs::copt`], never by clap.
    #[arg(short = 'w', long = "timeout", allow_hyphen_values = true, action = clap::ArgAction::Append)]
    timeout: Vec<String>,

    /// Wait for completion callback (`ca_put_callback`).
    #[arg(short = 'c', long = "callback", action = clap::ArgAction::Count)]
    callback: u8,

    /// CA priority (`sscanf("%u")`, clamped to `CA_PRIORITY_MAX`; `-p -1`
    /// and `-p 500` both clamp to 99 in C, `caput.c:344-351`).
    #[arg(short = 'p', long, allow_hyphen_values = true, action = clap::ArgAction::Append)]
    priority: Vec<String>,

    /// Terse output: print only the new value (no `Old :`/`New :`
    /// prefix, no PV name).
    #[arg(short = 't', long, action = clap::ArgAction::Count)]
    terse: u8,

    /// Long mode: post-write read prints `name timestamp value stat
    /// sevr` like `caget -a`.
    #[arg(short = 'l', long = "long", action = clap::ArgAction::Count)]
    long_mode: u8,

    /// Force interpretation of values as numbers (overrides ENUM
    /// auto-string-resolution). C `caput.c:298-305` makes `-n` and `-s`
    /// a mutually-exclusive pair where the LAST one given wins (each case
    /// sets its flag and clears the other), with no conflict error. Both are
    /// `Count` (R13-17: every C option repeats), so the pairing is resolved
    /// from the command-line indices by [`last_of_pair`] — clap's
    /// `overrides_with` cannot express it once a repeat is legal.
    #[arg(short = 'n', long = "num-enum", action = clap::ArgAction::Count)]
    force_numeric: u8,

    /// Force interpretation of values as strings (overrides numeric
    /// parse for ENUM). Paired with `-n` (last one wins, caput.c:302-305).
    #[arg(short = 's', long = "string-enum", action = clap::ArgAction::Count)]
    force_string: u8,

    /// Put long string as an array of chars (long-string convention).
    /// C `caput.c:306-319` makes `-S` and `-a` a mutually-exclusive pair
    /// where the LAST one given wins (each clears the other), resolved by
    /// [`last_of_pair`] for the same reason as `-n`/`-s`.
    #[arg(short = 'S', long = "long-string", action = clap::ArgAction::Count)]
    long_string: u8,

    /// Put as array. The remaining positionals are
    /// `<count> <v0> <v1> ...`. Paired with `-S` (last one wins,
    /// caput.c:316-319).
    #[arg(short = 'a', long = "array", action = clap::ArgAction::Count)]
    array_mode: u8,

    /// Vestigial array element count. C's getopt string accepts `-# <n>`
    /// (`caput.c:290`, `":cnlhatsVS#:w:p:F:"`) and scans it into `count`
    /// (`:336-343`), but `count` is OVERWRITTEN unconditionally before the
    /// put — with `argc - optind` in `-a` array mode (`:418`) and with `1` in
    /// scalar mode (`:441`) — so the value never reaches the wire, the
    /// readback element count, or the output. Accepted here for the same
    /// reason C accepts it: `caput -# 3 PV 1` must not fail, and clap
    /// otherwise exits 2 on the unknown flag.
    ///
    /// Held as a raw `String` so a non-numeric argument reproduces C's
    /// `sscanf`-failure warning ([`Args::scan_dead_element_count`]) instead of
    /// clap's own parse error.
    #[arg(
        short = '#',
        long = "max-elements",
        value_name = "COUNT",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    max_elements: Vec<String>,

    /// Alternate output field separator: C takes `(char) *optarg`, the FIRST
    /// character, discarding the rest (`caput.c:353`).
    #[arg(
        short = 'F',
        long = "field-separator",
        value_name = "OFS",
        allow_hyphen_values = true,
        action = clap::ArgAction::Append
    )]
    field_separator: Vec<String>,

    /// Positional PV name. NOT clap-`required` — see `caget-rs`; C checks
    /// `nPvs < 1` after the getopt loop (`caput.c:457-461`).
    pv_name: Option<String>,

    /// Positional values. In `-a` mode the first element is the
    /// count, the rest are the values. Negative numeric values are
    /// allowed via `--`.
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    values: Vec<String>,
}

impl Args {
    /// C `caput.c:336-343`: `-#` scans its argument with `sscanf("%d")` and,
    /// on failure, warns and falls back to `count = 0`. The count is dead
    /// either way (see [`Args::max_elements`]), so the warning is the flag's
    /// ONLY observable effect — but it is raised by the SAME owner every
    /// other C-scanned option uses ([`copt::Scan::req_elems_int`]), so `caput`
    /// cannot drift from `caget` on what "not a valid array element count"
    /// means, nor on WHERE the warning lands in the getopt order. The returned
    /// count is deliberately discarded.
    fn scan_dead_element_count(scan: &mut copt::Scan) {
        let _ = scan.req_elems_int("max_elements");
    }
}

/// C `caput.c:298-319`: `-n`/`-s` and `-S`/`-a` are pairs where each case
/// SETS its own flag and CLEARS its partner, so the LAST occurrence of either
/// member decides — and neither can be on when the other is. Both members
/// repeat (R13-17), and clap records the index of each flag's last
/// occurrence, which is exactly what "last of the pair wins" needs.
///
/// Returns `(this_wins, that_wins)`; both `false` when neither was given.
///
/// `get_count` is the gate, not `indices_of`: clap hands an ABSENT `Count`
/// flag its `0` default and still reports an index for it, so an
/// `indices_of`-only test reads every unused flag as present.
fn last_of_pair(matches: &clap::ArgMatches, this_id: &str, that_id: &str) -> (bool, bool) {
    let last = |id: &str| {
        (matches.get_count(id) > 0)
            .then(|| matches.indices_of(id).and_then(|mut i| i.next_back()))
            .flatten()
    };
    match (last(this_id), last(that_id)) {
        (None, None) => (false, false),
        (Some(_), None) => (true, false),
        (None, Some(_)) => (false, true),
        (Some(a), Some(b)) => (a > b, b > a),
    }
}

#[tokio::main]
async fn main() {
    let cmd = Args::command();
    let parsed = TOOL.get_matches(cmd.clone());
    let matches = parsed.matches();
    let args = Args::from_arg_matches(matches).expect("clap validated the arguments");

    // The two mutually-clearing pairs, resolved in command-line order.
    let (force_numeric, force_string) = last_of_pair(matches, "force_numeric", "force_string");
    let (long_string, array_mode) = last_of_pair(matches, "long_string", "array_mode");
    let terse = args.terse > 0;
    let callback = args.callback > 0;

    // C's ENTIRE getopt loop runs before the `nPvs < 1` / `nPvs < 2` checks
    // (`caput.c:290-455`, then `:457`), so every option argument is scanned —
    // and every warning raised — even when no PV name or value follows.
    let mut scan = parsed.scan();
    Args::scan_dead_element_count(&mut scan);
    let ca_timeout = scan.timeout("timeout", epics_ca_rs::cli::env_default_timeout());
    let priority = scan.priority("priority");
    let field_separator = scan.field_separator("field_separator");
    // End of C's getopt loop: warnings out in command-line order, then `-h` /
    // `-V` if the loop reached one (R13-26).
    scan.finish(&cmd, &epics_ca_rs::protocol::version_info(), TERMINALS);

    // -n / -s steer ENUM scalar handling below (force_numeric =
    // interpret as index; force_string = always send DBR_STRING for
    // server-side menu resolution). For non-ENUM channels they have
    // no effect — matching C `caput`, where enumAsNr / enumAsString
    // only gate the DBR_ENUM branch.

    // C `caput.c:457-465`: PV name first, then value — both are post-getopt
    // checks with C's own one-line diagnostic and status 1.
    let Some(pv_name) = args.pv_name else {
        TOOL.no_pv_name();
    };

    if args.values.is_empty() {
        TOOL.no_value();
    }

    let client = CaClient::new().await.expect("failed to create CA client");
    let timeout = epics_ca_rs::cli::timeout_duration(ca_timeout);
    // -p selects the priority virtual circuit. C `caput.c:406-410` runs the
    // same `connect_pvs` barrier as caget/cainfo ("If the connection fails,
    // we're done"): its ECA_TIMEOUT diagnostic names the single PV, and the
    // put phase never starts.
    let names = [pv_name.clone()];
    let ch = match epics_ca_rs::cli::connect_pvs(&client, &names, priority, timeout).await {
        Ok(mut channels) => channels.remove(0),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

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
    // C caput.c:455-465: for an ENUM field, read the menu (DBR_GR_ENUM)
    // BEFORE building the write, so each value can be matched against the
    // state names (caput.c:487-494). A menu-read timeout aborts the put
    // exactly as C does (caput.c:461-465). The menu is empty for non-ENUM
    // fields, which makes `build_write_value` skip the ENUM path entirely.
    let enum_menu: Vec<epics_ca_rs::PvString> = if native_type == epics_ca_rs::DbFieldType::Enum {
        match ch.get_with_metadata(DbrClass::Gr).await {
            Ok(snap) => snap.enums.map(|e| e.strings).unwrap_or_default(),
            Err(CaError::Timeout) => {
                eprintln!("Read operation timed out: ENUM data was not read.");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    } else {
        Vec::new()
    };
    // C caput.c:147-152: the readback (the `Old :`/`New :` display) is
    // requested in the STRING form for an ENUM field unless `-n`, so the
    // echoed value is the state label, not the index. `-l` keeps the
    // TIME-class string so the timestamp + alarm line is still populated.
    let enum_dbr = enum_string_readback_dbr(native_type, args.long_mode > 0, !force_numeric);

    // Read the pre-put value for the `Old :` display. Long mode also
    // wants the server timestamp + alarm pair captured BEFORE the put so
    // the `Old :` line reflects the actual pre-put state — the regular
    // path stays on the cheaper plain GET.
    let long_mode = args.long_mode > 0;
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
    // The readback's element count: C sets `reqElems = nElems =
    // ca_element_count(chid)` (`caput.c:142,154-155`) and hands that count to
    // the zeroed buffer it prints on a read timeout.
    let read_elems = ch.element_count().unwrap_or(1);
    // An ENUM read back in its label form is a DBR_STRING readback, so its
    // zeroed buffer is an empty string, not a 0 index.
    let read_as_string = enum_dbr.is_some();

    // The readback display format. C's `charArrAsStr` (`-S`) is read by BOTH
    // the write-value builder (`caput.c:514`, DBR_CHAR) and the readback print
    // loop (`caput.c:211-222`), which escapes a CHAR-array readback back into
    // its long-string form — so `-S` must reach the `Old :`/`New :` rendering,
    // not just the wire value. `format_value` owns C's gate
    // (`charArrAsStr && dbr_type_is_CHAR && (reqElems || nElems > 1)`).
    let mut fmt = ValueFormat {
        char_array_as_string: long_string,
        ..ValueFormat::default()
    };
    if let Some(c) = field_separator {
        fmt.field_separator = c;
    }
    let sep = fmt.field_separator;
    // C pads the PV-name column on the READ element count (`pvs[n].reqElems <=
    // 1 && fieldSeparator == ' '`, caput.c:196-198), which caput knows before
    // the put — it never depends on the value that comes back.
    let name_col = if read_elems <= 1 && sep == ' ' {
        format!("{pv_name:<width$}", width = PV_NAME_WIDTH)
    } else {
        pv_name.clone()
    };

    // C caput.c:531-535 gates the pre-put "Old :" read+print on
    // `if (format != terse)`. Terse mode prints only the new value, so the
    // pre-put GET must NOT be issued: C never issues it, and a PV that is slow
    // to read, read-denied before a write-side access transition, or backed by
    // an expensive/side-effecting read path must still proceed to the write.
    // The post-put read below is kept in every mode (C still calls caget()
    // after the put, caput.c:583; terse only suppresses the `New :` label).
    //
    // The read's RETURN VALUE IS DISCARDED (`result` is overwritten by the put
    // at caput.c:539-548), so NOTHING about this read can abort caput: a
    // read-denied but writable PV prints `*** no read access` here and still
    // gets its put, and a PV that dropped since the connect barrier prints
    // nothing at all (C returns from `!nConn` before its print loop) and lets
    // the PUT report the disconnect.
    // Build (and VALIDATE) the value to write BEFORE the `Old :` read+print.
    // C's order is not incidental: the enum/number parse sits at
    // `caput.c:466-530` and every failure in it `return 1`s on the spot, so
    // the `Old : ` block at `:531-535` is never reached (R13-22):
    //
    //     caput TST:MBBO Bogus
    //       C:  Enum string value 'Bogus' invalid.                    (exit 1)
    //       RS: Old : TST:MBBO   Zero
    //           Enum string value 'Bogus' invalid.                    (exit 1)
    //
    // A rejected put must not leave a readback line on stdout. Value
    // precedence inside the build is C's own — `-S` (long string) resolved
    // before any native-type parse; see `build_write_value`.
    let parsed_value = match build_write_value(
        &args.values,
        native_type,
        force_numeric,
        force_string,
        long_string,
        array_mode,
        &enum_menu,
    ) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    if !terse {
        print!("Old : ");
        let rb = classify_readback(
            read_display(ch.clone()).await,
            native_type,
            read_as_string,
            read_elems,
            long_mode,
        );
        // C's `caget()` emits its stderr warning (if any) BEFORE the print
        // loop (`caput.c:186-188` then `:191`). `readback_stderr` is the only
        // source of it.
        if let Some(warning) = readback_stderr(&rb) {
            eprintln!("{warning}");
        }
        match readback_line(&rb, &name_col, sep, &fmt, false, long_mode) {
            Some(line) => println!("{line}"),
            // C printed the `Old : ` prefix and nothing else — not even a
            // newline. Flush it so the put's stderr diagnostic does not race a
            // buffered partial line.
            None => {
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        }
    }

    let result = match &parsed_value {
        WriteValue::Wire { dbr_type, value } => {
            // C-tool wire model: send the explicit DBR type (DBR_STRING /
            // DBR_CHAR) and let the server convert. The CLI `-w` timeout
            // owns the callback wait, matching caput.c:556-567.
            if callback {
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
            if callback {
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
            if callback {
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

    // Re-read for echoing to stdout (matches C caput which always reads the PV
    // back after the put, caput.c:583). Same readback type selection as the
    // `Old :` read above, so an ENUM `New :` value also echoes as the state
    // label. C returns THIS readback's status as caput's exit status
    // (caput.c:583,589), and inside `caget()` only `!nConn` yields a non-zero
    // return (`caput.c:181`) — see [`Readback`]. A read TIMEOUT does NOT:
    // C prints `New : <name> <zeroed value>` and exits 0.
    if !terse {
        // C `caput -t` suppresses the label, not the read (caput.c:580-583).
        print!("New : ");
    }
    let rb = classify_readback(
        read_display(ch.clone()).await,
        native_type,
        read_as_string,
        read_elems,
        long_mode,
    );
    if let Some(warning) = readback_stderr(&rb) {
        eprintln!("{warning}");
    }
    match readback_line(&rb, &name_col, sep, &fmt, terse, long_mode) {
        Some(line) => println!("{line}"),
        // `!nConn`: C's `caget()` returns 1 from `caput.c:181` BEFORE its
        // print loop, so it emits NOTHING for this PV — no stdout line (not
        // even a newline after the `New : ` prefix) and no stderr
        // diagnostic. That return value IS caput's exit status
        // (`caput.c:583,589`). Flush so the bare prefix reaches stdout
        // before the process exits.
        None => {
            let _ = std::io::Write::flush(&mut std::io::stdout());
            std::process::exit(1);
        }
    }
}

/// What `caput-rs` will write. Like C `caput`, every value travels as an
/// explicit DBR wire type that the server converts to the native field
/// type — there is no native-binary-typed put path. Non-ENUM values go as
/// `DBR_STRING` / `DBR_CHAR`; ENUM values go as `DBR_STRING` (by name) or
/// `DBR_DOUBLE` (numeric fallback), per `caput.c:486-552`.
#[derive(Debug)]
enum WriteValue {
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
    /// against the record's menu — see `CaChannel::put_string`. Carried as
    /// a byte-preserving [`epics_ca_rs::PvString`] so a non-UTF-8 escape survives.
    EnumString(epics_ca_rs::PvString),
    /// An ENUM waveform written by name — each element a `DBR_STRING`
    /// the server resolves against the record's menu. See
    /// `CaChannel::put_string_array`.
    EnumStringArray(Vec<epics_ca_rs::PvString>),
}

/// One `caput` readback — the `Old :` read (`caput.c:535`) and the `New :`
/// read (`caput.c:583`) are the SAME C function, `caget()`
/// (`caput.c:130-240`) — classified by C's contract.
#[derive(Debug)]
enum Readback {
    /// The get completed: print this value.
    Value(EpicsValue, Option<Snapshot>),
    /// `ca_pend_io` timed out (`caput.c:186-188`): C warns on stderr and
    /// falls through to the print loop with the ZEROED buffer. Exit stays 0.
    TimedOut(EpicsValue, Option<Snapshot>),
    /// No channel connected: C `caget()` returns 1 from `if (!nConn) return
    /// 1` (`caput.c:181`) BEFORE its print loop, and caput propagates that as
    /// the exit status (`caput.c:583,589`).
    ///
    /// The variant is deliberately payload-free. C prints nothing at all on
    /// this path — not the PV's `*** not connected` marker (the print loop is
    /// never reached) and not any stderr diagnostic — so there is no CA error
    /// text to render, and carrying one only invited the port-invented
    /// `error: channel disconnected` line this replaces.
    Disconnected,
    /// Any other readback failure. C's `ca_array_get` failing *synchronously*
    /// — most notably read-access-denied, which libca rejects client-side with
    /// `ECA_NORDACCESS` before any I/O is outstanding — leaves `ca_pend_io` at
    /// `ECA_NORMAL`; `caget()` prints a `*** ...` marker for the PV but still
    /// returns 0 (`caput.c:200-206,239`), so C exits 0.
    Other(CaError),
}

/// The value C `caput` prints for a readback whose `ca_pend_io` timed out:
/// the zeroed `calloc` buffer, sized from the DBR type it REQUESTED. Owned
/// by [`epics_ca_rs::cli::zero_dbr_value`] — `caget` renders the same buffer
/// on its own synchronous timeout, and the contract is C's, not caput's.
///
/// `as_string` marks the ENUM-read-back-as-label case: that get requests a
/// `DBR_STRING`, so its zeroed buffer is an empty string, not a `0` index.
fn zero_readback(
    native: DbFieldType,
    as_string: bool,
    count: u32,
    long_mode: bool,
) -> (EpicsValue, Option<Snapshot>) {
    let base = if as_string {
        DbFieldType::String
    } else {
        native
    };
    let value = zero_dbr_value(base, count);
    let snap = long_mode.then(|| zero_dbr_snapshot(base, count));
    (value, snap)
}

/// Single owner of C `caput`'s readback contract: map one get result onto
/// [`Readback`], substituting the zeroed buffer C would print on timeout.
fn classify_readback(
    res: Result<(EpicsValue, Option<Snapshot>), CaError>,
    native: DbFieldType,
    as_string: bool,
    count: u32,
    long_mode: bool,
) -> Readback {
    match res {
        Ok((value, snap)) => Readback::Value(value, snap),
        Err(CaError::Timeout) => {
            let (value, snap) = zero_readback(native, as_string, count, long_mode);
            Readback::TimedOut(value, snap)
        }
        Err(CaError::Disconnected | CaError::Shutdown) => Readback::Disconnected,
        Err(e) => Readback::Other(e),
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

/// `raw_from_escaped` decoded into a byte-preserving [`epics_ca_rs::PvString`] for the
/// DBR_STRING / ENUM-by-name put paths. The common escapes (`\n`, `\t`,
/// `\\`, …) decode to ASCII; a high-byte `\xNN` reaches the wire as its
/// literal byte — `EpicsValue::String` now carries raw bytes ([`epics_ca_rs::PvString`]),
/// matching C's byte buffer with no UTF-8 lossy fixup.
///
/// The decoded byte run is truncated to [`DBR_STRING_PAYLOAD_MAX`] bytes, the
/// way C `caput` decodes into its fixed `EpicsStr` buffer and forces a
/// trailing NUL (caput.c:484-489 for ENUM names, caput.c:523-528 for
/// native DBR_STRING): an overlong CLI value is written as its 39-byte
/// prefix, not rejected. The Rust client's `validate_put_strings` /
/// libca's `nciu::stringVerify` reject `>= 40`, so the cap must happen in
/// the CLI builder to keep C-tool parity. `-S` long strings take the
/// DBR_CHAR path (`raw_from_escaped`, not this helper) and stay uncapped.
/// The cut is byte-oriented, exactly matching C's byte buffer — no UTF-8
/// char-boundary fixup, since the value is bytes, not text.
fn raw_from_escaped_string(s: &str) -> epics_ca_rs::PvString {
    let mut bytes = raw_from_escaped(s);
    bytes.truncate(DBR_STRING_PAYLOAD_MAX);
    epics_ca_rs::PvString::from_bytes(bytes)
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
///
/// For an ENUM field `enum_menu` is the record's state-name list
/// (`DBR_GR_ENUM`, read up front by the caller as C does at
/// `caput.c:459`). C classifies each value against that menu — a name
/// that matches a state goes as `DBR_STRING`, otherwise it falls back to
/// a number sent as `DBR_DOUBLE` (`caput.c:486-508`). The menu is empty
/// for non-ENUM fields.
fn build_write_value(
    values: &[String],
    native_type: epics_ca_rs::DbFieldType,
    force_numeric: bool,
    force_string: bool,
    long_string: bool,
    array_mode: bool,
    enum_menu: &[epics_ca_rs::PvString],
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
        // ENUM waveform: classify each element against the menu exactly as
        // the scalar path does (C `caput.c:467-509` runs the same per-value
        // loop for any count). Build one consistent wire type for the whole
        // array — see `build_enum_array`.
        if native_type == epics_ca_rs::DbFieldType::Enum {
            return build_enum_array(tokens, force_numeric, force_string, enum_menu);
        }
        // Non-ENUM array: C sends every element as a DBR_STRING after
        // epicsStrnRawFromEscaped (caput.c:540-552), regardless of the
        // native numeric or string field type — the server converts each.
        let escaped: Vec<epics_ca_rs::PvString> =
            tokens.iter().map(|t| raw_from_escaped_string(t)).collect();
        return Ok(WriteValue::Wire {
            dbr_type: epics_ca_rs::DbFieldType::String as u16,
            value: epics_ca_rs::EpicsValue::StringArray(escaped),
        });
    }

    // Scalar: C `caput` joins extra positionals with single spaces.
    let joined = values.join(" ");

    // (1) ENUM field type is handled FIRST (caput.c:455), BEFORE `-S` —
    // charArrAsStr never applies to an ENUM PV. C reads the menu and
    // classifies the value against it (`classify_enum_token`): a state
    // name goes as DBR_STRING, a number as DBR_DOUBLE. Sending a numeric-
    // looking *label* (e.g. "1" where state 1 is named "1") as a native
    // index would silently mean the wrong state — the menu match prevents
    // that (caput.c:486-508).
    if native_type == epics_ca_rs::DbFieldType::Enum {
        return match classify_enum_token(&joined, force_numeric, force_string, enum_menu)? {
            EnumToken::Name(s) => Ok(WriteValue::EnumString(s)),
            EnumToken::Number(n) => Ok(WriteValue::Wire {
                dbr_type: epics_ca_rs::DbFieldType::Double as u16,
                value: epics_ca_rs::EpicsValue::Double(n),
            }),
        };
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

/// One ENUM value's classification, mirroring C `caput.c`'s per-value
/// `dbrType` decision: a menu state name is written as `DBR_STRING`
/// ([`EnumToken::Name`]); anything else is written as a `DBR_DOUBLE`
/// number ([`EnumToken::Number`]).
enum EnumToken {
    Name(epics_ca_rs::PvString),
    Number(f64),
}

/// Classify one value written to an ENUM field, mirroring C
/// `caput.c:467-509`.
///
/// * `-n` (`force_numeric`): parse the token as a number and send it as
///   `DBR_DOUBLE` (caput.c:469-482); a non-number is an error.
/// * default / `-s`: escape-decode the token and compare it byte-for-byte
///   against the menu state names (`strcmp`, caput.c:487-494). A match is
///   sent as `DBR_STRING`. A non-match falls back to a number sent as
///   `DBR_DOUBLE` (caput.c:496-508) — UNLESS `-s` (`force_string`) forbids
///   the numeric fallback, in which case the value is rejected.
///
/// This is the structural fix for the numeric-label defect: a token like
/// "1" is matched against the menu FIRST, so when state 1 is literally
/// named "1" it is written by name (DBR_STRING) and resolves to the right
/// state, instead of being sent as a native index that could mean a
/// different state.
fn classify_enum_token(
    token: &str,
    force_numeric: bool,
    force_string: bool,
    menu: &[epics_ca_rs::PvString],
) -> Result<EnumToken, String> {
    if force_numeric {
        let index = parse_enum_double(token)
            .ok_or_else(|| format!("Enum index value '{token}' is not a number."))?;
        warn_if_enum_index_too_large(token, index, menu); // caput.c:477-479
        return Ok(EnumToken::Number(index));
    }
    // C escapes the value into a fixed EpicsStr buffer before comparing
    // it to the menu names (caput.c:487-488).
    let escaped = raw_from_escaped_string(token);
    if menu
        .iter()
        .any(|name| name.as_bytes() == escaped.as_bytes())
    {
        return Ok(EnumToken::Name(escaped));
    }
    // Not a menu name: `-s` rejects it outright (caput.c:499); otherwise
    // try the escaped text as a number (caput.c:498-507).
    if force_string {
        return Err(format!("Enum string value '{escaped}' invalid."));
    }
    let text = String::from_utf8_lossy(escaped.as_bytes()).into_owned();
    let index = parse_enum_double(&text)
        .ok_or_else(|| format!("Enum string value '{escaped}' invalid."))?;
    // C warns with `sbuf[i]` here — the ESCAPED text, not the raw argv token
    // it warns with on the `-n` path above (caput.c:505-507 vs :477-479).
    warn_if_enum_index_too_large(&text, index, menu);
    Ok(EnumToken::Number(index))
}

/// C `caput.c:477-479` and `:505-507` — the same warning at both places an ENUM
/// value ends up as a NUMBER (R13-23):
///
/// ```text
/// Warning: enum index value '%s' may be too large.
/// ```
///
/// `dbuf[i] >= bufGrEnum.no_str`: an index at or past the end of the menu is
/// suspicious, but NOT fatal — C prints this and puts the value anyway (neither
/// site `return`s, unlike every other diagnostic in the block). A negative index
/// is below `no_str`, so it does not warn; an empty menu (`no_str == 0`) warns
/// for every index, as C's comparison does.
///
/// A menu NAME never reaches here: C only compares the numeric `dbuf[i]`.
fn warn_if_enum_index_too_large(token: &str, index: f64, menu: &[epics_ca_rs::PvString]) {
    if index >= menu.len() as f64 {
        eprintln!("Warning: enum index value '{token}' may be too large.");
    }
}

/// Parse an ENUM value as a number, mirroring C `epicsStrtod`
/// (caput.c:470,498). Returns `None` when the token is not a number.
/// Stricter than `strtod` in rejecting trailing garbage (e.g. "1.5x"),
/// which is irrelevant for the clean indices ENUM values carry.
fn parse_enum_double(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok()
}

/// Build an ENUM waveform write, applying [`classify_enum_token`] to each
/// element (C `caput.c:467-509` loops over `count` values).
///
/// C shares a single `dbrType` across the array — the LAST element's
/// classification wins (caput.c:489/507) — which silently zeroes any
/// name element when the final element is numeric (`dbuf` is never set
/// for a name). We pick ONE wire type for the whole array instead: all
/// numbers → `DBR_DOUBLE[]`; otherwise → `DBR_STRING[]` so every name
/// still resolves (a numeric element falls to the server's index parse),
/// rather than corrupting name elements to 0.
fn build_enum_array(
    tokens: &[String],
    force_numeric: bool,
    force_string: bool,
    menu: &[epics_ca_rs::PvString],
) -> Result<WriteValue, String> {
    let classified: Vec<EnumToken> = tokens
        .iter()
        .map(|t| classify_enum_token(t, force_numeric, force_string, menu))
        .collect::<Result<_, _>>()?;

    let mut numbers = Vec::with_capacity(classified.len());
    let mut all_number = true;
    for c in &classified {
        match c {
            EnumToken::Number(n) => numbers.push(*n),
            EnumToken::Name(_) => all_number = false,
        }
    }
    if all_number {
        return Ok(WriteValue::Wire {
            dbr_type: epics_ca_rs::DbFieldType::Double as u16,
            value: epics_ca_rs::EpicsValue::DoubleArray(numbers),
        });
    }
    // At least one menu name → DBR_STRING array. A `Number` element keeps
    // its escaped token text; the server resolves it by index parse.
    let names = tokens
        .iter()
        .zip(&classified)
        .map(|(t, c)| match c {
            EnumToken::Name(s) => s.clone(),
            EnumToken::Number(_) => raw_from_escaped_string(t),
        })
        .collect();
    Ok(WriteValue::EnumStringArray(names))
}

#[cfg(test)]
mod tests {
    use super::{
        Args, Readback, ValueFormat, WriteValue, build_write_value, classify_readback,
        last_of_pair, raw_from_escaped, raw_from_escaped_string, readback_line, readback_stderr,
        zero_readback,
    };
    use clap::{CommandFactory, Parser};
    use epics_base_rs::types::WallTime;
    use epics_ca_rs::cli::EPICS_EPOCH_UNIX_SECS;
    use epics_ca_rs::copt::scan_i32;
    use epics_ca_rs::{CaError, DbFieldType, EpicsValue};

    fn vals(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn menu_vals(s: &[&str]) -> Vec<epics_ca_rs::PvString> {
        s.iter().map(|x| (*x).into()).collect()
    }

    /// C's getopt string accepts `-# <n>` (`caput.c:290`) and scans it into
    /// `count` (`:336-343`), but `count` is then overwritten unconditionally —
    /// `argc - optind` in `-a` mode (`:418`), `1` in scalar mode (`:441`) — so
    /// the flag is vestigial: it must PARSE, and it must change nothing.
    ///
    /// Pre-fix caput-rs had no `-#` at all, so clap exited 2 on the unknown
    /// flag where C runs the put.
    #[test]
    fn hash_flag_is_accepted_and_ignored() {
        // Scalar: `-# 3` parses, and the value list is untouched.
        let a = Args::try_parse_from(["caput", "-#", "3", "PV", "42"])
            .expect("C's getopt accepts -# (caput.c:290); clap must too");
        assert_eq!(a.max_elements, vals(&["3"]));
        assert_eq!(a.pv_name.as_deref(), Some("PV"));
        assert_eq!(a.values, vals(&["42"]));
        // The count that reaches the wire comes from the values, never from
        // `-#` — C overwrites it at caput.c:441. The scan is still performed
        // (through the shared owner), purely for its warning.
        assert_eq!(scan_i32("3"), Some(3));

        // Array mode: `-# 1` must not truncate the 3-element write.
        let a = Args::try_parse_from(["caput", "-a", "-#", "1", "WF", "3", "1", "2", "3"])
            .expect("-# parses alongside -a");
        assert!(a.array_mode > 0);
        assert_eq!(a.values, vals(&["3", "1", "2", "3"]));

        // A non-numeric argument: C's `sscanf` fails, warns, and keeps going
        // (caput.c:337-342). It must NOT be a hard error.
        let a = Args::try_parse_from(["caput", "-#", "abc", "PV", "42"])
            .expect("C warns on a bad -# argument, it does not exit");
        assert_eq!(a.max_elements, vals(&["abc"]));
        assert_eq!(scan_i32("abc"), None, "the owner warns and falls back to 0");
        // C's `sscanf("%d")` takes a LEADING integer and ignores the tail, so
        // `3x` scans fine and warns nothing.
        Args::try_parse_from(["caput", "-#", "3x", "PV", "42"]).expect("leading digits");
        assert_eq!(scan_i32("3x"), Some(3));
    }

    /// C `caput.c:298-319` parses `-n`/`-s` and `-S`/`-a` as two
    /// mutually-exclusive last-wins pairs via a getopt switch (each case
    /// sets its flag and clears the paired one), with NO conflict error.
    /// [`last_of_pair`] must reproduce that for every order boundary —
    /// including a REPEAT of either member (R13-17), which clap's former
    /// `overrides_with` spelling could not express.
    #[test]
    fn enum_and_array_flags_are_last_wins_pairs() {
        let enum_pair = |extra: &[&str]| {
            let mut argv = vec!["caput-rs"];
            argv.extend_from_slice(extra);
            argv.extend_from_slice(&["PV", "1"]);
            let m = Args::command().get_matches_from(argv);
            last_of_pair(&m, "force_numeric", "force_string")
        };
        let array_pair = |extra: &[&str]| {
            let mut argv = vec!["caput-rs"];
            argv.extend_from_slice(extra);
            argv.extend_from_slice(&["PV", "1"]);
            let m = Args::command().get_matches_from(argv);
            last_of_pair(&m, "long_string", "array_mode")
        };

        // -n / -s: last one wins, neither errors.
        assert_eq!(enum_pair(&["-n", "-s"]), (false, true), "-n -s → string");
        assert_eq!(enum_pair(&["-s", "-n"]), (true, false), "-s -n → numeric");
        assert_eq!(enum_pair(&["-n"]), (true, false), "-n alone → numeric");
        assert_eq!(enum_pair(&["-s"]), (false, true), "-s alone → string");
        assert_eq!(enum_pair(&[]), (false, false), "neither given");
        assert_eq!(
            enum_pair(&["-n", "-s", "-n"]),
            (true, false),
            "a repeat is legal and the LAST occurrence still wins"
        );

        // -S / -a: last one wins, never both set.
        assert_eq!(array_pair(&["-a", "-S"]), (true, false), "-a -S → long str");
        assert_eq!(array_pair(&["-S", "-a"]), (false, true), "-S -a → array");
        assert_eq!(array_pair(&["-a"]), (false, true), "-a alone → array");
        assert_eq!(array_pair(&["-S"]), (true, false), "-S alone → long str");
        assert_eq!(array_pair(&[]), (false, false), "neither given");
        assert_eq!(
            array_pair(&["-S", "-a", "-S"]),
            (true, false),
            "a repeat is legal and the LAST occurrence still wins"
        );
    }

    /// A readback timeout is NOT fatal in C. `ca_pend_io` returning
    /// ECA_TIMEOUT only prints the stderr warning (`caput.c:186-188`); the
    /// function keeps going, prints the (still zeroed) buffer and returns 0
    /// (`caput.c:239`).
    ///
    /// This test previously asserted the opposite ("read timeout must fail
    /// caput like C ca_pend_io ECA_TIMEOUT"), pinning a behaviour C does not
    /// have — `caput.c:186-188` has no `return`.
    #[test]
    fn readback_timeout_is_not_fatal_and_yields_the_zeroed_buffer() {
        let r = classify_readback(
            Err(CaError::Timeout),
            DbFieldType::Double,
            /* as_string */ false,
            /* count */ 1,
            /* long_mode */ false,
        );
        match r {
            Readback::TimedOut(v, snap) => {
                assert_eq!(
                    v,
                    EpicsValue::Double(0.0),
                    "C prints the calloc'd, never-filled buffer: a zeroed double"
                );
                assert!(snap.is_none(), "no -l → no timestamp/alarm line");
            }
            other => panic!("read timeout must classify as TimedOut, got {other:?}"),
        }
    }

    /// The zeroed buffer takes the shape of the READBACK type, not of the
    /// value that was written: a label-form ENUM readback is a DBR_STRING
    /// get, so its zeroed buffer prints as an empty string, and an array
    /// readback zeroes every element (C `dbr_size_n(dbrType, reqElems)`,
    /// `caput.c:167`).
    #[test]
    fn zero_readback_takes_the_shape_of_the_readback_type() {
        let (v, _) = zero_readback(DbFieldType::Enum, /* as_string */ true, 1, false);
        assert_eq!(
            v,
            EpicsValue::String(epics_ca_rs::PvString::from("")),
            "ENUM read back as a label is a DBR_STRING get → zeroed string"
        );

        let (v, _) = zero_readback(DbFieldType::Enum, /* as_string */ false, 1, false);
        assert_eq!(
            v,
            EpicsValue::Enum(0),
            "`caput -n` reads the ENUM index → zeroed index"
        );

        let (v, _) = zero_readback(DbFieldType::Long, false, 3, false);
        assert_eq!(
            v,
            EpicsValue::LongArray(vec![0, 0, 0]),
            "an array readback zeroes ca_element_count elements"
        );
    }

    /// Under `-l` C prints the zeroed `dbr_time_*` header too: the EPICS
    /// epoch stamp (secPastEpoch == 0) and NO_ALARM / NO_ALARM.
    #[test]
    fn zero_readback_long_mode_carries_the_epics_epoch_and_no_alarm() {
        let (_, snap) = zero_readback(DbFieldType::Double, false, 1, /* long_mode */ true);
        let snap = snap.expect("-l readback carries a timestamp/alarm line");
        assert_eq!(snap.alarm.status, 0);
        assert_eq!(snap.alarm.severity, 0);
        assert_eq!(
            snap.timestamp,
            WallTime::from_unix(EPICS_EPOCH_UNIX_SECS, 0),
            "a zeroed epicsTimeStamp is the EPICS epoch, 1990-01-01T00:00:00Z"
        );
    }

    /// C's `caget()` inside caput has exactly ONE `fprintf(stderr, ...)` — the
    /// `ca_pend_io` timeout warning (`caput.c:186-188`). Everything else is
    /// silent on stderr: a CA error / read denial renders its `*** ...` marker
    /// on STDOUT (`caput.c:200-206`), and the `!nConn` path returns from
    /// `caput.c:181` BEFORE the print loop, emitting nothing on EITHER stream.
    ///
    /// Pre-fix, `caput-rs`'s `New :` disconnect path wrote a port-invented
    /// `error: channel disconnected` to stderr (the exit code already matched
    /// C's `return 1`). The `Readback::Disconnected` variant is now
    /// payload-free, so there is no error text left to render, and both
    /// readback sites take their stderr from this one function.
    #[test]
    fn readback_disconnect_prints_nothing_on_either_stream() {
        let fmt = ValueFormat::default();

        // `!nConn`: silent on stdout AND stderr, in every output mode.
        assert_eq!(readback_stderr(&Readback::Disconnected), None);
        for (terse, long_mode) in [(false, false), (true, false), (false, true)] {
            assert_eq!(
                readback_line(&Readback::Disconnected, "PV", ' ', &fmt, terse, long_mode),
                None,
                "C never reaches its print loop (terse={terse}, long={long_mode})"
            );
        }

        // The ONE stderr line C does print.
        assert_eq!(
            readback_stderr(&Readback::TimedOut(EpicsValue::Double(0.0), None)),
            Some("Read operation timed out: PV data was not read.")
        );

        // Negative control: a CA error is NOT silent — but it speaks on
        // STDOUT, via the marker, and still says nothing on stderr.
        let other = Readback::Other(CaError::ServerError(epics_ca_rs::protocol::ECA_NORDACCESS));
        assert_eq!(readback_stderr(&other), None);
        let line = readback_line(&other, "PV", ' ', &fmt, true, false)
            .expect("a CA error still reaches C's print loop");
        assert_eq!(line, "*** no read access");

        // A good readback is silent on stderr too.
        assert_eq!(
            readback_stderr(&Readback::Value(EpicsValue::Double(1.0), None)),
            None
        );
    }

    /// Only `!nConn` (`caput.c:181`) makes C's readback return non-zero.
    #[test]
    fn readback_disconnect_is_fatal() {
        // Both disconnect and shutdown map to C's `!nConn` no-connection guard.
        assert!(matches!(
            classify_readback(
                Err(CaError::Disconnected),
                DbFieldType::Double,
                false,
                1,
                false
            ),
            Readback::Disconnected
        ));
        assert!(matches!(
            classify_readback(Err(CaError::Shutdown), DbFieldType::Double, false, 1, false),
            Readback::Disconnected
        ));
    }

    #[test]
    fn readback_other_errors_are_nonfatal() {
        // Read-access-denied and other synchronous CA failures: C's caget still
        // returns ECA_NORMAL (ca_pend_io never timed out) → caput exits 0.
        assert!(matches!(
            classify_readback(
                Err(CaError::ServerError(0x178)), // ECA_NORDACCESS
                DbFieldType::Double,
                false,
                1,
                false
            ),
            Readback::Other(_)
        ));
        assert!(matches!(
            classify_readback(
                Err(CaError::Protocol("bad frame".into())),
                DbFieldType::Double,
                false,
                1,
                false
            ),
            Readback::Other(_)
        ));
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
            &[],
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
            let r = build_write_value(&vals(&["not a number"]), nt, false, false, true, false, &[]);
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
        // (charArrAsStr) never applies to an ENUM PV: a value that matches
        // a menu state name routes to DBR_STRING (server resolves the
        // name), NOT a DBR_CHAR array. Pre-fix the top-level `-S` block
        // hijacked this.
        let menu = menu_vals(&["Stop", "Run", "not a number"]);
        let r = build_write_value(
            &vals(&["not a number"]),
            DbFieldType::Enum,
            false,
            false,
            true,
            false,
            &menu,
        );
        match r {
            Ok(WriteValue::EnumString(s)) => assert_eq!(s, "not a number"),
            other => panic!("-S on an ENUM PV must yield EnumString, got {other:?}"),
        }
        // An integer index that is NOT a menu state name falls back to a
        // number sent as DBR_DOUBLE (caput.c:507) even with `-S` set —
        // ENUM precedence, then the numeric fallback path.
        let idx = build_write_value(
            &vals(&["5"]),
            DbFieldType::Enum,
            false,
            false,
            true,
            false,
            &menu,
        );
        match idx {
            Ok(WriteValue::Wire {
                dbr_type,
                value: EpicsValue::Double(n),
            }) => {
                assert_eq!(dbr_type, DbFieldType::Double as u16);
                assert_eq!(n, 5.0);
            }
            other => panic!("out-of-menu index on ENUM PV → DBR_DOUBLE, got {other:?}"),
        }
    }

    #[test]
    fn enum_numeric_label_matches_menu_name_before_index() {
        // Regression: a record whose state 1 is literally named "1" must
        // be written by NAME (DBR_STRING) so the server resolves it to that
        // state — C matches the menu before the numeric fallback
        // (caput.c:487-494). Sending "1" as a native index instead could
        // mean a different state.
        let menu = menu_vals(&["0", "1", "2"]);
        match build_write_value(
            &vals(&["1"]),
            DbFieldType::Enum,
            false,
            false,
            false,
            false,
            &menu,
        ) {
            Ok(WriteValue::EnumString(s)) => assert_eq!(s, "1"),
            other => panic!("numeric-looking menu label must go by name, got {other:?}"),
        }
        // A value with no matching state name falls back to a number
        // (DBR_DOUBLE), not a native index (caput.c:496-507).
        match build_write_value(
            &vals(&["7"]),
            DbFieldType::Enum,
            false,
            false,
            false,
            false,
            &menu,
        ) {
            Ok(WriteValue::Wire {
                dbr_type,
                value: EpicsValue::Double(n),
            }) => {
                assert_eq!(dbr_type, DbFieldType::Double as u16);
                assert_eq!(n, 7.0);
            }
            other => panic!("out-of-menu value → DBR_DOUBLE fallback, got {other:?}"),
        }
    }

    #[test]
    fn enum_force_string_rejects_non_menu_value() {
        // `-s` (enumAsString) forbids the numeric fallback: a value that
        // matches no state name is an error, not a coerced index
        // (caput.c:499-503).
        let menu = menu_vals(&["Off", "On"]);
        let err = build_write_value(
            &vals(&["3"]),
            DbFieldType::Enum,
            false,
            true,
            false,
            false,
            &menu,
        );
        assert!(
            matches!(&err, Err(m) if m.contains("invalid")),
            "-s on a non-menu value must error, got {err:?}"
        );
        // A name that DOES match is accepted as DBR_STRING.
        match build_write_value(
            &vals(&["On"]),
            DbFieldType::Enum,
            false,
            true,
            false,
            false,
            &menu,
        ) {
            Ok(WriteValue::EnumString(s)) => assert_eq!(s, "On"),
            other => panic!("-s with a matching name → EnumString, got {other:?}"),
        }
    }

    #[test]
    fn enum_force_numeric_sends_dbr_double_ignoring_menu() {
        // `-n` (enumAsNr) interprets every value as a number sent as
        // DBR_DOUBLE, never matching the menu (caput.c:467-482) — even a
        // value that IS a state name.
        let menu = menu_vals(&["1", "2"]);
        match build_write_value(
            &vals(&["1"]),
            DbFieldType::Enum,
            true,
            false,
            false,
            false,
            &menu,
        ) {
            Ok(WriteValue::Wire {
                dbr_type,
                value: EpicsValue::Double(n),
            }) => {
                assert_eq!(dbr_type, DbFieldType::Double as u16);
                assert_eq!(n, 1.0);
            }
            other => panic!("-n must send DBR_DOUBLE, got {other:?}"),
        }
        // `-n` on a non-number is an error.
        let err = build_write_value(
            &vals(&["Open"]),
            DbFieldType::Enum,
            true,
            false,
            false,
            false,
            &menu,
        );
        assert!(
            matches!(&err, Err(m) if m.contains("is not a number")),
            "-n on a non-number must error, got {err:?}"
        );
    }

    #[test]
    fn enum_array_homogeneous_and_mixed_wire_types() {
        // ENUM waveform: all-name → DBR_STRING[]; all-number → DBR_DOUBLE[].
        let menu = menu_vals(&["Stop", "Run"]);
        // `-a PV 2 Stop Run`: both are state names → DBR_STRING[].
        match build_write_value(
            &vals(&["2", "Stop", "Run"]),
            DbFieldType::Enum,
            false,
            false,
            false,
            true,
            &menu,
        ) {
            Ok(WriteValue::EnumStringArray(a)) => {
                assert_eq!(a, vec!["Stop", "Run"]);
            }
            other => panic!("name array → DBR_STRING[], got {other:?}"),
        }
        // `-a PV 2 0 1`: neither matches a name → DBR_DOUBLE[] (caput.c:507).
        match build_write_value(
            &vals(&["2", "0", "1"]),
            DbFieldType::Enum,
            false,
            false,
            false,
            true,
            &menu,
        ) {
            Ok(WriteValue::Wire {
                dbr_type,
                value: EpicsValue::DoubleArray(a),
            }) => {
                assert_eq!(dbr_type, DbFieldType::Double as u16);
                assert_eq!(a, vec![0.0, 1.0]);
            }
            other => panic!("numeric array → DBR_DOUBLE[], got {other:?}"),
        }
        // Mixed `-a PV 2 Stop 1`: at least one name → DBR_STRING[] for the
        // whole array (we avoid C's last-element-wins corruption that would
        // zero the name element).
        match build_write_value(
            &vals(&["2", "Stop", "1"]),
            DbFieldType::Enum,
            false,
            false,
            false,
            true,
            &menu,
        ) {
            Ok(WriteValue::EnumStringArray(a)) => assert_eq!(a, vec!["Stop", "1"]),
            other => panic!("mixed array → DBR_STRING[], got {other:?}"),
        }
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
        ) {
            Ok(WriteValue::Wire {
                value: EpicsValue::StringArray(a),
                ..
            }) => assert_eq!(a, vec!["1", "2"]),
            other => panic!("non-numeric count token must be ignored, got {other:?}"),
        }
        // `-a PV 0`: zero trailing values reaches the write path as an
        // empty array (count == 0), decided by the server — not a CLI error.
        match build_write_value(
            &vals(&["0"]),
            DbFieldType::Long,
            false,
            false,
            false,
            true,
            &[],
        ) {
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
            match build_write_value(
                &vals(&[tok]),
                DbFieldType::Char,
                false,
                false,
                false,
                false,
                &[],
            ) {
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
            &[],
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
            &[],
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
        // ENUM-by-name scalar (`-s`) -> EnumString capped to 39. The value
        // is escape-decoded and truncated to 39 bytes BEFORE the menu
        // compare, so the matching state name is the 39-byte form.
        let menu_label: epics_ca_rs::PvString = "a".repeat(39).into();
        match build_write_value(
            &vals(&[long.as_str()]),
            DbFieldType::Enum,
            false,
            true,
            false,
            false,
            std::slice::from_ref(&menu_label),
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
            &[],
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

    /// PVA-89 / CA Latin-1 parity: a `\xNN` escape with a high byte must
    /// reach the wire as that literal byte. C `caput` decodes into a raw
    /// `EpicsStr` byte buffer (epicsString.c:55-118) with no UTF-8 fixup;
    /// the Rust port carries the bytes in `PvString::from_bytes`, so a
    /// non-UTF-8 byte run round-trips verbatim instead of being mangled
    /// into U+FFFD replacement sequences.
    #[test]
    fn escaped_high_bytes_reach_wire_verbatim_not_lossy() {
        // `\xff` and `\x80` are not valid standalone UTF-8; a lossy
        // `from_utf8_lossy` would turn each into the 3-byte U+FFFD. The
        // byte-preserving path keeps them as single 0xFF / 0x80 bytes.
        let pv = raw_from_escaped_string("\\xff\\x80\\x41");
        assert_eq!(
            pv.as_bytes(),
            &[0xff, 0x80, 0x41],
            "high-byte escapes must survive as literal bytes, not U+FFFD"
        );
        // The full Latin-1 range (0x00 excluded — a literal NUL stops the
        // decoder) is representable byte-for-byte.
        let pv = raw_from_escaped_string("\\xc3\\x28");
        assert_eq!(pv.as_bytes(), &[0xc3, 0x28], "invalid UTF-8 pair preserved");
    }

    /// PVA-89: the byte truncation to `DBR_STRING_PAYLOAD_MAX` (39) is
    /// byte-oriented, never a UTF-8 char-boundary fixup. 40 raw high bytes
    /// must cut to exactly 39 bytes — C decodes into a fixed byte buffer
    /// (`rem = 40`, NUL-terminated), so the cut is on bytes, not chars.
    #[test]
    fn escaped_high_bytes_truncate_on_byte_boundary() {
        // 40 `\xff` escapes → 40 raw 0xFF bytes, capped to 39.
        let input: String = "\\xff".repeat(40);
        let pv = raw_from_escaped_string(&input);
        assert_eq!(pv.as_bytes().len(), 39, "byte-oriented cut at 39");
        assert!(
            pv.as_bytes().iter().all(|&b| b == 0xff),
            "every surviving byte is the literal 0xFF, no UTF-8 fixup"
        );
    }

    /// PVA-89 end-to-end: `build_write_value` for a DBR_STRING put must
    /// carry the decoded high bytes into `EpicsValue::String(PvString)`
    /// verbatim — the gateway/server sees the same bytes a C `caput` sends.
    #[test]
    fn build_write_value_dbr_string_preserves_high_bytes() {
        match build_write_value(
            &vals(&["\\xff\\x80"]),
            DbFieldType::String,
            false,
            false,
            false,
            false,
            &[],
        ) {
            Ok(WriteValue::Wire {
                dbr_type,
                value: EpicsValue::String(s),
            }) => {
                assert_eq!(dbr_type, DbFieldType::String as u16, "DBR_STRING wire type");
                assert_eq!(
                    s.as_bytes(),
                    &[0xff, 0x80],
                    "DBR_STRING put carries raw high bytes to the wire"
                );
            }
            other => panic!("expected DBR_STRING Wire with raw bytes, got {other:?}"),
        }
    }
}
