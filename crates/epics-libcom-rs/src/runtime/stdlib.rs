//! The numeric core of C's `strtod` for C99 hex floats, shared by every
//! `strtod` port in the workspace (`calc::engine::strtod`, the CALC literal
//! scanner, and `epics-ca-rs`'s `estdlib`, the env-knob parser).
//!
//! Both ports used to build the significand in an `f64` (`mant = mant * 16.0 +
//! digit`) and then scale it with `mant * 2.0f64.powi(exp)`. That composition
//! is wrong twice over, and the whole subnormal range paid for it:
//!
//! * `powi` with a negative exponent evaluates `1.0 / 2^-exp`, and `2^1074`
//!   is already infinite — so every exponent below about -1023 scaled the
//!   significand by `1/inf == 0`. `0x1p-1074` came back as an underflow to
//!   zero where glibc returns the exact smallest subnormal.
//! * ERANGE was then *guessed back* from the result: any subnormal was
//!   reported as an ERANGE overflow. glibc raises ERANGE only when the exact
//!   value is tiny AND inexact, so `0x1p-1023` — an exactly representable
//!   subnormal — leaves errno clear.
//!
//! [`HexSignificand`] keeps the digits as an exact integer `m * 2^e2` with a
//! sticky bit for anything that falls off the bottom, so the conversion to
//! `f64` is a SINGLE correctly-rounded step (ties to even) and knows precisely
//! whether it was inexact. Every row of the boundary table in this module's
//! tests was probed against the compiled glibc `strtod` on this platform.

/// C `isspace` in the "C" locale: space, `\t`, `\n`, `\v`, `\f`, `\r`.
///
/// Rust's `is_ascii_whitespace` is the WhatWG set, which **omits the vertical
/// tab** — so every port of a C `strtol`/`strtod`/`sscanf`/`isspace` skip that
/// reached for it stopped one codepoint short of the C it cites. This is that
/// skip's one owner; a byte-oriented scanner passes `b as char`, which maps
/// 0x80..0xFF to U+0080..U+00FF and so stays false there, as C does.
pub const fn c_isspace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r')
}

/// A C99 hex float's significand, accumulated exactly.
///
/// The value is `m * 2^e2`, plus a non-zero tail below `m`'s window when
/// `sticky` is set. `m` holds up to 60 bits, well past `f64`'s 53 plus the
/// guard and round bits, so the sticky tail is all the rounding needs.
#[derive(Debug, Clone, Copy, Default)]
pub struct HexSignificand {
    m: u64,
    e2: i32,
    sticky: bool,
}

impl HexSignificand {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold in one hex digit. `fractional` marks the digits after the `.`,
    /// which lower the exponent instead of raising the value.
    pub fn push_digit(&mut self, digit: u8, fractional: bool) {
        if self.m >> 60 == 0 {
            self.m = (self.m << 4) | u64::from(digit);
            if fractional {
                self.e2 = self.e2.saturating_sub(4);
            }
        } else {
            // Past 60 bits the digit lies below what `f64` can hold; it can
            // only ever break a rounding tie, which is what `sticky` records.
            self.sticky |= digit != 0;
            if !fractional {
                self.e2 = self.e2.saturating_add(4);
            }
        }
    }

    /// Apply the `p<exp>` binary exponent.
    pub fn apply_binary_exponent(&mut self, exp: i32) {
        self.e2 = self.e2.saturating_add(exp);
    }

