//! The C library as SNL code calls it. A companion brings it into scope
//! with `use epics_seq_rs::c::*;`, and an item of its own with the same
//! name shadows the one here.
//!
//! snc-rs passes an argument by C's own rule, without a prototype: an
//! array (decayed to a pointer) or `&x` is the place itself (`&seqg_var.x`,
//! `&seqg_var.a.at(i)`); anything else is its value (`x.get()`), `as _`
//! when numeric. So a string argument is a [`CSrc`] that may be a
//! variable, and a destination is a `&impl Place<B>`. A shim copies each
//! source variable out before it takes another lock, so no call holds two
//! variables' locks at once, and `strcpy(x, x)` does not deadlock.
//!
//! Where C writes past the end of the destination, the copy stops one
//! byte short of it and keeps the terminator.

use std::borrow::Cow;
use std::ffi::{c_char, c_int};
use std::io;
use std::slice;
use std::sync::Arc;

use epics_base_rs::runtime::{crt_errno, stdio, stdlib};
use epics_libcom_rs::cfmt::{self, Directive, Length, Spec};

use crate::types::{CharPtr, SeqString};
use crate::var::{At, Place, SeqVar};

/// A variadic argument of `printf` / `sprintf`, after C's default argument
/// promotions.
#[derive(Debug, Clone)]
pub enum CArg<'a> {
    Int(i64),
    UInt(u64),
    Double(f64),
    /// `%s`; `None` is the null pointer.
    Str(Option<Cow<'a, [u8]>>),
}

macro_rules! carg_from {
    ($($t:ty => $v:ident as $w:ty),* $(,)?) => {
        $(impl From<$t> for CArg<'_> {
            fn from(v: $t) -> Self {
                CArg::$v(v as $w)
            }
        })*
    };
}

carg_from!(
    i8 => Int as i64, i16 => Int as i64, i32 => Int as i64, i64 => Int as i64,
    u8 => UInt as u64, u16 => UInt as u64, u32 => UInt as u64, u64 => UInt as u64,
    f32 => Double as f64, f64 => Double as f64,
);

impl From<bool> for CArg<'_> {
    fn from(v: bool) -> Self {
        CArg::Int(v.into())
    }
}

/// A string literal (`&b"..."[..]` when it is not UTF-8) borrows.
impl<'a> From<&'a str> for CArg<'a> {
    fn from(v: &'a str) -> Self {
        CArg::Str(Some(Cow::Borrowed(before_nul(v.as_bytes()))))
    }
}

impl<'a> From<&'a [u8]> for CArg<'a> {
    fn from(v: &'a [u8]) -> Self {
        CArg::Str(Some(Cow::Borrowed(before_nul(v))))
    }
}

impl From<CharPtr> for CArg<'_> {
    fn from(v: CharPtr) -> Self {
        CArg::Str(v.map(|s| Cow::Owned(before_nul(&s).to_vec())))
    }
}

/// A string variable or element is copied out under its lock.
impl<B: CSrc> From<&SeqVar<B>> for CArg<'_> {
    fn from(v: &SeqVar<B>) -> Self {
        CArg::Str(Some(Cow::Owned(v.with_c_bytes(<[u8]>::to_vec))))
    }
}

impl<B: CSrc, const N: usize, P: Place<[B; N]> + ?Sized> From<&At<'_, P, [B; N]>> for CArg<'_> {
    fn from(v: &At<'_, P, [B; N]>) -> Self {
        CArg::Str(Some(Cow::Owned(v.with_c_bytes(<[u8]>::to_vec))))
    }
}

/// A C string read as a source: the bytes before the first NUL.
pub trait CSrc {
    /// Run `f` on the bytes, under the lock when `self` is a variable; `f`
    /// must not touch another program variable.
    fn with_c_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R;

    /// Whether `self` is C's null pointer, which only a [`CharPtr`] can be:
    /// a callee that tests `!s` as C asks this before reading the bytes
    /// (`seq_pvAssign`'s `if (!pvName) pvName = "";`, `seq_if.c:684`).
    fn is_null(&self) -> bool {
        false
    }

    /// The bytes, copied out of any lock.
    fn to_c_vec(&self) -> Vec<u8> {
        self.with_c_bytes(<[u8]>::to_vec)
    }
}

fn before_nul(b: &[u8]) -> &[u8] {
    &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]
}

impl CSrc for str {
    fn with_c_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        f(before_nul(self.as_bytes()))
    }
}

impl CSrc for [u8] {
    fn with_c_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        f(before_nul(self))
    }
}

impl CSrc for SeqString {
    fn with_c_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        f(self.as_bytes())
    }
}

impl CSrc for Arc<[u8]> {
    fn with_c_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        f(before_nul(self))
    }
}

/// The null pointer panics where C would dereference it.
impl CSrc for CharPtr {
    fn with_c_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        match self {
            Some(s) => f(before_nul(s)),
            None => panic!("null pointer passed as a C string"),
        }
    }

    fn is_null(&self) -> bool {
        self.is_none()
    }
}

impl<const N: usize> CSrc for [c_char; N] {
    fn with_c_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        // SAFETY: `c_char` is `i8` or `u8`, the same size and alignment as
        // `u8`, and every bit pattern is a valid `u8`.
        let bytes = unsafe { std::slice::from_raw_parts(self.as_ptr().cast::<u8>(), N) };
        f(before_nul(bytes))
    }
}

impl<T: CSrc + ?Sized> CSrc for &T {
    fn with_c_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        (**self).with_c_bytes(f)
    }

    fn is_null(&self) -> bool {
        (**self).is_null()
    }
}

impl<B: CSrc> CSrc for SeqVar<B> {
    fn with_c_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        self.modify(|b| b.with_c_bytes(f))
    }

    fn is_null(&self) -> bool {
        self.modify(|b| b.is_null())
    }
}

