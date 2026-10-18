//! The standard converters of `StreamFormatConverter.cc`
//! (`StdLongConverter` and on). [`crate::converter`] registers them before
//! any other converter, as C's static registrars in that file do.

use crate::buffer::StreamBuffer;
use crate::cmsg;
use crate::converter::{ESC, StreamFormatConverter};
use crate::cprintf;
use crate::cstr::{CPtr, c_isspace, c_strtoul, c_tolower};
use crate::error::{Ch, stream_error};
use crate::format::*;
use epics_base_rs::runtime::stdlib::strtod;

/// The `RegisterConverter` lines of `StreamFormatConverter.cc`, in order.
pub(crate) fn standard() -> Vec<(&'static str, &'static [u8], Box<dyn StreamFormatConverter>)> {
    vec![
        ("StdLongConverter", b"diouxX", Box::new(StdLongConverter)),
        ("StdDoubleConverter", b"feEgG", Box::new(StdDoubleConverter)),
        ("StdStringConverter", b"s", Box::new(StdStringConverter)),
        ("StdCharsConverter", b"c", Box::new(StdCharsConverter)),
        ("StdCharsetConverter", b"[", Box::new(StdCharsetConverter)),
    ]
}

/// `copyFormatString`: `%`, then what the format has between its `%` (or
/// the `)` of a field name) and the conversion character, without `?` and
/// `=`.
fn copy_format_string(info: &mut StreamBuffer, source: &CPtr<'_>) {
    let s = source.base();
    let conv = source.offset() - 1;
    let mut p = conv;
    while s[p] != b'%' && s[p] != b')' {
        p -= 1;
    }
    info.append_byte(b'%');
    p += 1;
    while p != conv {
        if s[p] != b'?' && s[p] != b'=' {
            info.append_byte(s[p]);
        }
        p += 1;
    }
}

fn precision_error(fmt: &StreamFormat) {
    stream_error(&cmsg!(
        b"Use of precision field '.",
        fmt.prec.to_string(),
        b"' not allowed with %",
        Ch(fmt.conv as i32),
        b" input conversion\n"
    ));
}

/// `prepareval`: skip leading space and one sign. Returns the input to
/// convert, the bytes consumed and the sign, or `None` for -1.
///
/// With a width the input is cut to it in `cut`; C copies it into
/// `fmt.info`, which `parse` reserved, because `strto*` take no width.
fn prepareval<'a>(
    fmt: &StreamFormat,
    input: CPtr<'a>,
    cut: &'a mut Vec<u8>,
) -> Option<(CPtr<'a>, usize, bool)> {
    let mut input = input;
    let mut consumed = 0;
    while c_isspace(input.peek() as i32) {
        input.advance(1);
        consumed += 1;
    }
    if fmt.width != 0 {
        let mut width = fmt.width as usize;
        if fmt.flags & SPACE_FLAG != 0 {
            // normally whitespace does not count to width
            // but do so if space flag is present.
            // C bug not copied: C's size_t wraps when more space was
            // skipped than the width, and strncpy writes past fmt.info.
            width = width.saturating_sub(consumed);
        }
        cut.clear();
        cut.extend(input.cstr().iter().take(width));
        cut.push(0);
        input = CPtr::new(cut);
    }
    let mut neg = false;
    if input.peek() == b'+' || input.peek() == b'-' {
        neg = input.peek() == b'-';
        input.advance(1);
        consumed += 1;
    }
    // allow space after sign only if # flag is set
    if c_isspace(input.peek() as i32) && fmt.flags & ALT_FLAG == 0 {
        return None;
    }
    Some((input, consumed, neg))
}

/// `StdLongConverter`, `diouxX`.
struct StdLongConverter;

impl StreamFormatConverter for StdLongConverter {
    fn parse(
        &self,
        fmt: &StreamFormat,
        info: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        scan_format: bool,
    ) -> i32 {
        if scan_format && fmt.prec >= 0 {
            precision_error(fmt);
            return 0;
        }
        if scan_format {
            if fmt.width != 0 {
                info.reserve(fmt.width as usize + 1);
            }
        } else {
            copy_format_string(info, source);
            info.append_byte(b'l');
            info.append_byte(fmt.conv);
        }
        if fmt.conv == b'd'
            || fmt.conv == b'i'
            || ((fmt.conv == b'x' || fmt.conv == b'o') && fmt.flags & (LEFT_FLAG | SIGN_FLAG) != 0)
        {
            return SIGNED_FORMAT;
        }
        UNSIGNED_FORMAT
    }

