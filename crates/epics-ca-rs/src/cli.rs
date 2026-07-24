//! Helpers shared across the `caget` / `caput` / `cainfo` / `camonitor`
//! command-line binaries.

// RTEMS-EXEC-MODEL-ALLOW(1): the test's subject is arming a *tokio* timer with
// INDEFINITE_TIMEOUT; the tokio timer is the property under test. These run and pass in the
// feature-ON suite on the tokio driver.
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString, WallTime};

use crate::client::{CaChannel, CaClient};

/// Default CA CLI timeout in seconds when neither `-w` nor a usable
/// `EPICS_CLI_TIMEOUT` env var is set.
pub const DEFAULT_CLI_TIMEOUT_SECS: f64 = 1.0;

/// Deadline meaning "wait indefinitely", used for a CLI `-w 0` /
/// `EPICS_CLI_TIMEOUT=0`. C `caget`/`caput`/`camonitor` pass `caTimeout`
/// straight to `ca_pend_io` / `ca_pend_event`, where a value of `0.0`
/// waits forever — `ca_pend_io(0)` calls `pendIO(DBL_MAX)` and
/// `ca_pend_event(0)` loops `pendEvent(60.0)` without end (EPICS base
/// `access.cpp:495-499,468-474`). A far-future finite `Duration`
/// (≈10 years) reproduces that "0 == forever" without an `Option`,
/// keeping the `Duration`-typed client API (`wait_connected`,
/// `get_with_timeout`, `put_with_timeout`) unchanged; it is effectively
/// unbounded for any CLI session and stays well inside tokio's timer
/// range, so arming a `timeout` / `sleep` with it does not overflow.
pub const INDEFINITE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10 * 365 * 24 * 60 * 60);

/// Deadline meaning "already expired", which is what a NEGATIVE `-w` (or
/// `EPICS_CLI_TIMEOUT`) is in C (W10-B1).
///
/// C hands `caTimeout` straight to `ca_pend_io`, which turns it into a
/// deadline `now + caTimeout`. A negative value puts that deadline in the
/// PAST, so `ca_pend_io` returns `ECA_TIMEOUT` without ever waiting, and
/// `connect_pvs` (`tool_lib.c:628-638`) reports the connect failure and exits
/// 1 — even for a PV that is right there:
///
/// ```text
/// caget -w -1 TST:AO   C: Channel connect timed out: 'TST:AO' not found.  (exit 1)
/// ```
///
/// A zero-length `Duration` reproduces that exactly (an armed `tokio::timeout`
/// with it elapses at once) and cannot be confused with `-w 0`, which means
/// wait FOREVER and resolves to [`INDEFINITE_TIMEOUT`]. Naming it keeps
/// "expired" a state a reader can see, rather than a magic zero.
pub const EXPIRED_TIMEOUT: std::time::Duration = std::time::Duration::ZERO;

/// C `tool_lib.c:646-660` `use_ca_timeout_env`, which every tool calls BEFORE
/// its getopt loop (`caget.c:396`, `camonitor.c:222`, `caput.c:288`,
/// `cainfo.c:144`): `EPICS_CLI_TIMEOUT` sets `caTimeout` to whatever
/// `epicsScanDouble` accepts, and a value it REJECTS both warns on stderr and
/// falls back to `DEFAULT_TIMEOUT` (`tool_lib.h:51`, 1.0 s).
///
/// The scan is [`crate::copt::scan_double`] — the single owner of C's
/// `epicsScanDouble` semantics, the very scanner `-w` goes through. A second,
/// bare `str::parse` here read the env var STRICTLY where C is lenient:
/// `EPICS_CLI_TIMEOUT=" -1 "` scans as -1 in C (`epicsParseDouble` skips
/// leading and trailing whitespace) and made the tool exit 1 on an expired
/// deadline, while the port silently took the 1 s default and connected — a
/// flipped exit status from a value C accepts (R14-17). A rejected value was
/// silently defaulted too, where C names the variable on stderr.
///
/// The warning is printed HERE, not buffered into [`crate::copt::Scan`], because
/// C prints it before the getopt loop starts: it precedes every option warning
/// and the `-h` usage block, whatever the command line says.
///
/// A value of `0` means "wait forever" (see [`INDEFINITE_TIMEOUT`]) and a
/// NEGATIVE value is an already-expired deadline ([`EXPIRED_TIMEOUT`], W10-B1);
/// both are passed through to [`timeout_duration`]. Only the non-finite values
/// stay held back to the default — the deliberate deviation `timeout_duration`
/// documents (C's `nan` hangs forever), and C's own scan accepts them silently,
/// so no warning is raised for them either.
pub fn env_default_timeout() -> f64 {
    let Ok(raw) = std::env::var("EPICS_CLI_TIMEOUT") else {
        return DEFAULT_CLI_TIMEOUT_SECS;
    };
    let Some(secs) = crate::copt::scan_double(&raw) else {
        eprintln!(
            "'{raw}' is not a valid timeout value (from 'EPICS_CLI_TIMEOUT' in the \
             environment) - ignored. (use '-h' for help.)"
        );
        return DEFAULT_CLI_TIMEOUT_SECS;
    };
    if secs.is_finite() {
        secs
    } else {
        DEFAULT_CLI_TIMEOUT_SECS
    }
}

/// Convert a user-supplied timeout (CLI `-w` or env var) into a
/// `std::time::Duration`.
///
/// Three states, because C has three (W10-B1):
///
/// * `0` means wait INDEFINITELY — C `ca_pend_io(0)` / `ca_pend_event(0)`,
///   see [`INDEFINITE_TIMEOUT`]. NOT the 1 s default.
/// * a NEGATIVE value is a deadline in the past, i.e. already
///   [`EXPIRED_TIMEOUT`]: C waits not at all and reports a connect timeout.
///   The port used to clamp it to the default and connect happily, which is
///   the opposite outcome.
/// * anything positive and finite is that many seconds.
///
/// `Duration::from_secs_f64` panics on NaN and infinity, and clap hands those
/// through literally, so the guard against them stays — they resolve to
/// [`DEFAULT_CLI_TIMEOUT_SECS`].
///
/// DEVIATION, deliberate: C's `-w nan` reaches `ca_pend_io(nan)`, where every
/// deadline comparison is false and the tool HANGS forever. We treat NaN as
/// the default instead. An infinite `-w inf` is likewise treated as the
/// default rather than as a ~forever wait.
pub fn timeout_duration(secs: f64) -> std::time::Duration {
    if secs == 0.0 {
        // Covers -0.0 too: C's `caTimeout == 0` test is numeric, and -0.0 is
        // not negative for it either.
        return INDEFINITE_TIMEOUT;
    }
    if secs < 0.0 {
        return EXPIRED_TIMEOUT;
    }
    let s = if secs.is_finite() {
        secs
    } else {
        DEFAULT_CLI_TIMEOUT_SECS
    };
    std::time::Duration::from_secs_f64(s)
}

/// The C `connect_pvs` diagnostic for a connect timeout
/// (`tool_lib.c:630-636`). It depends only on the *number* of PVs asked
/// for, never on which of them failed: more than one PV collapses to
/// "some PV(s)", a lone PV is named.
pub fn connect_timeout_message(names: &[String]) -> String {
    if names.len() > 1 {
        "Channel connect timed out: some PV(s) not found.".to_string()
    } else {
        format!(
            "Channel connect timed out: '{}' not found.",
            names.first().map(String::as_str).unwrap_or("")
        )
    }
}

/// A failed [`connect_pvs`] barrier: carries the exact C diagnostic to
/// print on stderr before the tool exits 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectPvsTimeout {
    message: String,
}

impl std::fmt::Display for ConnectPvsTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConnectPvsTimeout {}

/// C `tool_lib.c::connect_pvs` (`:623-641`) — the single all-channels
/// barrier the synchronous CA tools put between channel creation and
/// their data phase.
///
/// C creates every channel, then waits for ALL of them in ONE
/// `ca_pend_io(caTimeout)`. On `ECA_TIMEOUT` it prints the diagnostic and
/// returns 1, and the caller skips its whole get+print phase
/// (`caget.c:553-556`, `cainfo.c:228-232`, `caput.c:406-410` all read
/// `if (!result)` / `if (result) return result`). A partial connect
/// therefore emits ZERO stdout value lines — not a per-PV interleaving of
/// values and `*** not connected` markers. Owning that barrier here keeps
/// the all-or-nothing contract in ONE place instead of re-deriving it in
/// each binary.
///
/// The connect wait runs concurrently across channels, so the whole
/// barrier fits in one `timeout` window, exactly like C's single
/// `ca_pend_io`. C's other failure mode — `ca_create_channel` itself
/// rejecting a name (`create_pvs`, `tool_lib.c:588-594`) — has no port
/// analogue: [`CaClient::create_channel_with_priority`] is infallible and
/// defers every failure to the connect wait.
pub async fn connect_pvs(
    client: &CaClient,
    names: &[String],
    priority: u8,
    timeout: std::time::Duration,
) -> Result<Vec<CaChannel>, ConnectPvsTimeout> {
    let channels: Vec<CaChannel> = names
        .iter()
        .map(|name| client.create_channel_with_priority(name, priority))
        .collect();
    let all_connected =
        futures_util::future::join_all(channels.iter().map(|ch| ch.wait_connected(timeout)))
            .await
            .into_iter()
            .all(|r| r.is_ok());

    if all_connected {
        Ok(channels)
    } else {
        Err(ConnectPvsTimeout {
            message: connect_timeout_message(names),
        })
    }
}

/// Field width the C tools (`caget` / `camonitor` / `caput -l`) use
/// when printing the PV name column: `printf("%-30s ...", name)` —
/// 30 chars left-aligned, then one space before the value. Mirrors
/// `epics-base/modules/ca/src/tools/tool_lib.c::print_value`'s width.
pub const PV_NAME_WIDTH: usize = 30;

/// `epicsAlarmConditionStrings` (`libcom/src/misc/alarmString.c:27-50`) —
/// the alarm-status mnemonics the CA tools print. Index is the wire `stat`
/// byte; note index 11 is `HWLIMIT`, with no underscore.
const ALARM_CONDITION_STRINGS: [&str; 22] = [
    "NO_ALARM",
    "READ",
    "WRITE",
    "HIHI",
    "HIGH",
    "LOLO",
    "LOW",
    "STATE",
    "COS",
    "COMM",
    "TIMEOUT",
    "HWLIMIT",
    "CALC",
    "SCAN",
    "LINK",
    "SOFT",
    "BAD_SUB",
    "UDF",
    "DISABLE",
    "SIMM",
    "READ_ACCESS",
    "WRITE_ACCESS",
];

/// `epicsAlarmSeverityStrings` (`libcom/src/misc/alarmString.c:17-22`).
const ALARM_SEVERITY_STRINGS: [&str; 4] = ["NO_ALARM", "MINOR", "MAJOR", "INVALID"];