impl<B: CSrc, const N: usize, P: Place<[B; N]> + ?Sized> CSrc for At<'_, P, [B; N]> {
    fn with_c_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        self.modify(|b| b.with_c_bytes(f))
    }

    fn is_null(&self) -> bool {
        self.modify(|b| b.is_null())
    }
}

/// A C character buffer written as a destination.
pub trait CBuf {
    fn bytes_mut(&mut self) -> &mut [u8];
}

impl CBuf for SeqString {
    fn bytes_mut(&mut self) -> &mut [u8] {
        self.as_buffer_mut()
    }
}

impl<const N: usize> CBuf for [c_char; N] {
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `CSrc for [c_char; N]`; the slice borrows `self`
        // mutably for its whole life.
        unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr().cast::<u8>(), N) }
    }
}

/// Copy `src` and a terminator to `buf[at..]`, bounded by the buffer.
fn put_terminated(buf: &mut [u8], at: usize, src: &[u8]) {
    let Some(room) = buf.len().checked_sub(at + 1) else {
        return;
    };
    let n = src.len().min(room);
    buf[at..at + n].copy_from_slice(&src[..n]);
    buf[at + n] = 0;
}

/// `strcpy`: the bytes after the terminator keep what they held.
pub fn strcpy<B: CBuf>(dst: &impl Place<B>, src: impl CSrc) {
    let src = src.to_c_vec();
    dst.modify(|b| put_terminated(b.bytes_mut(), 0, &src))
}

/// `strncpy`: at most `n` bytes, NUL-padded to `n`, unterminated when
/// `src` has `n` or more.
pub fn strncpy<B: CBuf>(dst: &impl Place<B>, src: impl CSrc, n: usize) {
    let s = src.to_c_vec();
    dst.modify(|b| {
        let buf = b.bytes_mut();
        let n = n.min(buf.len());
        let k = s.len().min(n);
        buf[..k].copy_from_slice(&s[..k]);
        buf[k..n].fill(0);
    })
}

/// `strcat`.
pub fn strcat<B: CBuf>(dst: &impl Place<B>, src: impl CSrc) {
    let src = src.to_c_vec();
    dst.modify(|b| {
        let buf = b.bytes_mut();
        let end = before_nul(buf).len();
        put_terminated(buf, end, &src)
    })
}

/// `strcmp`: the difference of the first differing bytes, as unsigned
/// char.
pub fn strcmp(a: impl CSrc, b: impl CSrc) -> c_int {
    let a = a.to_c_vec();
    b.with_c_bytes(|b| {
        let n = a.len().min(b.len());
        match a[..n].iter().zip(&b[..n]).find(|(x, y)| x != y) {
            Some((x, y)) => c_int::from(*x) - c_int::from(*y),
            None => c_int::from(*a.get(n).unwrap_or(&0)) - c_int::from(*b.get(n).unwrap_or(&0)),
        }
    })
}

/// `strlen`.
pub fn strlen(s: impl CSrc) -> usize {
    s.with_c_bytes(<[u8]>::len)
}

impl CArg<'_> {
    /// The argument as C reads an integer-class one; a pointer is its
    /// address.
    fn int(&self) -> Option<i64> {
        match self {
            CArg::Int(v) => Some(*v),
            CArg::UInt(v) => Some(*v as i64),
            CArg::Double(_) => None,
            CArg::Str(s) => Some(s.as_ref().map_or(0, |s| s.as_ptr() as i64)),
        }
    }
}

/// Why a directive printed nothing of its own.
enum Refused {
    /// An argument is missing, or of a class C would misread: the directive
    /// is printed back as written.
    Verbatim,
    /// glibc's `EILSEQ`: the call returns -1.
    Error,
}

/// One directive of `printf`, its arguments taken from `args`.
fn convert(
    out: &mut Vec<u8>,
    d: &Directive,
    args: &mut slice::Iter<'_, CArg<'_>>,
    errno: &io::Error,
) -> Result<(), Refused> {
    use Refused::*;
    let mut misread = false;
    let s = d.spec(|| match args.next().and_then(CArg::int) {
        Some(v) => v as c_int,
        None => {
            misread = true;
            0
        }
    });
    let takes_arg = b"diouxXcCsSpnfFeEgGaA".contains(&s.conv);
    let arg = takes_arg.then(|| args.next());
    if misread {
        return Err(Verbatim);
    }
    let arg = || arg.flatten().ok_or(Verbatim);
    let int = || arg()?.int().ok_or(Verbatim);
    // glibc reads `%c` and `%s` as wide under every length but `h`, `hh`.
    let wide =
        matches!(s.conv, b'C' | b'S') || !matches!(d.length, Length::None | Length::H | Length::Hh);
    let narrow = Spec {
        conv: s.conv.to_ascii_lowercase(),
        ..s
    };
    match s.conv {
        b'%' => out.push(b'%'),
        b'm' => {
            let text = crt_errno::strerror(errno);
            cfmt::print_str(out, &Spec { conv: b's', ..s }, text.as_bytes());
        }
        b'n' => {
            arg()?;
        }
        b'd' | b'i' => {
            let v = match d.length {
                Length::None => int()? as c_int as i64,
                Length::Hh => int()? as i8 as i64,
                Length::H => int()? as i16 as i64,
                _ => int()?,
            };
            cfmt::print_int(out, &s, v);
        }
        b'o' | b'u' | b'x' | b'X' => {
            let v = match d.length {
                Length::None => int()? as u32 as i64,
                Length::Hh => int()? as u8 as i64,
                Length::H => int()? as u16 as i64,
                _ => int()?,
            };
            cfmt::print_int(out, &s, v);
        }
        b'c' | b'C' => {
            let v = int()?;
            // A wide character converts through the C locale, ASCII only.
            if wide && !(0..0x80).contains(&(v as u32)) {
                return Err(Error);
            }
            cfmt::print_char(out, &narrow, v as u8);
        }
        b's' | b'S' => match arg()? {
            CArg::Str(Some(_)) if wide => return Err(Verbatim),
            CArg::Str(v) => {
                cfmt::print_c_str(out, &narrow, v.as_deref());
            }
            CArg::Int(0) | CArg::UInt(0) => {
                cfmt::print_c_str(out, &narrow, None);
            }
            _ => return Err(Verbatim),
        },
        b'p' => cfmt::print_pointer(out, &s, int()? as usize),
        _ if takes_arg => match arg()? {
            // `ll`, `q` and `L` make it a `long double`.
            CArg::Double(v) if !matches!(d.length, Length::Ll | Length::BigL) => {
                cfmt::print_double(out, &s, *v);
            }
            _ => return Err(Verbatim),
        },
        _ => cfmt::print_unknown(out, &s),
    }
    Ok(())
}

