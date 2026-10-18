//! `StreamFormatConverter.h`, and the base class half of
//! `StreamFormatConverter.cc` (registry, `parseFormat`, default methods).
//!
//! A converter handles one or more conversion characters. `parse` is called
//! when the protocol is compiled, with `conv`, `flags`, `prec` and `width`
//! already filled in and `source` just after the conversion character; it
//! may take more characters from the format string (moving `source`), write
//! what it needs into `info`, and returns the `StreamFormatType` of the
//! value (`*_FORMAT` in [`crate::format`]), or 0 on a parse error or when
//! the direction is not supported. The `print_*` / `scan_*` method matching
//! the returned type is then called for each output / input, with that info
//! in `fmt.info`; `scan_*` returns the number of input bytes consumed, or
//! -1.
//!
//! The standard converters (`StdLongConverter` and on in
//! `StreamFormatConverter.cc`) are in [`crate::std_converters`] and
//! registered with the table; the converter files are ported on top of this
//! and registered with [`register_converter`].

use std::sync::OnceLock;

use parking_lot::RwLock;

use crate::buffer::StreamBuffer;
use crate::cmsg;
use crate::cstr::{CPtr, c_strtoul};
use crate::error::{Ch, stream_error};
use crate::format::*;
use crate::protocol::FormatType;

/// The `esc` code: the next byte is literal.
pub const ESC: u8 = 0x1b;

pub trait StreamFormatConverter: Send + Sync {
    /// `parse(fmt, info, source, scanFormat)`
    fn parse(
        &self,
        fmt: &StreamFormat,
        info: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        scan_format: bool,
    ) -> i32;

    /// `printLong`
    fn print_long(&self, fmt: &StreamFormat, _output: &mut StreamBuffer, _value: i64) -> bool {
        // The C message puts the newline before "for".
        stream_error(&cmsg!(
            b"Unimplemented printLong method\n for %",
            Ch(fmt.conv as i32),
            b" format"
        ));
        false
    }

    /// `printDouble`
    fn print_double(&self, fmt: &StreamFormat, _output: &mut StreamBuffer, _value: f64) -> bool {
        stream_error(&cmsg!(
            b"Unimplemented printDouble method for %",
            Ch(fmt.conv as i32),
            b" format\n"
        ));
        false
    }

    /// `printString`
    fn print_string(&self, fmt: &StreamFormat, _output: &mut StreamBuffer, _value: &[u8]) -> bool {
        stream_error(&cmsg!(
            b"Unimplemented printString method for %",
            Ch(fmt.conv as i32),
            b" format\n"
        ));
        false
    }

    /// `printPseudo`
    fn print_pseudo(&self, fmt: &StreamFormat, _output: &mut StreamBuffer) -> bool {
        stream_error(&cmsg!(
            b"Unimplemented printPseudo method for %",
            Ch(fmt.conv as i32),
            b" format\n"
        ));
        false
    }

    /// `scanLong`
    fn scan_long(&self, fmt: &StreamFormat, _input: CPtr<'_>, _value: &mut i64) -> isize {
        stream_error(&cmsg!(
            b"Unimplemented scanLong method for %",
            Ch(fmt.conv as i32),
            b" format\n"
        ));
        -1
    }

    /// `scanDouble`
    fn scan_double(&self, fmt: &StreamFormat, _input: CPtr<'_>, _value: &mut f64) -> isize {
        stream_error(&cmsg!(
            b"Unimplemented scanDouble method for %",
            Ch(fmt.conv as i32),
            b" format\n"
        ));
        -1
    }

    /// `scanString`: write at most `value.len()` bytes, the terminating 0x00
    /// included, and set `size` to the bytes written.
    fn scan_string(
        &self,
        fmt: &StreamFormat,
        _input: CPtr<'_>,
        _value: &mut [u8],
        _size: &mut usize,
    ) -> isize {
        stream_error(&cmsg!(
            b"Unimplemented scanString method for %",
            Ch(fmt.conv as i32),
            b" format\n"
        ));
        -1
    }

    /// `scanPseudo`
    fn scan_pseudo(
        &self,
        fmt: &StreamFormat,
        _input_line: &mut StreamBuffer,
        _cursor: &mut usize,
    ) -> isize {
        stream_error(&cmsg!(
            b"Unimplemented scanPseudo method for %",
            Ch(fmt.conv as i32),
            b" format\n"
        ));
        -1
    }
}

/// A registered converter and the name it was registered under.
pub struct Registered {
    pub name: &'static str,
    pub converter: Box<dyn StreamFormatConverter>,
}

