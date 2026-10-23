//! `RawFloatConverter.cc`: `%R`, an IEEE float or double as its raw bytes.

use crate::buffer::StreamBuffer;
use crate::converter::StreamFormatConverter;
use crate::cstr::CPtr;
use crate::error::stream_error;
use crate::format::*;

/// `RawFloatConverter`
pub struct RawFloatConverter;

impl StreamFormatConverter for RawFloatConverter {
    fn parse(
        &self,
        format: &StreamFormat,
        _info: &mut StreamBuffer,
        _source: &mut CPtr<'_>,
        _scan_format: bool,
    ) -> i32 {
        // C's "Cannot find out byte order" probe has no counterpart: the
        // byte order is the target's, known when this is compiled.
        // Assume IEEE formats with 4 or 8 bytes (default: 4)
        if matches!(format.width, 0 | 4 | 8) {
            return DOUBLE_FORMAT;
        }
        stream_error(b"Only width 4 or 8 allowed for %R format.\n");
        0
    }

    fn print_double(&self, format: &StreamFormat, output: &mut StreamBuffer, value: f64) -> bool {
        // C swaps bytes when the host order differs from the one asked
        // for: big-endian, or little-endian with '#'.
        let little = format.flags & ALT_FLAG != 0;
        if format.width == 8 {
            output.append(&if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            });
        } else {
            let value = value as f32;
            output.append(&if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            });
        }
        true
    }

    fn scan_double(&self, format: &StreamFormat, input: CPtr<'_>, value: &mut f64) -> isize {
        let nb_of_bytes = if format.width == 8 { 8 } else { 4 };
        if format.flags & SKIP_FLAG != 0 {
            return nb_of_bytes as isize; // just skip input
        }
        // Past the end of the input C reads whatever follows; this reads
        // zeros. StreamCore refuses the value either way, as more than the
        // input is consumed.
        let mut bytes = [0u8; 8];
        for (i, b) in bytes[..nb_of_bytes].iter_mut().enumerate() {
            *b = input.at(i);
        }
        let little = format.flags & ALT_FLAG != 0;
        *value = if nb_of_bytes == 8 {
            if little {
                f64::from_le_bytes(bytes)
            } else {
                f64::from_be_bytes(bytes)
            }
        } else {
            let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
            (if little {
                f32::from_le_bytes(bytes)
            } else {
                f32::from_be_bytes(bytes)
            }) as f64
        };
        nb_of_bytes as isize
    }
}