/// `fmt` formatted with `args` as glibc's printf formats it, and what C
/// returns: its length, or -1 with the text up to the directive that
/// failed.
///
/// glibc's undefined behaviour is not copied: a directive whose argument is
/// missing or of a class C would misread (a double under `%d`, an integer
/// under `%f`, a nonzero one under `%s`) is printed back as written; it
/// takes the argument all the same. Positional `%n$` is not supported and
/// prints back the same way. `%n` stores nothing.
fn format(fmt: &[u8], args: &[CArg<'_>], errno: &io::Error) -> (Vec<u8>, c_int) {
    let mut out = Vec::new();
    let mut args = args.iter();
    let mut i = 0;
    while let Some(n) = fmt[i..].iter().position(|&c| c == b'%') {
        out.extend_from_slice(&fmt[i..i + n]);
        let d = Directive::parse(fmt, i + n);
        if d.base.conv == 0 {
            return (out, -1);
        }
        match convert(&mut out, &d, &mut args, errno) {
            Ok(()) => {}
            Err(Refused::Verbatim) => out.extend_from_slice(&fmt[i + n..d.end]),
            Err(Refused::Error) => return (out, -1),
        }
        i = d.end;
    }
    out.extend_from_slice(&fmt[i..]);
    let len = c_int::try_from(out.len()).unwrap_or(-1);
    (out, len)
}

/// `printf`, through the IOC's stdout.
pub fn printf(fmt: impl CSrc, args: &[CArg<'_>]) -> c_int {
    let errno = io::Error::last_os_error();
    let (text, len) = format(&fmt.to_c_vec(), args, &errno);
    stdio::stdout_bytes(&text);
    len
}

/// `sprintf`, bounded by the destination; the length returned is C's, the
/// text's whole length.
pub fn sprintf<B: CBuf>(dst: &impl Place<B>, fmt: impl CSrc, args: &[CArg<'_>]) -> c_int {
    let errno = io::Error::last_os_error();
    let (text, len) = format(&fmt.to_c_vec(), args, &errno);
    dst.modify(|b| put_terminated(b.bytes_mut(), 0, &text));
    len
}

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

/// A variable type `sscanf` stores into. A number converts to it; a string
/// goes to a string, bounded by it, and `%c`'s bytes to a string or, the
/// first of them, to a number. Any other pairing leaves the variable as it
/// was, where C writes the value's bytes over it.
pub trait ScanStore {
    fn store(&mut self, value: Scanned<'_>);
}

macro_rules! scan_store_num {
    ($($t:ty),*) => {
        $(impl ScanStore for $t {
            fn store(&mut self, value: Scanned<'_>) {
                *self = match value {
                    Scanned::Int(v) => v as $t,
                    Scanned::UInt(v) => v as $t,
                    Scanned::Double(v) => v as $t,
                    Scanned::Chars(b) => b[0] as $t,
                    Scanned::Str(_) => return,
                };
            }
        })*
    };
}

scan_store_num!(i8, u8, i16, u16, i32, u32, i64, u64, f32, f64);

fn scan_store_bytes(buf: &mut [u8], value: Scanned<'_>) {
    match value {
        Scanned::Str(b) => put_terminated(buf, 0, b),
        Scanned::Chars(b) => {
            let n = b.len().min(buf.len());
            buf[..n].copy_from_slice(&b[..n]);
        }
        _ => {}
    }
}

impl ScanStore for SeqString {
    fn store(&mut self, value: Scanned<'_>) {
        scan_store_bytes(self.bytes_mut(), value)
    }
}

impl<const N: usize> ScanStore for [c_char; N] {
    fn store(&mut self, value: Scanned<'_>) {
        scan_store_bytes(self.bytes_mut(), value)
    }
}

/// An `sscanf` destination: C's pointer argument.
pub trait ScanTarget {
    fn store(&self, value: Scanned<'_>);
}

impl<T: ScanStore> ScanTarget for SeqVar<T> {
    fn store(&self, value: Scanned<'_>) {
        self.modify(|v| v.store(value))
    }
}

impl<T: ScanStore, const N: usize, P: Place<[T; N]> + ?Sized> ScanTarget for At<'_, P, [T; N]> {
    fn store(&self, value: Scanned<'_>) {
        self.modify(|v| v.store(value))
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

/// The `[sign][0[x]]digits` an integer conversion reads, as glibc's
/// `vfscanf` collects them for `strtol`; `base` 0 is `%i`'s.
fn scan_int(
    r: &mut Input<'_>,
    width: &mut i64,
    mut base: u32,
    pointer: bool,
) -> Result<(Vec<u8>, u32), Stop> {
    let mut buf = Vec::new();
    let dec = |w: &mut i64| *w -= i64::from(*w > 0);
    let mut c = r.inchar();
    if c.is_none() {
        return Err(Stop::Input);
    }
    if let Some(sign @ (b'-' | b'+')) = c {
        buf.push(sign);
        dec(width);
        c = r.inchar();
    }
    if *width != 0 && c == Some(b'0') {
        dec(width);
        buf.push(b'0');
        c = r.inchar();
        if *width != 0 && lower(c) == Some(b'x') {
            if base == 0 {
                base = 16;
            }
            if base == 16 {
                dec(width);
                c = r.inchar();
            }
        } else if base == 0 {
            base = 8;
        }
    }
    if base == 0 {
        base = 10;
    }
    while let Some(d) = c.filter(|_| *width != 0) {
        let digit = if base == 16 {
            d.is_ascii_hexdigit()
        } else {
            d.is_ascii_digit() && u32::from(d - b'0') < base
        };
        if !digit {
            break;
        }
        buf.push(d);
        dec(width);
        c = r.inchar();
    }
    if buf.is_empty() || buf == b"+" || buf == b"-" {
        // `%p` reads "(nil)" as 0.
        if pointer
            && buf.is_empty()
            && (*width < 0 || *width >= 5)
            && c == Some(b'(')
            && b"nil)".iter().all(|&want| {
                c = r.inchar();
                lower(c) == Some(want)
            })
        {
            buf.push(b'0');
        } else {
            r.ungetc(c);
            return Err(Stop::Conv);
        }
    } else {
        r.ungetc(c);
    }
    Ok((buf, base))
}

/// `c` and then each letter of `word` into `buf`, as glibc reads `nan` and
/// `inf`: a letter that differs is a conversion error.
fn spell(
    r: &mut Input<'_>,
    c: &mut Option<u8>,
    buf: &mut Vec<u8>,
    width: &mut i64,
    word: &[u8],
) -> Result<(), Stop> {
    for &want in word {
        buf.push(c.unwrap());
        if *width == 0 {
            return Err(Stop::Conv);
        }
        *c = r.inchar();
        if lower(*c) != Some(want) {
            return Err(Stop::Conv);
        }
        *width -= i64::from(*width > 0);
    }
    buf.push(c.unwrap());
    Ok(())
}

/// The text a float conversion reads, as glibc's `vfscanf` collects it for
/// `strtod`: a sign, then `nan`, `inf[inity]`, or a decimal or `0x` hex
/// number in the C locale.
fn scan_float(r: &mut Input<'_>, width: &mut i64) -> Result<Vec<u8>, Stop> {
    let mut buf = Vec::new();
    let dec = |w: &mut i64| *w -= i64::from(*w > 0);
    let mut c = r.inchar();
    dec(width);
    if c.is_none() {
        return Err(Stop::Input);
    }
    let mut got_sign = false;
    if let Some(sign @ (b'-' | b'+')) = c {
        got_sign = true;
        buf.push(sign);
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
        spell(r, &mut c, &mut buf, width, b"an")?;
        return Ok(buf);
    }
    if lower(c) == Some(b'i') {
        spell(r, &mut c, &mut buf, width, b"nf")?;
        if *width != 0 {
            c = r.inchar();
            if lower(c) == Some(b'i') {
                dec(width);
                spell(r, &mut c, &mut buf, width, b"nity")?;
            } else {
                r.ungetc(c);
            }
        }
        return Ok(buf);
    }
    let mut exp_char = b'e';
    let (mut hex, mut got_digit, mut got_dot, mut got_e) = (false, false, false, false);
    if *width != 0 && c == Some(b'0') {
        buf.push(b'0');
        c = r.inchar();
        dec(width);
        if *width != 0 && lower(c) == Some(b'x') {
            buf.push(c.unwrap());
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
                buf.push(d);
                got_digit = true;
            }
            Some(s @ (b'-' | b'+')) if got_e && buf.last() == Some(&exp_char) => buf.push(s),
            Some(e) if got_digit && !got_e && e.to_ascii_lowercase() == exp_char => {
                buf.push(exp_char);
                got_e = true;
                got_dot = true;
            }
            Some(b'.') if !got_dot => {
                buf.push(b'.');
                got_dot = true;
            }
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
    if buf.len() == usize::from(got_sign) || hex && buf.len() == 2 + usize::from(got_sign) {
        return Err(Stop::Conv);
    }
    Ok(buf)
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

/// glibc's `vfscanf` (`__isoc99_sscanf`) reading `input`; the number of
/// conversions stored, or -1 when the input ends before the first.
fn scan(input: &[u8], fmt: &[u8], out: &[&dyn ScanTarget]) -> c_int {
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
                Ok(Some(Scanned::Chars(&input[start..r.at])))
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
            b'd' | b'i' | b'o' | b'u' | b'x' | b'X' | b'p' => {
                let base = match conv {
                    b'd' | b'u' => 10,
                    b'i' => 0,
                    b'o' => 8,
                    _ => 16,
                };
                let (buf, base) = scan_int(&mut r, &mut width, base, conv == b'p')?;
                let length = if conv == b'p' { Length::L } else { length };
                Ok(Some(if matches!(conv, b'd' | b'i') {
                    let (v, n) = match length {
                        Length::Ll | Length::J => stdlib::strtoll(&buf, base),
                        _ => {
                            let (v, n) = stdlib::strtol(&buf, base);
                            (i128::from(v) as i64, n)
                        }
                    };
                    if n == 0 {
                        return Err(Stop::Conv);
                    }
                    Scanned::Int(narrow_int(v, length))
                } else {
                    let (v, n) = match length {
                        Length::Ll | Length::J => stdlib::strtoull(&buf, base),
                        _ => {
                            let (v, n) = stdlib::strtoul(&buf, base);
                            (u128::from(v) as u64, n)
                        }
                    };
                    if n == 0 {
                        return Err(Stop::Conv);
                    }
                    Scanned::UInt(match length {
                        Length::None => v as u32 as u64,
                        Length::H => v as u16 as u64,
                        Length::Hh => v as u8 as u64,
                        _ => v,
                    })
                }))
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                let buf = scan_float(&mut r, &mut width)?;
                let d = stdlib::strtod(&buf);
                if d.len == 0 {
                    return Err(Stop::Conv);
                }
                Ok(Some(Scanned::Double(match length {
                    // `strtof` rounds the decimal once; a hex float rounds
                    // to a double first, the same below 54 significant bits.
                    Length::None => std::str::from_utf8(&buf[..d.len])
                        .ok()
                        .and_then(|t| t.parse::<f32>().ok())
                        .unwrap_or(d.value as f32) as f64,
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

/// `sscanf` as glibc's C99 one reads a string: the number of conversions
/// stored, -1 when the input ends before the first.
///
/// The destinations take the values in turn, each as [`ScanStore`] converts
/// it. A conversion with no destination left, `%m` allocation, positional
/// `%n$`, and a wide `%lc`, `%ls` or `%l[` end the scan as a mismatch does.
pub fn sscanf(src: impl CSrc, fmt: impl CSrc, out: &[&dyn ScanTarget]) -> c_int {
    scan(&src.to_c_vec(), &fmt.to_c_vec(), out)
}

/// `atof`: the longest prefix `strtod` takes, 0 if none.
pub fn atof(s: impl CSrc) -> f64 {
    s.with_c_bytes(|b| stdlib::strtod(b).value)
}

/// `atoi`: glibc's `(int) strtol(s, NULL, 10)`.
pub fn atoi(s: impl CSrc) -> c_int {
    s.with_c_bytes(|b| stdlib::atoi(b))
}

/// `epicsThreadSleep`; `secs` in seconds.
#[allow(non_snake_case)]
pub fn epicsThreadSleep(secs: f64) {
    epics_base_rs::runtime::time::sleep_secs(secs)
}

macro_rules! math1 {
    ($($c:ident => $m:ident),* $(,)?) => {
        $(#[doc = concat!("`", stringify!($c), "`.")]
        pub fn $c(x: f64) -> f64 {
            x.$m()
        })*
    };
}

math1!(
    fabs => abs, sin => sin, cos => cos, tan => tan, asin => asin, acos => acos,
    atan => atan, sqrt => sqrt, floor => floor, ceil => ceil, exp => exp,
    log => ln, log10 => log10,
);

/// `atan2`.
pub fn atan2(y: f64, x: f64) -> f64 {
    y.atan2(x)
}

/// `pow`.
pub fn pow(x: f64, y: f64) -> f64 {
    x.powf(y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::var::Zeroed;

    fn text<const N: usize>(v: &SeqVar<[c_char; N]>) -> Vec<u8> {
        v.to_c_vec()
    }

    #[test]
    fn strcpy_leaves_the_bytes_past_the_terminator() {
        let v = <SeqVar<[c_char; 8]>>::zeroed();
        strcpy(&v, "abcdef");
        strcpy(&v, "xy");
        let raw = v.get().map(|c| c as u8);
        assert_eq!(&raw, b"xy\0def\0\0");
    }

    #[test]
    fn strcpy_into_a_too_small_buffer_keeps_its_terminator() {
        let v = <SeqVar<[c_char; 4]>>::zeroed();
        strcpy(&v, "abcdef");
        assert_eq!(text(&v), b"abc");
    }

    #[test]
    fn strcat_appends_at_the_terminator_and_stops_at_the_end() {
        let v = <SeqVar<SeqString>>::zeroed();
        strcpy(&v, "xxx:m1");
        strcat(&v, ".RBV");
        assert_eq!(v.get().as_bytes(), b"xxx:m1.RBV");
        strcat(&v, [b'y'; 64].as_slice());
        assert_eq!(v.get().as_bytes().len(), 39);
    }

    #[test]
    fn strncpy_pads_short_and_leaves_long_unterminated() {
        let v = <SeqVar<[c_char; 6]>>::zeroed();
        strcpy(&v, "zzzzz");
        strncpy(&v, "ab", 4);
        assert_eq!(v.get().map(|c| c as u8), *b"ab\0\0z\0");
        strncpy(&v, "123456", 6);
        assert_eq!(v.get().map(|c| c as u8), *b"123456");
    }

    #[test]
    fn strcmp_orders_as_unsigned_char() {
        assert_eq!(strcmp("abc", "abc"), 0);
        assert!(strcmp("ab", "abc") < 0);
        assert!(strcmp("\u{e9}", "z") > 0);
    }

    #[test]
    fn a_variable_copies_into_itself_and_its_neighbours() {
        let v = <SeqVar<[SeqString; 2]>>::zeroed();
        let w = <SeqVar<SeqString>>::zeroed();
        strcpy(&w, "m1");
        strcat(&w, &w);
        assert_eq!(w.to_c_vec(), b"m1m1");
        strcpy(&v.at(0), &w);
        strcpy(&v.at(1), v.at(0));
        assert_eq!(strcmp(v.at(1), &w), 0);
        assert_eq!(strlen(v.at(1)), 4);
        let CArg::Str(Some(s)) = CArg::from(&v.at(1)) else {
            panic!("a string")
        };
        assert_eq!(&*s, b"m1m1");
    }

    /// `format` against glibc's `snprintf`, each argument given as C passes
    /// it and as the port does.
    #[cfg(all(unix, target_env = "gnu"))]
    macro_rules! same_as_glibc {
        ($errno:expr, $fmt:expr $(, $c:expr => $rs:expr)* $(,)?) => {{
            let fmt = std::ffi::CString::new($fmt).unwrap();
            let mut buf = vec![0u8; 512];
            // SAFETY: errno is this thread's; each conversion in `fmt` takes
            // the argument passed for it, and `buf` is writable for its
            // whole length, the size passed.
            let n = unsafe {
                *libc::__errno_location() = $errno;
                libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), fmt.as_ptr() $(, $c)*)
            };
            let len = buf.iter().position(|&b| b == 0).unwrap();
            let want = if n < 0 { &buf[..len] } else { &buf[..n as usize] };
            let errno = io::Error::from_raw_os_error($errno);
            let got = format($fmt.as_bytes(), &[$(CArg::from($rs)),*], &errno);
            assert_eq!((&got.0[..], got.1), (want, n), "{}", $fmt);
        }};
    }

    #[cfg(all(unix, target_env = "gnu"))]
    #[test]
    fn printf_matches_glibc() {
        use std::ffi::c_void;
        let s = std::ffi::CString::new("ab").unwrap();
        let null = std::ptr::null::<c_char>();
        same_as_glibc!(0, "%s %d %% %g %f|", s.as_ptr() => "ab", 42 => 42, 1.5 => 1.5, 2.25 => 2.25);
        same_as_glibc!(0, "%*s|%-*d|%.*f", 5 => 5, s.as_ptr() => "ab", -4 => -4, 7 => 7, -1 => -1, 0.5 => 0.5);
        same_as_glibc!(
            0,
            "%.3e %8.4f %c %.4e %e %u %.3f %1.1d %02d %x %ld %i %.4f %3d %1d",
            1234.5 => 1234.5, -0.25 => -0.25, b'q' as c_int => b'q', 1e-7 => 1e-7, 6.0 => 6.0,
            3000000000u32 => 3000000000u32, 2.0f64 / 3.0 => 2.0f64 / 3.0, 0 => 0, 5 => 5,
            255 => 255, -9i64 => -9i64, -3 => -3, 0.1 => 0.1, 7 => 7, 1 => 1,
        );
        // C narrows an int argument to the length modifier.
        same_as_glibc!(0, "%hhd %hd %hu %d %hhx|", 300 => 300, 70000 => 70000, -1 => -1,
            5 => 0x1_0000_0005i64, -1 => -1);
        same_as_glibc!(0, "%s|%.3s|%8s|%ls", null => CArg::Str(None), null => CArg::Str(None),
            null => 0, std::ptr::null::<libc::wchar_t>() => CArg::Str(None));
        same_as_glibc!(0, "%5%|%-08%|%*y|%-+08.3hy", 4 => 4);
        same_as_glibc!(libc::ENOENT, "[%-30m|%.5m]");
        same_as_glibc!(0, "%p|%-8p|%08p|%+p", std::ptr::null::<c_void>() => 0,
            0x1234 as *const c_void => 0x1234, 0x1234 as *const c_void => 0x1234u32,
            0x1234 as *const c_void => 0x1234);
        same_as_glibc!(0, "%lc|%5C|%lc|%hc", b'A' as std::ffi::c_uint => b'A', b'z' as std::ffi::c_uint => b'z',
            0 as std::ffi::c_uint => 0, 0x141 => 0x141);
        let mut count: c_int = -1;
        same_as_glibc!(0, "x%n%d", &raw mut count => 0, 7 => 7);
        assert_eq!(count, 1);
        // glibc's -1, with the text up to the failing directive.
        same_as_glibc!(0, "ab%-5");
        same_as_glibc!(0, "ab%d%", 3 => 3);
        same_as_glibc!(0, "a%lc|", 0xe9 as std::ffi::c_uint => 0xe9);
    }

    #[test]
    fn a_misread_or_missing_argument_prints_the_directive_back() {
        let errno = io::Error::from_raw_os_error(0);
        let cases: [(&str, Vec<CArg<'_>>, &str); 7] = [
            (
                "%d|%f|%s|",
                vec![1.5.into(), 2.into(), 5.into()],
                "%d|%f|%s|",
            ),
            ("%5.1f %d", vec![1.0.into()], "  1.0 %d"),
            ("%*d|%s", vec![1.5.into(), 3.into(), "x".into()], "%*d|x"),
            (
                "%ls|%Lf|%llf",
                vec!["ab".into(), 1.0.into(), 1.0.into()],
                "%ls|%Lf|%llf",
            ),
            ("%c%c", vec![b'o'.into(), b'k'.into()], "ok"),
            ("%1$d", vec![4.into()], "%1$d"),
            ("%p %x", vec![2.5.into(), (-1).into()], "%p ffffffff"),
        ];
        for (fmt, args, want) in cases {
            let (out, n) = format(fmt.as_bytes(), &args, &errno);
            assert_eq!(
                (&out[..], n),
                (want.as_bytes(), want.len() as c_int),
                "{fmt}"
            );
        }
    }

    #[test]
    fn sprintf_stops_at_the_destination_and_returns_the_whole_length() {
        let v = <SeqVar<[c_char; 4]>>::zeroed();
        assert_eq!(sprintf(&v, "%d", &[12345.into()]), 5);
        assert_eq!(text(&v), b"123");
        let w = <SeqVar<[SeqString; 2]>>::zeroed();
        strcpy(&w.at(0), "m1");
        let n = sprintf(&w.at(1), "%s.%s", &[(&w.at(0)).into(), "RBV".into()]);
        assert_eq!((n, w.at(1).to_c_vec()), (6, b"m1.RBV".to_vec()));
        // The format and the arguments are read before the destination.
        assert_eq!(sprintf(&w.at(0), w.at(0), &[]), 2);
        assert_eq!(w.at(0).to_c_vec(), b"m1");
        assert_eq!(sprintf(&v, "ab%", &[]), -1);
        assert_eq!(text(&v), b"ab");
    }

    #[test]
    fn printf_writes_the_thread_stdout() {
        #[derive(Clone, Default)]
        struct Sink(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
        impl io::Write for Sink {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let n = stdio::redirect_stdout(sink.clone(), || {
            printf("%s=%.2f\n", &["x".into(), 0.5.into()])
        });
        assert_eq!(n, 7);
        assert_eq!(&*sink.0.borrow(), b"x=0.50\n");
    }

    #[cfg(all(unix, target_env = "gnu"))]
    unsafe extern "C" {
        /// What C99 code calls as `sscanf`; the `sscanf` symbol is the
        /// pre-C99 one, where `%a` is the allocation modifier.
        fn __isoc99_sscanf(s: *const c_char, fmt: *const c_char, ...) -> c_int;
    }

    /// A destination of the comparison with glibc.
    trait Checked {
        fn target(&self) -> &dyn ScanTarget;
        fn bytes(&self) -> Vec<u8>;
    }

    impl<T: ScanStore + Copy> Checked for SeqVar<T> {
        fn target(&self) -> &dyn ScanTarget {
            self
        }
        fn bytes(&self) -> Vec<u8> {
            let v = self.get();
            // SAFETY: `v` is a plain number or byte array, every byte of
            // which is initialised.
            unsafe { std::slice::from_raw_parts((&raw const v).cast::<u8>(), size_of::<T>()) }
                .to_vec()
        }
    }

    /// `scan` against glibc's `sscanf` into zeroed destinations of the
    /// types given, compared byte for byte.
    #[cfg(all(unix, target_env = "gnu"))]
    macro_rules! same_as_glibc_scan {
        ($input:expr, $fmt:expr $(, $t:ty)* $(,)?) => {{
            let (input, fmt): (&str, &str) = ($input, $fmt);
            let mut c_bufs = [[0u64; 8]; 4];
            let p: Vec<*mut std::ffi::c_void> = c_bufs.iter_mut().map(|b| b.as_mut_ptr().cast()).collect();
            let (ci, cf) = (std::ffi::CString::new(input).unwrap(), std::ffi::CString::new(fmt).unwrap());
            // SAFETY: each destination is 64 writable, aligned bytes, more
            // than any conversion in these formats stores.
            let c_n = unsafe { __isoc99_sscanf(ci.as_ptr(), cf.as_ptr(), p[0], p[1], p[2], p[3]) };
            let vars: Vec<Box<dyn Checked>> = vec![$(Box::new(<SeqVar<$t>>::zeroed())),*];
            let targets: Vec<&dyn ScanTarget> = vars.iter().map(|v| v.target()).collect();
            let n = scan(input.as_bytes(), fmt.as_bytes(), &targets);
            assert_eq!(n, c_n, "{input:?} {fmt:?}");
            for (i, v) in vars.iter().enumerate() {
                let b = v.bytes();
                // SAFETY: `c_bufs[i]` is 64 bytes, and `b` no longer.
                let c = unsafe { std::slice::from_raw_parts(c_bufs[i].as_ptr().cast::<u8>(), b.len()) };
                assert_eq!(b, c, "{input:?} {fmt:?} destination {i}");
            }
        }};
    }

    #[cfg(all(unix, target_env = "gnu"))]
    #[test]
    fn sscanf_matches_glibc() {
        use std::ffi::{c_long, c_ulong};
        type S = [c_char; 16];
        // The 16 programs' formats.
        same_as_glibc_scan!("7", "%d", i32);
        same_as_glibc_scan!("1 2 3 4294967295", "%u%u%u%u", u32, u32, u32, u32);
        same_as_glibc_scan!("XIAHSC-A-12", "XIAHSC-%c-%d%s", c_char, i32, S);
        same_as_glibc_scan!("B-3 x", "%c-%d%s", c_char, i32, S);
        same_as_glibc_scan!("!ID %R 123", "%*s %*s %d", i32);
        same_as_glibc_scan!("2.5e-3", "%lg", f64);
        // Integers, their prefixes, widths and narrowing.
        for input in [
            "0x1A",
            "017",
            "0x",
            "0xg",
            "-0x10",
            "+",
            "-",
            " -12abc",
            "99999999999",
            "1e5",
        ] {
            same_as_glibc_scan!(input, "%i", i32);
            same_as_glibc_scan!(input, "%x%n", u32, i32);
            same_as_glibc_scan!(input, "%2d%n", i32, i32);
            same_as_glibc_scan!(input, "%o", u32);
        }
        same_as_glibc_scan!("12345", "%2d%3d", i32, i32);
        same_as_glibc_scan!("-5", "%1d%d", i32, i32);
        same_as_glibc_scan!("0x1f", "%2x%n", u32, i32);
        same_as_glibc_scan!("0x1f", "%3x", u32);
        same_as_glibc_scan!("300 70000 -1", "%hhd %hd %hu", i8, i16, u16);
        same_as_glibc_scan!("-1 9223372036854775807", "%ld %lld", c_long, i64);
        same_as_glibc_scan!("-1 4294967296", "%lu %llx", c_ulong, u64);
        same_as_glibc_scan!("(nil) (NIL) 0x1234 (ni", "%p %p %p %p", u64, u64, u64, u64);
        same_as_glibc_scan!("(nil)", "%4p", u64);
        // Floats.
        for input in [
            "1.5",
            "-2e3",
            "0x1p-2",
            "0X1.8P+1",
            "inf",
            "-INFINITY",
            "infinx",
            "info",
            "nan",
            "nax",
            "1e",
            "1e+",
            "1e+x",
            ".5",
            "-.",
            ".",
            "0x",
            "0xp1",
            "1.2.3",
            "+",
            "1e5.5",
            "3.4028236e38",
            "1.00000005960464477539",
            // Rounds to a double halfway between two floats.
            "1.000000059604644775390625001",
            "7e-46",
        ] {
            same_as_glibc_scan!(input, "%f%n", f32, i32);
            same_as_glibc_scan!(input, "%lf%n", f64, i32);
            same_as_glibc_scan!(input, "%3le%n", f64, i32);
        }
        same_as_glibc_scan!("1.5 2", "%a %lA", f32, f64);
        // Strings, characters and sets.
        same_as_glibc_scan!("abcdef ghi", "%3c|%s", S, S);
        same_as_glibc_scan!("abcdef ghi", "%3c%s %2s%n", S, S, S, i32);
        same_as_glibc_scan!("x,y,z", "%[^,],%[a-y],%n", S, S, i32);
        same_as_glibc_scan!("]-a-q", "%[]-a]%n", S, i32);
        same_as_glibc_scan!("--9", "%[-0-9]", S);
        same_as_glibc_scan!("abc", "%[0-9]", S);
        same_as_glibc_scan!("abc", "%2[a-z]%c", S, c_char);
        same_as_glibc_scan!("abc", "%[a-z", S);
        // Literals, whitespace, `%%`, `%n` and the end of input.
        same_as_glibc_scan!("a  %  b", "a %% %c%n", c_char, i32);
        same_as_glibc_scan!("a b", "a%c", c_char);
        same_as_glibc_scan!("a b", "a %c", c_char);
        same_as_glibc_scan!("12", "%d%n %n", i32, i32, i32);
        same_as_glibc_scan!("", "%d", i32);
        same_as_glibc_scan!("   ", "%s", S);
        same_as_glibc_scan!("   ", " x", i32);
        same_as_glibc_scan!("abc", "%d", i32);
        same_as_glibc_scan!("5", "%d x", i32);
        same_as_glibc_scan!("5", "%d%", i32);
        same_as_glibc_scan!("5 6", "%*d %d %2$d", i32, i32);
        same_as_glibc_scan!("ab", "%b", S);
    }

    #[test]
    fn sscanf_reads_variables_and_stops_without_a_destination() {
        let reply = <SeqVar<SeqString>>::zeroed();
        strcpy(&reply, "HSC-B-7 rest");
        let (c, n) = (<SeqVar<c_char>>::zeroed(), <SeqVar<[i32; 2]>>::zeroed());
        assert_eq!(sscanf(&reply, "HSC-%c-%d", &[&c, &n.at(1)]), 2);
        assert_eq!((c.get() as u8, n.get()), (b'B', [0, 7]));
        assert_eq!(sscanf("1 2", "%d %d", &[&c]), 1);
        assert_eq!(c.get(), 1);
        assert_eq!(sscanf("1", "%d %d", &[]), 0);
        // glibc allocates for `%ms`; the port has no `char **` to give it.
        assert_eq!(sscanf("ab", "%ms", &[&reply]), 0);
        // The source is read before the destination is locked.
        assert_eq!(sscanf(&reply, "%s", &[&reply]), 1);
        assert_eq!(reply.get().as_bytes(), b"HSC-B-7");
    }

    #[test]
    fn scan_stores_convert_to_the_variable() {
        let n = <SeqVar<[i16; 2]>>::zeroed();
        n.at(1).store(Scanned::Int(70000));
        n.at(0).store(Scanned::Chars(b"AB"));
        assert_eq!(n.get(), [65, 4464]);
        n.at(0).store(Scanned::Str(b"9"));
        assert_eq!(n.get()[0], 65);
        let x = <SeqVar<f32>>::zeroed();
        x.store(Scanned::Double(0.1));
        assert_eq!(x.get(), 0.1f32);
        let s = <SeqVar<[c_char; 4]>>::zeroed();
        s.store(Scanned::Str(b"abcdef"));
        assert_eq!(text(&s), b"abc");
        s.store(Scanned::Chars(b"xy"));
        assert_eq!(text(&s), b"xyc");
        s.store(Scanned::UInt(7));
        assert_eq!(text(&s), b"xyc");
        let t = <SeqVar<[SeqString; 2]>>::zeroed();
        t.at(1).store(Scanned::Str(b"m1"));
        assert_eq!(t.at(1).to_c_vec(), b"m1");
    }

    #[test]
    fn atoi_is_strtol_cut_to_int() {
        assert_eq!(atoi(" -42abc"), -42);
        assert_eq!(atoi("x1"), 0);
        assert_eq!(atoi("2147483648"), i32::MIN);
        let v = <SeqVar<SeqString>>::zeroed();
        strcpy(&v, "4");
        assert_eq!(atoi(&v), 4);
    }

    #[test]
    fn atof_takes_the_strtod_prefix() {
        assert_eq!(atof("  1.5e3xyz"), 1500.0);
        assert_eq!(atof("0x10"), 16.0);
        assert_eq!(atof("-inf"), f64::NEG_INFINITY);
        assert_eq!(atof("e5"), 0.0);
        let v = <SeqVar<[SeqString; 2]>>::zeroed();
        strcpy(&v.at(1), "R-2.25");
        assert_eq!(atof(&b"R-2.25"[1..]), -2.25);
        assert_eq!(atof(v.at(1)), 0.0);
    }

    #[test]
    fn epics_thread_sleep_sleeps_only_a_delay_nanosleep_takes() {
        let t = std::time::Instant::now();
        epicsThreadSleep(0.02);
        assert!(t.elapsed().as_secs_f64() >= 0.02);
        let t = std::time::Instant::now();
        for refused in [-1.0, f64::NAN, f64::INFINITY, 1e300] {
            epicsThreadSleep(refused);
        }
        assert!(t.elapsed().as_secs_f64() < 1.0);
    }

    #[test]
    fn a_string_element_of_an_array_is_a_destination() {
        let v = <SeqVar<[SeqString; 3]>>::zeroed();
        strcpy(&v.at(2), "Al");
        assert_eq!(v.get()[2].as_bytes(), b"Al");
    }

    #[test]
    fn only_a_null_char_ptr_is_null_through_every_forward() {
        let null: CharPtr = None;
        let set: CharPtr = Some(Arc::from(&b""[..]));
        assert!(null.is_null());
        assert!(!set.is_null());
        assert!(<&CharPtr as CSrc>::is_null(&&null));
        assert!(SeqVar::new(null.clone()).is_null());
        let v = SeqVar::new([set, null]);
        assert!(!v.at(0).is_null());
        assert!(v.at(1).is_null());
        assert!(!"".is_null());
        assert!(!<SeqVar<SeqString>>::zeroed().is_null());
    }
}
