//! The printf the standard converters hand their info string to:
//! `StreamBuffer::print(fmt.info, value)` is glibc's `vsnprintf` with one
//! conversion, `%[flags][width][.prec][l]conv`, as `copyFormatString`
//! builds it from the protocol's format.

use crate::buffer::StreamBuffer;
use epics_libcom_rs::cfmt::CFloat;

/// A conversion as glibc's parser reads it.
struct Spec {
    alt: bool,
    plus: bool,
    space: bool,
    left: bool,
    /// The `0` flag; glibc drops it when `-` is also given.
    zero: bool,
    width: usize,
    prec: Option<usize>,
    conv: u8,
    /// The text after the conversion character.
    rest: usize,
}

fn digits(s: &[u8], i: &mut usize) -> usize {
    let mut v = 0usize;
    while s.get(*i).is_some_and(u8::is_ascii_digit) {
        v = v.saturating_mul(10).saturating_add((s[*i] - b'0') as usize);
        *i += 1;
    }
    v
}

/// `info` is `%...`, as `copyFormatString` starts it; a length modifier
/// `l` is skipped.
fn parse(info: &[u8]) -> Spec {
    let mut s = Spec {
        alt: false,
        plus: false,
        space: false,
        left: false,
        zero: false,
        width: 0,
        prec: None,
        conv: 0,
        rest: 0,
    };
    let mut i = 1;
    loop {
        match info.get(i) {
            Some(b'#') => s.alt = true,
            Some(b'+') => s.plus = true,
            Some(b' ') => s.space = true,
            Some(b'-') => s.left = true,
            Some(b'0') => s.zero = true,
            _ => break,
        }
        i += 1;
    }
    s.zero &= !s.left;
    s.width = digits(info, &mut i);
    if info.get(i) == Some(&b'.') {
        i += 1;
        s.prec = Some(digits(info, &mut i));
    }
    if info.get(i) == Some(&b'l') {
        i += 1;
    }
    s.conv = info.get(i).copied().unwrap_or(0);
    s.rest = (i + 1).min(info.len());
    s
}

/// glibc's `printf_unknown`: a conversion character it does not know is
/// printed back as the spec it parsed, and the text after it as text.
///
/// `copyFormatString` keeps the input-only flags `!` and `*` when `=`
/// follows them; glibc takes `!` as the conversion. `*` makes C read a
/// width argument it never passed; it is printed the same way here.
fn unknown(out: &mut StreamBuffer, info: &[u8], s: &Spec) {
    // A scan format's info (none, or the width's 0x00s) is an empty format.
    if info.is_empty() {
        return;
    }
    out.append_byte(b'%');
    if s.alt {
        out.append_byte(b'#');
    }
    if s.plus {
        out.append_byte(b'+');
    } else if s.space {
        out.append_byte(b' ');
    }
    if s.left {
        out.append_byte(b'-');
    }
    if s.zero {
        out.append_byte(b'0');
    }
    if s.width != 0 {
        out.append(s.width.to_string().as_bytes());
    }
    if let Some(p) = s.prec {
        out.append_byte(b'.');
        out.append(p.to_string().as_bytes());
    }
    if s.conv != 0 {
        out.append_byte(s.conv);
    }
    out.append(&info[s.rest..]);
}

/// The text up to the info string's 0x00.
fn text(info: &[u8]) -> &[u8] {
    let n = info.iter().position(|&b| b == 0).unwrap_or(info.len());
    &info[..n]
}

/// Pad `prefix` + `body` to the width: spaces after with `-`, zeros
/// between them when `zero`, else spaces before.
fn pad(out: &mut StreamBuffer, s: &Spec, prefix: &[u8], body: &[u8], zero: bool) {
    let fill = s.width.saturating_sub(prefix.len() + body.len()) as isize;
    if s.left {
        out.append(prefix).append(body).append_repeat(b' ', fill);
    } else if zero {
        out.append(prefix).append_repeat(b'0', fill).append(body);
    } else {
        out.append_repeat(b' ', fill).append(prefix).append(body);
    }
}

/// `print(info, long)` for `%ld %li %lo %lu %lx %lX`.
pub fn print_long(out: &mut StreamBuffer, info: &[u8], value: i64) {
    let info = text(info);
    let s = parse(info);
    let (signed, base, upper) = match s.conv {
        b'd' | b'i' => (true, 10, false),
        b'u' => (false, 10, false),
        b'o' => (false, 8, false),
        b'x' => (false, 16, false),
        b'X' => (false, 16, true),
        _ => return unknown(out, info, &s),
    };
    let neg = signed && value < 0;
    let mut mag = if signed {
        value.unsigned_abs()
    } else {
        value as u64
    };
    let nonzero = mag != 0;
    let mut body = Vec::new();
    if nonzero || s.prec != Some(0) {
        loop {
            let d = (mag % base) as u8;
            body.push(match d {
                0..=9 => b'0' + d,
                _ if upper => b'A' + d - 10,
                _ => b'a' + d - 10,
            });
            mag /= base;
            if mag == 0 {
                break;
            }
        }
    }
    if let Some(p) = s.prec {
        body.resize(body.len().max(p), b'0');
    }
    if s.alt && s.conv == b'o' && body.last() != Some(&b'0') {
        body.push(b'0');
    }
    body.reverse();
    let prefix: &[u8] = if signed {
        if neg {
            b"-"
        } else if s.plus {
            b"+"
        } else if s.space {
            b" "
        } else {
            b""
        }
    } else if s.alt && nonzero && s.conv == b'x' {
        b"0x"
    } else if s.alt && nonzero && s.conv == b'X' {
        b"0X"
    } else {
        b""
    };
    pad(out, &s, prefix, &body, s.zero && s.prec.is_none());
}

