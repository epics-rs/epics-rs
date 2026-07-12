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

/// C99 `strtod`: leading whitespace, an optional sign, then an infinity, a NaN,
/// a hexadecimal significand, or a decimal one. Returns the value and how many
/// bytes it consumed; 0 bytes means no conversion (and a value of 0.0).
pub fn strtod(s: &[u8]) -> (f64, usize) {
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_whitespace() {
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

    let Some((magnitude, len)) = magnitude(&s[i..]) else {
        // C converts nothing, and reports it by not advancing the end pointer —
        // so a lone sign or a lone `.` consumes nothing at all.
        return (0.0, 0);
    };
    let value = if negative { -magnitude } else { magnitude };
    (value, i + len)
}

/// The unsigned part: `inf[inity]`, `nan[(n-char-sequence)]`, `0x…`, or decimal.
fn magnitude(s: &[u8]) -> Option<(f64, usize)> {
    if starts_with_ci(s, b"inf") {
        // strtod takes the longer spelling when the text continues that way,
        // which is why `INFINITY` is a literal and not `INF` plus `INITY`.
        let n = if starts_with_ci(s, b"infinity") { 8 } else { 3 };
        return Some((f64::INFINITY, n));
    }
    if starts_with_ci(s, b"nan") {
        return Some((f64::NAN, 3 + nan_char_sequence(&s[3..])));
    }
    if s.len() > 2 && s[0] == b'0' && matches!(s[1], b'x' | b'X') && s[2].is_ascii_hexdigit() {
        return Some(hex(s));
    }
    decimal(s)
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
/// caller has already checked that `s` starts `0x<hexdigit>`.
fn hex(s: &[u8]) -> (f64, usize) {
    let mut i = 2;
    let mut value = 0.0f64;
    while i < s.len() && s[i].is_ascii_hexdigit() {
        value = value * 16.0 + hex_digit(s[i]) as f64;
        i += 1;
    }
    if i < s.len() && s[i] == b'.' {
        i += 1;
        let mut scale = 1.0 / 16.0;
        while i < s.len() && s[i].is_ascii_hexdigit() {
            value += hex_digit(s[i]) as f64 * scale;
            scale /= 16.0;
            i += 1;
        }
    }
    if i < s.len() && matches!(s[i], b'p' | b'P') {
        if let Some((exp, len)) = exponent(&s[i..]) {
            value *= 2.0f64.powi(exp);
            i += len;
        }
    }
    (value, i)
}

/// Digits with at most one `.`, then an optional decimal (`e`) exponent. The
/// second `.` of `1.2.3` is not part of the number, and a mantissa with no digit
/// at all (a lone `.`) is not a number.
fn decimal(s: &[u8]) -> Option<(f64, usize)> {
    let mut i = 0;
    let mut digits = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < s.len() && s[i] == b'.' {
        i += 1;
        while i < s.len() && s[i].is_ascii_digit() {
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
    Some((text.parse().ok()?, i))
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
