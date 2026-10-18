//! `BCDConverter.cc`: packed BCD (0x00 - 0x99) `%D`, big endian, or little
//! endian with the `#` flag; `+` makes the value signed, a non-zero upper
//! half of the most significant byte being the sign.

use crate::buffer::StreamBuffer;
use crate::converter::StreamFormatConverter;
use crate::cstr::CPtr;
use crate::format::*;

pub struct BcdConverter;

impl StreamFormatConverter for BcdConverter {
    fn parse(&self, fmt: &StreamFormat, _: &mut StreamBuffer, _: &mut CPtr<'_>, _: bool) -> i32 {
        if fmt.flags & SIGN_FLAG != 0 {
            SIGNED_FORMAT
        } else {
            UNSIGNED_FORMAT
        }
    }

    fn print_long(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: i64) -> bool {
        let signed = fmt.flags & SIGN_FLAG != 0;
        // number of nibbles
        let mut prec = if fmt.prec < 0 {
            2 * size_of::<i64>() as u64
        } else {
            fmt.prec as u64
        };
        let mut width = (prec + if signed { 2 } else { 1 }) / 2;
        let mut val = value as u64;

        // BCDConverter.cc:50 appends before :51 widens, so C pads nothing for
        // a width over the precision and, with # or a negative +, writes
        // before its field; formats.html: "precision decimal digits are
        // printed in at least width output bytes".
        if fmt.width > width {
            width = fmt.width;
        }
        output.append_repeat(0, width as isize);
        if signed && value < 0 {
            val = value.wrapping_neg() as u64;
        }
        let (mut i, d, s) = if fmt.flags & ALT_FLAG != 0 {
            (-(width as isize), 1, -1)
        } else {
            (-1, -1, -(width as isize))
        };
        while width != 0 && prec != 0 {
            width -= 1;
            let mut bcd = (val % 10) as u8;
            prec -= 1;
            if prec != 0 {
                prec -= 1;
                val /= 10;
                bcd |= ((val % 10) as u8) << 4;
                val /= 10;
            }
            output.set_byte(i, bcd);
            i += d;
        }
        if signed && value < 0 {
            output.set_byte(s, output.get(s) | 0xf0);
        }
        true
    }

    fn scan_long(&self, fmt: &StreamFormat, input: CPtr<'_>, value: &mut i64) -> isize {
        let signed = fmt.flags & SIGN_FLAG != 0;
        let mut consumed = 0usize;
        let mut val: i64 = 0;
        let mut width = fmt.width as i64;
        if width == 0 {
            width = 1;
        }
        if fmt.flags & ALT_FLAG != 0 {
            // little endian
            // BCDConverter.cc:97 makes shift an int, which overflows (signed
            // overflow, undefined) as :113 takes it past 10^8; as a long it
            // reaches the 10^10 the cap allows, so 12 digits are read.
            let mut shift: i64 = 1;
            while width != 0 {
                width -= 1;
                let mut bcd1 = input.at(consumed);
                consumed += 1;
                // BCDConverter.cc:101-103 shift bcd10 a second time, so C
                // reads every tens digit and the sign as 0; formats.html:
                // "each byte contains two binary coded decimal digits".
                let bcd10 = bcd1 >> 4;
                bcd1 &= 0x0f;
                let (b1, b10) = (bcd1 as i64, bcd10 as i64);
                if b1 > 9 || shift * b1 < b1 {
                    break;
                }
                if width == 0 && signed {
                    val += b1 * shift;
                    if bcd10 != 0 {
                        val = -val;
                    }
                    break;
                }
                if b10 > 9 || shift * b10 < b10 {
                    break;
                }
                val += (b1 + 10 * b10) * shift;
                if shift <= 100000000 {
                    shift *= 100;
                } else {
                    shift = 0;
                }
            }
        } else {
            // big endian
            let mut sign = 1;
            while width != 0 {
                width -= 1;
                let mut bcd1 = input.at(consumed);
                let mut bcd10 = bcd1 >> 4;
                bcd1 &= 0x0f;
                if consumed == 0 && signed && bcd10 != 0 {
                    sign = -1;
                    bcd10 = 0;
                }
                if bcd1 > 9 || bcd10 > 9 {
                    break;
                }
                // C detects overflow by the wrapped sum.
                let temp = val
                    .wrapping_mul(100)
                    .wrapping_add((bcd1 + 10 * bcd10) as i64);
                if temp < val {
                    consumed = 0;
                    break;
                }
                val = temp;
                consumed += 1;
            }
            val *= sign;
        }
        if consumed == 0 {
            return -1;
        }
        *value = val;
        consumed as isize
    }
}

/// `RegisterConverter (BCDConverter, "D")`
pub(crate) fn registered() -> (&'static str, &'static [u8], Box<dyn StreamFormatConverter>) {
    ("BCDConverter", b"D", Box::new(BcdConverter))
}