/// Out-of-range alarm index rendering. C `tool_lib.h:28-36` bounds every
/// index against `lastEpicsAlarmCond` / `lastEpicsAlarmSev` and yields
/// `"??"` past it — the tools never print a made-up token.
const ALARM_STRING_UNKNOWN: &str = "??";

/// C `tool_lib.h:28-30` `stat_to_str` — the single owner of the CA tools'
/// alarm-status rendering (`caget -a`, `camonitor`, `caput -l` all print
/// through it). An index past the table yields `"??"` (`ALARM_STRING_UNKNOWN`).
///
/// The `stat_to_str` / `stat_to_str_unsigned` macro pair differs only in
/// C's `(stat) >= 0` lower bound, which is vacuous for the port's unsigned
/// wire `u16`, so one function covers both.
///
/// Deliberately NOT delegated to `epics_base_rs::server::recgbl::
/// alarm_condition_string`: that owner serves pvxs `alarm.message`, whose
/// out-of-range value is `""`, not `"??"`.
pub fn stat_to_str(stat: u16) -> &'static str {
    ALARM_CONDITION_STRINGS
        .get(stat as usize)
        .copied()
        .unwrap_or(ALARM_STRING_UNKNOWN)
}

/// C `tool_lib.h:32-34` `sevr_to_str` — see [`stat_to_str`].
pub fn sevr_to_str(sevr: u16) -> &'static str {
    ALARM_SEVERITY_STRINGS
        .get(sevr as usize)
        .copied()
        .unwrap_or(ALARM_STRING_UNKNOWN)
}

/// C's `*** ...` marker for a PV whose read failed, keyed on the PV's ECA
/// status — the single owner of those strings for `caget` and `caput`.
///
/// `caput` carries its own copy of `caget()` (`caput.c:130-240`), and the two
/// print loops are identical here (`caget.c:262-267`, `caput.c:201-206`):
/// `ECA_DISCONN` → `*** not connected`, `ECA_NORDACCESS` → `*** no read
/// access`, anything else → `*** CA error` plus `ca_message` text.
///
/// The fourth marker, `*** no data available (timeout)`
/// ([`NO_DATA_MARKER`]), is NOT keyed on a status: C reaches it on
/// `value == 0`, which only a callback get can leave — see
/// [`zero_dbr_value`].
pub fn ca_error_marker(status: u32) -> String {
    match status {
        crate::protocol::ECA_DISCONN => "*** not connected".to_string(),
        crate::protocol::ECA_NORDACCESS => "*** no read access".to_string(),
        s => format!("*** CA error {}", crate::protocol::eca_message(s)),
    }
}

/// C's marker for a readback that produced NO buffer at all (`value == 0`,
/// `caget.c:268`). Only the callback get (`caget -c`) can leave one — the
/// synchronous get callocs up front (see [`zero_dbr_value`]).
pub const NO_DATA_MARKER: &str = "*** no data available (timeout)";

/// Unix seconds at the EPICS epoch (1990-01-01T00:00:00Z) — the instant a
/// zeroed `epicsTimeStamp` denotes, and therefore the timestamp C's `-a` /
/// `-l` modes print for a timed-out synchronous readback (see
/// [`zero_dbr_snapshot`]).
pub const EPICS_EPOCH_UNIX_SECS: u64 = 631_152_000;

/// C `tool_lib.c:52` `timeFormatStr` — the ONE format every CA tool passes to
/// `epicsTimeToStrftime` for an absolute stamp (`caget -a`, `caget -d
/// DBR_TIME_*`, `caput -l`, every `camonitor` line, and the `*** disconnected`
/// / `*** CA error` lines at `tool_lib.c:515-529`).
///
/// This is the single owner of that rendering. The fractional field is NOT
/// what `chrono`'s `%.6f` produces: C ROUNDS the nanoseconds into the
/// requested width, and clamps so the rounding can never carry into the whole
/// seconds (`epicsTime.cpp:233-238`, W10-B4):
///
/// ```c
/// unsigned long frac = pTS->nsec + div[fracWid] / 2;   /* div[6] == 1000 */
/// if (frac >= nSecPerSec)
///     frac = nSecPerSec - 1;                           /* never carries */
/// frac /= div[fracWid];
/// ```
///
/// so `nsec = 1_500` prints `.000002` (chrono truncated it to `.000001`), and
/// `nsec = 999_999_600` prints `.999999` on the SAME second rather than
/// rolling the clock forward.
pub fn format_time(ts: WallTime) -> String {
    use chrono::{DateTime, Local, TimeZone};

    // C treats an all-zero `epicsTimeStamp` as UNINITIALIZED and prints a
    // sentinel instead of a date (`epicsTime.cpp:174-179`, W10-B3):
    //
    //     // presume that EPOCH date is an uninitialized time stamp
    //     if ( pTS->secPastEpoch == 0 && pTS->nsec == 0u ) {
    //         strncpy ( pBuff, "<undefined>", bufLength );
    //
    // The test is on the stamp AS IT ARRIVED — seconds past the EPICS epoch,
    // before any timezone applies — which is why it lives here and not at a
    // call site holding a local `DateTime`. A never-processed record
    // (`caget -a TST:NEVER`) and every timed-out synchronous readback (see
    // `zero_dbr_snapshot`) carry exactly this stamp.
    if ts.unix_secs() == EPICS_EPOCH_UNIX_SECS && ts.subsec_nanos() == 0 {
        return "<undefined>".to_string();
    }

    // Round to microseconds the way C does, then clamp: `frac` can reach
    // 1e9 only by rounding up, and C refuses to let that touch the seconds.
    let frac = u64::from(ts.subsec_nanos()) + 500;
    let usec = frac.min(999_999_999) / 1_000;

    // The whole seconds are formatted from the stamp with a ZERO fraction:
    // C runs `strftime` on the `tm` and prints the fraction itself, so the
    // seconds field must never see the rounded-up value.
    let secs = i64::try_from(ts.unix_secs()).unwrap_or(i64::MAX);
    let dt: DateTime<Local> = match Local.timestamp_opt(secs, 0).single() {
        Some(dt) => dt,
        // Unrepresentable instant: C's `strftime` would print garbage rather
        // than fail. Nothing on the CA wire can produce one (a u32
        // `secPastEpoch` tops out in 2106), so fall back to the epoch.
        None => Local.timestamp_opt(0, 0).unwrap(),
    };
    format!("{}.{usec:06}", dt.format("%Y-%m-%d %H:%M:%S"))
}

/// The value carrier of a CA request DBR code — the type C's
/// `dbr_size_n(dbrType, nElems)` sizes its buffer from (`db_access.h`).
/// Codes 0..=34 repeat the seven base types every 7 codes (`STRING SHORT
/// FLOAT ENUM CHAR LONG DOUBLE`, through `DBR_CTRL_DOUBLE`); the four
/// trailing special codes carry their own payload. `None` for a code
/// outside 0..=38 (C `INVALID_DB_REQ`).
pub fn dbr_value_field_type(dbr_type: u16) -> Option<DbFieldType> {
    match dbr_type {
        0..=34 => DbFieldType::from_u16(dbr_type % 7).ok(),
        // DBR_PUT_ACKT / DBR_PUT_ACKS: a bare `dbr_put_ackt_t` (u16).
        35 | 36 => Some(DbFieldType::UShort),
        // DBR_STSACK_STRING / DBR_CLASS_NAME: a `dbr_string_t`.
        37 | 38 => Some(DbFieldType::String),
        _ => None,
    }
}

/// The buffer C's *synchronous* CA readback prints when `ca_pend_io` times
/// out — the single owner of that contract for `caget` and `caput`.
///
/// Both tools `calloc` the readback buffer BEFORE issuing `ca_array_get`
/// (`caget.c:207-215`, `caput.c:167`), sized `dbr_size_n(dbrType, nElems)`.
/// On `ECA_TIMEOUT` they only warn on stderr (`caget.c:224-226`,
/// `caput.c:186-188`): the buffer is neither freed nor is the PV's status
/// touched, so the print loop still sees `status == ECA_NORMAL` and
/// `value != 0` (`caget.c:262-268`, `caput.c:201-207`) and renders the
/// still-ZEROED buffer — a numeric field prints `0`, a string (or
/// ENUM-as-label) field prints an empty value, and an array prints its
/// element count then that many zeros.
///
/// The `*** no data available (timeout)` branch (`caget.c:268`,
/// `caput.c:207`) needs `value == 0`, which ONLY the callback get can leave:
/// `caget -c` allocates inside its event handler (`caget.c:130`), so a
/// callback that never arrives leaves no buffer at all. That branch is
/// therefore unreachable from any synchronous readback, and dead in `caput`,
/// whose readback is always synchronous.
///
/// `base` is the carrier of the DBR type actually REQUESTED (see
/// [`dbr_value_field_type`]) — not the channel's native type: an ENUM read
/// back in label form is a `DBR_STRING` get, so its zeroed buffer is an
/// empty string, not a `0` index.
pub fn zero_dbr_value(base: DbFieldType, count: u32) -> EpicsValue {
    // C sizes the buffer `dbr_size_n(dbrType, nElems)`; a scalar channel has
    // nElems == 1.
    let n = count.max(1) as usize;
    let scalar = n == 1;
    let pick = |scalar_v: EpicsValue, array_v: EpicsValue| if scalar { scalar_v } else { array_v };
    match base {
        DbFieldType::String => pick(
            EpicsValue::String(PvString::from("")),
            EpicsValue::StringArray(vec![PvString::from(""); n]),
        ),
        DbFieldType::Short => pick(EpicsValue::Short(0), EpicsValue::ShortArray(vec![0; n])),
        DbFieldType::Float => pick(EpicsValue::Float(0.0), EpicsValue::FloatArray(vec![0.0; n])),
        DbFieldType::Enum => pick(EpicsValue::Enum(0), EpicsValue::EnumArray(vec![0; n])),
        DbFieldType::Char => pick(EpicsValue::Char(0), EpicsValue::CharArray(vec![0; n])),
        DbFieldType::Long => pick(EpicsValue::Long(0), EpicsValue::LongArray(vec![0; n])),
        DbFieldType::Double => pick(
            EpicsValue::Double(0.0),
            EpicsValue::DoubleArray(vec![0.0; n]),
        ),
        DbFieldType::Int64 => pick(EpicsValue::Int64(0), EpicsValue::Int64Array(vec![0; n])),
        DbFieldType::UInt64 => pick(EpicsValue::UInt64(0), EpicsValue::UInt64Array(vec![0; n])),
        DbFieldType::UShort => pick(EpicsValue::UShort(0), EpicsValue::UShortArray(vec![0; n])),
        DbFieldType::ULong => pick(EpicsValue::ULong(0), EpicsValue::ULongArray(vec![0; n])),
        DbFieldType::UChar => pick(EpicsValue::UChar(0), EpicsValue::UCharArray(vec![0; n])),
    }
}

