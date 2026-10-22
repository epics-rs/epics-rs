//! C `printf` float conversions, the workspace's one owner of "double → text
//! the way C prints it".
//!
//! Rust's float formatting is not `printf`: `{}` is the shortest round-trip
//! form, `{:e}` writes a bare exponent (`1e5`, not `1e+05`), there is no `%g`,
//! and a NaN is `NaN` with its sign dropped. [`CFloat`] keeps Rust's exact
//! decimal digits and changes only what C spells differently:
//!
//! | Rust | C | conversion |
//! | --- | --- | --- |
//! | `{:.N}` of `CFloat(x)` | `%.Nf` | [`Display`](fmt::Display) |
//! | `{:.Ne}` / `{:.NE}` | `%.Ne` / `%.NE` | [`LowerExp`](fmt::LowerExp) / [`UpperExp`](fmt::UpperExp) |
//! | `{:.N}` of `CFloat(x).g()` | `%.Ng` | [`CFloatG`] |
//!
//! Without a precision each takes C's default of 6. The width, `-`, `+`, `0`
//! and `#` flags mean what they mean in C. Non-finite values are spelled as
//! glibc spells them for every conversion — `nan`, `-nan` (the sign bit of a
//! NaN is printed), `inf`, `-inf`, upper case for `%E` — and the `0` flag pads
//! them with spaces, as glibc does. `-0.0` keeps its sign: `-0.000000`,
//! `-0`.
//!
//! Below it is glibc's printf one conversion at a time, into a `Vec<u8>`:
//! [`Directive::parse`] reads a `%` directive, [`Directive::spec`] its `*`
//! counts, and [`print_int`], [`print_double`], [`print_str`],
//! [`print_c_str`], [`print_char`], [`print_pointer`] and [`print_unknown`]
//! convert by the resulting [`Spec`].

use std::fmt::{self, Write as _};

/// A double printed as C `printf` prints it. See the [module docs](self).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CFloat(pub f64);

/// A double printed by C `%g`; made by [`CFloat::g`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CFloatG(pub f64);

impl CFloat {
    /// The `%g` conversion of this value.
    pub fn g(self) -> CFloatG {
        CFloatG(self.0)
    }
}

#[derive(Clone, Copy)]
enum Conv {
    F,
    E { upper: bool },
    G,
}

const DEFAULT_PRECISION: usize = 6;

impl fmt::Display for CFloat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        emit(f, self.0, Conv::F)
    }
}

impl fmt::LowerExp for CFloat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        emit(f, self.0, Conv::E { upper: false })
    }
}

impl fmt::UpperExp for CFloat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        emit(f, self.0, Conv::E { upper: true })
    }
}

impl fmt::Display for CFloatG {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        emit(f, self.0, Conv::G)
    }
}

fn emit(f: &mut fmt::Formatter<'_>, x: f64, conv: Conv) -> fmt::Result {
    let sign = if x.is_sign_negative() {
        "-"
    } else if f.sign_plus() {
        "+"
    } else {
        ""
    };
    let upper = matches!(conv, Conv::E { upper: true });
    let mut body = Text::new();
    if x.is_nan() {
        body.write_str(if upper { "NAN" } else { "nan" })?;
    } else if x.is_infinite() {
        body.write_str(if upper { "INF" } else { "inf" })?;
    } else {
        let a = x.abs();
        let alt = f.alternate();
        let p = f.precision().unwrap_or(DEFAULT_PRECISION);
        match conv {
            Conv::F => fixed(&mut body, a, p, alt),
            Conv::E { upper } => exponential(&mut body, a, p, alt, upper),
            Conv::G => general(&mut body, a, p, alt),
        }?;
    }
    let body = body.as_str();
    let len = sign.len() + body.len();
    let pad = f.width().unwrap_or(0).saturating_sub(len);
    if pad == 0 {
        return write!(f, "{sign}{body}");
    }
    if matches!(f.align(), Some(fmt::Alignment::Left)) {
        write!(f, "{sign}{body}")?;
        (0..pad).try_for_each(|_| f.write_char(f.fill()))
    } else if f.sign_aware_zero_pad() && x.is_finite() {
        write!(f, "{sign}{:0>pad$}{body}", "")
    } else {
        (0..pad).try_for_each(|_| f.write_char(f.fill()))?;
        write!(f, "{sign}{body}")
    }
}

/// A conversion's text, on the stack until a precision in the dozens
/// outgrows it, so the put path's number-to-string allocates nothing.
struct Text {
    stack: [u8; 64],
    len: usize,
    heap: Option<String>,
}

