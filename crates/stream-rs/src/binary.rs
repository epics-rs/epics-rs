//! `BinaryConverter.cc`: `%b` and `%B`, a number as one character per bit.

use crate::buffer::StreamBuffer;
use crate::converter::{ESC, StreamFormatConverter};
use crate::cstr::{CPtr, c_isspace};
use crate::error::stream_error;
use crate::format::*;

pub struct BinaryConverter;

impl StreamFormatConverter for BinaryConverter {
    fn parse(
        &self,
        fmt: &StreamFormat,
        info: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        _scan_format: bool,
    ) -> i32 {
        if fmt.conv == b'b' {
            // default characters 0 and 1 for %b
            info.append(b"01");
            return UNSIGNED_FORMAT;
        }

        // user defined characters for %B (next 2 in source)
        if source.peek() != 0 {
            if source.peek() == ESC {
                source.advance(1);
            }
            info.append_byte(source.peek());
            source.advance(1);
            if source.peek() != 0 {
                if source.peek() == ESC {
                    source.advance(1);
                }
                info.append_byte(source.peek());
                source.advance(1);
                return UNSIGNED_FORMAT;
            }
        }
        stream_error(b"Missing characters after %B format conversion\n");
        0
    }

    fn print_long(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: i64) -> bool {
        let mut value = value;
        let mut prec = fmt.prec;
        if prec == -1 {
            // Find number of significant bits is nothing is specified.
            let mut x = value as u64;
            prec = 32;
            if x > 0xFFFFFFFF {
                prec = 64;
                x >>= 32;
            }
            if x <= 0x0000FFFF {
                prec -= 16;
                x <<= 16;
            }
            if x <= 0x00FFFFFF {
                prec -= 8;
                x <<= 8;
            }
            if x <= 0x0FFFFFFF {
                prec -= 4;
                x <<= 4;
            }
            if x <= 0x3FFFFFFF {
                prec -= 2;
                x <<= 2;
            }
            if x <= 0x7FFFFFFF {
                prec -= 1;
            }
        }
        let prec = prec as u64;
        let mut width = prec.max(fmt.width);
        let zero = fmt.info[0];
        let one = fmt.info[1];
        let fill = if fmt.flags & ZERO_FLAG != 0 {
            zero
        } else {
            b' '
        };
        if fmt.flags & ALT_FLAG != 0 {
            // little endian (least significant bit first)
            if fmt.flags & LEFT_FLAG == 0 {
                // pad left
                while width > prec {
                    output.append_byte(b' ');
                    width -= 1;
                }
            }
            for _ in 0..prec {
                output.append_byte(if value & 1 != 0 { one } else { zero });
                value >>= 1;
                width -= 1;
            }
            // pad right
            for _ in 0..width {
                output.append_byte(fill);
            }
        } else {
            // big endian (most significant bit first)
            if fmt.flags & LEFT_FLAG == 0 {
                // pad left
                while width > prec {
                    output.append_byte(fill);
                    width -= 1;
                }
            }
            for bit in (0..prec).rev() {
                // C's 1L << prec past bit 63 is x86's shift, which counts
                // modulo 64.
                let set = value & 1i64.wrapping_shl(bit as u32) != 0;
                output.append_byte(if set { one } else { zero });
                width -= 1;
            }
            // pad right
            for _ in 0..width {
                output.append_byte(b' ');
            }
        }
        true
    }

    fn scan_long(&self, fmt: &StreamFormat, input: CPtr<'_>, value: &mut i64) -> isize {
        let mut val: i64 = 0;
        // 0 is no limit; C's long width counts down from -1.
        let mut width = if fmt.width == 0 { u64::MAX } else { fmt.width };
        let mut consumed = 0;
        let zero = fmt.info[0];
        let one = fmt.info[1];
        if !c_isspace(zero as i32) && !c_isspace(one as i32) {
            // skip whitespaces
            while c_isspace(input.at(consumed) as i32) {
                consumed += 1;
            }
        }
        if input.at(consumed) != zero && input.at(consumed) != one {
            return -1;
        }
        if fmt.flags & ALT_FLAG != 0 {
            // little endian (least significan bit first)
            let mut mask: i64 = 1;
            while width != 0 && (input.at(consumed) == zero || input.at(consumed) == one) {
                width -= 1;
                if input.at(consumed) == one {
                    val |= mask;
                }
                consumed += 1;
                mask = mask.wrapping_shl(1);
            }
        } else {
            // big endian (most significan bit first)
            while width != 0 && (input.at(consumed) == zero || input.at(consumed) == one) {
                width -= 1;
                val = val.wrapping_shl(1);
                if input.at(consumed) == one {
                    val |= 1;
                }
                consumed += 1;
            }
        }
        *value = val;
        consumed as isize
    }
}