/// The zeroed `dbr_time_*` header C renders alongside [`zero_dbr_value`] on
/// a synchronous readback timeout: a zeroed `epicsTimeStamp` is the EPICS
/// epoch, and the alarm pair is NO_ALARM / NO_ALARM — for which
/// `tool_lib.c::print_time_val_sts` prints two empty trailing fields. The
/// metadata blocks stay `None`, which renders as C's zeroed `dbr_gr_*` /
/// `dbr_ctrl_*` limits.
pub fn zero_dbr_snapshot(base: DbFieldType, count: u32) -> Snapshot {
    Snapshot::new(
        zero_dbr_value(base, count),
        0,
        0,
        WallTime::from_unix(EPICS_EPOCH_UNIX_SECS, 0),
    )
}

/// Float number representation requested via `-e` / `-f` / `-g`.
#[derive(Debug, Clone, Copy)]
pub enum FloatStyle {
    /// `%g` — shortest of `%f` / `%e`. C tools default. Precision is
    /// the count of *significant* digits.
    G,
    /// `%e` — scientific notation. Precision is digits after decimal.
    E,
    /// `%f` — fixed-point. Precision is digits after decimal.
    F,
}

/// Float formatting options. C precision defaults to 6 for all three
/// styles per `printf(3)`.
#[derive(Debug, Clone, Copy)]
pub struct FloatFormat {
    pub style: FloatStyle,
    pub precision: u32,
}

impl Default for FloatFormat {
    fn default() -> Self {
        Self {
            style: FloatStyle::G,
            precision: 6,
        }
    }
}

/// A `sprint_long` output base — C `tool_lib.c`'s `IntFormatT`
/// (`dec` / `bin` / `oct` / `hex`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntStyle {
    Dec,
    Hex,
    Oct,
    Bin,
}

/// Per-tool CLI formatting state.
///
/// C keeps the integer base and the float base in TWO independent globals
/// (`tool_lib.c:51-52`, `outTypeI` / `outTypeF`), and every tool's getopt
/// writes exactly one of them: `-0x`/`-0o`/`-0b` sets `outTypeI`,
/// `-lx`/`-lo`/`-lb` sets `outTypeF` (`caget.c:485-497`,
/// `camonitor.c:325-340`). Neither flag touches the other's global, so
/// `int_style` and `float_style` are mirrored as two separate fields — a
/// single shared base field made `-lx` hex an integer PV (C prints it
/// decimal) and `-0x` reach the DBR_CHAR / DBR_ENUM arms of `val2str`,
/// which C prints with a bare `%d`.
#[derive(Debug, Clone)]
pub struct ValueFormat {
    pub float: FloatFormat,
    /// C `outTypeI` — the `-0x` / `-0o` / `-0b` base. Applies to EXACTLY
    /// the two `val2str` arms that call `sprint_long(.., outTypeI)`:
    /// `DBR_INT` (SHORT) and `DBR_LONG` (`tool_lib.c:163-167`). `DBR_CHAR`
    /// and the `DBR_ENUM` index are plain `sprintf("%d")`
    /// (`tool_lib.c:160-161,187`) and never see this base; `DBR_FLOAT` /
    /// `DBR_DOUBLE` use [`ValueFormat::float_style`].
    pub int_style: IntStyle,
    /// C `outTypeF` — the `-lx` / `-lo` / `-lb` base, applying ONLY to
    /// `DBR_FLOAT` / `DBR_DOUBLE`. `Dec` (the default) means the value
    /// renders through the `-e`/`-f`/`-g` `dblFormatStr`; any other base
    /// means C rounds it half-away-from-zero into a `dbr_long_t` and
    /// prints that with `sprint_long(.., outTypeF)` (`tool_lib.c:138-158`).
    /// Folding this into a separate field makes the old
    /// `float_as_int: bool` + shared-base pair — whose inconsistent
    /// combinations were the defect — unrepresentable.
    pub float_style: IntStyle,
    /// `-n` flag: print enum value as its integer index instead of
    /// the menu string.
    pub enum_as_number: bool,
    /// `-S` flag: render `DBR_CHAR` arrays as a NUL-terminated string
    /// (long-string CA convention).
    pub char_array_as_string: bool,
    /// C's `reqElems` — the `-#` count, where `0` means "not specified"
    /// (`caget.c:386`, `int count = 0; /* 0 = not specified by -# option */`).
    ///
    /// `0` is the ONLY encoding of "no `-#`", and that is the point: the
    /// previous `Option<usize>` had TWO — `None` and `Some(0)` — and they
    /// drifted apart, so `caget -# 0 WF` displayed ZERO elements where C
    /// displays all of them. A bare `u64` makes the second encoding
    /// unrepresentable.
    ///
    /// The value is C's `unsigned long`, so a negative `-#` arrives here
    /// sign-extended (huge) and clamps to the native count — "all elements",
    /// but still "requested" for [`CountPrefix`]. Built only by
    /// [`crate::copt::Scan::req_elems_int`] / [`crate::copt::Scan::req_elems_ulong`].
    pub req_elems: u64,
    /// `-F <ofs>` flag: replacement field separator. Defaults to a
    /// single space.
    pub field_separator: char,
}

impl Default for ValueFormat {
    fn default() -> Self {
        Self {
            float: FloatFormat::default(),
            int_style: IntStyle::Dec,
            float_style: IntStyle::Dec,
            enum_as_number: false,
            char_array_as_string: false,
            req_elems: 0,
            field_separator: ' ',
        }
    }
}

/// Whether a print loop's array rendering leads with the element count.
///
/// C's value loops do NOT share one rule, and folding them onto a single
/// `req_elems` bool was the defect: the bool then had to mean both "the user
/// passed `-#`" and "emit the count", which are the same thing on three loops
/// and unrelated on the fourth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountPrefix {
    /// The `plain` / `terse` value loops (`caget.c:286`, `caput.c:223`) and
    /// `print_time_val_sts` (`tool_lib.c:486`, shared by `caget -a`,
    /// `caput -l` and `camonitor`): the count leads iff
    /// `reqElems || nElems > 1`.
    IfRequestedOrArray,
    /// The `caget -d` specifiedDbr `Value:` line (`caget.c:328-334`): the
    /// elements are joined BARE. That block already printed the count on its
    /// own `Element count:` line (`caget.c:317-319`), so C's loop there
    /// carries no `printf("%lu%c", ...)` gate at all — not even for an array.
    Never,
}

impl CountPrefix {
    fn leads(self, req_elems: bool, total: usize) -> bool {
        match self {
            CountPrefix::IfRequestedOrArray => req_elems || total > 1,
            CountPrefix::Never => false,
        }
    }
}

/// Render `EpicsValue` for CA tool output, matching C `tool_lib.c::val2str`
/// and the element loop of the calling print block. Enum strings are NOT
/// resolved here — the caller passes `enum_strings = Some(&["off","on",...])`
/// when it has them, else the integer index is used. `format_value` does not
/// emit a trailing newline.
///
/// C's `reqElems` (carried by [`ValueFormat::req_elems`]) drives two
/// INDEPENDENT decisions here, and `count_prefix` selects which print block's
/// rule applies to the second:
///
/// * the `-S` long-string branch, gated on every block that has one
///   (`charArrAsStr && dbr_type_is_CHAR && (reqElems || nElems > 1)`:
///   `caget.c:273` for plain/terse AND `caget.c:318` for specifiedDbr). That
///   branch prints the escaped string and NOTHING else — no count prefix.
/// * the leading element count. In C this `printf("%lu%c", nElems, ...)` sits
///   in the print block BEFORE the element loop (`caget.c:286`), so it fires
///   for a SCALAR too whenever `reqElems` is set — `caget -# 1 TST:LO` prints
///   `1 200`, not `200`. Keeping it inside the array rendering (as this
///   function once did) made it an array-only rule, which is the C behaviour
///   only by coincidence of `reqElems` usually being unset. See [`CountPrefix`].
pub fn format_value(
    v: &EpicsValue,
    fmt: &ValueFormat,
    enum_strings: Option<&[PvString]>,
    count_prefix: CountPrefix,
) -> String {
    // C's `reqElems` is ONE value, carried by `fmt`. It used to arrive a
    // second time as a `bool` parameter, and two sources for one C variable is
    // exactly the drift this closes.
    let req_elems = fmt.req_elems != 0;

    // C renders a CHAR array as a long-string only when
    // `charArrAsStr && (reqElems || nElems > 1)` — a 1-element CHAR array with
    // `-S` but no `-#` falls through to numeric. This gate reads `reqElems`
    // DIRECTLY and is present on the specifiedDbr block too (`caget.c:318`),
    // so it does NOT follow `count_prefix`, and it returns before C's count
    // printf — a long-string never carries the count.
    if let EpicsValue::CharArray(arr) = v
        && fmt.char_array_as_string
        && (req_elems || arr.len() > 1)
    {
        // Long-string convention: bytes up to first NUL, then EPICS-escaped
        // (`caget.c:322-327` escapes the prefix).
        let end = arr.iter().position(|&b| b == 0).unwrap_or(arr.len());
        return escape_from_raw(&arr[..end]);
    }

    let total = v.count() as usize;
    let body = render_elements(v, fmt, enum_strings);
    if count_prefix.leads(req_elems, total) {
        format!("{total}{sep}{body}", sep = fmt.field_separator)
    } else {
        body
    }
}

/// C `print_time_val_sts`'s VALUE SEGMENT (`tool_lib.c:481-489`) — everything
/// between the timestamp and the trailing alarm fields, separator included.
///
/// This is not the loop [`format_value`] renders. The plain/terse loops write
/// the count with a TRAILING separator and then JOIN the elements
/// (`printf("%lu%c", ...)`, then `if (i) printf("%c", ...)`, `caget.c:286-289`).
/// `print_time_val_sts` instead writes every item — the optional count, each
/// element, the `-S` long string — as `printf("%c%s", fieldSeparator, item)`:
/// the separator is a PREFIX.
///
/// Two consequences the joined form cannot express, and both are divergences
/// the port shipped (R13-19):
///
/// * the separator belongs to the VALUE, not to the timestamp before it, so an
///   ABSENT timestamp (`camonitor -t n`) must not swallow it — C prints the
///   name, then an unconditional separator, then the (possibly empty)
///   timestamp, then `sep`+value, giving two adjacent separators;
/// * a value with NO items prints NO separator at all (a never-processed
///   waveform reports `nElems == 0`, and C's element loop then runs zero
///   times).
pub fn format_value_segment(
    v: &EpicsValue,
    fmt: &ValueFormat,
    enum_strings: Option<&[PvString]>,
    count_prefix: CountPrefix,
) -> String {
    let sep = fmt.field_separator;
    let total = v.count() as usize;
    let leads = count_prefix.leads(fmt.req_elems != 0, total);

    if total == 0 {
        return if leads {
            // `-#` on an empty array: C prints `%c%lu` and the element loop
            // then runs zero times, so nothing trails the count.
            format!("{sep}{total}")
        } else {
            // No count, no elements: C's loop prints nothing whatsoever.
            String::new()
        };
    }
    // With at least one item, `sep + join(items, sep)` is byte-for-byte C's
    // `concat(sep + item)`.
    format!(
        "{sep}{body}",
        body = format_value(v, fmt, enum_strings, count_prefix)
    )
}