impl Text {
    fn new() -> Self {
        Text {
            stack: [0; 64],
            len: 0,
            heap: None,
        }
    }

    fn as_str(&self) -> &str {
        match &self.heap {
            Some(heap) => heap,
            None => std::str::from_utf8(&self.stack[..self.len]).expect("only str is written"),
        }
    }

    fn truncate(&mut self, len: usize) {
        match &mut self.heap {
            Some(heap) => heap.truncate(len),
            None => self.len = len,
        }
    }
}

impl fmt::Write for Text {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        if self.heap.is_none() && end <= self.stack.len() {
            self.stack[self.len..end].copy_from_slice(s.as_bytes());
            self.len = end;
            return Ok(());
        }
        if self.heap.is_none() {
            self.heap = Some(self.as_str().to_string());
        }
        self.heap.as_mut().expect("just set").push_str(s);
        Ok(())
    }
}

/// `%.pf` of a non-negative finite value. Rust's fixed-precision digits are
/// exact and round half to even, as glibc's are.
fn fixed(out: &mut Text, a: f64, p: usize, alt: bool) -> fmt::Result {
    write!(out, "{a:.p$}")?;
    if alt && p == 0 {
        out.write_char('.')?;
    }
    Ok(())
}

/// `%.pe`: Rust's digits, C's exponent — signed and at least two digits.
fn exponential(out: &mut Text, a: f64, p: usize, alt: bool, upper: bool) -> fmt::Result {
    let mut sci = Text::new();
    write!(sci, "{a:.p$e}")?;
    let (mantissa, exp) = split_exponent(sci.as_str());
    c_exponent(out, mantissa, exp, alt && p == 0, upper)
}

fn split_exponent(sci: &str) -> (&str, i32) {
    let (mantissa, exp) = sci.split_once('e').expect("LowerExp emits an exponent");
    (
        mantissa,
        exp.parse().expect("LowerExp emits a decimal exponent"),
    )
}

fn c_exponent(
    out: &mut Text,
    mantissa: &str,
    exp: i32,
    add_point: bool,
    upper: bool,
) -> fmt::Result {
    let point = if add_point { "." } else { "" };
    let e = if upper { 'E' } else { 'e' };
    let es = if exp < 0 { '-' } else { '+' };
    write!(out, "{mantissa}{point}{e}{es}{:02}", exp.unsigned_abs())
}

/// `%.pg` (C99 7.19.6.1): with `P` the precision (0 counts as 1) and `X` the
/// exponent `%e` would print at `P - 1`, `%f` at `P - 1 - X` when
/// `P > X >= -4`, otherwise `%e` at `P - 1`; trailing zeros and a bare point
/// go unless `#`.
fn general(out: &mut Text, a: f64, p: usize, alt: bool) -> fmt::Result {
    let p = p.max(1);
    let mut sci = Text::new();
    write!(sci, "{a:.prec$e}", prec = p - 1)?;
    let (mantissa, exp) = split_exponent(sci.as_str());
    if exp >= -4 && exp < p as i32 {
        fixed(out, a, (p as i32 - 1 - exp) as usize, alt)?;
        if !alt {
            let kept = strip_zeros(out.as_str()).len();
            out.truncate(kept);
        }
        Ok(())
    } else if alt {
        c_exponent(out, mantissa, exp, p == 1, false)
    } else {
        c_exponent(out, strip_zeros(mantissa), exp, false, false)
    }
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// A width or precision as a directive gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Count {
    None,
    Given(usize),
    /// `*`: read from the next argument, an `int`.
    Arg,
}

/// A length modifier; `q` is `Ll`, `Z` is `Z`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Length {
    None,
    Hh,
    H,
    L,
    Ll,
    BigL,
    J,
    Z,
    T,
}

/// One `%` directive as glibc's printf parses it, positional `n$` aside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Directive {
    /// The flags and the conversion character, which is 0 when the format
    /// ends first (glibc's `EINVAL`).
    pub base: Spec,
    pub width: Count,
    pub prec: Count,
    pub length: Length,
    /// One past the conversion character.
    pub end: usize,
}

fn digits(s: &[u8], i: &mut usize) -> usize {
    let mut v = 0usize;
    while s.get(*i).is_some_and(u8::is_ascii_digit) {
        v = v.saturating_mul(10).saturating_add((s[*i] - b'0') as usize);
        *i += 1;
    }
    v
}

