//! `Soft Timestamp` device support — EPICS base `devTimestamp.c`
//! (`devSoft.dbd`, the same built-in dbd that registers `getenv`).
//!
//! Two record variants, dispatched by record type:
//!
//! - `ai` (`devTimestampAI`): `VAL = secPastEpoch + nsec * 1e-9` — the
//!   record's resolved time stamp as a floating-point seconds count from
//!   the EPICS epoch (1990-01-01), always carrying the fraction.
//! - `stringin` (`devTimestampSI`): `VAL = epicsTimeToStrftime(INP, time)`,
//!   where the INST_IO `INP` instio string is the strftime format.
//!
//! C `read_ai` (`devTimestamp.c:38-41`) calls `recGblGetTimeStamp(prec)`
//! then derives VAL; `read_stringin` (`:57-66`) formats the resolved stamp
//! with the INP format string. Both resolve the stamp from `dbCommon.tse`,
//! so this port routes through [`get_time_stamp`] exactly as the sibling
//! std-module `Time of Day` / `Sec Past Epoch` device support does.
//!
//! ## INP access
//!
//! Device support receives the record *body* in `read()` (the stringin
//! body carries no `INP`), so the INST_IO format string is captured from
//! [`DeviceSupportContext::inp`](crate::server::ioc_app::DeviceSupportContext)
//! at construction — the same channel the asyn universal driver uses for
//! its INP. C `read_stringin` re-reads `prec->inp.value.instio.string` on
//! every process (the `devSoft_DSXT` extension permits runtime INP edits);
//! this port resolves the format once at wiring time, so a runtime change
//! to a `Soft Timestamp` stringin's `INP` format is not picked up — a
//! narrow divergence for an uncommon edit.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{CaError, CaResult};
use crate::server::device_support::{DeviceReadOutcome, DeviceSupport};
use crate::server::recgbl::get_time_stamp;
use crate::server::record::{ProcessContext, Record};
use crate::types::EpicsValue;

use chrono::{DateTime, Local};

/// Seconds from the Unix epoch (1970-01-01) to the EPICS epoch
/// (1990-01-01), matching C `epicsTimeStamp::secPastEpoch`.
const EPICS_EPOCH_OFFSET_SECS: u64 = 631_152_000;

/// `epicsTime.cpp` `nSecFracDigits` — the maximum fractional-second width
/// (nanoseconds). A `%f` token wider than this is clamped to it.
const NSEC_FRAC_DIGITS: u32 = 9;

/// `stringin` `VAL` is a `char[40]` buffer in C; it is the `bufLength`
/// passed to `epicsTimeToStrftime`, which writes into it with running
/// `bufLenLeft` accounting and always leaves room for the NUL — so the
/// formatted output is bounded to `STRINGIN_VAL_BYTES - 1` = 39 visible
/// bytes. C `read_stringin`'s `if (len >= sizeof prec->val)` overflow check
/// is therefore structurally unreachable (dead code) and never raises an
/// alarm; this port mirrors the bounded write so the same holds.
const STRINGIN_VAL_BYTES: usize = 40;

/// C `epicsTimeToStrftime`'s `char strftimePrefixBuf[256]` — the temporary
/// buffer the strftime prefix (the text between fractional tokens) is built
/// in. A prefix segment that does not fit it formats to `<invalid format>`.
const STRFTIME_PREFIX_BUF: usize = 256;

/// `(secPastEpoch, nsec)` of a resolved time stamp counted from the EPICS
/// epoch (1990-01-01), exactly as C `epicsTimeStamp`. A stamp at or before
/// the EPICS epoch (including the `UNIX_EPOCH` default of an unresolved
/// stamp) saturates `secPastEpoch` to 0, so the `secPastEpoch == 0 &&
/// nsec == 0` test matches C `epicsTimeToStrftime`'s "uninitialized time
/// stamp" check (`epicsTime.cpp:176`).
fn epics_time_parts(ts: SystemTime) -> (u64, u32) {
    let dur = ts.duration_since(UNIX_EPOCH).unwrap_or_default();
    (
        dur.as_secs().saturating_sub(EPICS_EPOCH_OFFSET_SECS),
        dur.subsec_nanos(),
    )
}

