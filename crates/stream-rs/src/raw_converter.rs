//! `RawConverter.cc`: `%r`, a long as raw bytes, big endian or with `#`
//! little endian; `0` makes it unsigned.

use crate::buffer::StreamBuffer;
use crate::converter::StreamFormatConverter;
use crate::cstr::CPtr;
use crate::format::*;

const LONG: u64 = std::mem::size_of::<i64>() as u64;

/// `RawConverter`, `r`.
pub struct RawConverter;

impl StreamFormatConverter for RawConverter {
    fn parse(
        &self,
        fmt: &StreamFormat,
        _info: &mut StreamBuffer,
        _source: &mut CPtr<'_>,
        _scan_format: bool,
    ) -> i32 {
        if fmt.flags & ZERO_FLAG != 0 {
            UNSIGNED_FORMAT
        } else {
            SIGNED_FORMAT
        }
    }

    fn print_long(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: i64) -> bool {
        // number of bytes from value, default 1
        let mut prec = if fmt.prec < 0 { 1 } else { fmt.prec as u64 };
        let mut width = prec; // number of bytes in output
        prec = prec.min(LONG);
        width = width.max(fmt.width);

        let mut value = value;
        if fmt.flags & ALT_FLAG != 0 {
            // little endian (lsb first)
            let mut byte = 0u8;
            for _ in 0..prec {
                byte = value as u8;
                output.append_byte(byte);
                value >>= 8;
                width -= 1;
            }
            // fill with zero, or with the sign of the last byte
            let fill = if fmt.flags & ZERO_FLAG == 0 && byte & 0x80 != 0 {
                0xff
            } else {
                0
            };
            output.append_repeat(fill, width as isize);
        } else {
            // big endian (msb first)
            // With no bytes from value C shifts by 8 * (0u - 1): x86 takes
            // the count mod 64, which tests the sign bit.
            let top = if prec == 0 {
                value >> 56
            } else {
                value >> (8 * (prec - 1))
            };
            let fill = if fmt.flags & ZERO_FLAG == 0 && top & 0x80 != 0 {
                0xff
            } else {
                0
            };
            output.append_repeat(fill, (width - prec) as isize);
            for i in (0..prec).rev() {
                output.append_byte((value >> (8 * i)) as u8);
            }
        }
        true
    }

    fn scan_long(&self, fmt: &StreamFormat, input: CPtr<'_>, value: &mut i64) -> isize {
        let width = fmt.width.max(1); // default: 1 byte
        if fmt.flags & SKIP_FLAG != 0 {
            return width as isize; // just skip input
        }
        let byte = |i: u64| input.at(i as usize);
        let mut val: u64 = 0;
        if fmt.flags & ALT_FLAG != 0 {
            // little endian (lsb first)
            // C bug not copied: past 8 bytes C shifts the ninth by 64, which
            // x86 ORs into the lowest byte, and consumes one byte less than
            // the width from ten bytes on. The bytes above a long are
            // ignored, as C's comment says and its big-endian half does.
            let n = width.min(LONG);
            for i in 0..n - 1 {
                val |= (byte(i) as u64) << (8 * i);
            }
            let last = byte(n - 1);
            val |= if fmt.flags & ZERO_FLAG != 0 {
                // fill with zero
                last as u64
            } else {
                // fill with sign
                last as i8 as i64 as u64
            } << (8 * (n - 1));
        } else {
            // big endian (msb first)
            val = if fmt.flags & ZERO_FLAG != 0 {
                // fill with zero
                byte(0) as u64
            } else {
                // fill with sign
                byte(0) as i8 as i64 as u64
            };
            for i in 1..width {
                val = val << 8 | byte(i) as u64;
            }
        }
        *value = val as i64;
        width as isize
    }
}