impl Directive {
    /// The directive whose `%` is `fmt[at]`.
    pub fn parse(fmt: &[u8], at: usize) -> Directive {
        let mut d = Directive {
            base: Spec::default(),
            width: Count::None,
            prec: Count::None,
            length: Length::None,
            end: fmt.len(),
        };
        let f = &mut d.base;
        let mut i = at + 1;
        loop {
            match fmt.get(i) {
                Some(b'#') => f.alt = true,
                Some(b'+') => f.plus = true,
                Some(b' ') => f.space = true,
                Some(b'-') => f.left = true,
                Some(b'0') => f.zero = true,
                Some(b'\'') => f.group = true,
                Some(b'I') => f.i18n = true,
                _ => break,
            }
            i += 1;
        }
        f.zero &= !f.left;
        if fmt.get(i) == Some(&b'*') {
            i += 1;
            d.width = Count::Arg;
        } else if fmt.get(i).is_some_and(u8::is_ascii_digit) {
            d.width = Count::Given(digits(fmt, &mut i));
        }
        if fmt.get(i) == Some(&b'.') {
            i += 1;
            d.prec = if fmt.get(i) == Some(&b'*') {
                i += 1;
                Count::Arg
            } else {
                Count::Given(digits(fmt, &mut i))
            };
        }
        let twice = |i: usize, c| fmt.get(i + 1) == Some(&c);
        (d.length, i) = match fmt.get(i) {
            Some(b'h') if twice(i, b'h') => (Length::Hh, i + 2),
            Some(b'h') => (Length::H, i + 1),
            Some(b'l') if twice(i, b'l') => (Length::Ll, i + 2),
            Some(b'l') => (Length::L, i + 1),
            Some(b'q') => (Length::Ll, i + 1),
            Some(b'L') => (Length::BigL, i + 1),
            Some(b'j') => (Length::J, i + 1),
            Some(b'z' | b'Z') => (Length::Z, i + 1),
            Some(b't') => (Length::T, i + 1),
            _ => (Length::None, i),
        };
        if let Some(&c) = fmt.get(i).filter(|&&c| c != 0) {
            d.base.conv = c;
            d.end = i + 1;
        }
        d
    }

    /// The spec, `next` giving each `*` count in turn: a negative width is
    /// the `-` flag and its magnitude, a negative precision is none.
    pub fn spec(&self, mut next: impl FnMut() -> i32) -> Spec {
        let mut s = self.base;
        match self.width {
            Count::None => {}
            Count::Given(w) => s.width = w,
            Count::Arg => {
                let w = next();
                s.width = w.unsigned_abs() as usize;
                if w < 0 {
                    s.left = true;
                    s.zero = false;
                }
            }
        }
        s.prec = match self.prec {
            Count::None => None,
            Count::Given(p) => Some(p),
            Count::Arg => usize::try_from(next()).ok(),
        };
        s
    }
}

/// One conversion with its counts read, what the `print_*` functions
/// format by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spec {
    pub alt: bool,
    pub plus: bool,
    pub space: bool,
    pub left: bool,
    /// The `0` flag; glibc drops it when `-` is also given.
    pub zero: bool,
    /// The `'` and `I` flags, which change nothing in the C locale but are
    /// printed back by [`print_unknown`].
    pub group: bool,
    pub i18n: bool,
    pub width: usize,
    pub prec: Option<usize>,
    pub conv: u8,
}

/// Pad `prefix` + `body` to the width: spaces after with `-`, zeros
/// between them when `zero`, else spaces before.
fn pad(out: &mut Vec<u8>, s: &Spec, prefix: &[u8], body: &[u8], zero: bool) {
    let fill = s.width.saturating_sub(prefix.len() + body.len());
    if s.left {
        out.extend_from_slice(prefix);
        out.extend_from_slice(body);
        out.resize(out.len() + fill, b' ');
    } else if zero {
        out.extend_from_slice(prefix);
        out.resize(out.len() + fill, b'0');
        out.extend_from_slice(body);
    } else {
        out.resize(out.len() + fill, b' ');
        out.extend_from_slice(prefix);
        out.extend_from_slice(body);
    }
}

