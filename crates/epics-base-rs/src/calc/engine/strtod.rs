//! C's `strtod`, ported once.
//!
//! Two places in the C calc engines turn text into a double, and both are
//! strtod:
//!
//!   - the compilers' `LITERAL_OPERAND` case, which rewinds to the element's
//!     first character and re-scans (`epicsParseDouble`, postfix.c:261;
//!     `epicsStrtod`, sCalcPostfix.c:492 / aCalcPostfix.c:462);
//!   - sCalc's `to_double`, the coercion every numeric operand goes through
//!     (`(ps)->d = atof((ps)->s)`, sCalcPerform.c:83) — and `atof(s)` is
//!     `strtod(s, NULL)`.
//!
//! They are therefore one function here. The lexer needs to know how far the
//! literal ran, and `atof` does not, so the length comes back with the value:
//! a length of 0 is C's `pnext == psrc`, "no conversion performed".
//!
//! It also reports the one bit of `errno` a caller can care about. `strtod` sets
//! `ERANGE` when the text names a nonzero number the format cannot hold — too
//! big (the result is infinite) or too small (the result is subnormal, or zero).
//! `atof` and `epicsStrtod` throw that away; `epicsParseDouble` returns it as a
//! failure (`epicsStdlib.c:164`), which is how base's compiler turns `1e400`
//! into CALC_ERR_BAD_LITERAL. Carrying it here means neither caller has to
//! re-derive it from the value.

use crate::runtime::stdlib::HexSignificand;

/// C `strtod`'s three results: the value, how far it read, and `errno`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Strtod {
    pub value: f64,
    /// Bytes consumed. 0 is C's `pnext == psrc`, "no conversion performed".
    pub len: usize,
    /// C `errno == ERANGE`: the text named a nonzero number that does not fit —
    /// overflow (the value is infinite) or underflow (subnormal, or zero).
    pub erange: bool,
}

/// C99 `strtod`: leading whitespace, an optional sign, then an infinity, a NaN,
/// a hexadecimal significand, or a decimal one.
pub fn strtod(s: &[u8]) -> Strtod {
    let mut i = 0;
    while i < s.len() && crate::runtime::stdlib::c_isspace(s[i] as char) {
        i += 1;
    }
    let negative = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };

    let Some((magnitude, len, erange)) = magnitude(&s[i..]) else {
        // C converts nothing, and reports it by not advancing the end pointer —
        // so a lone sign or a lone `.` consumes nothing at all.
        return Strtod {
            value: 0.0,
            len: 0,
            erange: false,
        };
    };
    Strtod {
        value: if negative { -magnitude } else { magnitude },
        len: i + len,
        erange,
    }
}

/// The unsigned part: `inf[inity]`, `nan[(n-char-sequence)]`, `0x…`, or decimal.
/// Returns the magnitude, its length, and whether the conversion ranged out.
fn magnitude(s: &[u8]) -> Option<(f64, usize, bool)> {
    if starts_with_ci(s, b"inf") {
        // strtod takes the longer spelling when the text continues that way,
        // which is why `INFINITY` is a literal and not `INF` plus `INITY`.
        // A spelled infinity is exact: no ERANGE (compiled base postfix accepts
        // `INF` and rejects `1e400`).
        let n = if starts_with_ci(s, b"infinity") { 8 } else { 3 };
        return Some((f64::INFINITY, n, false));
    }
    if starts_with_ci(s, b"nan") {
        return Some((f64::NAN, 3 + nan_char_sequence(&s[3..]), false));
    }
    if s.len() > 2 && s[0] == b'0' && matches!(s[1], b'x' | b'X') && s[2].is_ascii_hexdigit() {
        return Some(hex(s));
    }
    decimal(s)
}