/// `print(info, const char*)` for `%s`: glibc pads a string with spaces,
/// `0` or not.
pub fn print_string(out: &mut StreamBuffer, info: &[u8], value: &[u8]) {
    let info = text(info);
    let s = parse(info);
    if s.conv != b's' {
        return unknown(out, info, &s);
    }
    let mut value = text(value);
    if let Some(p) = s.prec {
        value = &value[..value.len().min(p)];
    }
    pad(out, &s, b"", value, false);
}

/// `print(info, long)` for `%c`: the `int` vararg cut to a byte, which may
/// be 0x00.
pub fn print_char(out: &mut StreamBuffer, info: &[u8], value: i64) {
    let info = text(info);
    let s = parse(info);
    if s.conv != b'c' {
        return unknown(out, info, &s);
    }
    pad(out, &s, b"", &[value as u8], false);
}

/// Digits past these are zeros: a double has at most 1074 fraction digits
/// and 767 significant ones. Rust's formatter takes a 16-bit precision,
/// glibc's an `int`.
const MAX_FRACTION: usize = 1100;
const MAX_SIGNIFICANT: usize = 800;

/// `a` (finite, not negative) as `%.{p}e` does, mantissa and exponent.
fn exp_digits(a: f64, p: usize) -> (String, i32) {
    let s = format!("{:.*e}", p.min(MAX_SIGNIFICANT), CFloat(a));
    let (m, x) = s.split_once('e').unwrap();
    let mut m = m.to_string();
    m.extend(std::iter::repeat_n('0', p.saturating_sub(MAX_SIGNIFICANT)));
    (m, x.parse().unwrap())
}

fn fixed_digits(a: f64, p: usize) -> String {
    let mut s = format!("{:.*}", p.min(MAX_FRACTION), CFloat(a));
    s.extend(std::iter::repeat_n('0', p.saturating_sub(MAX_FRACTION)));
    s
}

/// `%g` without `#`: no trailing zeros, and no point left alone.
fn trim_fraction(m: &mut String) {
    if m.contains('.') {
        let n = m.trim_end_matches('0').trim_end_matches('.').len();
        m.truncate(n);
    }
}

/// `print(info, double)` for `%f %e %E %g %G`.
pub fn print_double(out: &mut StreamBuffer, info: &[u8], value: f64) {
    let info = text(info);
    let s = parse(info);
    let (style, upper) = match s.conv {
        b'f' => (b'f', false),
        b'e' => (b'e', false),
        b'E' => (b'e', true),
        b'g' => (b'g', false),
        b'G' => (b'g', true),
        _ => return unknown(out, info, &s),
    };
    // glibc prints the sign bit, of -0 and of a NaN too.
    let prefix: &[u8] = if value.is_sign_negative() {
        b"-"
    } else if s.plus {
        b"+"
    } else if s.space {
        b" "
    } else {
        b""
    };
    if !value.is_finite() {
        let body: &[u8] = match (value.is_nan(), upper) {
            (true, false) => b"nan",
            (true, true) => b"NAN",
            (false, false) => b"inf",
            (false, true) => b"INF",
        };
        return pad(out, &s, prefix, body, false);
    }
    let a = value.abs();
    let p = s.prec.unwrap_or(6);
    let exp = |m: String, x: i32| {
        let mut b = m;
        if s.alt && !b.contains('.') {
            b.push('.');
        }
        b.push(if upper { 'E' } else { 'e' });
        b.push(if x < 0 { '-' } else { '+' });
        b.push_str(&format!("{:02}", x.unsigned_abs()));
        b
    };
    let body = match style {
        b'f' => {
            let mut b = fixed_digits(a, p);
            if s.alt && p == 0 {
                b.push('.');
            }
            b
        }
        b'e' => {
            let (m, x) = exp_digits(a, p);
            exp(m, x)
        }
        _ => {
            let p = p.max(1);
            let (mut m, x) = exp_digits(a, p - 1);
            // glibc 2.39 bug not copied: when rounding carries a value
            // below 10^P up to it, `%#g` keeps no zeros (999999.5 is
            // "1.e+06"); C99 7.21.6.1 keeps P-1 of them ("1.00000e+06").
            if x < -4 || x >= p as i32 {
                if !s.alt {
                    trim_fraction(&mut m);
                }
                exp(m, x)
            } else {
                let mut b = fixed_digits(a, (p as i32 - 1 - x) as usize);
                if s.alt {
                    if !b.contains('.') {
                        b.push('.');
                    }
                } else {
                    trim_fraction(&mut b);
                }
                b
            }
        }
    };
    pad(out, &s, prefix, body.as_bytes(), s.zero);
}