/// C `fracFormatFind` (`epicsTime.cpp:107-164`): split `fmt` at the first
/// fractional-second token (`%0Nf`, `%Nf`, or a bare `%f`).
///
/// Returns `(prefix, frac_width, rest)`:
/// - `prefix` — the ANSI-strftime text before the token (the whole string
///   when no token is present),
/// - `frac_width` — `None` when no token is present; `Some(u32::MAX)` for a
///   bare `%f` (a sentinel later clamped to [`NSEC_FRAC_DIGITS`]);
///   `Some(n)` for `%n f` with `n > 0`,
/// - `rest` — the text after the token (`None` when none was found).
///
/// A `%%` is a literal percent and stays in the prefix (C advances past
/// both characters); a `%0f`/`%f`-less specifier with `n == 0` is a normal
/// strftime field, not a fractional token (C requires `result > 0`).
///
/// Width parsing is a plain ASCII-digit scan, where C uses `strtoul`. This
/// diverges only on widths no real format carries: C's `strtoul` skips
/// leading whitespace and accepts a sign (`% 3f`/`%+3f` → 3, `%-3f` →
/// huge→clamped to 9) and parses the full 64-bit range (`%9999999999f` →
/// clamped to 9), whereas this scan treats those as literal strftime text.
/// Deliberately not replicated — matching `strtoul`'s sign/whitespace/64-bit
/// quirks for a malformed fractional-width token is complexity against
/// inputs that never occur.
fn frac_format_find(fmt: &str) -> (&str, Option<u32>, Option<&str>) {
    let bytes = fmt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'%' {
                // Literal `%%` — skip both, leaving it in the prefix.
                i += 2;
                continue;
            } else if i + 1 < bytes.len() && bytes[i + 1] == b'f' {
                // Bare `%f`: width sentinel, clamped to NSEC_FRAC_DIGITS.
                return (&fmt[..i], Some(u32::MAX), Some(&fmt[i + 2..]));
            } else {
                // `%<digits>f` with digits > 0.
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 1 && j < bytes.len() && bytes[j] == b'f' {
                    if let Ok(w) = fmt[i + 1..j].parse::<u32>() {
                        if w > 0 {
                            return (&fmt[..i], Some(w), Some(&fmt[j + 1..]));
                        }
                    }
                }
            }
        }
        i += 1;
    }
    (fmt, None, None)
}

/// Format the ANSI-strftime `prefix` against `dt` in local time.
///
/// C `epicsTimeToStrftime` feeds the prefix to the OS `strftime`
/// (`epicsTime_localtime` → `localtime_r`, i.e. LOCAL wall-clock). chrono's
/// `format` covers the common conversion specifiers; on one it cannot parse
/// (a locale-dependent `%c`/`%x`/`%X` or an unsupported specifier) chrono
/// yields an error item whose `to_string()` would panic, so this emits the
/// prefix verbatim instead — a narrow, panic-free divergence from C's
/// locale/platform-defined `strftime` for those exotic specifiers.
fn format_strftime_prefix(prefix: &str, dt: &DateTime<Local>) -> String {
    use chrono::format::{Item, StrftimeItems};
    let items: Vec<Item> = StrftimeItems::new(prefix).collect();
    if items.iter().any(|it| matches!(it, Item::Error)) {
        return prefix.to_string();
    }
    dt.format_with_items(items.into_iter()).to_string()
}

/// Append `s` to `out` truncated so it occupies at most `buf_len_left - 1`
/// bytes, mirroring C's `memcpy`/`strncpy` cap that always leaves room for
/// the NUL (`epicsTime.cpp:268-274`). Returns the bytes appended. Every
/// caller passes ASCII (`<undefined>`, the `************` sentinel, a decimal
/// digit string), so a byte cut is also a char boundary. (The
/// `<invalid format>` sentinel does NOT go through here — like C, it flows
/// through the strftime step, which omits it whole rather than cutting it.)
fn push_truncated(out: &mut String, s: &str, buf_len_left: usize) -> usize {
    let n = s.len().min(buf_len_left.saturating_sub(1));
    out.push_str(&s[..n]);
    n
}

