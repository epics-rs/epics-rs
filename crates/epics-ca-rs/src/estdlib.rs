//! The C parsing primitives every `double`-valued knob in this crate goes
//! through: `epicsParseDouble` / `epicsScanDouble`
//! (`libcom/src/misc/epicsStdlib.c:149-176`) and `envGetDoubleConfigParam`
//! (`libcom/src/env/envSubr.c:191-211`), plus the panic-free `f64` seconds
//! → [`Duration`] conversion those knobs feed.
//!
//! Why this module exists: `Duration::from_secs_f64` PANICS on NaN, on a
//! negative value, and on anything beyond `u64::MAX` seconds — i.e. on
//! exactly the inputs C's `strtod` accepts and hands to libca as a large
//! (or never-expiring) timeout. `EPICS_CA_CONN_TMO=inf` made the port abort
//! where C's `caget` reads the PV, and `EPICS_CAS_SEND_TMO=inf` let the
//! first client connect kill the whole server. Every env-derived duration
//! now resolves through [`env_double`] + [`duration_from_secs`], so no
//! environment string can panic the process.

use std::time::Duration;

/// C `isspace()` in the "C" locale — `strtod`'s leading/trailing skip set.
fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The `epicsParseDouble` failure codes (`epicsStdlib.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseDoubleError {
    /// `S_stdlib_noConversion` — `strtod` consumed nothing.
    NoConversion,
    /// `S_stdlib_overflow` — `errno == ERANGE` with a non-zero result.
    Overflow,
    /// `S_stdlib_underflow` — `errno == ERANGE` with a zero result.
    Underflow,
    /// `S_stdlib_extraneous` — non-space characters trail the number.
    Extraneous,
}

/// `errno` after `strtod`: unset, or `ERANGE` on either side.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Erange {
    No,
    Over,
    Under,
}

/// ERANGE classification for a value `strtod` computed from digits.
///
/// glibc raises ERANGE when the result overflows to infinity, when it
/// underflows to zero, and when it is inexactly representable as a
/// subnormal. It does NOT raise it for the `inf` / `nan` *words*, which
/// is why those are classified separately at their parse site.
fn classify(v: f64, mantissa_nonzero: bool) -> Erange {
    if v.is_infinite() {
        Erange::Over
    } else if v == 0.0 && mantissa_nonzero {
        Erange::Under
    } else if v != 0.0 && v.is_subnormal() {
        // `epicsParseDouble` maps a non-zero ERANGE to overflow.
        Erange::Over
    } else {
        Erange::No
    }
}

/// C `strtod` (glibc; `epicsStrtod` is `#define`d to it on every platform
/// with a working one — `osi/os/posix/osdStrtod.h`). Returns the value, the
/// number of bytes consumed (0 == no conversion, C's `endp == str`), and the
/// `errno` outcome.
///
/// Accepts what glibc accepts, verified against the compiled C: decimal and
/// scientific notation, C99 hex floats (`0x10` → 16, `0X1p4` → 16), the
/// `inf` / `infinity` / `nan` words (case-insensitive, optional `nan(...)`
/// payload), each with an optional sign.
fn strtod(s: &str) -> (f64, usize, Erange) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_c_space(b[i]) {
        i += 1;
    }
    let sign_at = i;
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let num = i;

    // C99 hex float: 0x <hexdigits> [. <hexdigits>] [p [+-] <digits>]
    if num + 1 < b.len() && b[num] == b'0' && (b[num + 1] | 0x20) == b'x' {
        let mut j = num + 2;
        let mut mant = 0.0f64;
        let mut digits = 0usize;
        let mut nonzero = false;
        let mut exp: i32 = 0;
        while j < b.len() && b[j].is_ascii_hexdigit() {
            mant = mant * 16.0 + hex_val(b[j]) as f64;
            nonzero |= b[j] != b'0';
            digits += 1;
            j += 1;
        }
        if j < b.len() && b[j] == b'.' {
            let mut k = j + 1;
            let mut frac = 0usize;
            while k < b.len() && b[k].is_ascii_hexdigit() {
                mant = mant * 16.0 + hex_val(b[k]) as f64;
                nonzero |= b[k] != b'0';
                exp = exp.saturating_sub(4);
                frac += 1;
                k += 1;
            }
            if digits > 0 || frac > 0 {
                digits += frac;
                j = k;
            }
        }
        if digits == 0 {
            // Bare "0x": glibc converts the leading "0" and stops at 'x'.
            return (if neg { -0.0 } else { 0.0 }, num + 1, Erange::No);
        }
        if j < b.len() && (b[j] | 0x20) == b'p' {
            let mut k = j + 1;
            let mut eneg = false;
            if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
                eneg = b[k] == b'-';
                k += 1;
            }
            let digits_at = k;
            let mut e: i32 = 0;
            while k < b.len() && b[k].is_ascii_digit() {
                e = e.saturating_mul(10).saturating_add((b[k] - b'0') as i32);
                k += 1;
            }
            if k > digits_at {
                exp = exp.saturating_add(if eneg { -e } else { e });
                j = k;
            }
        }
        // `powi` saturates to inf / 0, so an absurd exponent lands as
        // ERANGE rather than as garbage.
        let mut v = if mant.is_finite() {
            mant * 2.0f64.powi(exp.clamp(-5000, 5000))
        } else {
            f64::INFINITY
        };
        if neg {
            v = -v;
        }
        let erange = classify(v, nonzero);
        return (v, j, erange);
    }

    // The `inf` / `nan` words. glibc leaves errno clear for these, so an
    // explicit `EPICS_CA_CONN_TMO=inf` is a VALID (never-expiring) timeout
    // in C, not a parse failure.
    let rest = &s[num..];
    if starts_ci(rest, "infinity") {
        return (inf(neg), num + 8, Erange::No);
    }
    if starts_ci(rest, "inf") {
        return (inf(neg), num + 3, Erange::No);
    }
    if starts_ci(rest, "nan") {
        let mut j = num + 3;
        if j < b.len() && b[j] == b'(' {
            let mut k = j + 1;
            while k < b.len() && b[k] != b')' {
                k += 1;
            }
            if k < b.len() {
                j = k + 1;
            }
        }
        return (f64::NAN, j, Erange::No);
    }

    // Decimal / scientific.
    let mut j = num;
    let mut digits = 0usize;
    let mut nonzero = false;
    while j < b.len() && b[j].is_ascii_digit() {
        nonzero |= b[j] != b'0';
        digits += 1;
        j += 1;
    }
    if j < b.len() && b[j] == b'.' {
        let mut k = j + 1;
        let mut frac = 0usize;
        while k < b.len() && b[k].is_ascii_digit() {
            nonzero |= b[k] != b'0';
            frac += 1;
            k += 1;
        }
        if digits > 0 || frac > 0 {
            digits += frac;
            j = k;
        }
    }
    if digits == 0 {
        return (0.0, 0, Erange::No);
    }
    let mut end = j;
    if j < b.len() && (b[j] | 0x20) == b'e' {
        let mut k = j + 1;
        if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
            k += 1;
        }
        let digits_at = k;
        while k < b.len() && b[k].is_ascii_digit() {
            k += 1;
        }
        if k > digits_at {
            end = k;
        }
    }
    // Rust's `f64::from_str` accepts exactly this grammar (sign, digits,
    // optional point, optional exponent) and, like `strtod`, saturates to
    // ±inf on overflow and to 0 on underflow — `classify` turns those into
    // the ERANGE codes.
    let v = s[sign_at..end].parse::<f64>().unwrap_or(f64::NAN);
    let erange = classify(v, nonzero);
    (v, end, erange)
}

fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        _ => (c | 0x20) - b'a' + 10,
    }
}

fn inf(neg: bool) -> f64 {
    if neg {
        f64::NEG_INFINITY
    } else {
        f64::INFINITY
    }
}

fn starts_ci(s: &str, word: &str) -> bool {
    s.len() >= word.len() && s.as_bytes()[..word.len()].eq_ignore_ascii_case(word.as_bytes())
}

/// C `epicsParseDouble(str, to, NULL)` (`epicsStdlib.c:149-176`): skip
/// leading whitespace, run `strtod`, reject `ERANGE`, skip trailing
/// whitespace, reject anything left over.
pub fn epics_parse_double(s: &str) -> Result<f64, ParseDoubleError> {
    let (v, used, erange) = strtod(s);
    if used == 0 {
        return Err(ParseDoubleError::NoConversion);
    }
    match erange {
        Erange::Over => return Err(ParseDoubleError::Overflow),
        Erange::Under => return Err(ParseDoubleError::Underflow),
        Erange::No => {}
    }
    if !s.as_bytes()[used..].iter().all(|&c| is_c_space(c)) {
        return Err(ParseDoubleError::Extraneous);
    }
    Ok(v)
}

/// C `epicsScanDouble` (`epicsStdlib.h:203`) — `epicsParseDouble` with the
/// status collapsed to a boolean.
pub fn epics_scan_double(s: &str) -> Option<f64> {
    epics_parse_double(s).ok()
}

