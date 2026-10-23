//! `pcre_newline.c`: `is_newline` and `was_newline` for NLTYPE_ANY and
//! NLTYPE_ANYCRLF in 8-bit non-UTF mode, where a character is one byte and
//! U+2028/U+2029 cannot occur.

use super::internal::{CHAR_NEL, NLTYPE_ANYCRLF};

/// `PRIV(is_newline)`: is there a newline at `s[ptr]`? `ptr < endptr` is
/// guaranteed by the caller; `endptr` is the end of the subject.
pub fn is_newline(s: &[u8], ptr: usize, nltype: i32, endptr: usize, lenptr: &mut i32) -> bool {
    let c = s[ptr] as u32;
    let crlf = || {
        if ptr + 1 < endptr && s[ptr + 1] == b'\n' {
            2
        } else {
            1
        }
    };
    if nltype == NLTYPE_ANYCRLF {
        match c {
            0x0a => *lenptr = 1,
            0x0d => *lenptr = crlf(),
            _ => return false,
        }
    } else {
        match c {
            0x0a..=0x0c => *lenptr = 1, // LF, VT, FF
            0x0d => *lenptr = crlf(),
            CHAR_NEL => *lenptr = 1,
            _ => return false,
        }
    }
    true
}

/// `PRIV(was_newline)`: does a newline end just before `s[ptr]`?
/// `ptr > startptr` is guaranteed by the caller.
pub fn was_newline(s: &[u8], ptr: usize, nltype: i32, startptr: usize, lenptr: &mut i32) -> bool {
    let ptr = ptr - 1;
    let c = s[ptr] as u32;
    let lf = || {
        if ptr > startptr && s[ptr - 1] == b'\r' {
            2
        } else {
            1
        }
    };
    if nltype == NLTYPE_ANYCRLF {
        match c {
            0x0a => *lenptr = lf(),
            0x0d => *lenptr = 1,
            _ => return false,
        }
    } else {
        match c {
            0x0a => *lenptr = lf(),
            0x0b..=0x0d => *lenptr = 1, // VT, FF, CR
            CHAR_NEL => *lenptr = 1,
            _ => return false,
        }
    }
    true
}