type Table = RwLock<[Option<&'static Registered>; 256]>;

fn registered() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [None; 256];
        for (name, provided, converter) in crate::std_converters::standard() {
            provide(&mut table, name, provided, converter);
        }
        for (name, provided, converter) in [
            crate::bcd_converter::registered(),
            crate::regexp_converter::registered(),
        ] {
            provide(&mut table, name, provided, converter);
        }
        provide(
            &mut table,
            "EnumConverter",
            b"{",
            Box::new(crate::enum_converter::EnumConverter),
        );
        provide(
            &mut table,
            "RawConverter",
            b"r",
            Box::new(crate::raw_converter::RawConverter),
        );
        provide(
            &mut table,
            "MantissaExponentConverter",
            b"m",
            Box::new(crate::mantissa_exponent_converter::MantissaExponentConverter),
        );
        provide(
            &mut table,
            "ChecksumConverter",
            b"<",
            Box::new(crate::checksum::ChecksumConverter),
        );
        provide(
            &mut table,
            "BinaryConverter",
            b"bB",
            Box::new(crate::binary::BinaryConverter),
        );
        provide(
            &mut table,
            "TimestampConverter",
            b"T",
            Box::new(crate::timestamp_converter::TimestampConverter),
        );
        provide(
            &mut table,
            "RawFloatConverter",
            b"R",
            Box::new(crate::raw_float_converter::RawFloatConverter),
        );
        RwLock::new(table)
    })
}

fn provide(
    table: &mut [Option<&'static Registered>; 256],
    name: &'static str,
    provided: &[u8],
    converter: Box<dyn StreamFormatConverter>,
) {
    let r: &'static Registered = Box::leak(Box::new(Registered { name, converter }));
    for &c in provided {
        table[c as usize] = Some(r);
    }
}

/// `RegisterConverter(converter, "conversions")` / `provides`: the
/// converter handles each character of `provided`; a later registration of
/// a character replaces the earlier one.
pub fn register_converter(
    name: &'static str,
    provided: &[u8],
    converter: Box<dyn StreamFormatConverter>,
) {
    provide(&mut registered().write(), name, provided, converter);
}

/// `StreamFormatConverter::find`
pub fn find(c: u8) -> Option<&'static Registered> {
    registered().read()[c as usize]
}

/// `StreamFormatConverter::parseFormat`.
///
/// `source := [flags] [width] ['.' prec] conv [extra]`, `source` pointing at
/// the `%` (or at the `)` of a field name) on entry.
pub fn parse_format(
    source: &mut CPtr<'_>,
    format_type: FormatType,
    stream_format: &mut StreamFormat,
    info_string: &mut StreamBuffer,
) -> i32 {
    let mut format_type = format_type;
    // look for flags
    stream_format.flags = 0;
    loop {
        source.advance(1);
        match source.peek() {
            b'-' => stream_format.flags |= LEFT_FLAG,
            b'+' => stream_format.flags |= SIGN_FLAG,
            b' ' => stream_format.flags |= SPACE_FLAG,
            b'#' => stream_format.flags |= ALT_FLAG,
            b'0' => stream_format.flags |= ZERO_FLAG,
            b'*' => {
                if format_type != FormatType::ScanFormat {
                    stream_error(b"Use of skip modifier '*' only allowed in input formats\n");
                    return 0;
                }
                stream_format.flags |= SKIP_FLAG;
            }
            b'?' => {
                if format_type != FormatType::ScanFormat {
                    stream_error(b"Use of default modifier '?' only allowed in input formats\n");
                    return 0;
                }
                stream_format.flags |= DEFAULT_FLAG;
            }
            b'!' => {
                if format_type != FormatType::ScanFormat {
                    stream_error(
                        b"Use of fixed width modifier '!' only allowed in input formats\n",
                    );
                    return 0;
                }
                stream_format.flags |= FIX_WIDTH_FLAG;
            }
            b'=' => {
                if format_type != FormatType::ScanFormat {
                    stream_error(b"Use of compare modifier '=' only allowed in input formats\n");
                    return 0;
                }
                stream_format.flags |= COMPARE_FLAG;
                format_type = FormatType::PrintFormat;
            }
            _ => break,
        }
    }
    // look for width
    let (val, end) = c_strtoul(*source, 10);
    source.set_offset(end);
    if val > i64::MAX as u64 {
        stream_error(format!("Field width {} out of range\n", val as i64).as_bytes());
        return 0;
    }
    stream_format.width = val;
    // look for prec
    stream_format.prec = -1;
    if source.peek() == b'.' {
        source.advance(1);
        let (val, end) = c_strtoul(*source, 10);
        if end == source.offset() {
            crate::debug!(b"source = ", source.cstr(), b"\n");
            stream_error(b"Numeric precision field expected after '.'\n");
            return 0;
        }
        source.set_offset(end);
        if val > i16::MAX as u64 {
            stream_error(format!("Precision {} out of range\n", val as i64).as_bytes());
            return 0;
        }
        stream_format.prec = val as i16 as i64;
    }
    // look for converter
    stream_format.conv = source.peek();
    source.advance(1);
    if stream_format.conv == 0 || b"'\" (.0+-*?=".contains(&stream_format.conv) {
        stream_error(b"Missing converter character\n");
        return 0;
    }
    let Some(converter) = find(stream_format.conv) else {
        stream_error(&cmsg!(
            b"No converter registered for format '%",
            Ch(stream_format.conv as i32),
            b"'\n"
        ));
        return 0;
    };
    // parse format and get info string
    converter.converter.parse(
        stream_format,
        info_string,
        source,
        format_type == FormatType::ScanFormat,
    )
}
