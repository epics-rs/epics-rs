//! A parser client that compiles commands as `StreamCore::compile` and
//! `StreamCore::compileCommand` do, with stand-in converters, so parser
//! tests see the same calls and errors a C stream record sees.

#![allow(dead_code)]

use std::sync::Once;

use stream_rs::StreamBuffer;
use stream_rs::cmsg;
use stream_rs::converter::ESC;
use stream_rs::converter::{StreamFormatConverter, register_converter};
use stream_rs::core::commands;
use stream_rs::cstr::CPtr;
use stream_rs::error::{stream_error, stream_error_at};
use stream_rs::format::*;
use stream_rs::protocol::{self, Client, FormatType, Protocol};

/// A converter that takes nothing after its conversion character.
struct Stub(i32);

impl StreamFormatConverter for Stub {
    fn parse(&self, _: &StreamFormat, _: &mut StreamBuffer, _: &mut CPtr<'_>, _: bool) -> i32 {
        self.0
    }
}

/// The converters that read past their conversion character, consuming
/// what their C `parse` consumes; the info they build is left out.
struct Consuming;

impl StreamFormatConverter for Consuming {
    fn parse(
        &self,
        fmt: &StreamFormat,
        _: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        scan: bool,
    ) -> i32 {
        match fmt.conv {
            // ChecksumConverter
            b'<' => match source.strchr(b'>') {
                Some(p) => {
                    source.set_offset(p + 1);
                    if scan {
                        NEEDS_ORIGINAL_FORMAT
                    } else {
                        PSEUDO_FORMAT
                    }
                }
                None => {
                    stream_error(b"Missing closing '>' in checksum format.\n");
                    0
                }
            },
            // EnumConverter
            b'{' => {
                while source.peek() != 0 {
                    match source.peek() {
                        b'}' => {
                            source.advance(1);
                            return ENUM_FORMAT;
                        }
                        ESC => source.advance(2),
                        _ => source.advance(1),
                    }
                }
                stream_error(b"Missing '}' after %{ format conversion\n");
                0
            }
            // StdCharsetConverter
            b'[' => {
                if source.peek() == b'^' {
                    source.advance(1);
                }
                if source.peek() == b']' {
                    source.advance(1);
                }
                while source.peek() != 0 && source.peek() != b']' {
                    source.advance(if source.peek() == ESC { 2 } else { 1 });
                }
                if source.peek() == 0 {
                    stream_error(b"Missing ']' after %[ format conversion\n");
                    return 0;
                }
                source.advance(1);
                STRING_FORMAT
            }
            // BinaryConverter's %B: the next two characters
            b'B' => {
                // The test is before the escape: an escaped 0x00 is taken.
                for _ in 0..2 {
                    if source.peek() == 0 {
                        stream_error(b"Missing characters after %B format conversion\n");
                        return 0;
                    }
                    if source.peek() == ESC {
                        source.advance(1);
                    }
                    source.advance(1);
                }
                UNSIGNED_FORMAT
            }
            // TimestampConverter
            b'T' => {
                if source.peek() == b'(' {
                    source.advance(1);
                    while source.peek() != b')' {
                        if source.peek() == 0 {
                            stream_error(b"missing terminating ')'\n");
                            return 0;
                        }
                        source.advance(if source.peek() == ESC { 2 } else { 1 });
                    }
                    source.advance(1);
                }
                DOUBLE_FORMAT
            }
            _ => unreachable!(),
        }
    }
}

/// Register the stand-ins for C's converters (`%q` stays unregistered,
/// as does `%/`, PCRE not being built in the C oracle) and the error output
/// settings of the C oracle run: no colour, no timestamp, thread `_main_`.
pub fn setup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_converter("Long", b"diouxXc", Box::new(Stub(SIGNED_FORMAT)));
        register_converter("Double", b"feEgG", Box::new(Stub(DOUBLE_FORMAT)));
        register_converter("String", b"s", Box::new(Stub(STRING_FORMAT)));
        register_converter("Binary", b"b", Box::new(Stub(UNSIGNED_FORMAT)));
        register_converter("Raw", b"rD", Box::new(Stub(SIGNED_FORMAT)));
        register_converter("Double", b"Rm", Box::new(Stub(DOUBLE_FORMAT)));
        register_converter("Consuming", b"<{[BT", Box::new(Consuming));
        stream_rs::error::set_stream_msg_time_stamped(0);
        stream_rs::error::set_stream_debug_colored(false);
        stream_rs::error::set_get_thread_name_function(Some(|| Some("_main_".into())));
    });
}

