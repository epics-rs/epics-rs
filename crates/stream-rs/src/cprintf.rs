//! The printf the standard converters hand their info string to:
//! `StreamBuffer::print(fmt.info, value)` is glibc's `vsnprintf` with one
//! conversion, `%[flags][width][.prec][l]conv`, as `copyFormatString`
//! builds it from the protocol's format. The conversions are libcom's
//! [`cfmt`].

use crate::buffer::StreamBuffer;
use epics_libcom_rs::cfmt::{self, Count, Directive, Spec};

/// The text up to the info string's 0x00.
fn text(info: &[u8]) -> &[u8] {
    let n = info.iter().position(|&b| b == 0).unwrap_or(info.len());
    &info[..n]
}

/// `info` converted by `conv`; a conversion it refuses is printed back by
/// glibc's `printf_unknown`, and the text after it as text.
///
/// `copyFormatString` keeps the input-only flags `!` and `*` when `=`
/// follows them; glibc takes `!` as the conversion. `*` makes C read a
/// width argument it never passed; it is printed as the conversion too.
fn print(out: &mut StreamBuffer, info: &[u8], conv: impl FnOnce(&mut Vec<u8>, &Spec) -> bool) {
    let info = text(info);
    // A scan format's info (none, or the width's 0x00s) is an empty format.
    if info.is_empty() {
        return;
    }
    let d = Directive::parse(info, 0);
    let mut bytes = Vec::new();
    if d.width == Count::Arg {
        let star = info.iter().position(|&c| c == b'*').unwrap();
        let s = Spec {
            conv: b'*',
            ..d.spec(|| 0)
        };
        cfmt::print_unknown(&mut bytes, &s);
        bytes.extend_from_slice(&info[star + 1..]);
    } else if !conv(&mut bytes, &d.spec(|| 0)) {
        bytes.extend_from_slice(&info[d.end..]);
    }
    out.append(&bytes);
}

/// `print(info, long)` for `%ld %li %lo %lu %lx %lX`.
pub fn print_long(out: &mut StreamBuffer, info: &[u8], value: i64) {
    print(out, info, |b, s| cfmt::print_int(b, s, value));
}

/// `print(info, const char*)` for `%s`.
pub fn print_string(out: &mut StreamBuffer, info: &[u8], value: &[u8]) {
    print(out, info, |b, s| cfmt::print_str(b, s, text(value)));
}

/// `print(info, long)` for `%c`: the `int` vararg cut to a byte, which may
/// be 0x00.
pub fn print_char(out: &mut StreamBuffer, info: &[u8], value: i64) {
    print(out, info, |b, s| cfmt::print_char(b, s, value as u8));
}

/// `print(info, double)` for `%f %e %E %g %G`.
pub fn print_double(out: &mut StreamBuffer, info: &[u8], value: f64) {
    print(out, info, |b, s| cfmt::print_double(b, s, value));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_only_flags_print_back() {
        for (info, want) in [
            (&b"%-*5ld\0"[..], &b"%-*5ld"[..]),
            (b"%0*12.3f\0", b"%0*12.3f"),
            (b"%!5ld\0", b"%!5ld"),
            (b"\0\0", b""),
        ] {
            let mut out = StreamBuffer::new();
            print_long(&mut out, info, 7);
            assert_eq!(out.bytes(), want);
        }
    }
}