/// glibc's `printf_unknown`: a conversion character it does not know is
/// printed back as the spec it parsed, the length modifier left out.
pub fn print_unknown(out: &mut Vec<u8>, s: &Spec) {
    out.push(b'%');
    if s.alt {
        out.push(b'#');
    }
    if s.group {
        out.push(b'\'');
    }
    if s.plus {
        out.push(b'+');
    } else if s.space {
        out.push(b' ');
    }
    if s.left {
        out.push(b'-');
    }
    if s.zero {
        out.push(b'0');
    }
    if s.i18n {
        out.push(b'I');
    }
    if s.width != 0 {
        out.extend_from_slice(s.width.to_string().as_bytes());
    }
    if let Some(p) = s.prec {
        out.push(b'.');
        out.extend_from_slice(p.to_string().as_bytes());
    }
    if s.conv != 0 {
        out.push(s.conv);
    }
}

/// The digits of `mag` in `base`, at least `prec` of them, most
/// significant first; none for 0 at precision 0.
fn int_digits(mut mag: u64, base: u64, upper: bool, prec: Option<usize>) -> Vec<u8> {
    let mut body = Vec::new();
    if mag != 0 || prec != Some(0) {
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
    if let Some(p) = prec {
        body.resize(body.len().max(p), b'0');
    }
    body.reverse();
    body
}

/// `%d %i %o %u %x %X` of `value`, an unsigned conversion taking it as
/// `u64`; any other conversion is [`print_unknown`] and `false`.
pub fn print_int(out: &mut Vec<u8>, s: &Spec, value: i64) -> bool {
    let (signed, base, upper) = match s.conv {
        b'd' | b'i' => (true, 10, false),
        b'u' => (false, 10, false),
        b'o' => (false, 8, false),
        b'x' => (false, 16, false),
        b'X' => (false, 16, true),
        _ => {
            print_unknown(out, s);
            return false;
        }
    };
    let neg = signed && value < 0;
    let mag = if signed {
        value.unsigned_abs()
    } else {
        value as u64
    };
    let mut body = int_digits(mag, base, upper, s.prec);
    if s.alt && s.conv == b'o' && body.first() != Some(&b'0') {
        body.insert(0, b'0');
    }
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
    } else if s.alt && mag != 0 && s.conv == b'x' {
        b"0x"
    } else if s.alt && mag != 0 && s.conv == b'X' {
        b"0X"
    } else {
        b""
    };
    pad(out, s, prefix, &body, s.zero && s.prec.is_none());
    true
}

/// `%p` of an address: `(nil)` for 0, padded as a string, else `0x` and
/// hex digits, which the sign flags, precision and `0` apply to.
pub fn print_pointer(out: &mut Vec<u8>, s: &Spec, addr: usize) {
    if addr == 0 {
        return pad(out, s, b"", b"(nil)", false);
    }
    let body = int_digits(addr as u64, 16, false, s.prec);
    let prefix: &[u8] = if s.plus {
        b"+0x"
    } else if s.space {
        b" 0x"
    } else {
        b"0x"
    };
    pad(out, s, prefix, &body, s.zero && s.prec.is_none());
}

/// `%s` of `value`, cut at the precision; glibc pads a string with
/// spaces, `0` or not. Any other conversion is [`print_unknown`] and
/// `false`.
pub fn print_str(out: &mut Vec<u8>, s: &Spec, value: &[u8]) -> bool {
    if s.conv != b's' {
        print_unknown(out, s);
        return false;
    }
    let value = &value[..value.len().min(s.prec.unwrap_or(usize::MAX))];
    pad(out, s, b"", value, false);
    true
}

/// [`print_str`] of a C string that may be the null pointer, which glibc
/// prints as `(null)`, or as nothing under a precision below 6.
pub fn print_c_str(out: &mut Vec<u8>, s: &Spec, value: Option<&[u8]>) -> bool {
    match value {
        Some(v) => print_str(out, s, v),
        None if s.prec.is_some_and(|p| p < 6) => print_str(out, s, b""),
        None => print_str(out, s, b"(null)"),
    }
}