    fn print_long(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: i64) -> bool {
        let mut value = value;
        // limits %x/%X formats to number of half bytes in width.
        if fmt.width != 0 && (fmt.conv == b'x' || fmt.conv == b'X') && fmt.width < 16 {
            value &= !(-1i64 << (fmt.width * 4));
        }
        cprintf::print_long(output, fmt.info, value);
        true
    }

    fn scan_long(&self, fmt: &StreamFormat, input: CPtr<'_>, value: &mut i64) -> isize {
        let mut cut = Vec::new();
        let Some((input, consumed, neg)) = prepareval(fmt, input, &mut cut) else {
            return -1;
        };
        let base = match fmt.conv {
            b'd' => 10,
            b'o' | b'x' | b'X' => {
                // allow negative hex and oct numbers with - flag
                if neg && fmt.flags & LEFT_FLAG == 0 {
                    return -1;
                }
                if fmt.conv == b'o' { 8 } else { 16 }
            }
            b'u' => {
                if neg {
                    return -1;
                }
                10
            }
            _ => 0,
        };
        let (mut v, end) = c_strtoul(input, base);
        let len = end - input.offset();
        if len == 0 {
            return -1;
        }
        if base == 16 && fmt.flags & SIGN_FLAG != 0 && len as u64 >= fmt.width {
            // sign-extend %+x if width is fully used
            let i = if input.peek() == b'0' && c_tolower(input.at(1) as i32) == b'x' as i32 {
                2
            } else {
                0
            };
            // C compares a (signed) char.
            if input.at(i) as i8 > b'7' as i8 && len.checked_sub(i).is_some_and(|n| n < 16) {
                v |= u64::MAX << ((len - i) * 4);
            }
        }
        *value = if neg { v.wrapping_neg() } else { v } as i64;
        (consumed + len) as isize
    }
}

/// `StdDoubleConverter`, `feEgG`.
struct StdDoubleConverter;

impl StreamFormatConverter for StdDoubleConverter {
    fn parse(
        &self,
        fmt: &StreamFormat,
        info: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        scan_format: bool,
    ) -> i32 {
        if scan_format && fmt.prec >= 0 {
            precision_error(fmt);
            return 0;
        }
        if scan_format {
            if fmt.width != 0 {
                info.reserve(fmt.width as usize + 1);
            }
        } else {
            copy_format_string(info, source);
            info.append_byte(fmt.conv);
        }
        DOUBLE_FORMAT
    }

    fn print_double(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: f64) -> bool {
        cprintf::print_double(output, fmt.info, value);
        true
    }

    fn scan_double(&self, fmt: &StreamFormat, input: CPtr<'_>, value: &mut f64) -> isize {
        let mut cut = Vec::new();
        let Some((input, consumed, neg)) = prepareval(fmt, input, &mut cut) else {
            return -1;
        };
        let r = strtod(input.cstr());
        // C stores the value before it checks that strtod read anything.
        *value = if neg { -r.value } else { r.value };
        if r.len == 0 {
            return -1;
        }
        (consumed + r.len) as isize
    }
}

/// What the `scanString` methods share: a byte is stored while room is left
/// for the terminating 0x00 (none with the skip flag or no buffer).
struct Store<'v> {
    value: &'v mut [u8],
    n: usize,
    space_left: usize,
}

impl<'v> Store<'v> {
    fn new(fmt: &StreamFormat, value: &'v mut [u8], size: usize) -> Self {
        let space_left = if fmt.flags & SKIP_FLAG != 0 || value.is_empty() {
            0
        } else {
            size.min(value.len())
        };
        Self {
            value,
            n: 0,
            space_left,
        }
    }

