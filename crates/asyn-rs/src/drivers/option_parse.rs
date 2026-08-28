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

/// C's `epicsStrCaseCmp(key, ...)` dispatch, as the one normalisation both
/// halves of a driver's option interface enter through.
///
/// Every C option entry point compares the key case-insensitively —
/// `drvAsynSerialPort.c` `getOption` (:143-230) and `setOption` (:261-598),
/// `drvAsynSerialPortWin32.c` `getOption` (:110-155) and `setOption`
/// (:192-344) — so `BAUD`, `Baud` and `baud` name one key on both paths. The
/// two halves used to disagree here: `setOption` lowercased the key while
/// `getOption` matched the caller's spelling against lowercase literals, so an
/// option an operator set as `BAUD` read back `OptionNotFound` and an
/// `asynRecord` readback of it stayed blank.
///
/// The `trim` is this crate's, not C's, and it lives here for the same reason:
/// whatever `setOption` accepts, `getOption` has to be able to answer.
pub fn option_key(key: &str) -> String {
    key.trim().to_ascii_lowercase()
}

/// C's `epicsStrCaseCmp(val, ...)` — the value half of the same rule
/// [`option_key`] owns for the key.
///
/// Every value C matches against a word it matches case-insensitively:
/// `drvAsynSerialPort.c:361-528` and `drvAsynSerialPortWin32.c:211-308` run
/// `none`/`odd`/`even`, `Y`/`N`, `on`/`off` and even the digit literals through
/// `epicsStrCaseCmp`. Folding once at each driver's `setOption` entry is what
/// keeps the literals below it from each needing their own comparison, the way
/// `parity` used to with a local `val_lower` while `break` had none — which is
/// how `asynSetOption port 0 break ON` came to report "Bad number" where C
/// asserts the line.
///
/// Only the option surface may use this. A value that is *data* rather than a
/// keyword must not be folded: C `epicsStrDup`s `hostInfo` verbatim
/// (`drvAsynIPPort.c:297`), so that port compares its own keywords
/// case-insensitively instead and keeps the value as typed.
pub fn option_value(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

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

/// What C's three-way `break` rule decides for one `setOption` call, given the
/// port's current `tty->break_active`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakAction {
    /// C's `on`: assert the line (`FlushFileBuffers` then `SetCommBreak`).
    pub assert_break: bool,
    /// C's `len`, with `Sleep(break_len > 0 ? break_len : 250)` already
    /// resolved. `None` means the form is not timed at all.
    pub hold_ms: Option<u64>,
    /// C's `off`: release the line (`ClearCommBreak`).
    pub clear_break: bool,
}

