//! C `sscanf` as EPICS gets it from glibc 2.43 with `_GNU_SOURCE`
//! (`__isoc23_sscanf`): the one `vfscanf` every scanf-shaped parse in the
//! workspace goes through.
//!
//! `_GNU_SOURCE` selects the C23 entry point, so `%b` reads binary and `%i`
//! and `%b` take a `0b` prefix as `%i` and `%x` take `0x`.

use std::cell::{Cell, RefCell};
use std::ffi::{c_int, c_long, c_longlong, c_short};

use super::stdlib;

/// What a conversion stores: the bytes glibc writes through its pointer, of
/// the type the length modifier names whatever the object pointed to is.
pub struct Scanned<'a>(Repr<'a>);

enum Repr<'a> {
    /// A number's object representation, its first `.1` bytes.
    Num([u8; 8], usize),
    /// `%c`'s bytes, or `%s`'s and `%[`'s and a terminator after them.
    Bytes(&'a [u8], bool),
}

impl Scanned<'_> {
    /// Writes the stored bytes over `object` from its first byte, as through
    /// C's pointer; those past its end are dropped, where C writes on.
    pub fn write_over(&self, object: &mut [u8]) {
        match self.0 {
            Repr::Num(b, n) => put(object, 0, &b[..n]),
            Repr::Bytes(b, terminated) => {
                put(object, 0, b);
                if terminated {
                    put(object, b.len(), &[0]);
                }
            }
        }
    }
}

/// As much of `bytes` as fits at `object[at..]`.
fn put(object: &mut [u8], at: usize, bytes: &[u8]) {
    let Some(room) = object.get_mut(at..) else {
        return;
    };
    let n = bytes.len().min(room.len());
    room[..n].copy_from_slice(&bytes[..n]);
}

fn num(b: &[u8]) -> Scanned<'static> {
    let mut a = [0; 8];
    a[..b.len()].copy_from_slice(b);
    Scanned(Repr::Num(a, b.len()))
}

/// `v` stored as a `size`-byte C integer: its low bytes.
fn int(v: u64, size: usize) -> Scanned<'static> {
    let b = v.to_ne_bytes();
    let low = if cfg!(target_endian = "big") {
        8 - size
    } else {
        0
    };
    num(&b[low..low + size])
}

/// An `sscanf` destination: C's pointer argument.
pub trait ScanTarget {
    fn store(&self, value: Scanned<'_>);
}

/// A C number type `sscanf` stores into.
pub trait ScanNum: Copy {
    /// `self` with `value`'s bytes written over it.
    fn stored(self, value: &Scanned<'_>) -> Self;
}

macro_rules! scan_num {
    ($($t:ty),*) => {
        $(impl ScanNum for $t {
            fn stored(self, value: &Scanned<'_>) -> Self {
                let mut b = self.to_ne_bytes();
                value.write_over(&mut b);
                Self::from_ne_bytes(b)
            }
        })*
    };
}

scan_num!(i8, u8, i16, u16, i32, u32, i64, u64, f32, f64);

impl<T: ScanNum> ScanTarget for Cell<T> {
    fn store(&self, value: Scanned<'_>) {
        self.set(self.get().stored(&value));
    }
}

/// A C `char` array.
impl<const N: usize> ScanTarget for RefCell<[u8; N]> {
    fn store(&self, value: Scanned<'_>) {
        value.write_over(&mut *self.borrow_mut());
    }
}

/// The argument a C caller did not pass, which glibc stores a surplus
/// conversion through anyway: put last, it counts the conversion and drops
/// it, so `sscanf(s, "%d %d", &one)` returns 2 as glibc does.
impl ScanTarget for () {
    fn store(&self, _: Scanned<'_>) {}
}

/// The C type a length modifier names, as glibc reads it: an integer
/// conversion stores it, a float one stores a `double` for `Long` and a
/// `long double` for `LongLong`, and `c`, `s` and `[` with either store
/// `wchar_t`.
#[derive(Clone, Copy)]
enum Modifier {
    Char,
    Short,
    Int,
    Long,
    LongLong,
}

impl Modifier {
    /// `j`, `z` and `t`: the first of `int`, `long` and `long long` as wide
    /// as their type.
    fn of_size(size: usize) -> Self {
        if size > size_of::<c_long>() {
            Self::LongLong
        } else if size > size_of::<c_int>() {
            Self::Long
        } else {
            Self::Int
        }
    }