/// The compiled protocol of one record.
#[derive(Default)]
pub struct Compiled {
    pub ignore_extra_input: bool,
    pub lock_timeout: u64,
    pub read_timeout: u64,
    pub reply_timeout: u64,
    pub write_timeout: u64,
    pub max_input: u64,
    pub poll_period: u64,
    pub in_terminator: StreamBuffer,
    pub out_terminator: StreamBuffer,
    pub separator: StreamBuffer,
    pub commands: StreamBuffer,
    pub on_init: StreamBuffer,
    pub on_write_timeout: StreamBuffer,
    pub on_reply_timeout: StreamBuffer,
    pub on_read_timeout: StreamBuffer,
    pub on_mismatch: StreamBuffer,
}

/// A record; `records` are the other records `%(NAME.VAL)` and
/// `%(NAME.RVAL)` can name, as `ai` records in the C oracle.
#[derive(Default)]
pub struct Rec {
    pub records: Vec<String>,
    /// Each non-empty command list compiled, with its handler (`(null)`
    /// for the protocol's own), as C's `getCommands` debug output has it.
    pub compiled: Vec<(String, Vec<u8>)>,
}

impl Client for Rec {
    fn compile_command(
        &mut self,
        protocol: &mut Protocol,
        buffer: &mut StreamBuffer,
        command: CPtr<'_>,
        args: &mut CPtr<'_>,
    ) -> bool {
        let name = command.cstr();
        let string = |code,
                      ft,
                      buffer: &mut StreamBuffer,
                      protocol: &mut Protocol,
                      args: &mut CPtr<'_>,
                      me: &mut Rec| {
            buffer.append_byte(code);
            if !protocol.compile_string(buffer, args, ft, Some(me), 0) {
                return false;
            }
            buffer.append_byte(protocol::codes::EOS);
            true
        };
        let number =
            |code, buffer: &mut StreamBuffer, protocol: &mut Protocol, args: &mut CPtr<'_>| {
                buffer.append_byte(code);
                let mut timeout = 0u64;
                if !protocol.compile_number(&mut timeout, args, 0xFFFF_FFFF) {
                    return false;
                }
                buffer.append(&timeout.to_ne_bytes());
                true
            };
        match name {
            b"in" => string(
                commands::IN,
                FormatType::ScanFormat,
                buffer,
                protocol,
                args,
                self,
            ),
            b"out" => string(
                commands::OUT,
                FormatType::PrintFormat,
                buffer,
                protocol,
                args,
                self,
            ),
            b"exec" => string(
                commands::EXEC,
                FormatType::PrintFormat,
                buffer,
                protocol,
                args,
                self,
            ),
            b"wait" => number(commands::WAIT, buffer, protocol, args),
            b"connect" => number(commands::CONNECT, buffer, protocol, args),
            b"disconnect" => {
                buffer.append_byte(commands::DISCONNECT);
                true
            }
            _ => {
                // No bus here supports events: the C record on an IP port
                // says the same.
                let msg = if name == b"event" {
                    b"Events not supported by businterface.\n".to_vec()
                } else {
                    cmsg!(b"Unknown command name '", name, b"'\n")
                };
                stream_error_at(command.line_number(), protocol.filename(), &msg);
                false
            }
        }
    }

    /// `Stream::getFieldAddress`: `RECORD.FIELD`, else a field of this
    /// (stringout) record, else `FIELD.VAL`.
    fn get_field_address(&mut self, fieldname: &[u8], address: &mut StreamBuffer) -> bool {
        let name = String::from_utf8_lossy(fieldname);
        let found = match name.split_once('.') {
            Some((rec, field)) => {
                self.records.iter().any(|r| r == rec) && (field == "VAL" || field == "RVAL")
            }
            None => name == "VAL" || self.records.iter().any(|r| *r == name),
        };
        if found {
            address.append(fieldname);
        }
        found
    }

    fn name(&self) -> String {
        "rec".into()
    }
}