/// C `epicsTimeToStrftime` (`epicsTime.cpp:169-279`): format `ts` with a
/// (possibly fractional-second-bearing) strftime format string, into a
/// bounded `STRINGIN_VAL_BYTES` buffer.
///
/// The C routine writes into a caller-supplied `char[bufLength]` tracking
/// `bufLenLeft`, so its result is always `< bufLength` bytes and a field
/// that does not fit is either omitted whole (strftime returns 0) or
/// replaced by a sentinel (`<invalid format>` for a ≥256-byte prefix
/// segment, `************` for a fractional field with no room). This port
/// reproduces that bounded accounting so the formatted string is ≤39 bytes
/// and `read_stringin`'s `len >= 40` overflow branch stays unreachable — C
/// never raises an alarm from it, and neither does this port.
fn epics_time_to_strftime(format: &str, ts: SystemTime) -> String {
    let (sec_past_epoch, nsec) = epics_time_parts(ts);
    let mut out = String::new();
    // C `bufLength` = `sizeof prec->val` = STRINGIN_VAL_BYTES; `bufLenLeft`
    // counts the NUL slot. `bufLength == 0` cannot occur here.
    let mut buf_len_left = STRINGIN_VAL_BYTES;

    // C `epicsTime.cpp:176`: an epoch (uninitialized) stamp formats to the
    // literal sentinel.
    if sec_past_epoch == 0 && nsec == 0 {
        push_truncated(&mut out, "<undefined>", buf_len_left);
        return out;
    }

    let dt: DateTime<Local> = ts.into();
    let mut rest = format;
    // C `epicsTime.cpp:185`: `while (*pFmt != '\0' && bufLenLeft > 1)`.
    while !rest.is_empty() && buf_len_left > 1 {
        let (raw_prefix, mut frac, after) = frac_format_find(rest);

        // C `fracFormatFind` (`epicsTime.cpp:144-161`): a strftime prefix
        // segment that overflows `strftimePrefixBuf[256]` is REPLACED by the
        // literal `<invalid format>`, the fractional token is dropped, and
        // `pAfter` becomes "" so the loop ends. The substituted text is then
        // an ordinary strftime prefix — it flows through the strftime step
        // below and obeys the same omit-whole-if-no-room rule (C feeds it to
        // `::strftime`, which writes nothing when it does not fit), NOT a
        // mid-field cut.
        let prefix;
        let stop;
        if raw_prefix.len() >= STRFTIME_PREFIX_BUF {
            prefix = "<invalid format>";
            frac = None;
            stop = true;
        } else {
            prefix = raw_prefix;
            stop = false;
        }

        // C `epicsTime.cpp:196`: nothing more to format → quit.
        if prefix.is_empty() && frac.is_none() {
            break;
        }

        // C `epicsTime.cpp:200-208`: strftime the prefix. `strftime` writes
        // only when the result (incl. NUL) fits `bufLenLeft`, otherwise it
        // returns 0 and writes nothing — the whole field is omitted, never
        // cut mid-field.
        if !prefix.is_empty() {
            let formatted = format_strftime_prefix(prefix, &dt);
            if formatted.len() < buf_len_left {
                out.push_str(&formatted);
                buf_len_left -= formatted.len();
            }
        }

        // C `epicsTime.cpp:210-277`: fractional seconds.
        if let Some(w) = frac {
            if buf_len_left > 1 {
                // Bare `%f` (u32::MAX) clamps to NSEC_FRAC_DIGITS; `%Nf` keeps N.
                let width = w.min(NSEC_FRAC_DIGITS) as usize;
                if width < buf_len_left {
                    // C `epicsTime.cpp:222-239`: `div = 10^(9-width)`,
                    // `frac = nsec + div/2` clamped below 1e9, then
                    // `frac /= div` — rounding the nanosecond field to
                    // `width` digits without a carry into whole seconds.
                    // `nsec < 1e9` always (`subsec_nanos`), so the C `OVF`
                    // branch (`:253-263`) is dead here exactly as in C.
                    let div = 10u64.pow(NSEC_FRAC_DIGITS - width as u32);
                    let mut f = nsec as u64 + div / 2;
                    if f >= 1_000_000_000 {
                        f = 1_000_000_000 - 1;
                    }
                    f /= div;
                    // `"%0<width>lu"` of `f < 10^width` → exactly `width` digits.
                    let digits = format!("{f:0width$}");
                    buf_len_left -= push_truncated(&mut out, &digits, buf_len_left);
                } else {
                    // C `epicsTime.cpp:265-276`: fractional field does not fit
                    // the remaining buffer → `************`, then stop.
                    push_truncated(&mut out, "************", buf_len_left);
                    break;
                }
            }
        }

        // C `fracFormatFind` set `pAfter=""` for the `<invalid format>` case;
        // the loop then ends. Otherwise continue with the postfix.
        if stop {
            break;
        }
        rest = after.unwrap_or("");
    }
    out
}