    fn push(&mut self, c: u8) {
        // keep space for terminal null byte
        if self.space_left > 1 {
            self.value[self.n] = c;
            self.n += 1;
            self.space_left -= 1;
        }
    }

    /// Terminate, and set `size` to the bytes written.
    fn finish(self, size: &mut usize) {
        if self.space_left != 0 {
            self.value[self.n] = 0;
            *size = self.n + 1;
        }
    }
}

/// `StdStringConverter`, `s`.
struct StdStringConverter;

impl StreamFormatConverter for StdStringConverter {
    fn parse(
        &self,
        fmt: &StreamFormat,
        info: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        scan_format: bool,
    ) -> i32 {
        if fmt.flags & SIGN_FLAG != 0 {
            stream_error(&cmsg!(
                b"Use of modifier '+'not allowed with %",
                Ch(fmt.conv as i32),
                b" conversion\n"
            ));
            return 0;
        }
        if scan_format && fmt.prec >= 0 {
            precision_error(fmt);
            return 0;
        }
        copy_format_string(info, source);
        info.append_byte(fmt.conv);
        if scan_format {
            info.append(b"%n");
        }
        STRING_FORMAT
    }

    fn print_string(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: &[u8]) -> bool {
        if fmt.flags & ZERO_FLAG != 0 && fmt.width != 0 {
            let mut value = CPtr::new(value).cstr();
            if fmt.prec > -1 {
                value = &value[..value.len().min(fmt.prec as usize)];
            }
            // C bug not copied: C's size_t width-l goes negative for a
            // longer string, and append(c, count) then deletes that many
            // bytes from the end of the output.
            let fill = (fmt.width as usize).saturating_sub(value.len()) as isize;
            if fmt.flags & LEFT_FLAG == 0 {
                output.append_repeat(0, fill);
            }
            output.append(value);
            if fmt.flags & LEFT_FLAG != 0 {
                output.append_repeat(0, fill);
            }
        } else {
            cprintf::print_string(output, fmt.info, value);
        }
        true
    }

    fn scan_string(
        &self,
        fmt: &StreamFormat,
        input: CPtr<'_>,
        value: &mut [u8],
        size: &mut usize,
    ) -> isize {
        let mut store = Store::new(fmt, value, *size);
        let mut input = input;
        let mut consumed = 0;
        // if user does not specify width assume "infinity" (-1)
        let mut width: i64 = match fmt.width {
            0 if fmt.conv == b'c' => 1,
            0 => -1,
            w => w as i64,
        };
        while c_isspace(input.peek() as i32) && width != 0 {
            // normally leading whitespace does not count to width
            // but do so if space flag is present
            if fmt.flags & SPACE_FLAG != 0 {
                store.push(input.peek());
                width -= 1;
            }
            consumed += 1;
            input.advance(1);
        }
        while input.peek() != 0 && width != 0 {
            // normally whitespace ends string
            // but don't end if # flag is present
            if fmt.flags & ALT_FLAG == 0 && c_isspace(input.peek() as i32) {
                break;
            }
            store.push(input.peek());
            consumed += 1;
            width -= 1;
            input.advance(1);
        }
        store.finish(size);
        consumed
    }
}

/// `StdCharsConverter`, `c`.
struct StdCharsConverter;

impl StreamFormatConverter for StdCharsConverter {
    fn parse(
        &self,
        fmt: &StreamFormat,
        info: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        scan_format: bool,
    ) -> i32 {
        if fmt.flags & (SIGN_FLAG | SPACE_FLAG | ZERO_FLAG | ALT_FLAG) != 0 {
            stream_error(b"Use of modifiers '+', ' ', '0', '#' not allowed with %c conversion\n");
            return 0;
        }
        if scan_format && fmt.prec >= 0 {
            precision_error(fmt);
            return 0;
        }
        copy_format_string(info, source);
        info.append_byte(fmt.conv);
        if scan_format {
            info.append(b"%n");
            return STRING_FORMAT;
        }
        UNSIGNED_FORMAT
    }

    fn print_long(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: i64) -> bool {
        cprintf::print_char(output, fmt.info, value);
        true
    }