    fn size(self) -> usize {
        match self {
            Self::Char => 1,
            Self::Short => size_of::<c_short>(),
            Self::Int => size_of::<c_int>(),
            Self::Long => size_of::<c_long>(),
            Self::LongLong => size_of::<c_longlong>(),
        }
    }
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
        let modifier;
        (modifier, f) = match fmt.get(f) {
            Some(b'h') if twice(b'h') => (Modifier::Char, f + 2),
            Some(b'h') => (Modifier::Short, f + 1),
            Some(b'l') if twice(b'l') => (Modifier::LongLong, f + 2),
            Some(b'l') => (Modifier::Long, f + 1),
            Some(b'q' | b'L') => (Modifier::LongLong, f + 1),
            Some(b'j') => (Modifier::of_size(size_of::<i64>()), f + 1),
            Some(b'z') => (Modifier::of_size(size_of::<usize>()), f + 1),
            Some(b't') => (Modifier::of_size(size_of::<isize>()), f + 1),
            _ => (Modifier::Int, f),
        };
        let Some(&conv) = fmt.get(f) else {
            return done;
        };
        f += 1;
        if skip_space || !matches!(conv, b'[' | b'c' | b'n') {
            r.skip_space();
            skip_space = false;
        }
        let wide = matches!(modifier, Modifier::Long | Modifier::LongLong);
        let value: Result<Option<Scanned<'_>>, Stop> = (|| match conv {
            b'%' => match r.inchar() {
                None => Err(Stop::Input),
                Some(b'%') => Ok(None),
                c => {
                    r.ungetc(c);
                    Err(Stop::Conv)
                }
            },
            b'n' => Ok(Some(int(r.at as u64, modifier.size()))),
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
                let chars = Scanned(Repr::Bytes(&input[start..r.at], false));
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
                Ok(Some(Scanned(Repr::Bytes(&input[start..r.at], true))))
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
                Ok(Some(Scanned(Repr::Bytes(&input[start..r.at], true))))
            }
            b'd' | b'i' | b'o' | b'u' | b'x' | b'X' | b'b' | b'p' => {
                let base = match conv {
                    b'd' | b'u' => 10,
                    b'i' => 0,
                    b'o' => 8,
                    b'b' => 2,
                    _ => 16,
                };
                // A pointer is a `long`.
                let size = if conv == b'p' {
                    size_of::<c_long>()
                } else {
                    modifier.size()
                };
                let start = r.at;
                let Some(base) = scan_int(&mut r, &mut width, base, conv == b'p')? else {
                    return Ok(Some(int(0, size)));
                };
                let text = &input[start..r.at];
                let long_long = size > size_of::<c_long>();
                let v = match (matches!(conv, b'd' | b'i'), long_long) {
                    (true, true) => stdlib::strtoll(text, base).0 as u64,
                    (true, false) => i128::from(stdlib::strtol(text, base).0) as u64,
                    (false, true) => stdlib::strtoull(text, base).0,
                    (false, false) => u128::from(stdlib::strtoul(text, base).0) as u64,
                };
                Ok(Some(int(v, size)))
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                let start = r.at;
                scan_float(&mut r, &mut width)?;
                let text = &input[start..r.at];
                let float = !matches!(modifier, Modifier::Long | Modifier::LongLong);
                // `strtof` rounds the decimal once, as `parse` does; the hex
                // and `nan(...)` forms `parse` refuses go through `strtod`.
                if let Some(v) = float
                    .then(|| std::str::from_utf8(text).ok()?.parse::<f32>().ok())
                    .flatten()
                    .filter(|v| !v.is_nan())
                {
                    return Ok(Some(num(&v.to_ne_bytes())));
                }
                let d = stdlib::strtod(text);
                if d.len == 0 {
                    return Err(Stop::Conv);
                }
                Ok(Some(if !float {
                    // There is no `strtold`: a `long double` is stored as
                    // the `double`.
                    num(&d.value.to_ne_bytes())
                } else if d.value.is_nan() {
                    // `strtof` puts a `nan(...)` payload below its own quiet
                    // bit.
                    let b = d.value.to_bits();
                    let sign = (b >> 32) as u32 & 0x8000_0000;
                    num(&f32::from_bits(sign | 0x7fc0_0000 | b as u32 & 0x3f_ffff).to_ne_bytes())
                } else {
                    // A hex float rounds to a double and then to a float,
                    // where `strtof` rounds once: the two differ when the
                    // double lands on a float tie, so `0x1.fffffefffffffffp0`
                    // is 2 here and `0x1.fffffep0` in C.
                    num(&(d.value as f32).to_ne_bytes())
                }))
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
        let t = RefCell::new([b'x'; 4]);
        assert_eq!(sscanf("ab", "%3c", &[&t]), -1);
        assert_eq!(&*t.borrow(), b"abxx");
        assert_eq!(sscanf("cd\0ef", "%s", &[&t]), 1);
        assert_eq!(&*t.borrow(), b"cd\0x");
    }

