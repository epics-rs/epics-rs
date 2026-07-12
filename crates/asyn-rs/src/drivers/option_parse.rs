//! The `asynOption` value grammar and its diagnostics — one owner for every
//! port driver.
//!
//! C validates option values in `setOption` with two tools and nothing else:
//! `sscanf(val, "%d")` / `sscanf(val, "%u")` for the numeric keys, and
//! `epicsStrCaseCmp(val, "Y"/"N")` for the boolean ones — and it reports the
//! failure with a fixed string per key (`drvAsynSerialPort.c:261-616`,
//! `drvAsynSerialPortWin32.c:192-345`, `drvAsynIPPort.c:924-935`). Those strings
//! are the operator's diagnostic: they reach ERRS through
//! `pasynUser->errorMessage` (asynRecord.c `reportError`), and an OPI or a
//! script that keys off them sees the driver's own words.
//!
//! Each driver used to author its own text ("invalid baud rate: '9600x'",
//! "invalid boolean value: 'yes' (expected Y or N)") and its own number grammar
//! (`str::parse`, which rejects what C accepts). Both live here now, so the
//! three backends — POSIX serial, Win32 serial, IP — cannot drift from C or
//! from each other.

use crate::error::{AsynError, AsynResult, AsynStatus};

/// C `sscanf(val, "%d", &n)` — the grammar every numeric option key is parsed
/// with (baud, bits on Win32).
///
/// `%d` is a *prefix* parse: it skips leading whitespace, takes an optional sign
/// and the run of decimal digits that follows, and stops at the first character
/// that is not one — the trailing text is left in the stream, not rejected. So C
/// reads `"9600x"` as 9600 and `" -1 "` as -1, where Rust's `str::parse::<i32>`
/// rejects both. `None` is C's `sscanf(...) != 1`: no digits at all, which every
/// caller reports as [`bad_number`].
///
/// A digit run that overflows `int` is undefined in C; it saturates here rather
/// than becoming a `None` that C would never produce (the caller then rejects it
/// on its own terms — an unsupported data rate, say).
pub fn sscanf_int(value: &str) -> Option<i32> {
    let s = value.trim_start();
    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let run: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
    if run.is_empty() {
        return None;
    }
    let magnitude: i64 = run.parse().unwrap_or(i64::from(i32::MAX) + 1);
    let signed = if negative { -magnitude } else { magnitude };
    Some(signed.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
}

/// C `sscanf(val, "%u", &n)` — the grammar of the unsigned option keys (the
/// break duration, the RS-485 RTS delays).
///
/// The same prefix parse as [`sscanf_int`] on the unsigned domain. Deviation,
/// stated: C's `%u` accepts a leading `-` and wraps it around the unsigned
/// range, so `"break -1"` asks C for a 4294967295 ms break. That is a C
/// pathology, not a contract; a negative value is `None` here and the caller
/// reports [`bad_number`], which is what C's own `sscanf` returns for every
/// other malformed value.
pub fn sscanf_uint(value: &str) -> Option<u32> {
    let s = value.trim_start();
    if s.starts_with('-') {
        return None;
    }
    let digits = s.strip_prefix('+').unwrap_or(s);
    let run: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
    if run.is_empty() {
        return None;
    }
    Some(
        run.parse::<u64>()
            .unwrap_or(u64::from(u32::MAX))
            .min(u64::from(u32::MAX)) as u32,
    )
}

/// C's diagnostic for a value that carries no number at all —
/// `"Bad number"` (drvAsynSerialPort.c:264, :512, :576, :585;
/// drvAsynSerialPortWin32.c:196, :205, :314).
pub fn bad_number() -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: "Bad number".into(),
    }
}

/// C's diagnostic for a boolean/enumerated key whose value it does not
/// recognise — `"Invalid <key> value."` (drvAsynSerialPort.c:419, :440, :464,
/// :481, :502, :539, :558; drvAsynIPPort.c:933).
///
/// The three keys whose C text is *not* of this shape have their own literal at
/// the call site: `"Invalid parity."`, `"Invalid number of bits."`,
/// `"Invalid number of stop bits."`.
pub fn invalid_option_value(key: &str) -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: format!("Invalid {key} value."),
    }
}

/// C's `epicsStrCaseCmp(val,"Y") / epicsStrCaseCmp(val,"N")` accept-set, with
/// C's per-key diagnostic on a miss.
///
/// Only `Y` and `N` (case-insensitive) are values; the looser `y/yes/1/true`
/// coercion a driver might reach for silently selects a setting the operator did
/// not ask for, which is exactly why C is strict here.
pub fn parse_yn_option(key: &str, value: &str) -> AsynResult<bool> {
    if value.eq_ignore_ascii_case("Y") {
        Ok(true)
    } else if value.eq_ignore_ascii_case("N") {
        Ok(false)
    } else {
        Err(invalid_option_value(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R11-49: C parses every numeric option value with `sscanf("%d")`, a prefix
    /// parse. `str::parse` is not one: it rejects the trailing garbage C ignores,
    /// so `baud 9600x` errored where C sets 9600.
    #[test]
    fn sscanf_int_is_a_prefix_parse_like_c() {
        assert_eq!(sscanf_int("9600"), Some(9600));
        assert_eq!(
            sscanf_int("9600x"),
            Some(9600),
            "C stops at the first non-digit"
        );
        assert_eq!(
            sscanf_int("  9600  "),
            Some(9600),
            "%d skips leading whitespace"
        );
        assert_eq!(sscanf_int("-1"), Some(-1));
        assert_eq!(sscanf_int("+300"), Some(300));
        assert_eq!(
            sscanf_int("0x10"),
            Some(0),
            "%d is decimal: it reads 0 and stops at 'x'"
        );

        // C's `sscanf(...) != 1` — no digits at all. Every caller reports
        // "Bad number" for these.
        assert_eq!(sscanf_int(""), None);
        assert_eq!(sscanf_int("fast"), None);
        assert_eq!(sscanf_int("x9600"), None);
        assert_eq!(sscanf_int("-"), None);
    }

    #[test]
    fn sscanf_uint_takes_the_unsigned_prefix() {
        assert_eq!(sscanf_uint("250"), Some(250));
        assert_eq!(sscanf_uint("250ms"), Some(250));
        assert_eq!(sscanf_uint(" 0"), Some(0));
        assert_eq!(sscanf_uint(""), None);
        assert_eq!(sscanf_uint("on"), None);
        // Stated deviation from C's wrap-around %u.
        assert_eq!(sscanf_uint("-1"), None);
    }

    #[test]
    fn the_texts_are_cs_texts() {
        assert_eq!(bad_number().message(), "Bad number");
        assert_eq!(
            invalid_option_value("clocal").message(),
            "Invalid clocal value."
        );
        assert_eq!(
            parse_yn_option("crtscts", "maybe").unwrap_err().message(),
            "Invalid crtscts value."
        );
        assert!(parse_yn_option("ixon", "y").unwrap());
        assert!(!parse_yn_option("ixon", "N").unwrap());
    }
}