    fn scan_string(
        &self,
        fmt: &StreamFormat,
        input: CPtr<'_>,
        value: &mut [u8],
        size: &mut usize,
    ) -> isize {
        let mut store = Store::new(fmt, value, *size);
        let mut input = input;
        let mut consumed = 0;
        // if user does not specify width assume 1
        let mut width = if fmt.width == 0 { 1 } else { fmt.width };
        while input.peek() != 0 && width != 0 {
            store.push(input.peek());
            consumed += 1;
            width -= 1;
            input.advance(1);
        }
        store.finish(size);
        consumed
    }
}

/// `markbit`: C takes `c` as a (signed) `char`, and `info[c>>3]` counts a
/// negative index from the end of the 32 bytes, which is the byte `c` has
/// as unsigned.
fn markbit(info: &mut StreamBuffer, notflag: bool, c: i8) {
    let index = (c as u8 >> 3) as isize;
    let mask = 1u8 << (c & 7);
    let byte = info.get(index);
    info.set_byte(index, if notflag { byte | mask } else { byte & !mask });
}

/// `StdCharsetConverter`, `[`: `info` is a 32-byte map of the bytes that
/// end the input.
struct StdCharsetConverter;

impl StreamFormatConverter for StdCharsetConverter {
    fn parse(
        &self,
        fmt: &StreamFormat,
        info: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        scan_format: bool,
    ) -> i32 {
        if !scan_format {
            stream_error(b"Format conversion %[ is only allowed in input formats\n");
            return 0;
        }
        if fmt.flags & (LEFT_FLAG | SIGN_FLAG | SPACE_FLAG | ZERO_FLAG | ALT_FLAG) != 0 {
            stream_error(
                b"Use of modifiers '-', '+', ' ', '0', '#'not allowed with %[ conversion\n",
            );
            return 0;
        }
        if scan_format && fmt.prec >= 0 {
            precision_error(fmt);
            return 0;
        }

        // C's (signed) char: a range runs by its signed value.
        let mut c: i8 = 0;

        let notflag = source.peek() == b'^';
        let map = info.clear().reserve(32);
        if notflag {
            source.advance(1);
        } else {
            // C bug not copied: C's memset(info(1), 255, 32) leaves the
            // first byte clear, so bytes 1 to 7 never end the input, and
            // writes one byte past the map.
            map.fill(255);
        }
        if source.peek() == b']' {
            markbit(info, notflag, b']' as i8);
            source.advance(1);
        }
        while source.peek() != 0 && source.peek() != b']' {
            if source.peek() == ESC {
                source.advance(1);
                markbit(info, notflag, source.peek() as i8);
                source.advance(1);
                continue;
            }
            if source.peek() == b'-' && c != 0 && source.at(1) != 0 && source.at(1) != b']' {
                source.advance(1);
                let to = source.peek() as i8;
                while c < to {
                    markbit(info, notflag, c);
                    c += 1;
                }
                while c > to {
                    markbit(info, notflag, c);
                    c -= 1;
                }
            }
            c = source.peek() as i8;
            markbit(info, notflag, c);
            source.advance(1);
        }
        if source.peek() == 0 {
            stream_error(b"Missing ']' after %[ format conversion\n");
            return 0;
        }
        source.advance(1); // consume ']'

        STRING_FORMAT
    }

    fn scan_string(
        &self,
        fmt: &StreamFormat,
        input: CPtr<'_>,
        value: &mut [u8],
        size: &mut usize,
    ) -> isize {
        let mut store = Store::new(fmt, value, *size);
        let mut input = input;
        let mut consumed = 0;
        // if user does not specify width assume "infinity" (-1)
        let mut width: i64 = if fmt.width == 0 { -1 } else { fmt.width as i64 };
        while input.peek() != 0 && width != 0 {
            let c = input.peek();
            // C bug not copied: C indexes fmt.info with a signed char and
            // reads before the map for the bytes 0x80 to 0xff.
            if fmt.info[(c >> 3) as usize] & (1 << (c & 7)) != 0 {
                break;
            }
            store.push(c);
            consumed += 1;
            width -= 1;
            input.advance(1);
        }
        store.finish(size);
        consumed
    }
}
