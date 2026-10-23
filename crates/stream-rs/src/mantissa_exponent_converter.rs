//! `MantissaExponentConverter.cc`: `%m`, a double as an integer mantissa
//! and a signed exponent without `e` or `.`, `+00011-01` for 1.1. The
//! precision is the number of mantissa digits; `#` and `0` are not
//! supported.

use crate::buffer::StreamBuffer;
use crate::converter::StreamFormatConverter;
use crate::cstr::{CPtr, c_sscanf_int};
use crate::format::*;
use epics_libcom_rs::cfmt::CFloat;

/// `MantissaExponentConverter`, `m`.
pub struct MantissaExponentConverter;

impl StreamFormatConverter for MantissaExponentConverter {
    fn parse(
        &self,
        _fmt: &StreamFormat,
        _info: &mut StreamBuffer,
        _source: &mut CPtr<'_>,
        _scan_format: bool,
    ) -> i32 {
        DOUBLE_FORMAT
    }

    fn scan_double(&self, fmt: &StreamFormat, input: CPtr<'_>, value: &mut f64) -> isize {
        // sscanf(input, "%d%d%n", &mantissa, &exponent, &length)
        let mut length = -1;
        let mut parts = (0, 0);
        if let Some((mantissa, m)) = c_sscanf_int(input)
            && let Some((exponent, e)) = c_sscanf_int(input.plus(m))
        {
            parts = (mantissa, exponent);
            length = (m + e) as isize;
        }
        if fmt.flags & SKIP_FLAG != 0 {
            return length;
        }
        if length == -1 {
            return -1;
        }
        let (mantissa, exponent) = parts;
        *value = mantissa as f64 * 10f64.powf(exponent as f64);
        length
    }

    fn print_double(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: f64) -> bool {
        // Have to divide value into mantissa and exponent
        // precision field is number of characters in mantissa
        // number of characters in exponent is at least 2
        let prec = if fmt.prec < 1 { 6 } else { fmt.prec as i32 };
        let mut buf = StreamBuffer::new();
        buf.append(
            format!(
                "{:.*e}",
                (prec - 1) as usize,
                CFloat(value.abs() / 10f64.powf((prec - 1) as f64))
            )
            .as_bytes(),
        );
        // C bug not copied: printDouble's buf.remove(1,1) and
        // buf.remove(buf.find('e'),1) (MantissaExponentConverter.cc:82-83)
        // remove byte 1 and then the 'e' found, or the last byte when there
        // is none. With precision 1 (no '.') that drops the 'e' and the
        // exponent's last digit, printing 1e300 as "1+30"; inf and nan come
        // out "i" and "n".
        if buf.get(1) == b'.' {
            buf.remove(1, 1);
        }
        let e = buf.find_byte(b'e', 0);
        if e >= 0 {
            buf.remove(e, 1);
        }

        let mut spaces = fmt.width as isize - buf.length() as isize;
        if fmt.flags & (SPACE_FLAG | SIGN_FLAG) != 0 || value < 0.0 {
            spaces -= 1;
        }
        spaces = spaces.max(0);
        if fmt.flags & LEFT_FLAG == 0 {
            output.append_repeat(b' ', spaces);
        }
        if fmt.flags & (SPACE_FLAG | SIGN_FLAG) == SPACE_FLAG && value >= 0.0 {
            output.append_byte(b' ');
        }
        if fmt.flags & SIGN_FLAG != 0 && value >= 0.0 {
            output.append_byte(b'+');
        }
        if value < 0.0 {
            output.append_byte(b'-');
        }
        output.append_buffer(&buf);
        if fmt.flags & LEFT_FLAG != 0 {
            output.append_repeat(b' ', spaces);
        }
        true
    }
}