/// `StreamCore::compile`: the variables, then the commands and handlers,
/// then `checkUnused`; the first failure ends it.
pub fn compile(protocol: &mut Protocol, rec: &mut Rec) -> Option<Compiled> {
    let mut c = Compiled {
        lock_timeout: 5000,
        read_timeout: 100,
        reply_timeout: 1000,
        write_timeout: 100,
        poll_period: 1000,
        ..Default::default()
    };
    let mut extra = 0u16;
    if !protocol.get_enum_variable(b"extrainput", &mut extra, &[b"error", b"ignore"]) {
        return None;
    }
    c.ignore_extra_input = extra != 0;
    let max = 0xFFFF_FFFF;
    let numbers_ok = protocol.get_number_variable(b"locktimeout", &mut c.lock_timeout, max)
        && protocol.get_number_variable(b"readtimeout", &mut c.read_timeout, max)
        && protocol.get_number_variable(b"replytimeout", &mut c.reply_timeout, max)
        && protocol.get_number_variable(b"writetimeout", &mut c.write_timeout, max)
        && protocol.get_number_variable(b"maxinput", &mut c.max_input, max)
        && protocol.get_number_variable(b"replytimeout", &mut c.poll_period, max)
        && protocol.get_number_variable(b"pollperiod", &mut c.poll_period, max);
    if !numbers_ok {
        return None;
    }
    let (mut in_defined, mut out_defined) = (false, false);
    let strings_ok =
        protocol.get_string_variable(b"interminator", &mut c.in_terminator, Some(&mut in_defined))
            && protocol.get_string_variable(
                b"outterminator",
                &mut c.out_terminator,
                Some(&mut out_defined),
            )
            && (in_defined
                || protocol.get_string_variable(
                    b"terminator",
                    &mut c.in_terminator,
                    Some(&mut in_defined),
                ))
            && (out_defined
                || protocol.get_string_variable(
                    b"terminator",
                    &mut c.out_terminator,
                    Some(&mut out_defined),
                ))
            && protocol.get_string_variable(b"separator", &mut c.separator, None);
    if !strings_ok {
        return None;
    }
    let handlers: [(Option<&[u8]>, &mut StreamBuffer); 6] = [
        (None, &mut c.commands),
        (Some(b"@init"), &mut c.on_init),
        (Some(b"@writetimeout"), &mut c.on_write_timeout),
        (Some(b"@replytimeout"), &mut c.on_reply_timeout),
        (Some(b"@readtimeout"), &mut c.on_read_timeout),
        (Some(b"@mismatch"), &mut c.on_mismatch),
    ];
    for (handler, code) in handlers {
        if !protocol.get_commands(handler, code, rec) {
            return None;
        }
        if code.length() > 0 {
            let name = handler.map_or("(null)".into(), |h| String::from_utf8_lossy(h).into_owned());
            rec.compiled.push((name, code.bytes().to_vec()));
        }
    }
    protocol.check_unused().then_some(c)
}

/// `StreamCore::parseLink`'s protocol argument: the name, then each
/// comma-separated parameter, 0x00-separated.
pub fn protocol_and_params(name: &str, params: &[&str]) -> StreamBuffer {
    let mut b = StreamBuffer::new();
    b.append(name.as_bytes());
    for p in params {
        b.append_byte(0);
        b.append(p.as_bytes());
    }
    b
}

/// Load and compile one protocol as a record would; `protocol` is the
/// link's `name` or `name(param,...)`.
pub fn load(file: &str, protocol: &str, rec: &mut Rec) -> Option<Compiled> {
    let (name, params) = match protocol.strip_suffix(')').and_then(|p| p.split_once('(')) {
        Some((name, params)) => (name, params.split(',').collect()),
        None => (protocol, Vec::new()),
    };
    let mut p = protocol::get_protocol(file, &protocol_and_params(name, &params))?;
    compile(&mut p, rec)
}