/// `Soft Timestamp` device support (`devTimestamp.c`), serving `ai` and
/// `stringin` records.
pub struct SoftTimestampDeviceSupport {
    /// The INST_IO `INP` instio string = the `stringin` strftime format
    /// (`devTimestampSI`). Captured from `DeviceSupportContext.inp` at
    /// construction with the libCom `@` prefix stripped. Unused by the `ai`
    /// variant — `devTimestampAI` ignores the instio string.
    format: String,
    /// `dbCommon.tse` / `dbCommon.time`, captured from [`ProcessContext`]
    /// before `read()`. C `read_ai` / `read_stringin` resolve `prec->time`
    /// from `prec->tse` via `recGblGetTimeStamp`; `read()` resolves the same
    /// way through [`get_time_stamp`]`(tse, time)`.
    tse: i16,
    time: SystemTime,
}

impl SoftTimestampDeviceSupport {
    /// Construct from the record's INST_IO `INP`. The optional leading `@`
    /// (libCom INP convention) is stripped; the remainder is the strftime
    /// format for the `stringin` variant.
    pub fn new(inp: &str) -> Self {
        let trimmed = inp.trim();
        let format = trimmed.strip_prefix('@').unwrap_or(trimmed).to_string();
        Self {
            format,
            tse: 0,
            time: SystemTime::UNIX_EPOCH,
        }
    }
}

impl DeviceSupport for SoftTimestampDeviceSupport {
    fn dtyp(&self) -> &str {
        "Soft Timestamp"
    }