    /// What `sscanf(input, fmt, p)` returns and writes at `p`, as the glibc
    /// probe prints it: a byte is written iff it comes out the same over a
    /// 0xaa and a 0x55 fill, and an unwritten one before the last written
    /// prints as `..`.
    fn stores(input: &[u8], fmt: &str) -> (c_int, String) {
        let run = |fill| {
            let p = RefCell::new([fill; 24]);
            (sscanf(input, fmt, &[&p]), p.into_inner())
        };
        let ((n, a), (m, b)) = (run(0xaa), run(0x55));
        assert_eq!(n, m, "{fmt:?}");
        let end = (0..a.len())
            .rev()
            .find(|&i| a[i] == b[i])
            .map_or(0, |i| i + 1);
        let bytes = (0..end)
            .map(|i| match a[i] == b[i] {
                true => format!("{:02x}", a[i]),
                false => "..".into(),
            })
            .collect();
        (n, bytes)
    }

    fn check_stores(rows: &[(&str, &[u8], c_int, &str)]) {
        let failed: Vec<String> = rows
            .iter()
            .filter_map(|&(fmt, input, n, bytes)| {
                let got = stores(input, fmt);
                (got != (n, bytes.into())).then(|| {
                    format!(
                        "{fmt} \"{}\": C {n} {bytes}, rs {got:?}",
                        input.escape_ascii()
                    )
                })
            })
            .collect();
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }

    /// A conversion stores the type its length modifier names, whatever the
    /// object: (format, input, return, bytes), measured with glibc 2.43 on
    /// x86_64.
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    #[test]
    fn a_conversion_stores_its_own_width() {
        check_stores(&[
            ("%hhd", b"200", 1, "c8"),
            ("%hhd", b"128", 1, "80"),
            ("%hhd", b"-1", 1, "ff"),
            ("%hhd", b"300", 1, "2c"),
            ("%hhd", b"-129", 1, "7f"),
            ("%hhi", b"0x80", 1, "80"),
            ("%hhi", b"0b11111111", 1, "ff"),
            ("%hhu", b"255", 1, "ff"),
            ("%hhu", b"256", 1, "00"),
            ("%hhu", b"-1", 1, "ff"),
            ("%hhx", b"1ff", 1, "ff"),
            ("%hho", b"777", 1, "ff"),
            ("%hhb", b"111111111", 1, "ff"),
            ("%hd", b"70000", 1, "7011"),
            ("%hu", b"-1", 1, "ffff"),
            ("%d", b"99999999999", 1, "ffe77648"),
            ("%u", b"-1", 1, "ffffffff"),
            ("%ld", b"-2", 1, "feffffffffffffff"),
            ("%lld", b"-2", 1, "feffffffffffffff"),
            ("%jd", b"-2", 1, "feffffffffffffff"),
            ("%zd", b"-2", 1, "feffffffffffffff"),
            ("%td", b"-2", 1, "feffffffffffffff"),
            ("%qd", b"-2", 1, "feffffffffffffff"),
            ("%Ld", b"-2", 1, "feffffffffffffff"),
            ("abc%hhn", b"abc", 0, "03"),
            ("abc%hn", b"abc", 0, "0300"),
            ("abc%n", b"abc", 0, "03000000"),
            ("abc%ln", b"abc", 0, "0300000000000000"),
            ("abc%lln", b"abc", 0, "0300000000000000"),
            ("abc%jn", b"abc", 0, "0300000000000000"),
            ("abc%zn", b"abc", 0, "0300000000000000"),
            ("%p", b"0x10", 1, "1000000000000000"),
            ("%p", b"(nil)", 1, "0000000000000000"),
            ("%f", b"1.5", 1, "0000c03f"),
            ("%hf", b"1.5", 1, "0000c03f"),
            ("%hhf", b"1.5", 1, "0000c03f"),
            ("%lf", b"1.5", 1, "000000000000f83f"),
            ("%jf", b"1.5", 1, "000000000000f83f"),
            ("%zf", b"1.5", 1, "000000000000f83f"),
            ("%tf", b"1.5", 1, "000000000000f83f"),
            ("%f", b"-nan(5)", 1, "0500c0ff"),
            ("%lf", b"-nan(5)", 1, "050000000000f8ff"),
            ("%c", b"abc", 1, "61"),
            ("%3c", b"abc", 1, "616263"),
            ("%3c", b"ab", -1, "6162"),
            ("%s", b"abc", 1, "61626300"),
            ("%3s", b"abcdef", 1, "61626300"),
            ("%[a-z]", b"abc1", 1, "61626300"),
            ("%*s", b"abc", 0, ""),
            ("%hlc", b"a", 0, ""),
            ("%s", b"a\xe9b", 1, "61e96200"),
            ("%c", b"\xe9", 1, "e9"),
            ("%[^,]", b"a\xe9b,", 1, "61e96200"),
        ]);
    }
}
