//! C's `strtod`, ported once ([`strtod`]), and the `epicsStdlib` parsers
//! built on it.
//!
//! The hex-float ports used to build the significand in an `f64` (`mant = mant * 16.0 +
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

/// C `strtod`'s three results: the value, how far it read, and `errno`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Strtod {
    pub value: f64,
    /// Bytes consumed: C's `endptr - str`. 0 is "no conversion performed".
    pub len: usize,
    /// C `errno == ERANGE`: the text named a nonzero number that does not fit —
    /// overflow (the value is infinite) or underflow (subnormal, or zero); or
    /// a `nan(...)` payload too big for `strtoull`.
    pub erange: bool,
}

/// C `strtod` (glibc; `epicsStrtod` is `#define`d to it on every platform
/// with a working one — `osi/os/posix/osdStrtod.h`): leading C whitespace, an
/// optional sign, then `inf[inity]`, `nan[(n-char-sequence)]`, a C99 hex float
/// or a decimal one, all case-insensitive. The workspace's one port; `s` need
/// not end at the number.
pub fn strtod(s: &[u8]) -> Strtod {
    let mut i = 0;
    while i < s.len() && c_isspace(s[i] as char) {
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
        // A lone sign or a lone `.` consumes nothing at all.
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

/// The unsigned part, its length, and ERANGE.
fn magnitude(s: &[u8]) -> Option<(f64, usize, bool)> {
    // A spelled infinity or NaN is exact: glibc leaves errno clear, so an
    // explicit `EPICS_CA_CONN_TMO=inf` is a valid timeout.
    if starts_ci(s, b"inf") {
        let n = if starts_ci(s, b"infinity") { 8 } else { 3 };
        return Some((f64::INFINITY, n, false));
    }
    if starts_ci(s, b"nan") {
        let n = nan_char_sequence(&s[3..]);
        let payload = if n == 0 { &[][..] } else { &s[4..2 + n] };
        let (nan, erange) = nan_payload(payload);
        return Some((nan, 3 + n, erange));
    }
    let hex_digit_at = |k: usize| s.get(k).is_some_and(u8::is_ascii_hexdigit);
    if s.len() > 2
        && s[0] == b'0'
        && (s[1] | 0x20) == b'x'
        && (hex_digit_at(2) || (s[2] == b'.' && hex_digit_at(3)))
    {
        return Some(hex(s));
    }
    decimal(s)
}

/// ERANGE for the DECIMAL path: a nonzero significand that came out infinite,
/// subnormal or zero.
///
/// glibc's rule is tiny AND inexact, so a decimal literal naming a subnormal
/// exactly would leave errno clear. That takes some 750 significant digits
/// (`2^-1074`); the hex path, where such a literal is three characters long,
/// gets the exact test from [`HexSignificand::to_f64`].
fn ranged_out(value: f64, significand_is_nonzero: bool) -> bool {
    significand_is_nonzero && (value.is_infinite() || value.abs() < f64::MIN_POSITIVE)
}

/// `nan(n-char-sequence)` (C99 7.22.1.3): consumed only when it closes over
/// letters, digits and `_`.
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

/// glibc's `SET_NAN_PAYLOAD`: the sequence, when `strtoull(seq, &end, 0)`
/// reads all of it, goes in the mantissa below the quiet bit; a zero
/// payload leaves the default NaN. strtoull's ERANGE stays in errno.
fn nan_payload(seq: &[u8]) -> (f64, bool) {
    let (digits, radix) = match seq {
        [b'0', x, d, ..] if *x | 0x20 == b'x' && d.is_ascii_hexdigit() => (&seq[2..], 16),
        [b'0', ..] => (seq, 8),
        _ => (seq, 10),
    };
    let mut mant = 0u64;
    let mut erange = false;
    let mut read = 0;
    for d in digits.iter().map_while(|&b| (b as char).to_digit(radix)) {
        match mant
            .checked_mul(radix as u64)
            .and_then(|m| m.checked_add(d as u64))
        {
            Some(m) => mant = m,
            None => erange = true,
        }
        read += 1;
    }
    if erange {
        mant = u64::MAX;
    }
    let mant = mant & ((1 << 51) - 1);
    if read != digits.len() || mant == 0 {
        return (f64::NAN, erange);
    }
    (f64::from_bits(f64::NAN.to_bits() | mant), erange)
}

/// `0x` significand and optional `p` exponent; the caller checked a hex digit
/// follows the `0x` or its `.`.
fn hex(s: &[u8]) -> (f64, usize, bool) {
    let mut i = 2;
    let mut sig = HexSignificand::new();
    while i < s.len() && s[i].is_ascii_hexdigit() {
        sig.push_digit(hex_val(s[i]), false);
        i += 1;
    }
    if i < s.len() && s[i] == b'.' {
        i += 1;
        while i < s.len() && s[i].is_ascii_hexdigit() {
            sig.push_digit(hex_val(s[i]), true);
            i += 1;
        }
    }
    if i < s.len() && (s[i] | 0x20) == b'p' {
        if let Some((exp, len)) = exponent(&s[i..]) {
            sig.apply_binary_exponent(exp);
            i += len;
        }
    }
    let (value, erange) = sig.to_f64();
    (value, i, erange)
}

/// Digits with at most one `.`, then an optional `e` exponent; a mantissa
/// with no digit (a lone `.`) is not a number.
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
    if i < s.len() && (s[i] | 0x20) == b'e' {
        if let Some((_, len)) = exponent(&s[i..]) {
            i += len;
        }
    }
    // Rust's parser takes this grammar and, like strtod, saturates to inf or
    // 0 out of range; the text is ASCII by construction.
    let text = std::str::from_utf8(&s[..i]).ok()?;
    let value: f64 = text.parse().ok()?;
    Some((value, i, ranged_out(value, nonzero)))
}

/// `e`/`p`, an optional sign, then at least one decimal digit; without a
/// digit the marker is not consumed (`1e` is 1 followed by `e`).
fn exponent(s: &[u8]) -> Option<(i32, usize)> {
    let mut i = 1;
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

fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        _ => (c | 0x20) - b'a' + 10,
    }
}

