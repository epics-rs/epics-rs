//! C `sscanf` as EPICS gets it from glibc 2.43 with `_GNU_SOURCE`
//! (`__isoc23_sscanf`): the one `vfscanf` every scanf-shaped parse in the
//! workspace goes through.
//!
//! `_GNU_SOURCE` selects the C23 entry point, so `%b` reads binary and `%i`
//! and `%b` take a `0b` prefix as `%i` and `%x` take `0x`.

use std::cell::Cell;
use std::ffi::c_int;

use super::stdlib;
use crate::cfmt::Length;

/// A value `sscanf` converted, at the width of its C type.
pub enum Scanned<'a> {
    Int(i64),
    UInt(u64),
    Double(f64),
    /// `%s` and `%[`, which the store terminates.
    Str(&'a [u8]),
    /// `%c`, stored without a terminator.
    Chars(&'a [u8]),
}

/// An `sscanf` destination: C's pointer argument.
pub trait ScanTarget {
    fn store(&self, value: Scanned<'_>);
}

/// A C number type `sscanf` stores into: a number converts to it as C's
/// cast does, `%c` stores its first byte, and a string leaves it as it was,
/// where C writes the bytes over it.
pub trait ScanNum: Copy {
    fn from_scanned(value: Scanned<'_>) -> Option<Self>;
}

macro_rules! scan_num {
    ($($t:ty),*) => {
        $(impl ScanNum for $t {
            fn from_scanned(value: Scanned<'_>) -> Option<Self> {
                Some(match value {
                    Scanned::Int(v) => v as $t,
                    Scanned::UInt(v) => v as $t,
                    Scanned::Double(v) => v as $t,
                    Scanned::Chars(b) => b[0] as $t,
                    Scanned::Str(_) => return None,
                })
            }
        })*
    };
}

scan_num!(i8, u8, i16, u16, i32, u32, i64, u64, f32, f64);

impl<T: ScanNum> ScanTarget for Cell<T> {
    fn store(&self, value: Scanned<'_>) {
        if let Some(v) = T::from_scanned(value) {
            self.set(v);
        }
    }
}

/// The argument a C caller did not pass, which glibc stores a surplus
/// conversion through anyway: put last, it counts the conversion and drops
/// it, so `sscanf(s, "%d %d", &one)` returns 2 as glibc does.
impl ScanTarget for () {
    fn store(&self, _: Scanned<'_>) {}
}

/// glibc's `inchar` and `ungetc` over a string; `None` is `EOF`.
struct Input<'a> {
    b: &'a [u8],
    at: usize,
}

impl Input<'_> {
    fn inchar(&mut self) -> Option<u8> {
        let c = self.b.get(self.at).copied();
        self.at += usize::from(c.is_some());
        c
    }

    fn ungetc(&mut self, c: Option<u8>) {
        self.at -= usize::from(c.is_some());
    }

    fn skip_space(&mut self) {
        let mut c = self.inchar();
        while c.is_some_and(is_space) {
            c = self.inchar();
        }
        self.ungetc(c);
    }
}

fn is_space(c: u8) -> bool {
    stdlib::c_isspace(c as char)
}

fn lower(c: Option<u8>) -> Option<u8> {
    c.map(|c| c.to_ascii_lowercase())
}

/// How a conversion ended short.
enum Stop {
    /// glibc's `input_error`: the input ended where a character was needed.
    Input,
    /// glibc's `conv_error`: the input or the format did not match.
    Conv,
}

/// Reads the `[sign][0[x|b]]digits` an integer conversion takes, as glibc's
/// `vfscanf` collects them for `strtol`; `base` 0 is `%i`'s. The read text
/// is the input span, which `strtol` takes in the returned base, prefix and
/// all. `None` is `%p`'s "(nil)".
fn scan_int(
    r: &mut Input<'_>,
    width: &mut i64,
    mut base: u32,
    pointer: bool,
) -> Result<Option<u32>, Stop> {
    let dec = |w: &mut i64| *w -= i64::from(*w > 0);
    let mut c = r.inchar();
    if c.is_none() {
        return Err(Stop::Input);
    }
    let signed = matches!(c, Some(b'-' | b'+'));
    if signed {
        dec(width);
        c = r.inchar();
    }
    let mut digits = false;
    if *width != 0 && c == Some(b'0') {
        dec(width);
        c = r.inchar();
        let prefix = match lower(c) {
            Some(b'x') if base == 0 || base == 16 => 16,
            Some(b'b') if base == 0 || base == 2 => 2,
            _ => 0,
        };
        // A `0x` or `0b` is a prefix, not the number `0`: with no digit after
        // it the item is a matching failure (C11 7.21.6.2p10). glibc before
        // 2.42 read it as 0 (BZ #12701), which is not copied.
        if *width != 0 && prefix != 0 {
            base = prefix;
            dec(width);
            if *width == 0 {
                return Err(Stop::Conv);
            }
            c = r.inchar();
        } else {
            if base == 0 {
                base = 8;
            }
            digits = true;
        }
    }
    if base == 0 {
        base = 10;
    }
    while let Some(d) = c.filter(|_| *width != 0) {
        if !(d as char).is_digit(base) {
            break;
        }
        digits = true;
        dec(width);
        c = r.inchar();
    }
    if !digits {
        // `%p` reads "(nil)" as 0.
        if pointer
            && !signed
            && (*width < 0 || *width >= 5)
            && c == Some(b'(')
            && b"nil)".iter().all(|&want| {
                c = r.inchar();
                lower(c) == Some(want)
            })
        {
            return Ok(None);
        }
        r.ungetc(c);
        return Err(Stop::Conv);
    }
    r.ungetc(c);
    Ok(Some(base))
}

/// `c` and then each letter of `word`, as glibc reads `nan` and `inf`: a
/// letter that differs is a conversion error.
fn spell(r: &mut Input<'_>, c: &mut Option<u8>, width: &mut i64, word: &[u8]) -> Result<(), Stop> {
    for &want in word {
        if *width == 0 {
            return Err(Stop::Conv);
        }
        *c = r.inchar();
        if lower(*c) != Some(want) {
            return Err(Stop::Conv);
        }
        *width -= i64::from(*width > 0);
    }
    Ok(())
}

/// Reads the text a float conversion takes, as glibc's `vfscanf` collects
/// it for `strtod`: a sign, then `nan`, `inf[inity]`, or a decimal or `0x`
/// hex number in the C locale. The read text is the input span.
fn scan_float(r: &mut Input<'_>, width: &mut i64) -> Result<(), Stop> {
    let dec = |w: &mut i64| *w -= i64::from(*w > 0);
    let mut c = r.inchar();
    dec(width);
    if c.is_none() {
        return Err(Stop::Input);
    }
    if let Some(b'-' | b'+') = c {
        if *width == 0 {
            return Err(Stop::Conv);
        }
        c = r.inchar();
        if c.is_none() {
            return Err(Stop::Conv);
        }
        dec(width);
    }
    if lower(c) == Some(b'n') {
        spell(r, &mut c, width, b"an")?;
        // glibc before 2.41 left a `(n-char-sequence)` unread (BZ #30647).
        if *width != 0 {
            c = r.inchar();
            if c != Some(b'(') {
                r.ungetc(c);
                return Ok(());
            }
            while c != Some(b')') {
                dec(width);
                if *width == 0 {
                    return Err(Stop::Conv);
                }
                c = r.inchar();
                if !c.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b')') {
                    return Err(Stop::Conv);
                }
            }
            dec(width);
        }
        return Ok(());
    }
    if lower(c) == Some(b'i') {
        spell(r, &mut c, width, b"nf")?;
        if *width != 0 {
            c = r.inchar();
            if lower(c) == Some(b'i') {
                dec(width);
                spell(r, &mut c, width, b"nity")?;
            } else {
                r.ungetc(c);
            }
        }
        return Ok(());
    }
    let mut exp_char = b'e';
    let (mut hex, mut got_digit, mut got_dot, mut got_e) = (false, false, false, false);
    // The last byte read was the exponent character, after which a sign may
    // come.
    let mut after_e = false;
    if *width != 0 && c == Some(b'0') {
        c = r.inchar();
        dec(width);
        if lower(c) == Some(b'x') {
            if *width == 0 {
                return Err(Stop::Conv);
            }
            hex = true;
            exp_char = b'p';
            c = r.inchar();
            dec(width);
        } else {
            got_digit = true;
        }
    }
    loop {
        match c {
            Some(d) if d.is_ascii_digit() || !got_e && hex && d.is_ascii_hexdigit() => {
                got_digit = true;
                after_e = false;
            }
            Some(b'-' | b'+') if after_e => after_e = false,
            Some(e) if got_digit && !got_e && e.to_ascii_lowercase() == exp_char => {
                got_e = true;
                got_dot = true;
                got_digit = false;
                after_e = true;
            }
            Some(b'.') if !got_dot => got_dot = true,
            _ => {
                r.ungetc(c);
                break;
            }
        }
        if *width == 0 {
            break;
        }
        c = r.inchar();
        if c.is_none() {
            break;
        }
        dec(width);
    }
    // No digit in the significand or after the exponent character, or a bare
    // `0x`: a matching failure. glibc before 2.42 handed `1e` or `0x.` to
    // `strtod` and took what it read (BZ #12701), which is not copied.
    if !got_digit {
        return Err(Stop::Conv);
    }
    Ok(())
}

/// The `[...]` set at `fmt[*f..]`, `*f` left past its `]`: the bytes in it
/// and whether it is `^` negated.
fn scan_set(fmt: &[u8], f: &mut usize) -> Option<([bool; 256], bool)> {
    let not_in = fmt.get(*f) == Some(&b'^');
    *f += usize::from(not_in);
    let mut set = [false; 256];
    if let Some(&c @ (b']' | b'-')) = fmt.get(*f) {
        set[c as usize] = true;
        *f += 1;
    }
    loop {
        let fc = *fmt.get(*f)?;
        *f += 1;
        match fmt.get(*f) {
            _ if fc == b']' => return Some((set, not_in)),
            Some(&to) if fc == b'-' && to != b']' && fmt[*f - 2] <= to => {
                for c in fmt[*f - 2]..to {
                    set[c as usize] = true;
                }
            }
            _ => set[fc as usize] = true,
        }
    }
}

/// C's cast of a signed conversion's `long` to its type.
fn narrow_int(v: i64, length: Length) -> i64 {
    match length {
        Length::None => v as i32 as i64,
        Length::H => v as i16 as i64,
        Length::Hh => v as i8 as i64,
        _ => v,
    }
}

/// The C string in `b`: the bytes before its first NUL.
fn c_str(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n])
}