/// C `errno = ERANGE` for the DECIMAL path: the conversion could not represent
/// a nonzero number.
///
/// Overflow gives an infinity; underflow gives a subnormal or a zero — glibc
/// raises ERANGE for BOTH, which is why compiled base postfix rejects
/// `2.2e-308` (subnormal) and accepts `2.3e-308` (the smallest normal decade),
/// and why it accepts `0e999`: a zero significand names zero exactly, so there
/// is nothing to round away.
///
/// Strictly, glibc's rule is tiny AND inexact, so a decimal literal naming a
/// subnormal EXACTLY would leave errno clear here where this says ERANGE. That
/// takes upwards of 750 significant digits to write (`2^-1074` in decimal), and
/// no shorter literal can hit one; the hex path, where such a literal is three
/// characters long, gets the exact test from [`HexSignificand::to_f64`].
fn ranged_out(value: f64, significand_is_nonzero: bool) -> bool {
    significand_is_nonzero && (value.is_infinite() || value.abs() < f64::MIN_POSITIVE)
}

/// `nan(n-char-sequence)` (C99 7.22.1.3): consumed only when it closes.
fn nan_char_sequence(s: &[u8]) -> usize {
    if s.first() != Some(&b'(') {
        return 0;
    }
    match s.iter().position(|&b| b == b')') {
        Some(close)
            if s[1..close]
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_') =>
        {
            close + 1
        }
        _ => 0,
    }
}

/// A C99 hexadecimal significand with an optional binary (`p`) exponent. The
/// caller has already checked that `s` starts `0x<hexdigit>`. The second result
/// is ERANGE, which [`HexSignificand`] knows exactly — the digits are kept as
/// an exact integer and rounded to `f64` once, so a subnormal that IS
/// representable (`0x1p-1074`) is not mistaken for an underflow.
fn hex(s: &[u8]) -> (f64, usize, bool) {
    let mut i = 2;
    let mut sig = HexSignificand::new();
    while i < s.len() && s[i].is_ascii_hexdigit() {
        sig.push_digit(hex_digit(s[i]), false);
        i += 1;
    }
    if i < s.len() && s[i] == b'.' {
        i += 1;
        while i < s.len() && s[i].is_ascii_hexdigit() {
            sig.push_digit(hex_digit(s[i]), true);
            i += 1;
        }
    }
    if i < s.len() && matches!(s[i], b'p' | b'P') {
        if let Some((exp, len)) = exponent(&s[i..]) {
            sig.apply_binary_exponent(exp);
            i += len;
        }
    }
    let (value, erange) = sig.to_f64();
    (value, i, erange)
}

/// Digits with at most one `.`, then an optional decimal (`e`) exponent. The
/// second `.` of `1.2.3` is not part of the number, and a mantissa with no digit
/// at all (a lone `.`) is not a number.
fn decimal(s: &[u8]) -> Option<(f64, usize, bool)> {
    let mut i = 0;
    let mut digits = 0;
    let mut nonzero = false;
    while i < s.len() && s[i].is_ascii_digit() {
        nonzero |= s[i] != b'0';
        i += 1;
        digits += 1;
    }
    if i < s.len() && s[i] == b'.' {
        i += 1;
        while i < s.len() && s[i].is_ascii_digit() {
            nonzero |= s[i] != b'0';
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if i < s.len() && matches!(s[i], b'e' | b'E') {
        if let Some((_, len)) = exponent(&s[i..]) {
            i += len;
        }
    }
    // Rust's parser and strtod agree on this grammar's text.
    let text = std::str::from_utf8(&s[..i]).ok()?;
    let value: f64 = text.parse().ok()?;
    Some((value, i, ranged_out(value, nonzero)))
}

/// An exponent — `e`/`p`, an optional sign, then at least one DECIMAL digit.
/// Without a digit the marker is not consumed at all (`1E` is the number 1
/// followed by the letter E).
fn exponent(s: &[u8]) -> Option<(i32, usize)> {
    let mut i = 1; // the `e` / `p` itself
    let negative = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let start = i;
    let mut exp: i32 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        exp = exp.saturating_mul(10).saturating_add((s[i] - b'0') as i32);
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((if negative { -exp } else { exp }, i))
}

fn hex_digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    }
}

/// C `epicsStrnCaseCmp(text, name, strlen(name)) == 0`.
pub fn starts_with_ci(text: &[u8], name: &[u8]) -> bool {
    text.len() >= name.len() && text[..name.len()].eq_ignore_ascii_case(name)
}