/// C `envGetConfigParamPtr` (`envSubr.c:83-100`): the parameter is present
/// only when the environment holds a NON-EMPTY string for it (C folds an
/// empty value back to "unset", then to the compiled default — which this
/// port keeps at the call site).
pub fn env_raw(name: &str) -> Option<String> {
    epics_base_rs::runtime::env::get(name).filter(|s| !s.is_empty())
}

/// Why [`env_double`] did not yield a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvDoubleError {
    /// The parameter is absent (or empty) — C falls back to the compiled
    /// default string, silently. No diagnostic.
    Unset,
    /// The parameter is set but `epicsScanDouble` rejected it — C's
    /// `envGetDoubleConfigParam` returns -1 after printing a diagnostic.
    Invalid(ParseDoubleError),
}

/// C `envGetDoubleConfigParam` (`envSubr.c:191-211`).
///
/// The caller supplies the default (C reads it from the compiled
/// `ENV_PARAM` table) and, per C, its own extra diagnostic lines.
pub fn env_double(name: &str) -> Result<f64, EnvDoubleError> {
    let raw = env_raw(name).ok_or(EnvDoubleError::Unset)?;
    epics_parse_double(&raw).map_err(EnvDoubleError::Invalid)
}

/// Panic-free `f64` seconds → [`Duration`].
///
/// `Duration::from_secs_f64` panics on NaN, on negatives, and beyond
/// `u64::MAX` seconds; C just stores the `double` and compares against it.
/// Mirror the C outcome instead of aborting:
///
/// * `+inf`, or a magnitude past `Duration`'s range → [`Duration::MAX`];
///   in C every `now < expire` test against such a deadline is true, i.e.
///   the timer never fires.
/// * `NaN` → [`Duration::MAX`] as well: in C every comparison against NaN
///   is false, so the deadline likewise never trips.
/// * negative (including `-inf`) → [`Duration::ZERO`], C's already-expired
///   deadline.
pub fn duration_from_secs(secs: f64) -> Duration {
    Duration::try_from_secs_f64(secs).unwrap_or(if secs < 0.0 {
        Duration::ZERO
    } else {
        Duration::MAX
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row probed against the compiled C `strtod` on this platform
    /// (glibc; `epicsStrtod` is `#define strtod`).
    #[test]
    fn parses_what_strtod_parses() {
        assert_eq!(epics_parse_double("30.0"), Ok(30.0));
        assert_eq!(epics_parse_double("0.5"), Ok(0.5));
        assert_eq!(epics_parse_double(".5"), Ok(0.5));
        assert_eq!(epics_parse_double("5."), Ok(5.0));
        assert_eq!(epics_parse_double("+3"), Ok(3.0));
        assert_eq!(epics_parse_double("-2.5e2"), Ok(-250.0));
        // Leading AND trailing whitespace are skipped, not extraneous.
        assert_eq!(epics_parse_double(" \t 5 \n"), Ok(5.0));
    }

    /// C99 hex floats — `strtod("0x10")` is 16.0, not a parse failure.
    #[test]
    fn parses_hex_floats() {
        assert_eq!(epics_parse_double("0x10"), Ok(16.0));
        assert_eq!(epics_parse_double("0X1A"), Ok(26.0));
        assert_eq!(epics_parse_double("-0x10"), Ok(-16.0));
        assert_eq!(epics_parse_double("0X1p4"), Ok(16.0));
        assert_eq!(epics_parse_double("0x1.8p1"), Ok(3.0));
        // Bare "0x": strtod converts the leading '0' and leaves "x".
        assert_eq!(
            epics_parse_double("0x"),
            Err(ParseDoubleError::Extraneous),
            "strtod consumes only the '0', so 'x' is extraneous"
        );
    }

    /// The `inf` / `nan` words are VALID doubles for `strtod` — errno stays
    /// clear, so `epicsParseDouble` returns them.
    #[test]
    fn accepts_inf_and_nan_words() {
        assert_eq!(epics_parse_double("inf"), Ok(f64::INFINITY));
        assert_eq!(epics_parse_double("INFINITY"), Ok(f64::INFINITY));
        assert_eq!(epics_parse_double("-inf"), Ok(f64::NEG_INFINITY));
        assert!(epics_parse_double("nan").unwrap().is_nan());
        assert!(epics_parse_double("NaN(x)").unwrap().is_nan());
    }

    /// ERANGE: `strtod("1e400")` returns inf AND sets errno, so
    /// `epicsParseDouble` fails where the bare `inf` word succeeds.
    #[test]
    fn rejects_erange() {
        assert_eq!(epics_parse_double("1e400"), Err(ParseDoubleError::Overflow));
        assert_eq!(
            epics_parse_double("-1e400"),
            Err(ParseDoubleError::Overflow)
        );
        assert_eq!(
            epics_parse_double("1e-400"),
            Err(ParseDoubleError::Underflow)
        );
        // ERANGE is checked before the trailing-garbage test, as in C.
        assert_eq!(
            epics_parse_double("1e400x"),
            Err(ParseDoubleError::Overflow)
        );
    }

    #[test]
    fn rejects_no_conversion_and_extraneous() {
        assert_eq!(epics_parse_double(""), Err(ParseDoubleError::NoConversion));
        assert_eq!(
            epics_parse_double("   "),
            Err(ParseDoubleError::NoConversion)
        );
        assert_eq!(
            epics_parse_double("abc"),
            Err(ParseDoubleError::NoConversion)
        );
        assert_eq!(epics_parse_double("3x"), Err(ParseDoubleError::Extraneous));
        assert_eq!(epics_parse_double("5 6"), Err(ParseDoubleError::Extraneous));
    }

    /// The panic boundary: none of these may abort, and each maps to the
    /// C outcome for that deadline.
    #[test]
    fn duration_conversion_is_total() {
        assert_eq!(duration_from_secs(2.5), Duration::from_millis(2500));
        assert_eq!(duration_from_secs(0.0), Duration::ZERO);
        assert_eq!(duration_from_secs(f64::INFINITY), Duration::MAX);
        assert_eq!(duration_from_secs(f64::NAN), Duration::MAX);
        assert_eq!(duration_from_secs(1e300), Duration::MAX);
        assert_eq!(duration_from_secs(-1.0), Duration::ZERO);
        assert_eq!(duration_from_secs(f64::NEG_INFINITY), Duration::ZERO);
    }

    #[test]
    fn env_double_treats_empty_as_unset() {
        let name = "EPICS_RS_ESTDLIB_TEST_EMPTY";
        unsafe { std::env::set_var(name, "") };
        assert_eq!(env_double(name), Err(EnvDoubleError::Unset));
        unsafe { std::env::set_var(name, "inf") };
        assert_eq!(env_double(name), Ok(f64::INFINITY));
        unsafe { std::env::set_var(name, "1e400") };
        assert_eq!(
            env_double(name),
            Err(EnvDoubleError::Invalid(ParseDoubleError::Overflow))
        );
        unsafe { std::env::remove_var(name) };
        assert_eq!(env_double(name), Err(EnvDoubleError::Unset));
    }
}