/// C-Win32 `setOption`'s `break` arm (`drvAsynSerialPortWin32.c:302-323`) as a
/// pure decision over the latch, so the driver holds no second copy of the rule
/// and the rule is reachable by a test without a COM port:
///
/// ```text
/// "on"   on = break_active ? 0 : 1;   off = len = 0
/// "off"  off = break_active ? 1 : 0;  on  = len = 0
/// else   on = break_active ? 0 : 1;   off = len = 1
/// ```
///
/// The consequence that matters is that `"on"` sets `len` to 0, so C never
/// sleeps and never clears: the break stays asserted until `"off"` or a timed
/// form releases it. Only the numeric and empty forms are timed.
///
/// `value` arrives folded through [`option_value`], which is why the literals
/// below are bare `==` and not a second copy of C's `epicsStrCaseCmp`.
///
/// This is Win32 only. The POSIX driver has no `break_active` field at all and
/// its break really is a momentary `tcsendbreak` with `"off"` a bare
/// `return asynSuccess` (`drvAsynSerialPort.c:507-528`); that asymmetry is C's
/// own.
pub fn break_action(value: &str, break_active: bool) -> AsynResult<BreakAction> {
    if value == "on" {
        Ok(BreakAction {
            assert_break: !break_active,
            hold_ms: None,
            clear_break: false,
        })
    } else if value == "off" {
        Ok(BreakAction {
            assert_break: false,
            hold_ms: None,
            clear_break: break_active,
        })
    } else {
        // C skips the `sscanf` when the value is empty, leaving `break_len` 0,
        // which the `Sleep` then reads as the conventional 250 ms.
        let ms = if value.is_empty() {
            0
        } else {
            u64::from(sscanf_uint(value).ok_or_else(bad_number)?)
        };
        Ok(BreakAction {
            assert_break: !break_active,
            hold_ms: Some(if ms > 0 { ms } else { 250 }),
            clear_break: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::BreakAction;
    use super::*;

    /// C matches option *values* case-insensitively too, so `break ON` is the
    /// same request as `break on`. Without the fold it fell past both words
    /// into the numeric arm and errored "Bad number".
    #[test]
    fn option_value_folds_case_so_break_on_is_the_on_arm() {
        assert_eq!(option_value(" ON "), "on");
        assert_eq!(option_value("Off"), "off");
        assert_eq!(option_value("EVEN"), "even");

        let asserted = break_action(&option_value("ON"), false).unwrap();
        assert_eq!(asserted, break_action("on", false).unwrap());
        assert!(asserted.assert_break, "ON must assert the line");
        assert_eq!(asserted.hold_ms, None, "the on arm is never timed");

        let released = break_action(&option_value("OFF"), true).unwrap();
        assert_eq!(released, break_action("off", true).unwrap());
        assert!(released.clear_break);

        // A number still parses, and is still the only timed form.
        assert_eq!(
            break_action(&option_value("100"), false).unwrap().hold_ms,
            Some(100)
        );
    }

    /// C dispatches option keys through `epicsStrCaseCmp` on both the get and
    /// the set path, so the key an operator writes in any case has to reach the
    /// same handler and read back through it.
    #[test]
    fn option_key_folds_case_and_surrounding_space() {
        assert_eq!(option_key("BAUD"), "baud");
        assert_eq!(option_key("Baud"), "baud");
        assert_eq!(option_key(" rs485_Enable "), "rs485_enable");
        assert_eq!(option_key("Break"), "break");
        assert_eq!(option_key(""), "");
    }

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

    /// One case per branch of C's three-way break rule crossed with the latch
    /// state it reads (`drvAsynSerialPortWin32.c:302-323`). The first case is
    /// the defect: `"on"` must assert and STOP — no hold, no clear — because
    /// C's `"on"` arm leaves `len` and `off` at 0 and returns with the line
    /// still in break.
    #[test]
    fn break_on_latches_and_only_off_or_a_timed_form_releases_it() {
        let act = |v: &str, active: bool| super::break_action(v, active).unwrap();

        // "on", latch down: assert, hold nothing, clear nothing.
        assert_eq!(
            act("on", false),
            BreakAction {
                assert_break: true,
                hold_ms: None,
                clear_break: false
            }
        );
        // "on" again while held is a no-op in C (on = break_active ? 0 : 1).
        assert_eq!(
            act("on", true),
            BreakAction {
                assert_break: false,
                hold_ms: None,
                clear_break: false
            }
        );
        // "off" releases only what is actually held.
        assert_eq!(
            act("off", true),
            BreakAction {
                assert_break: false,
                hold_ms: None,
                clear_break: true
            }
        );
        assert_eq!(
            act("off", false),
            BreakAction {
                assert_break: false,
                hold_ms: None,
                clear_break: false
            }
        );
        // Empty value: timed, and C's break_len 0 resolves to 250 ms.
        assert_eq!(
            act("", false),
            BreakAction {
                assert_break: true,
                hold_ms: Some(250),
                clear_break: true
            }
        );
        // Timed while already held: C sets off = len = 1 regardless, so the
        // hold still runs and the line is released at the end.
        assert_eq!(
            act("", true),
            BreakAction {
                assert_break: false,
                hold_ms: Some(250),
                clear_break: true
            }
        );
        // Numeric form carries its own duration.
        assert_eq!(
            act("50", false),
            BreakAction {
                assert_break: true,
                hold_ms: Some(50),
                clear_break: true
            }
        );
        // Explicit 0 is C's `break_len > 0 ? break_len : 250`.
        assert_eq!(act("0", false).hold_ms, Some(250));
        // Anything else is C's "Bad number".
        assert_eq!(
            super::break_action("soon", false).unwrap_err().message(),
            "Bad number"
        );
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