    fn set_process_context(&mut self, ctx: &ProcessContext) {
        self.tse = ctx.tse;
        self.time = ctx.time;
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        let rt = record.record_type();
        if !matches!(rt, "ai" | "stringin") {
            return Err(CaError::InvalidValue(format!(
                "DTYP='Soft Timestamp': unsupported record type '{rt}' (use ai or stringin)"
            )));
        }
        Ok(())
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        // C `recGblGetTimeStamp` leaves `prec->time` alone when
        // `epicsTimeGetEvent` fails, and the record-level owner
        // (`apply_timestamp`) emits the errlog this cycle.
        let ts = get_time_stamp(self.tse, self.time).unwrap_or(self.time);
        match record.record_type() {
            "ai" => {
                // C `devTimestamp.c:38-41` `read_ai`: `prec->val =
                // time.secPastEpoch + nsec * 1e-9`, return 2 (VAL written
                // directly — skip the ai conversion).
                let (sec, nsec) = epics_time_parts(ts);
                let val = sec as f64 + nsec as f64 * 1e-9;
                record.put_field("VAL", EpicsValue::Double(val))?;
                Ok(DeviceReadOutcome::computed())
            }
            "stringin" => {
                // C `devTimestamp.c:57-66` `read_stringin`: format the
                // resolved stamp with the INP instio string, then return 0
                // (stringin has no RVAL→VAL conversion to skip, so unlike
                // `read_ai`'s `return 2` this is a plain success — `computed()`
                // is behaviorally inert here, just consistent with `read_ai`).
                // `epics_time_to_strftime` bounds the result to ≤39 bytes (C's
                // `bufLenLeft` accounting), so C's `if (len >= sizeof
                // prec->val)` overflow check is dead code — it never sets
                // `udf`/`UDF_ALARM`/`return -1`, and neither do we: a too-long
                // format silently truncates with no alarm.
                let s = epics_time_to_strftime(&self.format, ts);
                record.put_field("VAL", EpicsValue::String(s.into()))?;
                Ok(DeviceReadOutcome::computed())
            }
            // init() gated the record type to ai / stringin.
            other => Err(CaError::InvalidValue(format!(
                "DTYP='Soft Timestamp': unsupported record type '{other}'"
            ))),
        }
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Err(CaError::InvalidValue(
            "Soft Timestamp device support is read-only (ai / stringin)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::record::{AlarmSeverity, ProcessContext};
    use crate::server::records::ai::AiRecord;
    use crate::server::records::stringin::StringinRecord;
    use std::time::Duration;

    /// A `ProcessContext` carrying an explicit device-time stamp with
    /// `tse = -2` (`epicsTimeEventDeviceTime`), so `read()` resolves the
    /// stamp to exactly `time` via `get_time_stamp(-2, time)`.
    fn ctx_device_time(time: SystemTime) -> ProcessContext {
        ProcessContext {
            udf: false,
            udfs: AlarmSeverity::Invalid,
            nsev: AlarmSeverity::NoAlarm,
            phas: 0,
            tse: -2,
            time,
            tsel: String::new(),
            dtyp: String::new(),
        }
    }

    /// Unix 1_000_000_000 (2001-09-09 UTC) + 0.25 s.
    fn fixed_device_time() -> SystemTime {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(1_000_000_000)
            + Duration::from_nanos(250_000_000)
    }

    /// C `read_ai`: `val = secPastEpoch + nsec*1e-9`. For Unix 1e9 + 0.25 s
    /// that is `(1e9 - EPICS_EPOCH_OFFSET) + 0.25`, ALWAYS carrying the
    /// fraction (devTimestampAI has no PHAS gate, unlike `Sec Past Epoch`).
    #[test]
    fn ai_writes_seconds_past_epoch_with_fraction() {
        let mut dev = SoftTimestampDeviceSupport::new("@n");
        let mut rec = AiRecord::new(0.0);
        dev.set_process_context(&ctx_device_time(fixed_device_time()));
        dev.read(&mut rec).unwrap();
        let expected = (1_000_000_000u64 - EPICS_EPOCH_OFFSET_SECS) as f64 + 0.25;
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!(
                (v - expected).abs() < 1e-6,
                "ai VAL must be secPastEpoch+frac, got {v} expected {expected}"
            ),
            other => panic!("expected Double VAL, got {other:?}"),
        }
    }

    /// C `read_stringin`: `epicsTimeToStrftime(val, ..., inp.instio.string,
    /// &time)`. A device time of Unix 1e9 (year 2001 in every real zone)
    /// formatted with `%Y` must render 2001, proving the resolved device
    /// time — not the wall clock — was formatted.
    #[test]
    fn stringin_formats_device_time() {
        let mut dev = SoftTimestampDeviceSupport::new("@%Y-%m-%d");
        let mut rec = StringinRecord::new("");
        dev.set_process_context(&ctx_device_time(fixed_device_time()));
        dev.read(&mut rec).unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::String(s)) => {
                let s = s.as_str_lossy();
                assert!(s.starts_with("2001-"), "expected 2001 date, got {s}");
            }
            other => panic!("expected String VAL, got {other:?}"),
        }
    }