/// C's element loop alone: every element of the value, capped at `reqElems`
/// and joined by the `-F` separator. The leading count is NOT emitted here —
/// [`format_value`] owns it, so scalar and array carriers cannot disagree
/// about when it appears.
fn render_elements(v: &EpicsValue, fmt: &ValueFormat, enum_strings: Option<&[PvString]>) -> String {
    match v {
        EpicsValue::String(s) => escape_from_raw(s.as_bytes()),
        EpicsValue::Short(n) => format_int_i64(*n as i64, fmt.int_style),
        EpicsValue::Long(n) => format_int_i64(*n as i64, fmt.int_style),
        EpicsValue::Int64(n) => format_int_wide(n.to_string(), *n as u64, fmt.int_style),
        EpicsValue::UInt64(n) => format_int_wide(n.to_string(), *n, fmt.int_style),
        // u16/u32 widen losslessly into i64 (non-negative, in range), so the
        // plain integer formatter is correct — no wide-unsigned path needed.
        EpicsValue::UShort(n) => format_int_i64(*n as i64, fmt.int_style),
        EpicsValue::ULong(n) => format_int_i64(*n as i64, fmt.int_style),
        EpicsValue::Char(n) => format_char((*n as i8) as i64),
        // epicsUInt8 formats unsigned (0xFF -> 255), unlike the signed `Char`.
        EpicsValue::UChar(n) => format_char(*n as i64),
        EpicsValue::Enum(idx) => format_enum(*idx as i64, fmt, enum_strings),
        // Transient NTEnum carrier never reaches CA serialization (coerced in
        // base at the link-write boundary); format its index like a DBF_ENUM.
        EpicsValue::EnumWithChoices { index, .. } => format_enum(*index as i64, fmt, enum_strings),
        EpicsValue::Float(x) => format_float(*x as f64, fmt),
        EpicsValue::Double(x) => format_float(*x, fmt),
        EpicsValue::ShortArray(arr) => join_elements(
            arr.iter().map(|&n| format_int_i64(n as i64, fmt.int_style)),
            arr.len(),
            fmt,
        ),
        EpicsValue::LongArray(arr) => join_elements(
            arr.iter().map(|&n| format_int_i64(n as i64, fmt.int_style)),
            arr.len(),
            fmt,
        ),
        EpicsValue::Int64Array(arr) => join_elements(
            arr.iter()
                .map(|&n| format_int_wide(n.to_string(), n as u64, fmt.int_style)),
            arr.len(),
            fmt,
        ),
        EpicsValue::UInt64Array(arr) => join_elements(
            arr.iter()
                .map(|&n| format_int_wide(n.to_string(), n, fmt.int_style)),
            arr.len(),
            fmt,
        ),
        EpicsValue::UShortArray(arr) => join_elements(
            arr.iter().map(|&n| format_int_i64(n as i64, fmt.int_style)),
            arr.len(),
            fmt,
        ),
        EpicsValue::ULongArray(arr) => join_elements(
            arr.iter().map(|&n| format_int_i64(n as i64, fmt.int_style)),
            arr.len(),
            fmt,
        ),
        // DBF_UCHAR[] is numeric unsigned-byte image data: render each element
        // unsigned (0xFF -> 255), not the signed-i8 / long-string CharArray path.
        EpicsValue::UCharArray(arr) => {
            join_elements(arr.iter().map(|&b| format_char(b as i64)), arr.len(), fmt)
        }
        EpicsValue::EnumArray(arr) => join_elements(
            arr.iter()
                .map(|&idx| format_enum(idx as i64, fmt, enum_strings)),
            arr.len(),
            fmt,
        ),
        EpicsValue::FloatArray(arr) => join_elements(
            arr.iter().map(|&x| format_float(x as f64, fmt)),
            arr.len(),
            fmt,
        ),
        EpicsValue::DoubleArray(arr) => {
            join_elements(arr.iter().map(|&x| format_float(x, fmt)), arr.len(), fmt)
        }
        // The `-S` long-string form is handled by `format_value` before this
        // is reached; here a CHAR array is always numeric (signed i8).
        EpicsValue::CharArray(arr) => join_elements(
            arr.iter().map(|&b| format_char((b as i8) as i64)),
            arr.len(),
            fmt,
        ),
        EpicsValue::StringArray(arr) => join_elements(
            arr.iter().map(|s| escape_from_raw(s.as_bytes())),
            arr.len(),
            fmt,
        ),
    }
}

/// The single owner of C's array element loop: the elements, capped at
/// `reqElems` and joined by the `-F` separator. Every carrier renders its own
/// elements and hands them here, so no array type can grow a private copy of
/// the cap rule.
fn join_elements<I: Iterator<Item = String>>(iter: I, total: usize, fmt: &ValueFormat) -> String {
    // C fetches only `reqElems` elements and then prints every element it
    // fetched, so the display cap IS `reqElems` — with `0` meaning "all",
    // never "none" (`caget.c:208`, `nElems = reqElems && reqElems < nElems
    // ? reqElems : nElems`).
    let take = if fmt.req_elems == 0 {
        total
    } else {
        (fmt.req_elems as usize).min(total)
    };
    iter.take(take)
        .collect::<Vec<_>>()
        .join(&fmt.field_separator.to_string())
}

fn format_enum(idx: i64, fmt: &ValueFormat, enum_strings: Option<&[PvString]>) -> String {
    if !fmt.enum_as_number
        && let Some(strs) = enum_strings
        && idx >= 0
        && (idx as usize) < strs.len()
    {
        // Escape the label bytes exactly like a DBR_STRING (line 166):
        // enum choice labels are raw, not-guaranteed-UTF-8 bytes, so a
        // byte-wise escaper renders them faithfully on the CLI.
        return escape_from_raw(strs[idx as usize].as_bytes());
    }
    // C `val2str`'s DBR_ENUM arm prints a bare index with `sprintf("%d")`
    // (`tool_lib.c:187`) — the `-0x`/`-0o`/`-0b` base (`outTypeI`) is
    // reserved for the DBR_INT / DBR_LONG arms and never reaches here.
    // `caget -n` / `camonitor -n` on a native ENUM field re-request the
    // value as DBR_TIME_INT (`caget.c:179`, `camonitor.c:159`), so those
    // DO carry the base — but they arrive as a SHORT carrier, not as this
    // one. Only a request that keeps the ENUM type (`caget -d DBR_ENUM`,
    // `-d DBR_GR_ENUM -n`) lands here, and C prints those in decimal.
    format_int_i64(idx, IntStyle::Dec)
}

/// C `val2str`'s `DBR_CHAR` arm: `sprintf(str, "%d", ch)` — a bare decimal,
/// unconditionally (`tool_lib.c:160-161`). Unlike `DBR_INT` / `DBR_LONG`,
/// a CHAR value NEVER goes through `sprint_long`, so `-0x` / `-0o` / `-0b`
/// leave it alone: `caget -0x` on a CHAR PV holding `0xFF` prints `-1`, not
/// `0xFFFFFFFF`. Both CA-CHAR carriers (signed `Char`, unsigned `UChar`)
/// share the arm; the caller supplies the already-widened value.
fn format_char(n: i64) -> String {
    format_int_i64(n, IntStyle::Dec)
}