    /// Round to the nearest `f64` (ties to even) and report `errno == ERANGE`.
    ///
    /// glibc sets ERANGE when the result overflows to infinity, and when the
    /// EXACT value is tiny (below the smallest normal, `2^-1022`) and the
    /// conversion is inexact. Note both halves: `0x1p-1074` is tiny but exact
    /// (no ERANGE), and `0x1.fffffffffffffp-1023` is tiny-and-inexact yet
    /// rounds up to the smallest *normal* — ERANGE all the same, because the
    /// test is on the value before rounding.
    pub fn to_f64(&self) -> (f64, bool) {
        let m = self.m;
        if m == 0 {
            return (0.0, false);
        }
        // Normalize so the top bit of `mm` is the significand's MSB, whose
        // weight is 2^e: the value is `(mm / 2^63) * 2^e`, a number in [1, 2)
        // scaled by 2^e.
        let shift = m.leading_zeros();
        let mm = m << shift;
        let e = i64::from(self.e2) + 63 - i64::from(shift);
        if e > 1023 {
            return (f64::INFINITY, true);
        }

        let tiny = e < -1022;
        // Bits of significand this exponent can carry: 53 for a normal, fewer
        // once the subnormal floor (the lowest bit is worth 2^-1074) eats into
        // the bottom.
        let keep = if tiny { e + 1075 } else { 53 };
        if keep <= 0 {
            // Below half of the smallest subnormal, or exactly half — a tie
            // rounds to even, i.e. to zero. Only a strict majority rounds up.
            let round_up = keep == 0 && (mm != 1 << 63 || self.sticky);
            return if round_up {
                (pow2(-1074), true)
            } else {
                (0.0, true)
            };
        }

        let drop = 64 - keep as u32; // keep is 1..=53, so drop is 11..=63
        let kept = mm >> drop;
        let rest = mm & ((1u64 << drop) - 1);
        let half = 1u64 << (drop - 1);
        let inexact = rest != 0 || self.sticky;
        let round_up = rest > half || (rest == half && (self.sticky || kept & 1 == 1));
        // `kept` is at most 53 bits and the scale is an exact power of two, so
        // this product is the single correctly-rounded result — subnormal
        // results included, where the product is still exact.
        let value = (kept + u64::from(round_up)) as f64 * pow2((e - keep + 1) as i32);

        let erange = value.is_infinite() || (tiny && inexact);
        (value, erange)
    }
}

