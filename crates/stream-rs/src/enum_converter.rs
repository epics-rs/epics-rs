//! `EnumConverter.cc`: `%{string0|string1|...}`.
//!
//! The info is `<numEnums><index><string>0<index><string>0...`, each number
//! a native `long`; `numEnums` is `-(n+1)` when the last string is the
//! `=?` default.

use crate::buffer::StreamBuffer;
use crate::converter::{ESC, StreamFormatConverter};
use crate::cstr::{CPtr, c_strtol};
use crate::error::stream_error;
use crate::format::*;
use crate::protocol::codes::SKIP;
use crate::{debug, debug2};

const LONG: usize = std::mem::size_of::<i64>();

/// `extract<long>(s)`. C reads past the info after the last string when no
/// value matched (`printLong`); those bytes are never used.
fn extract_long(s: &mut CPtr<'_>) -> i64 {
    let mut b = [0u8; LONG];
    for (i, x) in b.iter_mut().enumerate() {
        *x = s.at(i);
    }
    s.advance(LONG);
    i64::from_ne_bytes(b)
}

fn put_long(info: &mut StreamBuffer, at: usize, v: i64) {
    info.bytes_mut()[at..at + LONG].copy_from_slice(&v.to_ne_bytes());
}

/// `EnumConverter`, `{`.
pub struct EnumConverter;

impl StreamFormatConverter for EnumConverter {
    fn parse(
        &self,
        fmt: &StreamFormat,
        info: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        scan_format: bool,
    ) -> i32 {
        if fmt.flags & (LEFT_FLAG | SIGN_FLAG | SPACE_FLAG | ZERO_FLAG) != 0 {
            stream_error(b"Use of modifiers '-', '+', ' ', '0' not allowed with %{ conversion\n");
            return 0;
        }
        let mut num_enums: i64 = 0;
        let n = info.length(); // put numEnums here later
        info.append(&num_enums.to_ne_bytes());
        let mut index: i64 = 0;
        let mut i = info.length(); // put index here later
        info.append(&index.to_ne_bytes());
        while source.peek() != 0 {
            if source.peek() == b'=' && fmt.flags & ALT_FLAG != 0 {
                source.advance(1);
                if source.peek() == b'?' {
                    // default choice
                    if scan_format {
                        stream_error(b"Default value only allowed in output formats\n");
                        return 0;
                    }
                    source.advance(1);
                    if source.peek() != b'}' {
                        stream_error(b"Default value must be last\n");
                        return 0;
                    }
                    source.advance(1);
                    num_enums = -(num_enums + 1);
                    info.append_byte(0);
                    put_long(info, n, num_enums);
                    debug2!(
                        format!("EnumConverter::parse {} choices with default: ", -num_enums),
                        info.expand(),
                        b"\n"
                    );
                    return ENUM_FORMAT;
                }

                let (v, p) = c_strtol(*source, 0);
                let end = CPtr::at_offset(source.base(), p);
                if p == source.offset() || (end.peek() != b'|' && end.peek() != b'}') {
                    stream_error(b"Integer expected after '=' in %{ format conversion\n");
                    return 0;
                }
                index = v;
                put_long(info, i, index);
                source.set_offset(p);
            }
            if source.peek() == b'|' || source.peek() == b'}' {
                num_enums += 1;
                info.append_byte(0);

                let c = source.peek();
                source.advance(1);
                if c == b'}' {
                    put_long(info, n, num_enums);
                    debug2!(
                        format!("EnumConverter::parse {num_enums} choices: "),
                        info.expand(),
                        b"\n"
                    );
                    return ENUM_FORMAT;
                }
                // C's signed overflow past LONG_MAX wraps on its targets.
                index = index.wrapping_add(1);
                i = info.length();
                info.append(&index.to_ne_bytes());
            } else {
                if source.peek() == ESC {
                    info.append_byte(ESC);
                    source.advance(1);
                }
                info.append_byte(source.peek());
                source.advance(1);
            }
        }
        stream_error(b"Missing '}' after %{ format conversion\n");
        0
    }

    fn print_long(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: i64) -> bool {
        let mut s = CPtr::new(fmt.info);
        let mut num_enums = extract_long(&mut s);
        let mut index = extract_long(&mut s);
        let no_default = num_enums >= 0;

        if num_enums < 0 {
            num_enums = -num_enums - 1;
        }
        loop {
            let more = num_enums != 0;
            num_enums -= 1;
            if !more || value == index {
                break;
            }
            while s.peek() != 0 {
                if s.peek() == ESC {
                    s.advance(1);
                }
                s.advance(1);
            }
            s.advance(1);
            index = extract_long(&mut s);
        }
        if num_enums == -1 && no_default {
            stream_error(format!("Value {value} not found in enum set\n").as_bytes());
            return false;
        }
        while s.peek() != 0 {
            if s.peek() == ESC {
                s.advance(1);
            }
            output.append_byte(s.peek());
            s.advance(1);
        }
        true
    }

    fn scan_long(&self, fmt: &StreamFormat, input: CPtr<'_>, value: &mut i64) -> isize {
        debug!(
            format!("EnumConverter::scanLong(%{}, \"", fmt.conv as char),
            input.cstr(),
            b"\")\n"
        );
        let mut s = CPtr::new(fmt.info);
        let mut num_enums = extract_long(&mut s);

        // Only an output format has a default (a negative count), so this
        // ends.
        while num_enums != 0 {
            num_enums -= 1;
            let index = extract_long(&mut s);
            debug!(
                format!("EnumConverter::scanLong: check #{index} \""),
                s.cstr(),
                b"\"\n"
            );
            // A skip past the end of the input counts; StreamCore refuses
            // a count longer than the input.
            let mut consumed = 0;
            let mut matched = true;
            while s.peek() != 0 {
                if s.peek() == SKIP {
                    s.advance(1);
                    consumed += 1;
                    continue;
                }
                if s.peek() == ESC {
                    s.advance(1);
                }
                if s.peek() != input.at(consumed) {
                    matched = false;
                }
                s.advance(1);
                consumed += 1;
            }
            if matched {
                debug!(format!("EnumConverter::scanLong: value {index} matches\n"));
                *value = index;
                return consumed as isize;
            }
            s.advance(1);
        }
        debug!(b"EnumConverter::scanLong: no value matches\n");
        -1
    }
}