    /// C `epicsTimeToStrftime` (`epicsTime.cpp:176`): an epoch
    /// (uninitialized) stamp formats to the literal `<undefined>`.
    #[test]
    fn stringin_epoch_is_undefined() {
        let mut dev = SoftTimestampDeviceSupport::new("@%Y-%m-%d %H:%M:%S");
        let mut rec = StringinRecord::new("");
        dev.set_process_context(&ctx_device_time(SystemTime::UNIX_EPOCH));
        dev.read(&mut rec).unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::String(s)) => {
                assert_eq!(s.as_str_lossy(), "<undefined>");
            }
            other => panic!("expected String VAL, got {other:?}"),
        }
    }

    /// `init()` rejects a record type that is neither `ai` nor `stringin`,
    /// flagging the record rather than presenting a healthy no-op.
    #[test]
    fn init_rejects_unsupported_record_type() {
        use crate::server::records::bo::BoRecord;
        let mut dev = SoftTimestampDeviceSupport::new("@n");
        let mut rec = BoRecord::default();
        assert!(dev.init(&mut rec).is_err());
    }

    /// `new()` strips the libCom `@` INP prefix, leaving the strftime
    /// format.
    #[test]
    fn new_strips_at_prefix() {
        let dev = SoftTimestampDeviceSupport::new("@%Y-%m-%d");
        assert_eq!(dev.format, "%Y-%m-%d");
        let dev = SoftTimestampDeviceSupport::new("  %H:%M  ");
        assert_eq!(dev.format, "%H:%M");
    }

    /// C `fracFormatFind` (`epicsTime.cpp:107`): splits at `%0Nf`/`%Nf`/`%f`;
    /// `%%` is a literal kept in the prefix; `%0f` (width 0) is not a
    /// fractional token.
    #[test]
    fn frac_format_find_splits_fractional_token() {
        // %03f → width 3, prefix "%S.", rest "".
        let (p, w, r) = frac_format_find("%S.%03f");
        assert_eq!(p, "%S.");
        assert_eq!(w, Some(3));
        assert_eq!(r, Some(""));

        // bare %f → sentinel width, prefix "%S.".
        let (p, w, _) = frac_format_find("%S.%f");
        assert_eq!(p, "%S.");
        assert_eq!(w, Some(u32::MAX));

        // No fractional token → whole string as prefix.
        let (p, w, r) = frac_format_find("%Y-%m-%d %H:%M:%S");
        assert_eq!(p, "%Y-%m-%d %H:%M:%S");
        assert_eq!(w, None);
        assert_eq!(r, None);

        // %% is literal, not a token boundary.
        let (p, w, _) = frac_format_find("100%%%2f");
        assert_eq!(p, "100%%");
        assert_eq!(w, Some(2));
    }

    /// C `epicsTime.cpp:222-239`: `%0Nf` ROUNDS the nanosecond field to N
    /// digits. 250_000_000 ns at width 3 rounds to `250`; width 1 to `2`
    /// (0.25 → 0.2 rounds to 0.3? no: 2_500_000_00 ns / 1e8 with +5e7
    /// rounding = 3). Exercise both widths against the C divisor algorithm.
    #[test]
    fn strftime_fractional_seconds_round() {
        let t = SystemTime::UNIX_EPOCH
            + Duration::from_secs(1_000_000_000)
            + Duration::from_nanos(250_000_000);
        // width 3: 250_000_000 → 250
        assert!(epics_time_to_strftime("%S.%03f", t).ends_with(".250"));
        // width 9: full nanoseconds
        assert!(epics_time_to_strftime("%S.%9f", t).ends_with(".250000000"));
        // width 1: round(2.5) → 3 (0.25 s to 1 digit, C +div/2 rounds up)
        assert!(epics_time_to_strftime("%S.%1f", t).ends_with(".3"));
    }

    /// C `epicsTimeToStrftime` writes into a fixed 40-byte buffer, so its
    /// result is ALWAYS ≤39 bytes and `read_stringin`'s `len >= 40` check is
    /// dead code — a too-long format truncates silently with NO alarm. A
    /// `stringin` processed with a format whose natural output exceeds 39
    /// bytes must therefore return `Ok(computed())` (not the old spurious
    /// `Err`) and leave VAL ≤39 bytes.
    #[test]
    fn stringin_long_format_truncates_without_alarm() {
        // A `%3f` token splits the format: the date segment + 3-digit
        // fraction fit, but the long trailing literal overflows the 40-byte
        // buffer and is omitted whole (C strftime returns 0 → writes nothing).
        let fmt = "@%Y-%m-%d %H:%M:%S.%3f trailing zzzzzzzzzzzzzzzzzzzzzzzzz";
        let mut dev = SoftTimestampDeviceSupport::new(fmt);
        let mut rec = StringinRecord::new("");
        dev.set_process_context(&ctx_device_time(fixed_device_time()));
        let outcome = dev.read(&mut rec).expect("no alarm on a too-long format");
        assert!(outcome.did_compute, "stringin read returns computed (C: 2)");
        match rec.get_field("VAL") {
            Some(EpicsValue::String(s)) => {
                let s = s.as_str_lossy();
                assert!(s.starts_with("2001-"), "date segment kept, got {s:?}");
                assert!(s.ends_with(".250"), "3-digit fraction kept, got {s:?}");
                assert!(
                    !s.contains('z'),
                    "overflowing tail omitted whole, got {s:?}"
                );
                assert!(s.len() <= 39, "VAL bounded to ≤39 bytes, got {}", s.len());
            }
            other => panic!("expected String VAL, got {other:?}"),
        }
    }

    /// C `epicsTime.cpp:265-276`: when a fractional field has no room left in
    /// the buffer, C emits `************` (capped to fit) and stops. A 35-byte
    /// literal prefix leaves 5 bytes (`bufLenLeft`); a `%9f` (width 9 ≥ 5)
    /// does not fit → `****` (capped to `bufLenLeft - 1 = 4`).
    #[test]
    fn strftime_fractional_field_overflow_emits_stars() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let prefix = "x".repeat(35);
        let out = epics_time_to_strftime(&format!("{prefix}%9f"), t);
        assert_eq!(out, format!("{prefix}****"));
        assert_eq!(out.len(), 39);
    }

    /// C `epicsTime.cpp:156-161`: a strftime prefix segment that overflows
    /// `strftimePrefixBuf[256]` formats to `<invalid format>` — and, since C
    /// feeds it back through `::strftime`, it is written WHOLE when it fits
    /// (here a fresh 40-byte buffer holds all 16 chars).
    #[test]
    fn strftime_prefix_too_long_emits_invalid_format() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let out = epics_time_to_strftime(&format!("{}%f", "a".repeat(256)), t);
        assert_eq!(out, "<invalid format>");
    }

    /// `<invalid format>` obeys the strftime omit-whole-if-no-room rule, NOT a
    /// mid-field cut. A 35-char prefix + `%1f` leaves 4 bytes; the following
    /// 256-char segment's `<invalid format>` (16 chars) does not fit 4 bytes,
    /// so C's `::strftime` writes NOTHING — no truncated `<inv…` fragment.
    #[test]
    fn strftime_invalid_format_omitted_whole_when_no_room() {
        let t = SystemTime::UNIX_EPOCH
            + Duration::from_secs(1_000_000_000)
            + Duration::from_nanos(250_000_000);
        let fmt = format!("{}%1f{}%f", "x".repeat(35), "a".repeat(256));
        let out = epics_time_to_strftime(&fmt, t);
        // 35 x's fit (buf_len_left 40→5); `%1f` writes "3" (buf_len_left→4);
        // the `<invalid format>` segment does not fit 4 bytes → omitted whole.
        assert_eq!(out, format!("{}3", "x".repeat(35)));
        assert!(
            !out.contains('<'),
            "no truncated sentinel fragment, got {out:?}"
        );
    }
}