/// `%c` of `value`, which may be 0x00. Any other conversion is
/// [`print_unknown`] and `false`.
pub fn print_char(out: &mut Vec<u8>, s: &Spec, value: u8) -> bool {
    if s.conv != b'c' {
        print_unknown(out, s);
        return false;
    }
    pad(out, s, b"", &[value], false);
    true
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

/// `%a` of `a` (finite, not negative) after the `0x`: glibc's leading
/// digit, which a rounding carry can make 2, and binary exponent.
fn hex_digits(a: f64, prec: Option<usize>, alt: bool, upper: bool) -> String {
    let bits = a.to_bits();
    let (mut lead, exp) = match bits >> 52 {
        0 if a == 0.0 => (0u64, 0i64),
        0 => (0, -1022),
        e => (1, e as i64 - 1023),
    };
    let mut frac = bits & ((1 << 52) - 1);
    let mut n = 13;
    if let Some(p) = prec.filter(|&p| p < 13) {
        let drop = 4 * (13 - p) as u32;
        let rest = frac & ((1 << drop) - 1);
        let half = 1 << (drop - 1);
        frac >>= drop;
        // Ties go to the even last kept digit, the leading one at `%.0a`.
        let odd = if p == 0 { lead } else { frac } & 1 == 1;
        if rest > half || (rest == half && odd) {
            frac += 1;
            if frac >> (4 * p) != 0 {
                frac = 0;
                lead += 1;
            }
        }
        n = p;
    }
    let mut digits = if n == 0 {
        String::new()
    } else {
        format!("{frac:0n$x}")
    };
    match prec {
        Some(p) => digits.extend(std::iter::repeat_n('0', p.saturating_sub(13))),
        None => digits.truncate(digits.trim_end_matches('0').len()),
    }
    let point = if digits.is_empty() && !alt { "" } else { "." };
    let s = format!("{lead}{point}{digits}p{exp:+}");
    if upper { s.to_uppercase() } else { s }
}

/// `%f %F %e %E %g %G %a %A` of `value`; any other conversion is
/// [`print_unknown`] and `false`.
pub fn print_double(out: &mut Vec<u8>, s: &Spec, value: f64) -> bool {
    let (style, upper) = match s.conv {
        b'f' => (b'f', false),
        b'F' => (b'f', true),
        b'e' => (b'e', false),
        b'E' => (b'e', true),
        b'g' => (b'g', false),
        b'G' => (b'g', true),
        b'a' => (b'a', false),
        b'A' => (b'a', true),
        _ => {
            print_unknown(out, s);
            return false;
        }
    };
    // glibc prints the sign bit, of -0 and of a NaN too.
    let sign: &[u8] = if value.is_sign_negative() {
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
        pad(out, s, sign, body, false);
        return true;
    }
    let a = value.abs();
    if style == b'a' {
        let prefix = [sign, if upper { b"0X" } else { b"0x" }].concat();
        let body = hex_digits(a, s.prec, s.alt, upper);
        pad(out, s, &prefix, body.as_bytes(), s.zero);
        return true;
    }
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
    pad(out, s, sign, body.as_bytes(), s.zero);
    true
}

/// Byte equality with glibc's `snprintf`; newlib (RTEMS), musl and the macOS
/// libc (which prints a negative NaN as `nan`) are not that reference.
#[cfg(all(test, unix, target_env = "gnu"))]
mod glibc {
    use super::tests::GLIBC_ALT_G_CARRY;
    use super::*;

    /// glibc's own rendering of `x` through the one-conversion `spec`.
    fn c(spec: &str, x: f64) -> String {
        let spec = std::ffi::CString::new(spec).unwrap();
        let mut buf = vec![0u8; 512];
        // SAFETY: `spec` holds exactly one double conversion and `buf` is
        // writable for its whole length, which is the size passed.
        let n = unsafe { libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), spec.as_ptr(), x) };
        assert!(n >= 0 && (n as usize) < buf.len(), "snprintf {n}");
        buf.truncate(n as usize);
        String::from_utf8(buf).unwrap()
    }

    fn values() -> Vec<f64> {
        let mut v = vec![
            0.0,
            -0.0,
            f64::NAN,
            -f64::NAN,
            f64::from_bits(0xfff8_0000_0000_0001),
            f64::INFINITY,
            f64::NEG_INFINITY,
            1.0,
            -1.0,
            0.5,
            1.5,
            2.5,
            0.125,
            0.375,
            -0.125,
            0.1,
            0.2 + 0.1,
            1.0 / 3.0,
            2.0 / 3.0,
            9.5,
            9.999995,
            9.9999949,
            99999.95,
            999999.5,
            123456.0,
            1234567.0,
            1e-4,
            1e-5,
            0.00012345,
            0.000099999,
            1e15,
            1e16,
            1e21,
            1e22,
            1e100,
            -1e-300,
            123.456,
            -987654.321,
            f64::MAX,
            f64::MIN_POSITIVE,
            5e-324,
            f64::EPSILON,
            std::f64::consts::PI,
            -std::f64::consts::E,
            4503599627370497.0,
        ];
        v.extend((-30..=30).map(|e| 1.2345678901234567 * 10f64.powi(e)));
        v
    }

    #[test]
    fn precision_conversions_match_glibc() {
        for x in values() {
            for p in 0..=20usize {
                let ps = format!(".{p}");
                assert_eq!(
                    format!("{:.p$}", CFloat(x)),
                    c(&format!("%{ps}f"), x),
                    "%{ps}f {x:e}"
                );
                assert_eq!(
                    format!("{:.p$e}", CFloat(x)),
                    c(&format!("%{ps}e"), x),
                    "%{ps}e {x:e}"
                );
                assert_eq!(
                    format!("{:.p$E}", CFloat(x)),
                    c(&format!("%{ps}E"), x),
                    "%{ps}E {x:e}"
                );
                assert_eq!(
                    format!("{:.p$}", CFloat(x).g()),
                    c(&format!("%{ps}g"), x),
                    "%{ps}g {x:e}"
                );
                if !GLIBC_ALT_G_CARRY.iter().any(|&(v, q, _)| v == x && q == p) {
                    assert_eq!(
                        format!("{:#.p$}", CFloat(x).g()),
                        c(&format!("%#{ps}g"), x),
                        "%#{ps}g {x:e}"
                    );
                }
            }
        }
    }

    /// Past the 64 bytes `Text` keeps on the stack.
    #[test]
    fn long_conversions_match_glibc() {
        for x in values() {
            for p in [40usize, 70, 100] {
                let cases = [
                    (format!("{:.p$}", CFloat(x)), format!("%.{p}f")),
                    (format!("{:.p$e}", CFloat(x)), format!("%.{p}e")),
                    (format!("{:.p$}", CFloat(x).g()), format!("%.{p}g")),
                    (format!("{:#.p$}", CFloat(x).g()), format!("%#.{p}g")),
                ];
                for (got, spec) in cases {
                    assert_eq!(got, c(&spec, x), "{spec} {x:e}");
                }
            }
        }
    }

    #[test]
    fn default_precision_and_flags_match_glibc() {
        for x in values() {
            let cases = [
                (format!("{}", CFloat(x)), "%f"),
                (format!("{:e}", CFloat(x)), "%e"),
                (format!("{}", CFloat(x).g()), "%g"),
                (format!("{:+.3}", CFloat(x)), "%+.3f"),
                (format!("{:12.3}", CFloat(x)), "%12.3f"),
                (format!("{:<12.3}", CFloat(x)), "%-12.3f"),
                (format!("{:012.3}", CFloat(x)), "%012.3f"),
                (format!("{:+015.4e}", CFloat(x)), "%+015.4e"),
                (format!("{:#.0}", CFloat(x)), "%#.0f"),
                (format!("{:#.0e}", CFloat(x)), "%#.0e"),
                (format!("{:<14}", CFloat(x).g()), "%-14g"),
                (format!("{:010}", CFloat(x).g()), "%010g"),
            ];
            for (got, spec) in cases {
                assert_eq!(got, c(spec, x), "{spec} {x:e}");
            }
        }
    }

    /// glibc's `snprintf` of a format and its arguments.
    macro_rules! c_printf {
        ($fmt:expr $(, $arg:expr)*) => {{
            let fmt = std::ffi::CString::new($fmt).unwrap();
            let mut buf = vec![0u8; 512];
            // SAFETY: each conversion in `fmt` takes the argument passed for
            // it, and `buf` is writable for its whole length, the size passed.
            let n = unsafe {
                libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), fmt.as_ptr() $(, $arg)*)
            };
            assert!(n >= 0 && (n as usize) < buf.len(), "snprintf {n}");
            buf.truncate(n as usize);
            buf
        }};
    }

    /// `fmt`, one directive without `*`, through [`Directive`] and `print`.
    fn rs(fmt: &str, print: impl FnOnce(&mut Vec<u8>, &Spec) -> bool) -> Vec<u8> {
        let d = Directive::parse(fmt.as_bytes(), 0);
        let mut out = Vec::new();
        print(&mut out, &d.spec(|| unreachable!()));
        out.extend_from_slice(&fmt.as_bytes()[d.end..]);
        out
    }

    const FLAGS: [&str; 14] = [
        "", "-", "+", " ", "#", "0", "-0", "+0", " 0", "#0", "-#", "+ ", "'", "I",
    ];

    /// Each flag set, width and precision, `conv` last.
    fn specs(convs: &[&str], widths: &[&str], precs: &[&str]) -> Vec<String> {
        let mut v = Vec::new();
        for f in FLAGS {
            for w in widths {
                for p in precs {
                    for c in convs {
                        v.push(format!("%{f}{w}{p}{c}"));
                    }
                }
            }
        }
        v
    }

    #[test]
    fn integer_conversions_match_glibc() {
        let values = [
            0i64,
            1,
            -1,
            7,
            8,
            255,
            4096,
            -12345,
            i32::MIN as i64,
            i32::MAX as i64,
            i64::MIN,
            i64::MAX,
        ];
        let convs = ["ld", "li", "lo", "lu", "lx", "lX"];
        for spec in specs(&convs, &["", "1", "8", "25"], &["", ".", ".0", ".1", ".5"]) {
            for v in values {
                let want = c_printf!(spec.as_str(), v as libc::c_long);
                assert_eq!(rs(&spec, |o, s| print_int(o, s, v)), want, "{spec} {v}");
            }
        }
    }

    #[test]
    fn float_conversions_match_glibc() {
        let convs = ["f", "F", "e", "E", "g", "G", "a", "A"];
        let precs = ["", ".", ".0", ".1", ".3", ".12", ".13", ".17"];
        for spec in specs(&convs, &["", "8", "30"], &precs) {
            for x in values() {
                let d = Directive::parse(spec.as_bytes(), 0);
                let p = match d.prec {
                    Count::Given(p) => p,
                    _ => 6,
                };
                let decimal = !matches!(d.base.conv, b'a' | b'A');
                if decimal && d.base.i18n
                    || d.base.alt
                        && matches!(d.base.conv, b'g' | b'G')
                        && GLIBC_ALT_G_CARRY.iter().any(|&(v, q, _)| v == x && q == p)
                {
                    continue;
                }
                let want = c_printf!(spec.as_str(), x);
                let got = rs(&spec, |o, s| print_double(o, s, x));
                assert_eq!(got, want, "{spec} {x:e}");
            }
        }
    }

    #[test]
    fn hex_float_rounding_matches_glibc() {
        for x in values() {
            for p in 0..=14 {
                for spec in [format!("%.{p}a"), format!("%#.{p}A")] {
                    let want = c_printf!(spec.as_str(), x);
                    let got = rs(&spec, |o, s| print_double(o, s, x));
                    assert_eq!(got, want, "{spec} {x:e}");
                }
            }
        }
    }

    #[test]
    fn string_char_and_pointer_conversions_match_glibc() {
        let precs = ["", ".", ".0", ".3", ".5", ".6", ".10"];
        for spec in specs(&["s"], &["", "3", "10"], &precs) {
            for v in [&b""[..], b"hi", b"hello world"] {
                let cs = std::ffi::CString::new(v).unwrap();
                let want = c_printf!(spec.as_str(), cs.as_ptr());
                assert_eq!(rs(&spec, |o, s| print_c_str(o, s, Some(v))), want, "{spec}");
            }
            let want = c_printf!(spec.as_str(), std::ptr::null::<libc::c_char>());
            assert_eq!(
                rs(&spec, |o, s| print_c_str(o, s, None)),
                want,
                "{spec} NULL"
            );
        }
        for spec in specs(&["c"], &["", "3"], &[""]) {
            for v in [0u8, b'x', 200] {
                let want = c_printf!(spec.as_str(), v as libc::c_int);
                assert_eq!(rs(&spec, |o, s| print_char(o, s, v)), want, "{spec} {v}");
            }
        }
        for spec in specs(&["p"], &["", "5", "12"], &["", ".0", ".8"]) {
            for v in [0usize, 1, 0x1234, usize::MAX] {
                let want = c_printf!(spec.as_str(), v as *const libc::c_void);
                let got = rs(&spec, |o, s| {
                    print_pointer(o, s, v);
                    true
                });
                assert_eq!(got, want, "{spec} {v:#x}");
            }
        }
    }

    /// An unknown conversion is printed back, its length modifier left out,
    /// and takes no argument.
    #[test]
    fn unknown_conversions_match_glibc() {
        for spec in specs(&["y", "hy", "lk", "Ly"], &["", "8"], &["", ".0", ".3"]) {
            let tail = format!("{spec}|");
            let want = c_printf!(tail.as_str());
            assert_eq!(rs(&tail, |o, s| print_int(o, s, 0)), want, "{tail}");
            assert_eq!(rs(&tail, |o, s| print_double(o, s, 0.0)), want, "{tail}");
        }
    }

    /// A `*` width below zero is the `-` flag, a `*` precision below zero is
    /// none.
    #[test]
    fn star_counts_match_glibc() {
        for (w, p) in [(5, 2), (-5, 2), (5, -2), (-7, -1), (0, 0)] {
            let cases = [
                ("%*d", vec![w], c_printf!("%*d", w, -3)),
                ("%0*d", vec![w], c_printf!("%0*d", w, -3)),
                ("%.*d", vec![p], c_printf!("%.*d", p, -3)),
                ("%-*.*d", vec![w, p], c_printf!("%-*.*d", w, p, -3)),
                ("%0*.*f", vec![w, p], c_printf!("%0*.*f", w, p, -3.25)),
            ];
            for (fmt, counts, want) in cases {
                let mut counts = counts.into_iter();
                let s = Directive::parse(fmt.as_bytes(), 0).spec(|| counts.next().unwrap());
                assert_eq!(counts.next(), None, "{fmt}");
                let mut got = Vec::new();
                if fmt.ends_with('f') {
                    print_double(&mut got, &s, -3.25);
                } else {
                    print_int(&mut got, &s, -3);
                }
                assert_eq!(got, want, "{fmt} {w} {p}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// glibc 2.39 `%#.Pg` drops the zeros `#` keeps when rounding carries the
    /// value into `%e` form (`1.e+05`); C99 7.19.6.1 keeps them. Not copied.
    pub(super) const GLIBC_ALT_G_CARRY: [(f64, usize, &str); 2] =
        [(99999.95, 5, "1.0000e+05"), (999999.5, 6, "1.00000e+06")];

    /// glibc 2.39 drops every digit of a decimal float under the `I` flag
    /// in the C locale (`%If` of 0.0 is "."). Not copied: the flag changes
    /// nothing.
    #[test]
    fn i18n_flag_keeps_float_digits() {
        let s = Directive::parse(b"%I8.2e", 0).spec(|| 0);
        let mut out = Vec::new();
        print_double(&mut out, &s, -1.5);
        assert_eq!(out, b"-1.50e+00");
    }

    #[test]
    fn alternate_g_keeps_zeros_through_a_rounding_carry() {
        for (x, p, want) in GLIBC_ALT_G_CARRY {
            assert_eq!(format!("{:#.p$}", CFloat(x).g()), want);
        }
    }

    #[test]
    fn non_finite_and_negative_zero_spellings() {
        assert_eq!(CFloat(f64::NAN).to_string(), "nan");
        assert_eq!(CFloat(-f64::NAN).to_string(), "-nan");
        assert_eq!(format!("{:e}", CFloat(-f64::INFINITY)), "-inf");
        assert_eq!(format!("{:E}", CFloat(f64::NAN)), "NAN");
        assert_eq!(CFloat(f64::INFINITY).g().to_string(), "inf");
        assert_eq!(CFloat(-0.0).to_string(), "-0.000000");
        assert_eq!(CFloat(-0.0).g().to_string(), "-0");
    }

    #[test]
    fn flags_pad_as_c_does() {
        assert_eq!(format!("{:+08.2}", CFloat(-1.5)), "-0001.50");
        assert_eq!(format!("{:+.1e}", CFloat(12345.0)), "+1.2e+04");
        assert_eq!(format!("{:<8}|", CFloat(0.5).g()), "0.5     |");
        assert_eq!(format!("{:08}", CFloat(-f64::NAN).g()), "    -nan");
        assert_eq!(format!("{:#.0}", CFloat(3.0)), "3.");
        assert_eq!(format!("{:.3}", CFloat(1e-5).g()), "1e-05");
    }

    #[test]
    fn directive_parses_as_glibc() {
        let d = Directive::parse(b"ab%-0'+#12.lld|", 2);
        let f = d.base;
        assert!(f.left && f.plus && f.alt && f.group && !f.zero && !f.space && !f.i18n);
        assert_eq!(
            (d.width, d.prec, d.length, f.conv, d.end),
            (Count::Given(12), Count::Given(0), Length::Ll, b'd', 14)
        );
        let d = Directive::parse(b"%*.*hhx", 0);
        assert_eq!(
            (d.width, d.prec, d.length),
            (Count::Arg, Count::Arg, Length::Hh)
        );
        for (fmt, length) in [
            (&b"%qd"[..], Length::Ll),
            (b"%Lf", Length::BigL),
            (b"%jd", Length::J),
            (b"%Zd", Length::Z),
            (b"%td", Length::T),
            (b"%hd", Length::H),
        ] {
            assert_eq!(Directive::parse(fmt, 0).length, length);
        }
        // The format ends inside the directive.
        for fmt in [&b"%"[..], b"%-5", b"%5.2l", b"%-\0d"] {
            assert_eq!(Directive::parse(fmt, 0).base.conv, 0);
        }
    }
}