/// glibc's `sscanf(input, fmt, ...)`: the number of conversions stored, or
/// -1 when the input ends before the first.
///
/// The destinations take the values in turn, each as its [`ScanTarget`]
/// stores it. A conversion with no destination left, `%m` allocation,
/// positional `%n$`, and a wide `%lc`, `%ls` or `%l[` end the scan as a
/// mismatch does.
pub fn sscanf(input: impl AsRef<[u8]>, fmt: impl AsRef<[u8]>, out: &[&dyn ScanTarget]) -> c_int {
    let input = c_str(input.as_ref());
    let fmt = c_str(fmt.as_ref());
    let mut r = Input { b: input, at: 0 };
    let mut targets = out.iter();
    let mut done: c_int = 0;
    let mut skip_space = false;
    let mut f = 0;
    let stopped = |stop: Stop, done: c_int| match stop {
        Stop::Input if done == 0 => -1,
        _ => done,
    };
    while let Some(&fc) = fmt.get(f) {
        f += 1;
        if fc != b'%' {
            if is_space(fc) {
                skip_space = true;
                continue;
            }
            let Some(mut c) = r.inchar() else {
                return stopped(Stop::Input, done);
            };
            if skip_space {
                while is_space(c) {
                    let Some(next) = r.inchar() else {
                        return stopped(Stop::Input, done);
                    };
                    c = next;
                }
                skip_space = false;
            }
            if c != fc {
                r.ungetc(Some(c));
                return done;
            }
            continue;
        }
        let mut suppress = false;
        while let Some(&flag @ (b'*' | b'\'' | b'I')) = fmt.get(f) {
            suppress |= flag == b'*';
            f += 1;
        }
        let mut width = 0i64;
        while let Some(d) = fmt.get(f).filter(|d| d.is_ascii_digit()) {
            width = width.saturating_mul(10).saturating_add(i64::from(d - b'0'));
            f += 1;
        }
        if width == 0 {
            width = -1;
        }
        let twice = |c| fmt.get(f + 1) == Some(&c);
        let length;
        (length, f) = match fmt.get(f) {
            Some(b'h') if twice(b'h') => (Length::Hh, f + 2),
            Some(b'h') => (Length::H, f + 1),
            Some(b'l') if twice(b'l') => (Length::Ll, f + 2),
            Some(b'l') => (Length::L, f + 1),
            Some(b'q' | b'L') => (Length::Ll, f + 1),
            Some(b'j' | b'z' | b't') => (Length::J, f + 1),
            _ => (Length::None, f),
        };
        let Some(&conv) = fmt.get(f) else {
            return done;
        };
        f += 1;
        if skip_space || !matches!(conv, b'[' | b'c' | b'n') {
            r.skip_space();
            skip_space = false;
        }
        let wide = matches!(length, Length::L | Length::Ll | Length::J);
        let value: Result<Option<Scanned<'_>>, Stop> = (|| match conv {
            b'%' => match r.inchar() {
                None => Err(Stop::Input),
                Some(b'%') => Ok(None),
                c => {
                    r.ungetc(c);
                    Err(Stop::Conv)
                }
            },
            b'n' => Ok(Some(Scanned::Int(narrow_int(r.at as i64, length)))),
            b'c' if !wide => {
                let start = r.at;
                r.inchar().ok_or(Stop::Input)?;
                let mut width = if width < 0 { 1 } else { width };
                loop {
                    width -= 1;
                    if width <= 0 || r.inchar().is_none() {
                        break;
                    }
                }
                let chars = Scanned::Chars(&input[start..r.at]);
                if width > 0 {
                    // Short of `width`: glibc has stored what it read, then
                    // fails the input. glibc before 2.42 counted the
                    // conversion (BZ #12701), which is not copied.
                    if !suppress {
                        targets.next().ok_or(Stop::Conv)?.store(chars);
                    }
                    return Err(Stop::Input);
                }
                Ok(Some(chars))
            }
            b's' if !wide => {
                let start = r.at;
                let mut c = r.inchar();
                if c.is_none() {
                    return Err(Stop::Input);
                }
                while let Some(ch) = c {
                    if is_space(ch) {
                        r.ungetc(c);
                        break;
                    }
                    if width > 0 {
                        width -= 1;
                        if width == 0 {
                            break;
                        }
                    }
                    c = r.inchar();
                }
                Ok(Some(Scanned::Str(&input[start..r.at])))
            }
            b'[' if !wide => {
                let (set, not_in) = scan_set(fmt, &mut f).ok_or(Stop::Conv)?;
                let start = r.at;
                let mut c = r.inchar();
                if c.is_none() {
                    return Err(Stop::Input);
                }
                let mut width = if width < 0 {
                    i64::from(c_int::MAX)
                } else {
                    width
                };
                while let Some(ch) = c {
                    if set[ch as usize] == not_in {
                        r.ungetc(c);
                        break;
                    }
                    width -= 1;
                    if width <= 0 {
                        break;
                    }
                    c = r.inchar();
                }
                if r.at == start {
                    return Err(Stop::Conv);
                }
                Ok(Some(Scanned::Str(&input[start..r.at])))
            }
            b'd' | b'i' | b'o' | b'u' | b'x' | b'X' | b'b' | b'p' => {
                let base = match conv {
                    b'd' | b'u' => 10,
                    b'i' => 0,
                    b'o' => 8,
                    b'b' => 2,
                    _ => 16,
                };
                let start = r.at;
                let Some(base) = scan_int(&mut r, &mut width, base, conv == b'p')? else {
                    return Ok(Some(Scanned::UInt(0)));
                };
                let text = &input[start..r.at];
                let length = if conv == b'p' { Length::L } else { length };
                Ok(Some(if matches!(conv, b'd' | b'i') {
                    let v = match length {
                        Length::Ll | Length::J => stdlib::strtoll(text, base).0,
                        _ => i128::from(stdlib::strtol(text, base).0) as i64,
                    };
                    Scanned::Int(narrow_int(v, length))
                } else {
                    let v = match length {
                        Length::Ll | Length::J => stdlib::strtoull(text, base).0,
                        _ => u128::from(stdlib::strtoul(text, base).0) as u64,
                    };
                    Scanned::UInt(match length {
                        Length::None => v as u32 as u64,
                        Length::H => v as u16 as u64,
                        Length::Hh => v as u8 as u64,
                        _ => v,
                    })
                }))
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                let start = r.at;
                scan_float(&mut r, &mut width)?;
                let text = &input[start..r.at];
                // `strtof` rounds the decimal once, as `parse` does; the hex
                // and `nan(...)` forms `parse` refuses go through `strtod`.
                if let Some(v) = (length == Length::None)
                    .then(|| std::str::from_utf8(text).ok()?.parse::<f32>().ok())
                    .flatten()
                    .filter(|v| !v.is_nan())
                {
                    return Ok(Some(Scanned::Double(v as f64)));
                }
                let d = stdlib::strtod(text);
                if d.len == 0 {
                    return Err(Stop::Conv);
                }
                Ok(Some(Scanned::Double(match length {
                    // `strtof` puts a `nan(...)` payload below its own quiet
                    // bit.
                    Length::None if d.value.is_nan() => {
                        let b = d.value.to_bits();
                        let sign = (b >> 32) as u32 & 0x8000_0000;
                        f32::from_bits(sign | 0x7fc0_0000 | b as u32 & 0x3f_ffff) as f64
                    }
                    // A hex float rounds to a double first, the same as
                    // `strtof` below 54 significant bits.
                    Length::None => d.value as f32 as f64,
                    _ => d.value,
                })))
            }
            _ => Err(Stop::Conv),
        })();
        match value {
            Err(stop) => return stopped(stop, done),
            Ok(Some(v)) if !suppress => {
                let Some(target) = targets.next() else {
                    return done;
                };
                target.store(v);
                done += c_int::from(conv != b'n');
            }
            Ok(_) => {}
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long(input: &str, fmt: &str) -> (c_int, i64, c_int) {
        let (v, n) = (Cell::new(0i64), Cell::new(-1));
        (sscanf(input, fmt, &[&v, &n]), v.get(), n.get())
    }

    fn float_bits(input: &str, fmt: &str) -> (c_int, u32, c_int) {
        let (v, n) = (Cell::new(0f32), Cell::new(-1));
        (sscanf(input, fmt, &[&v, &n]), v.get().to_bits(), n.get())
    }

    /// Measured with glibc 2.43 `sscanf` under `-D_GNU_SOURCE`: (input,
    /// format, return, value, `%n`).
    #[test]
    fn integers_take_c23_binary_like_glibc_with_gnu_source() {
        const ROWS: &[(&str, &str, c_int, i64, c_int)] = &[
            ("0b11", "%li%n", 1, 3, 4),
            ("-0b11", "%li%n", 1, -3, 5),
            ("0b", "%li%n", 0, 0, -1),
            ("0b2", "%li%n", 0, 0, -1),
            ("0x", "%li%n", 0, 0, -1),
            ("0xg", "%li%n", 0, 0, -1),
            ("08", "%li%n", 1, 0, 1),
            ("", "%li%n", -1, 0, -1),
            ("   ", "%li%n", -1, 0, -1),
            ("11", "%lb%n", 1, 3, 2),
            ("-101", "%lb%n", 1, -5, 4),
            ("0B1", "%lb%n", 1, 1, 3),
            ("+0B11", "%lb%n", 1, 3, 5),
            (" 0b101x", "%lb%n", 1, 5, 6),
            ("-0b1", "%lb%n", 1, -1, 4),
            ("2", "%lb%n", 0, 0, -1),
            ("0b", "%lb%n", 0, 0, -1),
            ("0x1", "%lb%n", 1, 0, 1),
            ("11", "%lB%n", 0, 0, -1),
            ("0b1", "%2b%n", 0, 0, -1),
            ("0b1", "%3b%n", 1, 1, 3),
            ("0b1", "%1b%n", 1, 0, 1),
            ("0b1", "%2i%n", 0, 0, -1),
            ("0x1", "%2i%n", 0, 0, -1),
            ("-0x1", "%2i%n", 1, 0, 2),
            ("-0x", "%3i%n", 0, 0, -1),
            ("0b1", "%ld%n", 1, 0, 1),
            ("0b1", "%lo%n", 1, 0, 1),
            ("0b1", "%lu%n", 1, 0, 1),
            ("0b1", "%lx%n", 1, 0xb1, 3),
            ("-8000000000000000", "%lx%n", 1, i64::MIN, 17),
            ("10000000000000000", "%lx%n", 1, -1, 17),
            ("99999999999", "%d%n", 1, 1215752191, 11),
            ("(nil)", "%p%n", 1, 0, 5),
            ("(nil)", "%3p%n", 0, 0, -1),
            ("(nil)", "%5p%n", 1, 0, 5),
        ];
        for &(input, fmt, ret, v, n) in ROWS {
            assert_eq!(long(input, fmt), (ret, v, n), "{input:?} {fmt:?}");
        }
    }

    /// `%2x%n`, StreamDevice's checksum read: glibc 2.43 refuses a bare
    /// prefix even when the width cuts it off.
    #[test]
    fn hex_width_two_matches_glibc() {
        const ROWS: &[(&str, c_int, i64, c_int)] = &[
            ("41\"", 1, 0x41, 2),
            ("4g", 1, 4, 1),
            (" 5", 1, 5, 2),
            ("-1", 1, 0xffff_ffff, 2),
            ("+5", 1, 5, 2),
            ("0x5", 0, 0, -1),
            ("0x", 0, 0, -1),
            ("-0x", 1, 0, 2),
            ("g", 0, 0, -1),
            ("-", 0, 0, -1),
        ];
        for &(input, ret, v, n) in ROWS {
            assert_eq!(long(input, "%2x%n"), (ret, v, n), "{input:?}");
        }
    }

    /// `%f` into a `float`, measured with glibc 2.43: (input, format,
    /// return, bits, `%n`).
    #[test]
    fn floats_match_glibc() {
        const ROWS: &[(&str, &str, c_int, u32, c_int)] = &[
            ("-nan(5)", "%f%n", 1, 0xffc0_0005, 7),
            ("nan(12345678901)", "%f%n", 1, 0x7fdc_1c35, 16),
            ("nan(4194305)", "%f%n", 1, 0x7fc0_0001, 12),
            ("nan(5)", "%5f%n", 0, 0, -1),
            ("nan(5)", "%6f%n", 1, 0x7fc0_0005, 6),
            ("nan(5)", "%3f%n", 1, 0x7fc0_0000, 3),
            ("nan(5)", "%4f%n", 0, 0, -1),
            ("infinity", "%3f%n", 1, 0x7f80_0000, 3),
            ("infinity", "%5f%n", 0, 0, -1),
            ("infinity", "%8f%n", 1, 0x7f80_0000, 8),
            ("infinit", "%f%n", 0, 0, -1),
            ("1e", "%f%n", 0, 0, -1),
            ("1e+", "%f%n", 0, 0, -1),
            ("0x", "%f%n", 0, 0, -1),
            ("0x.", "%f%n", 0, 0, -1),
            (".", "%f%n", 0, 0, -1),
            (".e5", "%f%n", 0, 0, -1),
            ("0x5", "%2f%n", 0, 0, -1),
            ("0x5", "%3f%n", 1, 0x40a0_0000, 3),
            ("0x1p1", "%g%n", 1, 0x4000_0000, 5),
            ("\u{0b}1", "%g%n", 1, 0x3f80_0000, 2),
            ("1E+2x", "%e%n", 1, 0x42c8_0000, 4),
        ];
        for &(input, fmt, ret, bits, n) in ROWS {
            assert_eq!(float_bits(input, fmt), (ret, bits, n), "{input:?} {fmt:?}");
        }
    }

    #[test]
    fn literals_strings_and_short_chars_match_glibc() {
        let (a, b) = (Cell::new(0i32), Cell::new(0i32));
        assert_eq!(sscanf("1 % 2", "%d %% %i", &[&a, &b]), 2);
        assert_eq!((a.get(), b.get()), (1, 2));
        assert_eq!(sscanf("1 x", "%d %% %i", &[&a, &b]), 1);
        struct Text(Cell<usize>);
        impl ScanTarget for Text {
            fn store(&self, value: Scanned<'_>) {
                if let Scanned::Chars(b) | Scanned::Str(b) = value {
                    self.0.set(b.len());
                }
            }
        }
        let t = Text(Cell::new(0));
        assert_eq!(sscanf("ab", "%3c", &[&t]), -1);
        assert_eq!(t.0.get(), 2);
        assert_eq!(sscanf("ab\0cd", "%s", &[&t]), 1);
        assert_eq!(t.0.get(), 2);
    }
}