/// Exact `2^k` for `k` in `[-1074, 1023]` — the only range [`HexSignificand`]
/// asks for. `2.0f64.powi(k)` cannot serve: below -1023 it takes the
/// reciprocal of an already-infinite `2^-k` and collapses to zero.
fn pow2(k: i32) -> f64 {
    debug_assert!((-1074..=1023).contains(&k));
    if k >= -1022 {
        f64::from_bits(((k + 1023) as u64) << 52)
    } else {
        f64::from_bits(1u64 << (k + 1074))
    }
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

/// ERANGE classification for a value the DECIMAL path computed from digits.
///
/// glibc raises ERANGE when the result overflows to infinity, when it
/// underflows to zero, and when it is inexactly representable as a
/// subnormal. It does NOT raise it for the `inf` / `nan` *words*, which
/// is why those are classified separately at their parse site.
///
/// The "inexactly" is the whole rule in glibc — a subnormal it can name
/// exactly leaves errno clear. Writing one in decimal takes some 750
/// significant digits (`2^-1074`), so every decimal literal short enough to
/// appear in an env var and land in the subnormal range is inexact, and this
/// value-only test agrees with C on all of them. The hex path, where such a
/// literal is three characters long, does NOT use this: it gets the exact
/// inexactness from [`HexSignificand::to_f64`].
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
    while i < b.len() && c_isspace(b[i] as char) {
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
        let mut sig = HexSignificand::new();
        let mut digits = 0usize;
        while j < b.len() && b[j].is_ascii_hexdigit() {
            sig.push_digit(hex_val(b[j]), false);
            digits += 1;
            j += 1;
        }
        if j < b.len() && b[j] == b'.' {
            let mut k = j + 1;
            let mut frac = 0usize;
            while k < b.len() && b[k].is_ascii_hexdigit() {
                sig.push_digit(hex_val(b[k]), true);
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
                sig.apply_binary_exponent(if eneg { -e } else { e });
                j = k;
            }
        }
        // The significand is exact and rounds to `f64` in one step, so ERANGE
        // is known rather than guessed back from the value: an exactly
        // representable subnormal (`0x1p-1074`, `0x1p-1023`) leaves errno
        // clear, as it does in glibc.
        let (mut v, erange) = sig.to_f64();
        if neg {
            v = -v;
        }
        // C derives underflow-vs-overflow from the value alone
        // (`epicsStdlib.c:164`), and so does `epics_parse_double` below.
        let erange = if !erange {
            Erange::No
        } else if v == 0.0 {
            Erange::Under
        } else {
            Erange::Over
        };
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

/// C `epicsParseDouble(str, to, units)` with a NON-NULL `units`
/// (`epicsStdlib.c:149-176`): skip leading whitespace, run `strtod`, reject
/// `ERANGE`, skip trailing whitespace, and hand back whatever remains
/// instead of refusing it. This is the form `store_double_value` uses for
/// filter options (`chfPlugin.c:273`), which is why `{"dbnd":{"d":"0.5 V"}}`
/// stores 0.5 rather than failing the parse.
pub fn epics_parse_double_units(s: &str) -> Result<(f64, &str), ParseDoubleError> {
    let (v, used, erange) = strtod(s);
    if used == 0 {
        return Err(ParseDoubleError::NoConversion);
    }
    match erange {
        Erange::Over => return Err(ParseDoubleError::Overflow),
        Erange::Under => return Err(ParseDoubleError::Underflow),
        Erange::No => {}
    }
    Ok((v, s[used..].trim_start_matches(c_isspace)))
}

/// C `epicsParseDouble(str, to, NULL)` — the same function with the
/// remainder refused as `S_stdlib_extraneous` (`epicsStdlib.c:169-170`)
/// rather than returned.
pub fn epics_parse_double(s: &str) -> Result<f64, ParseDoubleError> {
    match epics_parse_double_units(s)? {
        (v, "") => Ok(v),
        _ => Err(ParseDoubleError::Extraneous),
    }
}

/// C `epicsScanDouble` (`epicsStdlib.h:203`) — `epicsParseDouble` with the
/// status collapsed to a boolean.
pub fn epics_scan_double(s: &str) -> Option<f64> {
    epics_parse_double(s).ok()
}

/// C `epicsStrHash` (`libcom/src/misc/epicsString.c:365-376` at `R7.0.10`) —
/// the string hash every general-purpose hash table in base keys on
/// (`gpHashLib.c:107`, the registry, the record-name directory).
///
/// ```c
/// unsigned int hash = seed;
/// while ((c = *str++)) {
///     hash ^= ~((hash << 11) ^ c ^ (hash >> 5));
///     if (!(c = *str++)) break;
///     hash ^= (hash << 7) ^ c ^ (hash >> 3);
/// }
/// ```
///
/// C's `char c` is signed on every platform base builds for, and it widens to
/// `int` before the XOR, so a byte at or above `0x80` contributes its
/// SIGN-EXTENDED value. `i8 as i32 as u32` is that widening; taking the byte
/// as `u32` directly would diverge on any non-ASCII name. The shifts are
/// C's on a 32-bit `unsigned int`, so they wrap rather than overflow.
#[must_use]
pub fn epics_str_hash(s: &str, seed: u32) -> u32 {
    let mut hash = seed;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = i32::from(bytes[i] as i8) as u32;
        hash ^= !((hash << 11) ^ c ^ (hash >> 5));
        i += 1;
        if i >= bytes.len() {
            break;
        }
        let c = i32::from(bytes[i] as i8) as u32;
        hash ^= (hash << 7) ^ c ^ (hash >> 3);
        i += 1;
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boundary: the empty string is the seed untouched — C's `while` never
    /// runs — and an odd length exits through the mid-loop `break` while an
    /// even one exits through the `while` test, so both arms are covered.
    #[test]
    fn epics_str_hash_matches_the_c_reference() {
        for &(s, seed, want) in &[
            ("", 0u32, 0u32),
            ("", 0x1234_5678, 0x1234_5678),
            ("a", 0, 0xffff_ff9e),
            ("ab", 0, 0x1fff_cf0f),
            ("abc", 0, 0x1e87_b6eb),
            ("operator", 0, 0xe15e_03c1),
            ("host.example.org", 0, 0x5134_8623),
            ("abc", 0x1234_5678, 0xc25f_c587),
            ("host.example.org", 0x1234_5678, 0xd9df_e17d),
        ] {
            assert_eq!(
                epics_str_hash(s, seed),
                want,
                "epicsStrHash({s:?}, {seed:#x})"
            );
        }
    }

    /// Boundary: a byte at or above `0x80`. C's `char` is signed, so the
    /// byte enters the XOR sign-extended; reading it as `u32` would give
    /// `0x1fff_9e72` here instead.
    #[test]
    fn epics_str_hash_sign_extends_a_high_byte() {
        assert_eq!(epics_str_hash("\u{e9}", 0), 0xffff_e192);
    }

    /// Parse a `0x…` hex float through the accumulator, the way both callers
    /// drive it.
    fn hex(s: &str) -> (f64, bool) {
        let b = s.as_bytes();
        let mut sig = HexSignificand::new();
        let mut i = 2; // skip "0x"
        while i < b.len() && b[i].is_ascii_hexdigit() {
            sig.push_digit(hex_val(b[i]), false);
            i += 1;
        }
        if i < b.len() && b[i] == b'.' {
            i += 1;
            while i < b.len() && b[i].is_ascii_hexdigit() {
                sig.push_digit(hex_val(b[i]), true);
                i += 1;
            }
        }
        if i < b.len() && (b[i] | 0x20) == b'p' {
            sig.apply_binary_exponent(s[i + 1..].parse::<i32>().unwrap());
        }
        sig.to_f64()
    }

    fn hex_val(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            _ => (c | 0x20) - b'a' + 10,
        }
    }

    /// Every row probed against the compiled glibc `strtod`: `(text, bits of
    /// the result, errno == ERANGE)`.
    #[test]
    fn matches_glibc_strtod_across_the_subnormal_boundary() {
        let rows: &[(&str, u64, bool)] = &[
            // Exactly representable subnormals: glibc leaves errno CLEAR.
            ("0x1p-1074", 0x0000_0000_0000_0001, false),
            ("0x2p-1075", 0x0000_0000_0000_0001, false),
            ("0x1p-1073", 0x0000_0000_0000_0002, false),
            ("0x1p-1023", 0x0008_0000_0000_0000, false),
            // Tiny AND inexact: ERANGE, non-zero result (C: overflow).
            ("0x1.8p-1075", 0x0000_0000_0000_0001, true),
            ("0x1.4p-1074", 0x0000_0000_0000_0001, true),
            ("0x1.cp-1074", 0x0000_0000_0000_0002, true),
            ("0x3p-1075", 0x0000_0000_0000_0002, true),
            ("0x1.0000000000001p-1074", 0x0000_0000_0000_0001, true),
            ("0x123456789abcdefp-1100", 0x0000_0000_48d1_59e2, true),
            // Tiny and inexact, yet it rounds up to the smallest NORMAL —
            // ERANGE is decided before rounding, so it still fires.
            ("0x1.fffffffffffffp-1023", 0x0010_0000_0000_0000, true),
            // Tiny, inexact, rounds to zero: ERANGE (C: underflow).
            ("0x1p-1075", 0, true), // exactly half → ties to even → zero
            ("0x1p-1076", 0, true),
            ("0x1p-2000", 0, true),
            // Normal range.
            ("0x1p-1022", 0x0010_0000_0000_0000, false),
            ("0x1p1023", 0x7fe0_0000_0000_0000, false),
            ("0x10", 0x4030_0000_0000_0000, false),
            ("0x1.8p1", 0x4008_0000_0000_0000, false),
            // Overflow.
            ("0x1p1024", 0x7ff0_0000_0000_0000, true),
            ("0x1.fffffffffffff8p1023", 0x7ff0_0000_0000_0000, true),
            // A zero significand names zero exactly, at any exponent.
            ("0x0p0", 0, false),
            ("0x0p-5000", 0, false),
            // More than 53 bits of significand: one rounding, ties to even.
            ("0x1.00000000000008p0", 0x3ff0_0000_0000_0000, false),
            ("0x1.00000000000018p0", 0x3ff0_0000_0000_0002, false),
            ("0x1.0000000000000fp0", 0x3ff0_0000_0000_0001, false),
        ];
        for &(text, bits, erange) in rows {
            let (v, e) = hex(text);
            assert_eq!(v.to_bits(), bits, "value of {text}");
            assert_eq!(e, erange, "ERANGE of {text}");
        }
    }
}