fn starts_ci(s: &[u8], word: &[u8]) -> bool {
    s.len() >= word.len() && s[..word.len()].eq_ignore_ascii_case(word)
}
/// C `epicsParseDouble(str, to, units)` with a NON-NULL `units`
/// (`epicsStdlib.c:149-176`): skip leading whitespace, run `strtod`, reject
/// `ERANGE`, skip trailing whitespace, and hand back whatever remains
/// instead of refusing it. This is the form `store_double_value` uses for
/// filter options (`chfPlugin.c:273`), which is why `{"dbnd":{"d":"0.5 V"}}`
/// stores 0.5 rather than failing the parse.
pub fn epics_parse_double_units(s: &str) -> Result<(f64, &str), ParseDoubleError> {
    let r = strtod(s.as_bytes());
    if r.len == 0 {
        return Err(ParseDoubleError::NoConversion);
    }
    if r.erange {
        return Err(if r.value == 0.0 {
            ParseDoubleError::Underflow
        } else {
            ParseDoubleError::Overflow
        });
    }
    Ok((r.value, s[r.len..].trim_start_matches(c_isspace)))
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
pub fn epics_str_hash(s: impl AsRef<[u8]>, seed: u32) -> u32 {
    let mut hash = seed;
    let bytes = s.as_ref();
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

    /// `strtod` against the C library's on every edge the ports disagreed on:
    /// hex floats (`0x.8`, a bare `0x`, `p` with no digit), the `inf`/`nan`
    /// words and `nan(...)`'s character set, leading C whitespace (`\v`
    /// included), an exponent with no digits, and both ERANGE sides.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn strtod_matches_c_strtod() {
        let cases: &[&[u8]] = &[
            b"0x10",
            b"0X1P4",
            b"0x1.8p1",
            b"0x.8",
            b"-0x.8p1",
            b"0x.",
            b"0x.p1",
            b"0x",
            b"0xg",
            b"0x1p",
            b"0x1p+",
            b"0x1p-1074",
            b"0x1p-1075",
            b"0x1p1024",
            b"inf",
            b"-Inf",
            b"infinit",
            b"INFINITY",
            b"nan",
            b"-NaN",
            b"nan(abc_1)",
            b"nan(123)",
            b"-nan(5)",
            b"nan(0x7b)",
            b"nan(0X7B)",
            b"nan(017)",
            b"nan(08)",
            b"nan(0)",
            b"nan(0x)",
            b"nan(0xg)",
            b"nan(2251799813685248)",
            b"nan(2251799813685249)",
            b"nan(99999999999999999999999)",
            b"nan(99999999999999999999999z)",
            b"nan(a-b)",
            b"nan(",
            b"nan()",
            b" 1.5",
            b"\t\n\x0b\x0c\r-2",
            b"\xa01",
            b"1e",
            b"1e+",
            b"1E-x",
            b"1e5",
            b".5",
            b".e1",
            b".",
            b"+",
            b"-",
            b"",
            b"1.2.3",
            b"12abc",
            b"2.2e-308",
            b"2.3e-308",
            b"1e400",
            b"-1e400",
            b"1e-400",
            b"0e999",
            b"4.9e-324",
        ];
        for &text in cases {
            let c = std::ffi::CString::new(text).unwrap();
            let mut end = std::ptr::null_mut();
            // SAFETY: `c` is NUL-terminated and outlives the call.
            let (v, erange) = unsafe {
                *libc::__errno_location() = 0;
                let v = libc::strtod(c.as_ptr(), &mut end);
                (v, *libc::__errno_location() == libc::ERANGE)
            };
            let len = end as usize - c.as_ptr() as usize;
            let r = strtod(text);
            let shown = String::from_utf8_lossy(text);
            assert_eq!(r.len, len, "length of {shown:?}");
            assert_eq!(r.erange, erange, "ERANGE of {shown:?}");
            assert_eq!(r.value.to_bits(), v.to_bits(), "value of {shown:?}");
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