/// Port of EPICS `epicsStrnEscapedFromRaw` (`epicsString.c:120-159`):
/// render raw bytes as printable text for CA CLI readback. The C control
/// escapes (`\a \b \f \n \r \t \v \\ \' \" \0`) map to their two-char
/// form, ASCII-printable bytes (0x20-0x7E) pass through, and every other
/// byte becomes `\xHH` (lowercase hex). C `val2str` runs every DBR_STRING
/// element through this (tool_lib.c:135), and `caget -S` escapes the
/// long-string byte prefix (caget.c:322-327). Operates per byte so
/// multi-byte UTF-8 is escaped exactly as C escapes the raw char buffer,
/// rather than emitting real control/non-printable bytes into the stream.
fn escape_from_raw(src: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(src.len());
    for &c in src {
        match c {
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0b => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'"' => out.push_str("\\\""),
            0 => out.push_str("\\0"),
            0x20..=0x7e => out.push(c as char),
            _ => {
                out.push('\\');
                out.push('x');
                out.push(HEX[(c >> 4) as usize] as char);
                out.push(HEX[(c & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Format a CA classic integer (`DBR_INT`/`DBR_LONG`/`DBR_CHAR`, an ENUM
/// index, or a float rounded to long) with EPICS `sprint_long` semantics
/// (`tool_lib.c:64-91`). C widens every such value to a 32-bit
/// `dbr_long_t` and prints:
///   dec: `%d`   oct: `0o%o`   hex: `0x%X` (UPPERCASE)
///   bin: bare bit digits, leading zeros skipped, no `0b`; zero -> "0".
/// The non-decimal bases reinterpret the low 32 bits as unsigned, so `-1`
/// prints as `0xFFFFFFFF` / `0o37777777777` / 32 ones — NOT the 64-bit
/// `0xffffffffffffffff` the pre-fix `as u64` cast produced. `format!("{:b}",
/// 0u32)` already yields the bare `0` C special-cases.
fn format_int_i64(n: i64, style: IntStyle) -> String {
    let v32 = n as i32; // C: (dbr_long_t)(int) val
    let bits = v32 as u32; // unsigned reinterpretation for the bases
    match style {
        IntStyle::Dec => v32.to_string(),
        IntStyle::Hex => format!("0x{bits:X}"),
        IntStyle::Oct => format!("0o{bits:o}"),
        IntStyle::Bin => format!("{bits:b}"),
    }
}

/// Format a Rust-only 64-bit field (`DBF_INT64` / `DBF_UINT64`). These
/// have no CA classic wire type (served as `Double` over CA), so they are
/// NOT subject to the 32-bit `sprint_long` truncation in
/// [`format_int_i64`] — the full 64-bit value is printed. The base shape
/// still matches `sprint_long` (`0x` UPPERCASE hex, `0o` octal, bare
/// binary) applied to the 64-bit pattern; `decimal` carries the
/// type-correct signed (`Int64`) or unsigned (`UInt64`) rendering.
fn format_int_wide(decimal: String, bits: u64, style: IntStyle) -> String {
    match style {
        IntStyle::Dec => decimal,
        IntStyle::Hex => format!("0x{bits:X}"),
        IntStyle::Oct => format!("0o{bits:o}"),
        IntStyle::Bin => format!("{bits:b}"),
    }
}

/// `printf`'s default precision for `%e` / `%f` / `%g` when the conversion
/// carries none — 6 (C99 7.19.6.1). C's `dblFormatStr` starts as `"%g"`
/// (`tool_lib.c:53`) and the `FMT_GR` / `FMT_CTRL` limit macros hardcode a
/// bare `%g`, so both land on this precision.
const C_DEFAULT_PRECISION: usize = 6;

/// The single owner of C's HARDCODED `%g` for a `dbr_gr_*` / `dbr_ctrl_*`
/// FLOAT/DOUBLE limit.
///
/// `dbr2str` builds the graphic/control limit block from the `FMT_GR(%g)` /
/// `FMT_CTRL(%g)` macros (`tool_lib.c:248-254,266-270`), which embed the
/// literal `%g` — printf's default 6 significant digits. The `-e` / `-f` /
/// `-g` flags rewrite `dblFormatStr`, which ONLY `val2str` reads
/// (`tool_lib.c:138,150`), so they never reach a limit: `caget -d
/// DBR_CTRL_DOUBLE -f 9` still prints every limit at 6 significant digits.
///
/// Non-finite limits fall through to `format_non_finite`, the same escape
/// `format_float` takes.
pub fn format_c_g(x: f64) -> String {
    if !x.is_finite() {
        return format_non_finite(x);
    }
    format_g(x, C_DEFAULT_PRECISION)
}

/// C `printf`'s spelling of a non-finite double, shared by EVERY conversion —
/// `%g`, `%f` and `%e` all print the same three words (verified against glibc,
/// R13-20).
///
/// Rust's `{}` agrees on `inf` and `-inf` but prints `NaN` where C prints
/// `nan`, so `caget` on a NaN-valued `ao` printed `NaN` against C's `nan`.
/// glibc also carries the SIGN BIT through, printing `-nan` for a negative
/// NaN; `f64::is_sign_negative` reports exactly that bit.
///
/// Every float the tools print — value, array element, and each graphic /
/// control limit — reaches C's `printf` through one of the two callers here,
/// so this is the only place the spelling is decided.
fn format_non_finite(x: f64) -> String {
    let sign = if x.is_sign_negative() { "-" } else { "" };
    let word = if x.is_nan() { "nan" } else { "inf" };
    format!("{sign}{word}")
}

fn format_float(x: f64, fmt: &ValueFormat) -> String {
    if fmt.float_style != IntStyle::Dec {
        // C `val2str` (`tool_lib.c:138-158`): a non-`dec` `outTypeF` rounds
        // the value half-away-from-zero (`x > 0 ? x + 0.5 : x - 0.5`, then a
        // truncating cast to `dbr_long_t`) and prints it with
        // `sprint_long(.., outTypeF)`. `f64::round` is the same
        // half-away-from-zero rule. The INTEGER base (`outTypeI`) plays no
        // part here.
        //
        // Out-of-int32-range, NaN and ±Inf are a DEFINED-BEHAVIOUR DEVIATION
        // (R13-21, adopted): C's truncating cast is UB there — x86-64 happens
        // to produce 0x80000000 for every such input, aarch64 would saturate.
        // The port defines the rule instead of mirroring one platform's UB:
        // saturate at the int32 boundary (+Inf/overflow → 0x7FFFFFFF,
        // −Inf/underflow → 0x80000000) and NaN → 0 — Rust `as` cast semantics.
        let rounded = i64::from(x.round() as i32);
        return format_int_i64(rounded, fmt.float_style);
    }
    if !x.is_finite() {
        return format_non_finite(x);
    }
    let p = fmt.float.precision as usize;
    match fmt.float.style {
        FloatStyle::F => format!("{x:.p$}"),
        FloatStyle::E => format_e(x, p),
        FloatStyle::G => format_g(x, p.max(1)),
    }
}

/// Decimal exponent of `abs` after rounding to `precision` significant
/// digits — i.e. `floor(log10(round_to_sig_digits(abs, precision)))`.
///
/// C `%g` rounds to `precision` significant digits FIRST and only then
/// decides between `%e` and `%f`. At a rounding boundary the rounded
/// magnitude can tick up by a power of ten (e.g. `999999.5` at
/// precision 6 rounds to `1000000`, exponent 5 → 6), which flips the
/// fixed-vs-scientific choice. Computing the decision exponent from the
/// UNROUNDED value misses that — see the regression test
/// `g_rounding_boundary_picks_scientific`.
fn decision_exponent(abs: f64, precision: usize) -> i32 {
    let raw_exp = abs.log10().floor() as i32;
    // Scale so the value has `precision` digits before the decimal
    // point, round half-to-even, and read back the magnitude. If the
    // round carries into a new decade the exponent increments.
    let scale = 10f64.powi(precision as i32 - 1 - raw_exp);
    // For magnitudes near the f64 range limits the scale factor can
    // overflow to ±inf (or underflow to 0); `abs * scale` then yields a
    // non-finite product whose `log10` saturates to a garbage exponent.
    // The rounded magnitude cannot meaningfully differ from the raw one
    // at those scales, so fall back to `raw_exp`.
    if !scale.is_finite() || scale == 0.0 {
        return raw_exp;
    }
    let rounded_scaled = (abs * scale).round();
    if !rounded_scaled.is_finite() || rounded_scaled <= 0.0 {
        return raw_exp;
    }
    raw_exp + (rounded_scaled.log10().floor() as i32 - (precision as i32 - 1))
}

/// `%g`-equivalent formatter. C semantics: choose `%e` or `%f`
/// depending on the exponent, drop trailing zeros and the trailing
/// decimal point. Precision is the *significant-digit* count.
fn format_g(x: f64, precision: usize) -> String {
    if x == 0.0 {
        return "0".to_string();
    }
    let abs = x.abs();
    // C `%g` rounds to `precision` significant digits before choosing
    // the format, so the decision exponent must come from the rounded
    // magnitude, not the raw value.
    let exp = decision_exponent(abs, precision);
    // C `%g` uses fixed-point when `precision > exp >= -4`. Compare as
    // i32 to avoid the silent `i32 → usize` wrap for negative `exp`.
    if exp >= -4 && exp < precision as i32 {
        // Fixed-point. Digits after the decimal point = precision-1-exp.
        let digits = (precision as i32 - 1 - exp).max(0) as usize;
        let s = format!("{x:.digits$}");
        trim_g_fixed(&s)
    } else {
        format_g_scientific(x, precision)
    }
}

fn trim_g_fixed(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn format_g_scientific(x: f64, precision: usize) -> String {
    // Rust `{:e}` precision means digits after the decimal in the
    // mantissa. C `%g` with N significant digits is mantissa
    // precision N-1.
    let s = format!("{:.*e}", precision - 1, x);
    rewrite_rust_e_as_c(&s, true)
}

/// Pure `%e`: precision is digits AFTER the decimal point.
fn format_e(x: f64, precision: usize) -> String {
    let s = format!("{x:.precision$e}");
    rewrite_rust_e_as_c(&s, false)
}

/// Rust formats scientific as `1.23e2`; C as `1.23e+02` (signed,
/// 2-digit exponent minimum). Optionally also strips the trailing
/// zeros from the mantissa (the `%g` post-trim behaviour).
fn rewrite_rust_e_as_c(s: &str, trim_mantissa: bool) -> String {
    let Some(e_pos) = s.find('e') else {
        return s.to_string();
    };
    let mantissa = &s[..e_pos];
    let exp_part = &s[e_pos + 1..];
    let mantissa_out = if trim_mantissa && mantissa.contains('.') {
        let t = mantissa.trim_end_matches('0').trim_end_matches('.');
        t.to_string()
    } else {
        mantissa.to_string()
    };
    let (sign, digits) = if let Some(d) = exp_part.strip_prefix('-') {
        ('-', d)
    } else if let Some(d) = exp_part.strip_prefix('+') {
        ('+', d)
    } else {
        ('+', exp_part)
    };
    let exp_padded = if digits.len() < 2 {
        format!("{sign}0{digits}")
    } else {
        format!("{sign}{digits}")
    };
    format!("{mantissa_out}e{exp_padded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt_default() -> ValueFormat {
        ValueFormat::default()
    }

    /// Every C print loop except `caget -d` uses `CountPrefix::IfRequestedOrArray`;
    /// the `-d` Value line is covered explicitly by
    /// `specified_dbr_value_line_never_leads_with_the_count`.
    /// `req_elems` is now carried by `fmt` (C has ONE `reqElems`), so the
    /// helper stamps it onto a copy rather than passing a second source.
    fn fv(
        v: &EpicsValue,
        fmt: &ValueFormat,
        enum_strings: Option<&[PvString]>,
        req_elems: bool,
    ) -> String {
        let mut fmt = fmt.clone();
        if req_elems && fmt.req_elems == 0 {
            fmt.req_elems = 1;
        }
        format_value(v, &fmt, enum_strings, CountPrefix::IfRequestedOrArray)
    }

    #[test]
    fn g_default_precision_matches_c() {
        // C `printf("%g", 475.123)` → "475.123"
        assert_eq!(format_g(475.123, 6), "475.123");
        // C `printf("%g", 1.0)` → "1"
        assert_eq!(format_g(1.0, 6), "1");
        // C `printf("%g", 0.0)` → "0"
        assert_eq!(format_g(0.0, 6), "0");
        // C `printf("%g", 1e-5)` → "1e-05"
        assert_eq!(format_g(1e-5, 6), "1e-05");
        // C `printf("%g", 1e10)` → "1e+10"
        assert_eq!(format_g(1e10, 6), "1e+10");
        // C `printf("%g", 0.0001)` → "0.0001" (boundary)
        assert_eq!(format_g(0.0001, 6), "0.0001");
        // C `printf("%g", 1234567.0)` → "1.23457e+06"
        assert_eq!(format_g(1234567.0, 6), "1.23457e+06");
    }

    /// C `%g` rounds to `precision` significant digits BEFORE deciding
    /// between `%e` and `%f`. At a rounding boundary the rounded
    /// magnitude can carry into a new decade and flip the choice:
    /// `printf("%g", 999999.5)` → "1e+06" (not "1000000"), because the
    /// rounded value `1000000` has exponent 6 >= precision 6.
    #[test]
    fn g_rounding_boundary_picks_scientific() {
        // 999999.5 rounds up to 1000000 → exponent ticks 5 → 6 → %e.
        assert_eq!(format_g(999999.5, 6), "1e+06");
        // Just below the boundary stays fixed-point.
        assert_eq!(format_g(999998.0, 6), "999998");
        // 9.999995 rounds to 10 (exponent 0 → 1, still fixed range).
        assert_eq!(format_g(9.999995, 6), "10");
        // Negative value at the same boundary keeps the sign.
        assert_eq!(format_g(-999999.5, 6), "-1e+06");
    }

    #[test]
    fn g_extreme_magnitudes_do_not_produce_garbage() {
        // Magnitudes near the f64 range limits make the internal scale
        // factor overflow/underflow; decision_exponent must fall back
        // to the raw exponent instead of saturating to a garbage value.
        // Tiny: classifies as scientific (exp < -4).
        assert_eq!(format_g(1e-308, 6), "1e-308");
        assert_eq!(format_g(5e-300, 6), "5e-300");
        // Huge: classifies as scientific (exp >= precision).
        assert_eq!(format_g(1e308, 6), "1e+308");
        // Smallest normal f64 — no panic, scientific form.
        assert!(format_g(f64::MIN_POSITIVE, 6).contains("e-"));
    }

    #[test]
    fn e_format_matches_c() {
        // C `printf("%e", 1.5)` → "1.500000e+00"
        assert_eq!(format_e(1.5, 6), "1.500000e+00");
        // C `printf("%.2e", 1234.5)` → "1.23e+03"
        assert_eq!(format_e(1234.5, 2), "1.23e+03");
    }

    #[test]
    fn classic_int_base_format_matches_sprint_long() {
        // EPICS sprint_long (tool_lib.c:64-91) formats a 32-bit dbr_long_t.
        // -1: hex/oct/bin reinterpret the low 32 bits as unsigned; hex is
        // uppercase with `0x`, octal `0o`, binary BARE (no `0b`).
        assert_eq!(format_int_i64(-1, IntStyle::Hex), "0xFFFFFFFF");
        assert_eq!(format_int_i64(-1, IntStyle::Oct), "0o37777777777");
        assert_eq!(format_int_i64(-1, IntStyle::Bin), "1".repeat(32));
        assert_eq!(format_int_i64(-1, IntStyle::Dec), "-1");
        // C special-cases val == 0 in binary as a bare "0".
        assert_eq!(format_int_i64(0, IntStyle::Bin), "0");
        // Positive value: uppercase hex, `0o` octal, bare binary.
        assert_eq!(format_int_i64(1235, IntStyle::Hex), "0x4D3");
        assert_eq!(format_int_i64(1235, IntStyle::Oct), "0o2323");
        assert_eq!(format_int_i64(1235, IntStyle::Bin), "10011010011");
    }

    #[test]
    fn wide_int64_uint64_keep_64_bits_explicit() {
        // The Rust-only 64-bit extension is NOT truncated to 32 bits.
        let mut hex = fmt_default();
        hex.int_style = IntStyle::Hex;
        assert_eq!(
            fv(&EpicsValue::Int64(-1), &hex, None, false),
            "0xFFFFFFFFFFFFFFFF",
            "Int64 -1 keeps the full 64-bit pattern, uppercase"
        );
        // UInt64 above i64::MAX prints its full unsigned decimal.
        assert_eq!(
            fv(&EpicsValue::UInt64(u64::MAX), &fmt_default(), None, false),
            u64::MAX.to_string()
        );
        // UInt64 binary is bare (no `0b`).
        let mut bin = fmt_default();
        bin.int_style = IntStyle::Bin;
        assert_eq!(fv(&EpicsValue::UInt64(5), &bin, None, false), "101");
    }

    #[test]
    fn array_renders_count_then_values() {
        let v = EpicsValue::DoubleArray(vec![1.0, 2.5, 3.0]);
        let s = fv(&v, &fmt_default(), None, false);
        // C: `3 1 2.5 3` (count + space-separated %g values)
        assert_eq!(s, "3 1 2.5 3");
    }

    /// C `caget.c:286` gates the count prefix on `reqElems || nElems > 1`.
    /// A genuine 1-element waveform read WITHOUT `-#` prints just the
    /// value, no `1 ` prefix.
    #[test]
    fn single_element_array_omits_count_without_req_elems() {
        let v = EpicsValue::DoubleArray(vec![2.5]);
        // No `-#` on the command line → no count prefix.
        assert_eq!(fv(&v, &fmt_default(), None, false), "2.5");
        // `-#` supplied → count prefix returns even for 1 element.
        assert_eq!(fv(&v, &fmt_default(), None, true), "1 2.5");
        // Multi-element always carries the count prefix.
        let v2 = EpicsValue::DoubleArray(vec![1.0, 2.5]);
        assert_eq!(fv(&v2, &fmt_default(), None, false), "2 1 2.5");
    }

    /// C's `caget -d` specifiedDbr Value loop (`caget.c:328-334`) joins the
    /// elements BARE — no `printf("%lu%c", nElems, ...)` gate at all, because
    /// the block already printed `Element count:` on its own line. Only the
    /// plain/terse loop (`:286`) leads with the count.
    ///
    /// Pre-fix the `-d` call passed `req_elems = false` and the shared
    /// renderer STILL prefixed on `total > 1`, so `caget -d DBR_LONG` on a
    /// 3-element array printed `Value: 3 v0 v1 v2` where C prints
    /// `Value: v0 v1 v2`.
    #[test]
    fn specified_dbr_value_line_never_leads_with_the_count() {
        let f = fmt_default();
        let arr = EpicsValue::LongArray(vec![10, 20, 30]);
        assert_eq!(format_value(&arr, &f, None, CountPrefix::Never), "10 20 30");
        // `-#` cannot bring the prefix back on this block either — C's loop
        // has no gate to enable.
        let mut req = f.clone();
        req.req_elems = 3;
        assert_eq!(
            format_value(&arr, &req, None, CountPrefix::Never),
            "10 20 30"
        );
        // Every array carrier, not just the integer one.
        assert_eq!(
            format_value(
                &EpicsValue::StringArray(vec!["a".into(), "b".into()]),
                &f,
                None,
                CountPrefix::Never
            ),
            "a b"
        );
        let strs: Vec<PvString> = vec!["off".into(), "on".into()];
        assert_eq!(
            format_value(
                &EpicsValue::EnumArray(vec![1, 0]),
                &f,
                Some(&strs),
                CountPrefix::Never
            ),
            "on off"
        );
        // Negative control: the SAME value on the plain/terse loop keeps C's
        // `reqElems || nElems > 1` prefix.
        assert_eq!(
            format_value(&arr, &f, None, CountPrefix::IfRequestedOrArray),
            "3 10 20 30"
        );
    }

    /// `reqElems` still reaches the `-S` long-string gate on the specifiedDbr
    /// block (`caget.c:318` repeats `charArrAsStr && dbr_type_is_CHAR &&
    /// (reqElems || nElems > 1)` verbatim) — dropping the count prefix must
    /// NOT drop that. Pre-fix the `-d` call hardcoded `false`, so
    /// `caget -d DBR_CHAR -S -# 1` on a 1-element CHAR array fell through to
    /// the numeric branch instead of printing the long string.
    #[test]
    fn specified_dbr_still_honours_req_elems_for_the_long_string_gate() {
        let mut fmt = fmt_default();
        fmt.char_array_as_string = true;
        let one = EpicsValue::CharArray(b"A".to_vec());
        let mut req = fmt.clone();
        req.req_elems = 1;
        assert_eq!(
            format_value(&one, &req, None, CountPrefix::Never),
            "A",
            "`-S -# 1`: reqElems opens the long-string gate"
        );
        assert_eq!(
            format_value(&one, &fmt, None, CountPrefix::Never),
            "65",
            "`-S` alone on a 1-element CHAR array stays numeric (C's gate)"
        );
    }

    #[test]
    fn enum_with_strings_renders_string() {
        let strs: Vec<PvString> = vec!["off".into(), "on".into()];
        let v = EpicsValue::Enum(1);
        let s = fv(&v, &fmt_default(), Some(&strs), false);
        assert_eq!(s, "on");
    }

    #[test]
    fn enum_n_flag_renders_index() {
        let strs: Vec<PvString> = vec!["off".into(), "on".into()];
        let v = EpicsValue::Enum(1);
        let mut fmt = fmt_default();
        fmt.enum_as_number = true;
        let s = fv(&v, &fmt, Some(&strs), false);
        assert_eq!(s, "1");
    }

    #[test]
    fn char_array_long_string_strips_at_nul() {
        let v = EpicsValue::CharArray(b"hello\0xxxx".to_vec());
        let mut fmt = fmt_default();
        fmt.char_array_as_string = true;
        assert_eq!(fv(&v, &fmt, None, false), "hello");
    }

    #[test]
    fn cli_readback_escapes_raw_string_bytes() {
        // C val2str runs every DBR_STRING element through
        // epicsStrnEscapedFromRaw (tool_lib.c:135); caget -S escapes the
        // long-string byte prefix (caget.c:322-327). Control chars,
        // backslash, quotes and non-printable bytes escape; ASCII passes.
        assert_eq!(escape_from_raw(b"a\tb\nc"), "a\\tb\\nc");
        assert_eq!(escape_from_raw(b"a\\b\"c'd"), "a\\\\b\\\"c\\'d");
        assert_eq!(escape_from_raw(&[0x00, 0x01, b'A', 0x7f]), "\\0\\x01A\\x7f");
        // 'é' = UTF-8 0xC3 0xA9: each raw byte escapes as \xHH, like C.
        assert_eq!(escape_from_raw(&[0xc3, 0xa9]), "\\xc3\\xa9");
        // String scalar through format_value is escaped.
        assert_eq!(
            fv(
                &EpicsValue::String("x\ty".into()),
                &fmt_default(),
                None,
                false
            ),
            "x\\ty"
        );
        // StringArray elements escaped; count prefix preserved.
        let a = EpicsValue::StringArray(vec!["a\nb".into(), "c".into()]);
        assert_eq!(fv(&a, &fmt_default(), None, false), "2 a\\nb c");
        // `-S` long-string: escape the printable prefix up to NUL.
        let mut sfmt = fmt_default();
        sfmt.char_array_as_string = true;
        let cv = EpicsValue::CharArray(b"hi\tthere\0junk".to_vec());
        assert_eq!(fv(&cv, &sfmt, None, true), "hi\\tthere");
    }

    #[test]
    fn float_as_int_rounds_then_renders() {
        let v = EpicsValue::Double(1234.6);
        let mut fmt = fmt_default();
        // C `-lx` sets outTypeF only (caget.c:493-496).
        fmt.float_style = IntStyle::Hex;
        // 1235 = 0x4D3 (sprint_long uses uppercase %X)
        assert_eq!(fv(&v, &fmt, None, false), "0x4D3");
    }

    /// R13-21 (adopted deviation): C's float→dbr_long_t cast is UB for
    /// NaN/±Inf/out-of-range. The port's defined rule saturates at the
    /// int32 boundary and maps NaN to 0. One case per boundary.
    #[test]
    fn float_as_int_saturates_at_the_int32_boundary() {
        let mut fmt = fmt_default();
        fmt.float_style = IntStyle::Hex;
        let hex = |x: f64| fv(&EpicsValue::Double(x), &fmt, None, false);

        assert_eq!(hex(f64::INFINITY), "0x7FFFFFFF", "+Inf saturates to MAX");
        assert_eq!(hex(1.0e10), "0x7FFFFFFF", "overflow saturates to MAX");
        assert_eq!(
            hex(f64::from(i32::MAX)),
            "0x7FFFFFFF",
            "MAX itself is exact, not wrapped"
        );
        assert_eq!(
            hex(f64::NEG_INFINITY),
            "0x80000000",
            "-Inf saturates to MIN"
        );
        assert_eq!(hex(-1.0e10), "0x80000000", "underflow saturates to MIN");
        assert_eq!(hex(f64::NAN), "0x0", "NaN maps to 0");
    }

    /// C `val2str` routes DBR_CHAR through `sprintf("%d", ch)`
    /// (`tool_lib.c:160-161`), NOT through `sprint_long(.., outTypeI)` —
    /// only DBR_INT / DBR_LONG take the `-0x`/`-0o`/`-0b` base. Every CHAR
    /// carrier (scalar and array, signed `Char` and unsigned `UChar`) is
    /// therefore plain decimal whatever the base flag says.
    ///
    /// Pre-fix: `caget -0x` on a CHAR PV holding 0xFF printed `0xFFFFFFFF`;
    /// C prints `-1`.
    #[test]
    fn char_ignores_the_int_base_flag() {
        for style in [IntStyle::Hex, IntStyle::Oct, IntStyle::Bin, IntStyle::Dec] {
            let mut fmt = fmt_default();
            fmt.int_style = style;
            // Signed CHAR carrier: 0xFF is -1 as a C `char`.
            assert_eq!(
                fv(&EpicsValue::Char(0xFF), &fmt, None, false),
                "-1",
                "DBR_CHAR is %d, never sprint_long ({style:?})"
            );
            assert_eq!(
                fv(&EpicsValue::UChar(255), &fmt, None, false),
                "255",
                "the unsigned CHAR carrier is %d too ({style:?})"
            );
            assert_eq!(
                fv(&EpicsValue::CharArray(vec![255, 1]), &fmt, None, false),
                "2 -1 1",
                "a CHAR array renders every element via the same %d arm ({style:?})"
            );
            assert_eq!(
                fv(&EpicsValue::UCharArray(vec![255, 1]), &fmt, None, false),
                "2 255 1",
                "an unsigned CHAR array likewise ({style:?})"
            );
        }
        // Negative control: DBR_INT / DBR_LONG DO carry the base — the
        // integer arms are the ones C hands to `sprint_long(.., outTypeI)`.
        let mut hex = fmt_default();
        hex.int_style = IntStyle::Hex;
        assert_eq!(fv(&EpicsValue::Short(-1), &hex, None, false), "0xFFFFFFFF");
        assert_eq!(fv(&EpicsValue::Long(-1), &hex, None, false), "0xFFFFFFFF");
        assert_eq!(
            fv(&EpicsValue::LongArray(vec![-1]), &hex, None, true),
            "1 0xFFFFFFFF"
        );
    }

    /// C `val2str`'s DBR_ENUM arm prints the index with a bare `sprintf("%d")`
    /// (`tool_lib.c:187`), so an ENUM index never carries the `-0x` base
    /// either. (`caget -n` on a native ENUM field re-requests DBR_TIME_INT,
    /// which arrives as a SHORT carrier and DOES carry the base — that is the
    /// negative control in [`char_ignores_the_int_base_flag`].)
    #[test]
    fn enum_index_ignores_the_int_base_flag() {
        let mut fmt = fmt_default();
        fmt.int_style = IntStyle::Hex;
        // No labels available (`caget -d DBR_ENUM`): C prints the index.
        assert_eq!(fv(&EpicsValue::Enum(255), &fmt, None, false), "255");
        // `-n` with labels present (`-d DBR_GR_ENUM -n`): still the index.
        let strs: Vec<PvString> = vec!["off".into(), "on".into()];
        fmt.enum_as_number = true;
        assert_eq!(fv(&EpicsValue::Enum(1), &fmt, Some(&strs), false), "1");
    }

    /// The two C base globals are independent (`outTypeI` / `outTypeF`):
    /// `-lx` must NOT hex an integer PV, and `-0x` must NOT hex a float PV.
    /// Pre-fix both flags wrote one shared `int_style`, so `-lx` on a LONG PV
    /// printed hex where C prints decimal.
    #[test]
    fn int_and_float_bases_do_not_cross() {
        // `-lx`: outTypeF = hex, outTypeI stays dec.
        let mut lx = fmt_default();
        lx.float_style = IntStyle::Hex;
        assert_eq!(
            fv(&EpicsValue::Long(1235), &lx, None, false),
            "1235",
            "-lx leaves DBR_LONG on outTypeI = dec"
        );
        assert_eq!(
            fv(&EpicsValue::Double(1234.6), &lx, None, false),
            "0x4D3",
            "-lx rounds the float and prints it in outTypeF"
        );
        // `-0x`: outTypeI = hex, outTypeF stays dec → the float keeps %g.
        let mut ix = fmt_default();
        ix.int_style = IntStyle::Hex;
        assert_eq!(fv(&EpicsValue::Long(1235), &ix, None, false), "0x4D3");
        assert_eq!(
            fv(&EpicsValue::Double(1234.6), &ix, None, false),
            "1234.6",
            "-0x leaves DBR_DOUBLE on outTypeF = dec (the -e/-f/-g format)"
        );
    }

    #[test]
    fn pv_name_width_constant_is_30() {
        // Lock the pad width so a future tweak gets caught.
        assert_eq!(PV_NAME_WIDTH, 30);
    }

    /// W10-B1. `-w` has THREE states in C, and the port had two: it clamped a
    /// negative timeout to the default and connected happily, where C's
    /// deadline is already in the past and the connect fails at once. The
    /// boundaries of `secs`, one case each:
    #[test]
    fn timeout_duration_has_c_s_three_states() {
        // < 0 — the deadline is in the PAST. C: `ca_pend_io` returns
        // ECA_TIMEOUT without waiting → `Channel connect timed out`, exit 1.
        assert_eq!(timeout_duration(-1.0), EXPIRED_TIMEOUT);
        assert_eq!(timeout_duration(-0.5), EXPIRED_TIMEOUT);
        assert_eq!(timeout_duration(-1e9), EXPIRED_TIMEOUT);
        assert_eq!(
            timeout_duration(f64::NEG_INFINITY),
            EXPIRED_TIMEOUT,
            "an infinitely-past deadline is still just past"
        );
        // == 0 — wait FOREVER (`ca_pend_io(0)`), not the 1 s default.
        assert_eq!(timeout_duration(0.0), INDEFINITE_TIMEOUT);
        assert_eq!(timeout_duration(-0.0), INDEFINITE_TIMEOUT, "C: `-0.0 == 0`");
        // > 0 — that many seconds.
        assert_eq!(timeout_duration(2.5).as_secs_f64(), 2.5);

        // `Duration::from_secs_f64` panics on NaN and +Inf and clap hands both
        // through literally ("-w nan"), so the guard stays. DEVIATION: C's
        // `-w nan` hangs forever inside `ca_pend_io`; we use the default.
        assert_eq!(
            timeout_duration(f64::NAN).as_secs_f64(),
            DEFAULT_CLI_TIMEOUT_SECS
        );
        assert_eq!(
            timeout_duration(f64::INFINITY).as_secs_f64(),
            DEFAULT_CLI_TIMEOUT_SECS
        );
    }

    /// Sane positive values pass through unchanged.
    #[test]
    fn timeout_duration_preserves_positive_finite() {
        let d = timeout_duration(2.5);
        assert!((d.as_secs_f64() - 2.5).abs() < 1e-9);
    }

    /// C `caget`/`caput`/`camonitor` pass `-w 0` straight to
    /// `ca_pend_io(0)` / `ca_pend_event(0)`, which wait forever. `-w 0`
    /// must therefore resolve to the far-future [`INDEFINITE_TIMEOUT`],
    /// NOT the 1 s default — the bug was clamping it to the default.
    #[test]
    fn timeout_zero_means_indefinite() {
        assert_eq!(timeout_duration(0.0), INDEFINITE_TIMEOUT);
        assert_eq!(timeout_duration(-0.0), INDEFINITE_TIMEOUT);
        // Effectively unbounded: far longer than any CLI session.
        assert!(INDEFINITE_TIMEOUT.as_secs() > 365 * 24 * 60 * 60);
    }

    /// `INDEFINITE_TIMEOUT` must be safe to arm a tokio timer with — a
    /// duration that overflowed `Instant` would panic when `timeout` /
    /// `sleep` computes its deadline. Proven by setting up a `timeout`
    /// with it (the inner future is already ready, so it returns at once).
    #[tokio::test]
    async fn indefinite_timeout_arms_tokio_timer_without_panic() {
        let r = tokio::time::timeout(INDEFINITE_TIMEOUT, async { 7 }).await;
        assert_eq!(r.unwrap(), 7);
    }

    /// The env path is the SAME timeout as `-w` and must fold the same way
    /// (W10-B1). C `use_ca_timeout_env` sets `caTimeout` to any value
    /// `epicsScanDouble` accepts, so a negative `EPICS_CLI_TIMEOUT` reaches
    /// `ca_pend_io` as an expired deadline exactly as `-w -1` does — observed
    /// on the compiled C: `EPICS_CLI_TIMEOUT=-1 caget TST:AO` prints
    /// `Channel connect timed out: 'TST:AO' not found.` and exits 1.
    ///
    /// Only NaN / inf are held back, and only because
    /// `Duration::from_secs_f64` panics on them (see `timeout_duration`).
    /// Serialised because env-var mutation races any other reader.
    #[serial_test::serial]
    #[test]
    fn env_default_timeout_passes_negatives_through_and_holds_back_nan_inf() {
        // SAFETY: serial_test::serial guarantees no concurrent env access.
        unsafe { std::env::set_var("EPICS_CLI_TIMEOUT", "NaN") };
        assert_eq!(env_default_timeout(), DEFAULT_CLI_TIMEOUT_SECS);
        unsafe { std::env::set_var("EPICS_CLI_TIMEOUT", "inf") };
        assert_eq!(env_default_timeout(), DEFAULT_CLI_TIMEOUT_SECS);
        unsafe { std::env::set_var("EPICS_CLI_TIMEOUT", "-3") };
        assert_eq!(env_default_timeout(), -3.0, "C keeps the negative");
        assert_eq!(
            timeout_duration(env_default_timeout()),
            EXPIRED_TIMEOUT,
            "and it resolves to an already-expired deadline"
        );
        unsafe { std::env::set_var("EPICS_CLI_TIMEOUT", "2.5") };
        assert!((env_default_timeout() - 2.5).abs() < 1e-9);
        unsafe { std::env::remove_var("EPICS_CLI_TIMEOUT") };
    }

    /// R14-17. The env var is scanned by `epicsScanDouble`, the same scanner
    /// `-w` uses, so it accepts exactly what `-w` accepts and rejects exactly
    /// what `-w` rejects — the boundaries of [`crate::copt::scan_double`], not
    /// those of `str::parse::<f64>`:
    ///
    /// * surrounding whitespace is skipped (`epicsParseDouble`), so `" -1 "`
    ///   is the expired deadline -1 and NOT the 1 s default;
    /// * a trailing character is extraneous (`S_stdlib_extraneous`), so `"3x"`
    ///   is rejected and the default stands.
    #[serial_test::serial]
    #[test]
    fn env_timeout_scans_with_epics_scan_double_not_str_parse() {
        let env = |v: &str| {
            // SAFETY: serial_test::serial guarantees no concurrent env access.
            unsafe { std::env::set_var("EPICS_CLI_TIMEOUT", v) };
            env_default_timeout()
        };
        assert_eq!(env(" -1 "), -1.0, "epicsParseDouble skips the whitespace");
        assert_eq!(
            timeout_duration(env(" -1 ")),
            EXPIRED_TIMEOUT,
            "so it is the same expired deadline `-w -1` is, exit 1 and all"
        );
        assert_eq!(env(" 2.5\t"), 2.5);
        assert_eq!(env("3x"), DEFAULT_CLI_TIMEOUT_SECS, "extraneous character");
        assert_eq!(env("abc"), DEFAULT_CLI_TIMEOUT_SECS);
        assert_eq!(env(""), DEFAULT_CLI_TIMEOUT_SECS);
        unsafe { std::env::remove_var("EPICS_CLI_TIMEOUT") };
    }

    /// C `use_ca_timeout_env` (tool_lib.c:646) sets `caTimeout` to any
    /// value `epicsScanDouble` accepts — including `0`, which then means
    /// "wait forever". `EPICS_CLI_TIMEOUT=0` must pass through as `0.0`
    /// and resolve to [`INDEFINITE_TIMEOUT`], not the 1 s default.
    #[serial_test::serial]
    #[test]
    fn env_zero_resolves_to_indefinite() {
        // SAFETY: serial_test::serial guarantees no concurrent env access.
        unsafe { std::env::set_var("EPICS_CLI_TIMEOUT", "0") };
        assert_eq!(env_default_timeout(), 0.0);
        assert_eq!(timeout_duration(env_default_timeout()), INDEFINITE_TIMEOUT);
        unsafe { std::env::remove_var("EPICS_CLI_TIMEOUT") };
    }

    /// Every index of C `epicsAlarmConditionStrings` / `epicsAlarmSeverityStrings`
    /// (`alarmString.c:17-50`), transcribed from the C arrays in order, plus the
    /// `"??"` out-of-range fallback of the `tool_lib.h:28-36` macros.
    ///
    /// Index 11 is `HWLIMIT` (no underscore) and an index past the table is
    /// `"??"` — the two the three CA-tool copies got wrong (`HW_LIMIT` /
    /// `"Illegal value"`).
    #[test]
    fn alarm_strings_match_the_c_tables() {
        const C_CONDITIONS: [&str; 22] = [
            "NO_ALARM",
            "READ",
            "WRITE",
            "HIHI",
            "HIGH",
            "LOLO",
            "LOW",
            "STATE",
            "COS",
            "COMM",
            "TIMEOUT",
            "HWLIMIT",
            "CALC",
            "SCAN",
            "LINK",
            "SOFT",
            "BAD_SUB",
            "UDF",
            "DISABLE",
            "SIMM",
            "READ_ACCESS",
            "WRITE_ACCESS",
        ];
        for (i, want) in C_CONDITIONS.iter().enumerate() {
            assert_eq!(stat_to_str(i as u16), *want, "stat_to_str({i})");
        }
        assert_eq!(stat_to_str(11), "HWLIMIT", "HW_LIMIT_ALARM prints HWLIMIT");

        const C_SEVERITIES: [&str; 4] = ["NO_ALARM", "MINOR", "MAJOR", "INVALID"];
        for (i, want) in C_SEVERITIES.iter().enumerate() {
            assert_eq!(sevr_to_str(i as u16), *want, "sevr_to_str({i})");
        }

        // Past `lastEpicsAlarmCond` / `lastEpicsAlarmSev`: C yields "??".
        assert_eq!(stat_to_str(22), "??");
        assert_eq!(stat_to_str(u16::MAX), "??");
        assert_eq!(sevr_to_str(4), "??");
        assert_eq!(sevr_to_str(u16::MAX), "??");
    }

    #[test]
    fn req_elems_caps_array() {
        let v = EpicsValue::LongArray((0..10).collect());
        let mut fmt = fmt_default();
        fmt.req_elems = 3;
        // Total count is full (10) per C `caget -# 3` behaviour: "10 0 1 2".
        assert_eq!(
            format_value(&v, &fmt, None, CountPrefix::IfRequestedOrArray),
            "10 0 1 2"
        );
    }

    /// R12-17. C has ONE encoding of "no `-#`": `reqElems == 0`
    /// (`caget.c:386`). The pre-fix `Option<usize>` had two, and they drifted:
    /// `-# 0` became `Some(0)`, which capped the DISPLAY to zero elements.
    /// Observed on the compiled C against a live softIoc:
    ///   `caget -# 0 TST:WF` → `TST:WF 8 0 0 0 0 0 0 0 0`   (all 8)
    /// while caget-rs printed `TST:WF 8` — the count and nothing else.
    ///
    /// A negative `-#` sign-extends into a huge `unsigned long`, which clamps
    /// to the native count but still reads as "requested":
    ///   `caget -# -3 TST:LO` → `TST:LO   1 200`   (count prefix, all elems)
    #[test]
    fn req_elems_zero_means_all_elements_not_none() {
        let v = EpicsValue::LongArray(vec![0, 1, 2, 3, 4, 5, 6, 7]);
        let mut fmt = fmt_default();

        fmt.req_elems = 0; // `-# 0`, `-# abc`, and no `-#` at all
        assert_eq!(
            format_value(&v, &fmt, None, CountPrefix::IfRequestedOrArray),
            "8 0 1 2 3 4 5 6 7",
            "reqElems == 0 is C's 'not specified' — every element prints"
        );

        fmt.req_elems = u64::MAX - 2; // `-# -3`, sign-extended
        assert_eq!(
            format_value(&v, &fmt, None, CountPrefix::IfRequestedOrArray),
            "8 0 1 2 3 4 5 6 7",
            "a negative count clamps to the native count, still 'requested'"
        );

        // The one-element case is where the count prefix distinguishes them.
        let one = EpicsValue::LongArray(vec![200]);
        fmt.req_elems = 0;
        assert_eq!(
            format_value(&one, &fmt, None, CountPrefix::IfRequestedOrArray),
            "200"
        );
        fmt.req_elems = u64::MAX - 2;
        assert_eq!(
            format_value(&one, &fmt, None, CountPrefix::IfRequestedOrArray),
            "1 200",
            "C: `caget -# -3 TST:LO` → `1 200`"
        );
    }

    /// W10-B4. C rounds the nanoseconds into the fractional field and CLAMPS
    /// so the rounding can never carry into the whole seconds
    /// (`epicsTime.cpp:233-238`); `chrono`'s `%.6f` truncates. Boundary cases
    /// of `frac = nsec + 500`, one per boundary rather than one per scenario:
    #[test]
    fn microseconds_round_with_c_and_never_carry_into_the_seconds() {
        // A fixed second whose local rendering we do not depend on: assert
        // only the fractional field, which is timezone-independent.
        let frac = |nsec: u32| {
            let s = format_time(WallTime::from_unix(EPICS_EPOCH_UNIX_SECS + 1, nsec));
            s.rsplit_once('.')
                .expect("a fractional field")
                .1
                .to_string()
        };
        let secs = |nsec: u32| {
            let s = format_time(WallTime::from_unix(EPICS_EPOCH_UNIX_SECS + 1, nsec));
            s.rsplit_once('.')
                .expect("a whole-seconds field")
                .0
                .to_string()
        };

        // Below / at / above the half-microsecond rounding boundary.
        assert_eq!(frac(1_000), "000001");
        assert_eq!(frac(1_499), "000001", "rounds down");
        assert_eq!(frac(1_500), "000002", "rounds up; truncation gave 000001");
        assert_eq!(frac(1_501), "000002");
        // Zero and the exact microsecond.
        assert_eq!(frac(0), "000000");
        assert_eq!(frac(499), "000000");
        assert_eq!(frac(500), "000001");
        // The clamp: rounding up out of range must NOT advance the second.
        assert_eq!(frac(999_999_499), "999999");
        assert_eq!(
            frac(999_999_500),
            "999999",
            "C clamps to nSecPerSec-1 rather than carrying"
        );
        assert_eq!(frac(999_999_999), "999999");
        assert_eq!(
            secs(999_999_999),
            secs(0),
            "the whole second is the same on both sides of the clamp"
        );

        // The rendering this replaced, pinned so the divergence stays visible:
        // every tool formatted the stamp with `chrono`'s `%.6f`, which
        // TRUNCATES. On the boundary above it printed one microsecond less
        // than C.
        let dt: chrono::DateTime<chrono::Local> =
            std::time::SystemTime::from(WallTime::from_unix(EPICS_EPOCH_UNIX_SECS + 1, 1_500))
                .into();
        assert_eq!(
            dt.format("%.6f").to_string(),
            ".000001",
            "chrono truncates; C rounds to .000002"
        );
    }

    /// R13-20. C `printf` spells a non-finite double `nan` / `-nan` / `inf` /
    /// `-inf` in EVERY conversion (`%g`, `%f`, `%e` — probed against glibc).
    /// Rust's `{}` agrees on the infinities but prints `NaN`, so the port
    /// printed `NaN` where C prints `nan`.
    #[test]
    fn a_non_finite_double_is_spelled_the_way_c_printf_spells_it() {
        let plain = ValueFormat::default();
        let styles = [
            ("g", FloatStyle::G),
            ("f", FloatStyle::F),
            ("e", FloatStyle::E),
        ];
        for (name, style) in styles {
            let fmt = ValueFormat {
                float: FloatFormat {
                    style,
                    precision: 3,
                },
                ..plain.clone()
            };
            assert_eq!(
                format_value(
                    &EpicsValue::Double(f64::NAN),
                    &fmt,
                    None,
                    CountPrefix::Never
                ),
                "nan",
                "-{name}: C prints `nan`, not Rust's `NaN`"
            );
            assert_eq!(
                format_value(
                    &EpicsValue::Double(f64::INFINITY),
                    &fmt,
                    None,
                    CountPrefix::Never
                ),
                "inf",
                "-{name}"
            );
            assert_eq!(
                format_value(
                    &EpicsValue::Double(f64::NEG_INFINITY),
                    &fmt,
                    None,
                    CountPrefix::Never
                ),
                "-inf",
                "-{name}"
            );
        }
        // glibc carries the NaN sign bit through: `printf("%g", -NAN)` → `-nan`.
        assert_eq!(
            format_value(
                &EpicsValue::Double(-f64::NAN),
                &plain,
                None,
                CountPrefix::Never
            ),
            "-nan"
        );
        // A FLOAT reaches the same formatter.
        assert_eq!(
            format_value(
                &EpicsValue::Float(f32::NAN),
                &plain,
                None,
                CountPrefix::Never
            ),
            "nan"
        );
        // So does every graphic / control limit (`caget -a`, `caget -d
        // DBR_CTRL_DOUBLE`), which C prints with its own literal `%g`.
        assert_eq!(format_c_g(f64::NAN), "nan");
        assert_eq!(format_c_g(f64::NEG_INFINITY), "-inf");
    }
}
