//! `StreamProtocol.cc` / `StreamProtocol.h`: the `.proto` file parser.
//!
//! Step 1 reads a whole file into protocols, handlers and variable
//! assignments, replacing protocol references and global variable
//! references. Step 2 ([`Protocol`]'s `get*` methods, called by the client
//! on a copy of one protocol) replaces positional parameters and compiles
//! numbers, strings, formats and, through the client, commands.
//!
//! Parsed values keep the C encoding, which the compiled code and the
//! client's `compile_command` read: every word and quoted string is followed
//! by a 0x00 and the native `int` line number it started on; single special
//! characters (`,`, ` `, `+` ...) are stored bare; a command is its name
//! token followed by its argument tokens and a 0x00. Compiled strings use
//! [`codes`] for the byte that follows an escape-free run, and formats
//! compile to
//! `<format> formatstr <eos> StreamFormat [info <eos>]` or
//! `<format_field> field <eos> addrLength AddressStructure formatstr <eos>
//! StreamFormat [info <eos>]`.

use std::io::{Read, Write};
use std::sync::OnceLock;

use parking_lot::{Mutex, RwLock};

use crate::buffer::StreamBuffer;
use crate::converter::{ESC, parse_format};
use crate::cstr::*;
use crate::error::{Ch, stream_error, stream_error_at};
use crate::format::*;
use crate::{cmsg, debug, debug2};

/// `ENUM (FormatType, ...)`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatType {
    NoFormat,
    ScanFormat,
    PrintFormat,
}

impl FormatType {
    /// `toStr`
    pub fn to_str(self) -> &'static str {
        match self {
            Self::NoFormat => "NoFormat",
            Self::ScanFormat => "ScanFormat",
            Self::PrintFormat => "PrintFormat",
        }
    }
}

/// `StreamProtocolParser::Codes`: the bytes compiled strings use.
pub mod codes {
    pub const EOS: u8 = 0;
    pub const SKIP: u8 = 1;
    pub const WHITESPACE: u8 = 2;
    pub const FORMAT: u8 = 3;
    pub const FORMAT_FIELD: u8 = 4;
    pub const LAST_FUNCTION_CODE: u8 = 5;
}
use codes::*;

const EOF: i32 = -1;

static SPECIAL_CHARS: &[u8] = b" ,;{}=()$'\"+-*/";

/// `StreamProtocolParser::Client`: compiles the commands of a protocol.
pub trait Client {
    /// `compileCommand`: append the code for `command` (a token pointer, so
    /// `command.line_number()` is its line) to `buffer`, consuming its
    /// arguments from `args`. On return `args` must point at the 0x00 that
    /// ends the command.
    fn compile_command(
        &mut self,
        protocol: &mut Protocol,
        buffer: &mut StreamBuffer,
        command: CPtr<'_>,
        args: &mut CPtr<'_>,
    ) -> bool;
    /// `getFieldAddress`; see [`crate::core`].
    fn get_field_address(&mut self, fieldname: &[u8], address: &mut StreamBuffer) -> bool;
    fn name(&self) -> String;
}

#[derive(Clone)]
struct Variable {
    name: StreamBuffer,
    value: StreamBuffer,
    line: i32,
    used: bool,
}

impl Variable {
    fn new(name: &[u8], line: i32, startsize: usize) -> Self {
        Self {
            name: StreamBuffer::from_bytes(name),
            value: StreamBuffer::with_size(startsize),
            line,
            used: false,
        }
    }
}

/// `StreamProtocolParser::Protocol`. The first variable holds the commands.
#[derive(Clone)]
pub struct Protocol {
    variables: Vec<Variable>,
    protocolname: StreamBuffer,
    line: i32,
    /// `$0` ... `$9`: offsets into `protocolname`, whose 0x00-separated
    /// parts are the protocol name and its arguments.
    parameter: [Option<usize>; 10],
    pub filename: StreamBuffer,
}

fn line_bytes(l: i32) -> [u8; LINE_SIZE] {
    l.to_ne_bytes()
}

/// Bytes `from..to` of the buffer under `p`, reading on into the zero tail
/// as C does.
fn c_range(p: CPtr<'_>, from: usize, to: usize) -> Vec<u8> {
    (from..to.max(from))
        .map(|i| CPtr::at_offset(p.base(), i).peek())
        .collect()
}

fn back(p: CPtr<'_>, n: usize) -> CPtr<'_> {
    CPtr::at_offset(p.base(), p.offset() - n)
}

impl Protocol {
    /// `Protocol(const char* filename)`: the global settings.
    fn new_global(filename: &[u8]) -> Self {
        Self {
            variables: vec![Variable::new(b"", 0, 500)],
            protocolname: StreamBuffer::new(),
            line: 0,
            parameter: [None; 10],
            filename: StreamBuffer::from_bytes(filename),
        }
    }

    /// `Protocol(const Protocol& p, StreamBuffer& name, int line)`: a deep
    /// copy named `name`, whose 0x00-separated parts set `$0`...`$9`.
    fn copy_from(p: &Protocol, name: &StreamBuffer, line: i32) -> Self {
        let line = if line != 0 { line } else { p.line };
        debug!(
            b"new Protocol(name=\"",
            name.cstr(),
            format!("\", line={line})\n")
        );
        let mut protocol = Self {
            variables: p.variables.clone(),
            protocolname: name.clone(),
            line,
            parameter: [None; 10],
            filename: p.filename.clone(),
        };
        if line != 0 {
            protocol.variables[0].line = line;
        }
        // get parameters from name
        protocol.parameter[0] = Some(0);
        for i in 0..9 {
            let cur = protocol.parameter[i].unwrap();
            if i != 0 {
                debug!(
                    format!("StreamProtocolParser::Protocol::Protocol ${i}=\""),
                    protocol.protocolname.ptr(cur as isize).cstr(),
                    b"\"\n"
                );
            }
            let next = cur + protocol.protocolname.ptr(cur as isize).strlen() + 1;
            if next > name.length() {
                break;
            }
            protocol.parameter[i + 1] = Some(next);
        }
        protocol
    }

    /// The protocol name, without its arguments.
    pub fn protocolname(&self) -> &[u8] {
        self.protocolname.cstr()
    }

    pub fn filename(&self) -> &[u8] {
        self.filename.bytes()
    }

    /// `report()`
    pub fn report(&self) {
        let mut out = Vec::new();
        if self.protocolname.is_set() {
            out.extend(cmsg!(b"  Protocol ", self.protocolname.expand(), b"\n"));
        }
        out.extend_from_slice(b"    Variables:\n");
        for v in &self.variables[1..] {
            if v.name.get(0) != b'@' {
                out.extend(cmsg!(
                    b"    ",
                    v.name.expand(),
                    b" = ",
                    v.value.expand(),
                    b";\n"
                ));
            }
        }
        out.extend_from_slice(b"    Handlers:\n");
        for v in &self.variables[1..] {
            if v.name.get(0) == b'@' {
                out.extend(cmsg!(
                    b"    ",
                    v.name.expand(),
                    b" {",
                    v.value.expand(),
                    b"}\n"
                ));
            }
        }
        out.extend_from_slice(b"    Commands:\n");
        out.extend(cmsg!(b"     { ", self.variables[0].value.expand(), b" }\n"));
        let _ = std::io::stdout().write_all(&out);
    }

    /// `createVariable`: the variable `name`, created if new; its line is
    /// set to `linenr` either way.
    fn create_variable(&mut self, name: &[u8], linenr: i32) -> usize {
        if let Some(i) = self
            .variables
            .iter()
            .position(|v| v.name.startswith(Some(name)))
        {
            self.variables[i].line = linenr;
            return i;
        }
        self.variables.push(Variable::new(name, linenr, 0));
        self.variables.len() - 1
    }

    /// `getVariable`: marks the variable used. `None` is the commands.
    fn get_variable(&mut self, name: Option<&[u8]>) -> Option<usize> {
        let i = self
            .variables
            .iter()
            .position(|v| v.name.startswith(name))?;
        self.variables[i].used = true;
        Some(i)
    }

    /// `getNumberVariable(varname, value, max = 0xFFFFFFFF)`: leaves `value`
    /// alone when the variable is not set.
    pub fn get_number_variable(&mut self, varname: &[u8], value: &mut u64, max: u64) -> bool {
        let Some(i) = self.get_variable(Some(varname)) else {
            return true;
        };
        let pvalue = self.variables[i].value.clone();
        let mut source = pvalue.ptr(0);
        if !self.compile_number(value, &mut source, max) {
            let linenr = source.line_number();
            stream_error_at(
                linenr,
                self.filename.bytes(),
                &cmsg!(b"in variable ", varname, b"\n"),
            );
            return false;
        }
        if source.offset() != pvalue.length() {
            stream_error_at(
                source.line_number(),
                self.filename.bytes(),
                &cmsg!(
                    b"Garbage in variable '",
                    varname,
                    format!("' after numeric value {}: ", *value as i64),
                    source.cstr(),
                    b"\n"
                ),
            );
            return false;
        }
        true
    }

    /// `getEnumVariable`: `value` is the index of the matching string.
    pub fn get_enum_variable(
        &mut self,
        varname: &[u8],
        value: &mut u16,
        enumstrings: &[&[u8]],
    ) -> bool {
        let Some(i) = self.get_variable(Some(varname)) else {
            return true;
        };
        let pvar = &self.variables[i];
        for (k, e) in enumstrings.iter().enumerate() {
            *value = k as u16;
            if pvar.value.startswith(Some(e)) {
                return true;
            }
        }
        stream_error(&cmsg!(b"Value '", pvar.value.cstr(), b"' must be one of"));
        for (k, e) in enumstrings.iter().enumerate() {
            stream_error(&cmsg!(if k != 0 { " or" } else { "" }, b" '", e, b"'"));
        }
        *value = enumstrings.len() as u16;
        stream_error(&cmsg!(
            b"\nin variable '",
            varname,
            b"' in protocol file '",
            self.filename,
            format!("' line {}\n", pvar.value.ptr(0).line_number())
        ));
        false
    }

    /// `getStringVariable`: `value` is cleared, then compiled from the
    /// variable when it is set (`defined` then set to true).
    pub fn get_string_variable(
        &mut self,
        varname: &[u8],
        value: &mut StreamBuffer,
        defined: Option<&mut bool>,
    ) -> bool {
        value.clear();
        let Some(i) = self.get_variable(Some(varname)) else {
            return true;
        };
        if let Some(d) = defined {
            *d = true;
        }
        let pvalue = self.variables[i].value.clone();
        let mut source = pvalue.ptr(0);
        if !self.compile_string(value, &mut source, FormatType::NoFormat, None, 0) {
            stream_error(&cmsg!(
                b"in string variable '",
                varname,
                b"' in protocol file '",
                self.filename,
                format!("' line {}\n", source.line_number())
            ));
            debug!(varname, b" = ", pvalue.expand(), b"\n");
            return false;
        }
        if source.offset() != pvalue.length() {
            debug!(varname, b" = ", pvalue.expand(), b"\n");
            debug!(b"  => ", value.expand(), b"\n");
            let base = pvalue.bytes().as_ptr();
            stream_error(&cmsg!(
                b"INTERNAL ERROR after '",
                varname,
                format!(
                    "': source = {:p} != {:p}\n",
                    base.wrapping_add(source.offset()),
                    base.wrapping_add(pvalue.length())
                )
            ));
            return false;
        }
        true
    }

    /// `getCommands(handlername, code, client)`: `None` is the protocol's
    /// own commands, else a handler such as `@init`.
    pub fn get_commands(
        &mut self,
        handlername: Option<&[u8]>,
        code: &mut StreamBuffer,
        client: &mut dyn Client,
    ) -> bool {
        code.clear();
        let Some(i) = self.get_variable(handlername) else {
            return true;
        };
        if !self.variables[i].value.is_set() {
            return true;
        }
        let pvalue = self.variables[i].value.clone();
        let mut source = pvalue.ptr(0);
        debug2!(
            b"StreamProtocolParser::Protocol::getCommands(handlername=\"",
            handlername.unwrap_or(b"(null)".as_slice()),
            b"\", client=\"",
            client.name(),
            b"\"): source=",
            pvalue.expand(),
            b"\n"
        );
        if !self.compile_commands(code, &mut source, client) {
            let line = self.variables[i].line;
            if let Some(h) = handlername {
                stream_error_at(
                    line,
                    self.filename.bytes(),
                    &cmsg!(b"in handler '", h, b"'\n"),
                );
                stream_error_at(
                    self.variables[0].line,
                    self.filename.bytes(),
                    &cmsg!(b"used by protocol '", self.protocolname.cstr(), b"'\n"),
                );
                return false;
            }
            stream_error_at(
                line,
                self.filename.bytes(),
                &cmsg!(b"in protocol '", self.protocolname.cstr(), b"'\n"),
            );
            return false;
        }
        debug2!(
            b"commands ",
            handlername.unwrap_or(b"(null)".as_slice()),
            b": ",
            pvalue.expand(),
            b"\n"
        );
        debug2!(b"compiled to: ", code.expand(), b"\n");
        true
    }

    /// `replaceVariable`: append the value of the variable or parameter
    /// whose `$` token `varname` points at.
    fn replace_variable(&mut self, buffer: &mut StreamBuffer, varname: CPtr<'_>) -> bool {
        debug2!(
            b"StreamProtocolParser::Protocol::replaceVariable ",
            varname.cstr(),
            b"\n"
        );
        let mut varname = varname.plus(1);
        let mut quoted = false;
        if varname.peek() == b'"' {
            varname.advance(1);
            quoted = true;
        }
        let linenr = varname.line_number();
        let d = varname.peek();
        if d.is_ascii_digit() {
            let Some(p) = self.parameter[(d - b'0') as usize] else {
                stream_error_at(
                    linenr,
                    self.filename.bytes(),
                    &cmsg!(b"Missing value for parameter $", Ch(d as i32), b"\n"),
                );
                return false;
            };
            let p = self.protocolname.ptr(p as isize).cstr();
            if !quoted {
                buffer.append(p).append_byte(0);
                buffer.append(&line_bytes(linenr));
                return true;
            }
            buffer.append_byte(b'"');
            let mut escaped = false;
            for &c in p {
                if c == b'"' && !escaped {
                    buffer.append_byte(b'\\');
                }
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                }
                buffer.append_byte(c);
            }
            buffer.append_byte(b'"').append_byte(0);
            buffer.append(&line_bytes(linenr));
            return true;
        }
        let Some(i) = self.get_variable(Some(varname.cstr())) else {
            stream_error_at(
                linenr,
                self.filename.bytes(),
                &cmsg!(b"Undefined variable '", varname.cstr(), b"' referenced\n"),
            );
            return false;
        };
        let v = &self.variables[i];
        if !quoted {
            buffer.append_buffer(&v.value);
            return true;
        }
        // quoted
        buffer.append_byte(b'"');
        let mut escaped = false;
        let mut k = 0;
        while k < v.value.length() {
            let c = v.value.get(k as isize);
            if c == b'"' && !escaped {
                buffer.append_byte(b'\\');
            }
            if c == 0 && !escaped {
                // skip line info of token
                k += 1 + LINE_SIZE;
                continue;
            }
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            }
            buffer.append_byte(c);
            k += 1;
        }
        buffer.append_byte(b'"').append_byte(0);
        buffer.append(&line_bytes(v.line));
        true
    }

    /// `compileNumber(number, source, max = 0xFFFFFFFF)`
    pub fn compile_number(&mut self, number: &mut u64, source: &mut CPtr<'_>, max: u64) -> bool {
        let mut buffer = StreamBuffer::new();
        debug2!(
            b"StreamProtocolParser::Protocol::compileNumber source=\"",
            source.cstr(),
            b"\"\n"
        );
        while source.peek() == b'$' || source.peek().is_ascii_digit() {
            debug2!(
                format!(
                    "StreamProtocolParser::Protocol::compileNumber *source={} source=\"",
                    source.peek()
                ),
                source.cstr(),
                b"\"\n"
            );
            if source.peek() == b'$' {
                if !self.replace_variable(&mut buffer, *source) {
                    return false;
                }
                debug2!(b"buffer=", buffer.expand(), b"\n");
                buffer.truncate(-1 - LINE_SIZE as isize);
            } else {
                buffer.append_cstr(*source);
            }
            source.skip_token(); // skip eos + line
        }
        let (n, end) = c_strtoul(buffer.ptr(0), 0);
        if end == 0 {
            debug!(
                b"StreamProtocolParser::Protocol::compileNumber: ",
                buffer.expand(),
                b"\n"
            );
            stream_error_at(
                source.line_number(),
                self.filename.bytes(),
                &cmsg!(b"Unsigned numeric value expected: ", buffer.cstr(), b"\n"),
            );
            return false;
        }
        if buffer.get(end as isize) != 0 {
            debug!(
                b"StreamProtocolParser::Protocol::compileNumber: ",
                buffer.expand(),
                b"\n"
            );
            stream_error_at(
                source.line_number(),
                self.filename.bytes(),
                &cmsg!(b"Garbage after numeric value: ", buffer.cstr(), b"\n"),
            );
            return false;
        }
        if n > max {
            debug!(
                b"StreamProtocolParser::Protocol::compileNumber: ",
                buffer.expand(),
                b"\n"
            );
            stream_error_at(
                source.line_number(),
                self.filename.bytes(),
                &cmsg!(
                    b"Value ",
                    buffer.cstr(),
                    format!(" out of range [0...{}]\n", max as i64)
                ),
            );
            return false;
        }
        *number = n;
        debug2!(
            b"StreamProtocolParser::Protocol::compileNumber ",
            buffer.cstr(),
            format!(" = {n}\n")
        );
        true
    }

    /// `compileString(buffer, source, formatType = NoFormat, client = NULL,
    /// quoted = false)`: read a string (quoted or numerical byte values)
    /// from source and append it coded to buffer, replacing all variable
    /// references and formats.
    pub fn compile_string<'c>(
        &mut self,
        buffer: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        format_type: FormatType,
        client: Option<&mut (dyn Client + 'c)>,
        quoted: u8,
    ) -> bool {
        self.compile_string_at(buffer, source, format_type, client, quoted, 0)
    }

    fn compile_string_at<'c>(
        &mut self,
        buffer: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        format_type: FormatType,
        mut client: Option<&mut (dyn Client + 'c)>,
        quoted: u8,
        recursion_depth: i32,
    ) -> bool {
        let mut quoted = quoted;
        let mut escaped = false;
        let mut newline = 0;
        let mut formatbuffer = StreamBuffer::new();
        let mut formatpos = buffer.length();
        self.line = source.line_number();

        debug2!(
            format!(
                "StreamProtocolParser::Protocol::compileString line {} source=\"",
                self.line
            ),
            source.cstr(),
            format!(
                "\" formatType={} quoted={} recursionDepth={}\n",
                format_type.to_str(),
                quoted,
                recursion_depth
            )
        );

        // coding is done in two steps:
        // 1) read a line from protocol source and code quoted strings,
        //    numerical bytes, tokens, etc, and replace variables and parameters
        // 2) replace the formats in the line
        // thus variables can be replaced inside the format info string

        loop {
            // this is step 2: replacing the formats
            let at_end = source.peek() == 0;
            if !at_end {
                newline = source.line_number();
            }
            if at_end || newline != self.line {
                debug2!(
                    format!(
                        "StreamProtocolParser::Protocol::compileString line {}: ",
                        self.line
                    ),
                    buffer.expand(),
                    b"\n"
                );
                // compile all formats in this line
                // We do this here after all variables in this line
                // have been replaced and after string has been coded.
                if recursion_depth == 0 && format_type != FormatType::NoFormat {
                    let mut nformats = 0;
                    while formatpos < buffer.length() && buffer.get(formatpos as isize) != 0 {
                        let c = buffer.get(formatpos as isize);
                        if c == ESC {
                            // ignore escaped %
                            formatpos += 2;
                            continue;
                        }
                        if c == b'%' {
                            if buffer.get(formatpos as isize + 1) == b'%' {
                                // treat %% as literal % like printf/scanf do
                                // replace with escaped %
                                buffer.set_byte(formatpos as isize, ESC);
                                formatpos += 2;
                                continue;
                            }
                            debug2!(
                                b"StreamProtocolParser::Protocol::compileString format=\"",
                                buffer.expand_from(formatpos as isize),
                                b"\"\n"
                            );
                            nformats += 1;
                            formatbuffer.clear();
                            let mut p = buffer.ptr(formatpos as isize);
                            if !self.compile_format(
                                &mut formatbuffer,
                                &mut p,
                                format_type,
                                client.as_deref_mut(),
                            ) {
                                formatbuffer.clear();
                                print_string(&mut formatbuffer, buffer.ptr(formatpos as isize));
                                stream_error_at(
                                    self.line,
                                    self.filename.bytes(),
                                    &cmsg!(b"in format string: \"", formatbuffer.cstr(), b"\"\n"),
                                );
                                return false;
                            }
                            let formatlen = p.offset() - formatpos;
                            buffer.replace(
                                formatpos as isize,
                                formatlen as isize,
                                formatbuffer.bytes(),
                            );
                            debug2!(
                                b"StreamProtocolParser::Protocol::compileString replaced by: \"",
                                buffer.expand_from(formatpos as isize),
                                b"\"\n"
                            );
                            formatpos += formatbuffer.length();
                            continue;
                        }
                        formatpos += 1;
                    }
                    debug2!(format!(
                        "StreamProtocolParser::Protocol::compileString {} formats found in line {}\n",
                        nformats, self.line
                    ));
                }
                if at_end {
                    break;
                }
                self.line = newline;
            }
            // this is step 1: coding the string
            let c0 = source.peek();
            if (c0 & 0x7f) < 0x20 {
                stream_error(format!("Unexpected byte {}\n", fmt_alt_hex04(c0 as u32)).as_bytes());
                return false;
            }
            if escaped {
                // char after \ in quoted text
                escaped = false;
                let c: u32;
                let mut n = 1;
                match c0 {
                    b'$' => {
                        // can't be: readToken would have made a token from this
                        stream_error_at(
                            self.line,
                            self.filename.bytes(),
                            b"INTERNAL ERROR: unconverted \\$ in quoted string\n",
                        );
                        return false;
                    }
                    b'?' => {
                        buffer.append_byte(SKIP);
                        source.advance(1);
                        continue;
                    }
                    b'_' => {
                        buffer.append_byte(WHITESPACE);
                        source.advance(1);
                        continue;
                    }
                    b'a' => c = 7,
                    b'b' => c = 8,
                    b't' => c = 9,
                    b'n' => c = b'\n' as u32,
                    b'r' => c = b'\r' as u32,
                    b'e' => c = ESC as u32,
                    b'0' => {
                        // octal numbers (max 4 digits)
                        (c, n) = c_sscanf_digits(*source, 4, 8);
                        if c > 0xFF {
                            stream_error_at(
                                self.line,
                                self.filename.bytes(),
                                &cmsg!(
                                    format!(
                                        "Octal number {} does not fit in byte: \"",
                                        fmt_alt_octal(c)
                                    ),
                                    back(*source, 1).cstr(),
                                    b"\"\n"
                                ),
                            );
                            return false;
                        }
                    }
                    b'x' => {
                        // hex numbers (max 2 digits after x)
                        let Some((v, k)) = c_sscanf_hex(source.plus(1), 2) else {
                            stream_error_at(
                                self.line,
                                self.filename.bytes(),
                                &cmsg!(
                                    b"Hex digit expected after \\x: \"",
                                    back(*source, 1).cstr(),
                                    b"\"\n"
                                ),
                            );
                            return false;
                        };
                        c = v;
                        n = k + 1;
                    }
                    b'1'..=b'9' => {
                        // decimal numbers (max 3 digits)
                        (c, n) = c_sscanf_digits(*source, 3, 10);
                        if c > 0xFF {
                            stream_error_at(
                                self.line,
                                self.filename.bytes(),
                                &cmsg!(
                                    format!("Decimal number {c} does not fit in byte: \""),
                                    back(*source, 1).cstr(),
                                    b"\"\n"
                                ),
                            );
                            return false;
                        }
                    }
                    other => c = other as u32, // escaped literals
                }
                source.advance(n);
                if format_type != FormatType::NoFormat {
                    buffer.append_byte(ESC);
                }
                buffer.append_byte(c as u8);
                continue;
            }
            if quoted != 0 {
                // look for ending quotes and escapes
                match c0 {
                    b'\\' => escaped = true, // escape next character
                    b'"' | b'\'' if c0 == quoted => {
                        // ending quote
                        source.advance(1 + LINE_SIZE); // skip line number
                        quoted = 0;
                    }
                    _ => {
                        buffer.append_byte(c0); // literal character
                    }
                }
                source.advance(1);
                continue;
            }
            match c0 {
                b'$' => {
                    // parameter or variable
                    let mut value = StreamBuffer::new();
                    if !self.replace_variable(&mut value, *source) {
                        return false;
                    }
                    source.skip_token();
                    let mut p = value.ptr(0);
                    let saveline = self.line;
                    if !self.compile_string_at(
                        buffer,
                        &mut p,
                        format_type,
                        client.as_deref_mut(),
                        0,
                        recursion_depth + 1,
                    ) {
                        return false;
                    }
                    self.line = saveline;
                    continue;
                }
                b'\'' | b'"' => {
                    // starting quotes
                    quoted = c0;
                    source.advance(1);
                    continue;
                }
                b' ' | b',' => {
                    // skip whitespace; treat comma as whitespace
                    source.advance(1);
                    continue;
                }
                _ => {}
            }
            // try numeric byte value
            let (v, end) = c_strtol(*source, 0);
            if end != source.offset() {
                let c = v as i32;
                if CPtr::at_offset(source.base(), end).peek() != 0 {
                    stream_error_at(
                        self.line,
                        self.filename.bytes(),
                        &cmsg!(b"Garbage after numeric source: ", source.cstr()),
                    );
                    return false;
                }
                if !(-0x80..=0xFF).contains(&c) {
                    stream_error_at(
                        self.line,
                        self.filename.bytes(),
                        &cmsg!(b"Value ", source.cstr(), b" does not fit in byte\n"),
                    );
                    return false;
                }
                if format_type != FormatType::NoFormat {
                    buffer.append_byte(ESC);
                }
                buffer.append_byte(c as u8);
                source.set_offset(end + 1 + LINE_SIZE);
                continue;
            }
            // try constant token
            let token = source.cstr();
            if let Some(i) = CODES.iter().position(|(name, _)| *name == token) {
                let c = CODES[i].1;
                if c == SKIP && format_type != FormatType::ScanFormat {
                    stream_error_at(
                        self.line,
                        self.filename.bytes(),
                        &cmsg!(b"Use of '", token, b"' only allowed in input formats\n"),
                    );
                    return false;
                }
                // C escapes from index 3 on (`i > 2`, StreamProtocol.cc
                // compileString), so `nul` went unescaped into a format
                // string and ended it there; only the two skip entries are
                // meant to stay bare.
                if format_type != FormatType::NoFormat && i > 1 {
                    buffer.append_byte(ESC);
                }
                buffer.append_byte(c);
                source.skip_token();
                continue;
            }
            // source may contain a function name
            stream_error_at(
                self.line,
                self.filename.bytes(),
                &cmsg!(b"Unexpected '", token, b"' in string\n"),
            );
            return false;
        }
        debug2!(
            b"StreamProtocolParser::Protocol::compileString buffer=",
            buffer.expand(),
            b"\n"
        );
        true
    }

    /// `compileFormat`:
    /// `formatstr := '%' ['(' field ')'] [flags] [width] ['.' prec] conv [extra]`.
    fn compile_format<'c>(
        &mut self,
        buffer: &mut StreamBuffer,
        formatstr: &mut CPtr<'_>,
        format_type: FormatType,
        client: Option<&mut (dyn Client + 'c)>,
    ) -> bool {
        let mut source = *formatstr;
        let mut stream_format = StreamFormat::default();
        let mut fieldname = 0;
        // look for fieldname
        if source.at(1) == b'(' {
            buffer.append_byte(FORMAT_FIELD);
            let Some(client) = client else {
                stream_error_at(
                    self.line,
                    self.filename.bytes(),
                    b"Using fieldname is not possible in this context\n",
                );
                return false;
            };
            source.advance(2);
            let Some(fieldname_end) = source.strchr(b')') else {
                stream_error_at(
                    self.line,
                    self.filename.bytes(),
                    b"Missing ')' after field name\n",
                );
                return false;
            };
            // add fieldname for debug purpose
            fieldname = buffer.length();
            buffer.append(&c_range(source, source.offset(), fieldname_end));
            buffer.append_byte(EOS);
            let name = buffer.ptr(fieldname as isize).cstr().to_vec();
            debug2!(
                b"StreamProtocolParser::Protocol::compileFormat: fieldname='",
                name,
                b"'\n"
            );
            let mut field_address = StreamBuffer::new();
            if !client.get_field_address(&name, &mut field_address) {
                stream_error_at(
                    self.line,
                    self.filename.bytes(),
                    &cmsg!(b"Field '", name, b"' not found\n"),
                );
                return false;
            }
            source.set_offset(fieldname_end);
            let length = field_address.length() as u16;
            buffer.append(&length.to_ne_bytes());
            buffer.append_buffer(&field_address);
        } else {
            buffer.append_byte(FORMAT);
        }
        let formatstart = source.offset() + 1;

        // parse format and get info string
        let mut info_string = StreamBuffer::new();
        let type_ = parse_format(
            &mut source,
            format_type,
            &mut stream_format,
            &mut info_string,
        );

        if type_ == 0 {
            // parsing failed
            return false;
        }
        // C tests `type < 1 && type > pseudo_format`, which is never true
        // (StreamProtocol.cc compileFormat); needs_original_format is the
        // highest type a converter returns (ChecksumConverter).
        if !(1..=NEEDS_ORIGINAL_FORMAT).contains(&type_) {
            stream_error_at(
                self.line,
                self.filename.bytes(),
                &cmsg!(
                    format!("Illegal format type {type_} returned from '%"),
                    Ch(stream_format.conv as i32),
                    b"' converter\n"
                ),
            );
            return false;
        }
        if type_ == PSEUDO_FORMAT && fieldname != 0 {
            stream_error_at(
                self.line,
                self.filename.bytes(),
                &cmsg!(
                    b"Fieldname not allowed with pseudo format: '%(",
                    buffer.ptr(fieldname as isize).cstr(),
                    b")",
                    Ch(stream_format.conv as i32),
                    b"'\n"
                ),
            );
            return false;
        }
        if fieldname != 0 && stream_format.flags & SKIP_FLAG != 0 {
            stream_error_at(
                self.line,
                self.filename.bytes(),
                b"Use of skip modifier '*' not allowed together with redirection\n",
            );
            return false;
        }
        stream_format.type_ = type_;
        if info_string.is_set() && info_string.get(-1) != EOS {
            // terminate if necessary
            info_string.append_byte(EOS);
        }
        stream_format.infolen = info_string.length() as u16 as u64;
        // add formatstr for debug purpose
        let formatstring = c_range(source, formatstart, source.offset());
        buffer.append(&formatstring).append_byte(EOS);

        debug2!(
            b"StreamProtocolParser::Protocol::compileFormat: formatstring=\"",
            StreamBuffer::from_bytes(&formatstring).expand(),
            b"\"\n"
        );

        // add streamFormat structure and info
        buffer.append(&stream_format.encode());
        buffer.append_buffer(&info_string);

        debug2!(
            format!(
                "StreamProtocolParser::Protocol::compileFormat: format.type={}, infolen={} infostring=\"",
                STREAM_FORMAT_TYPE_STR
                    .get(stream_format.type_ as usize)
                    .copied()
                    .unwrap_or("(null)"),
                stream_format.infolen
            ),
            info_string.expand(),
            b"\"\n"
        );
        *formatstr = source; // move pointer after parsed format
        true
    }

    /// `compileCommands`: compile each command through the client.
    fn compile_commands(
        &mut self,
        buffer: &mut StreamBuffer,
        source: &mut CPtr<'_>,
        client: &mut dyn Client,
    ) -> bool {
        while source.peek() != 0 {
            let command = *source;
            let mut args = source.plus(source.strlen() + 1 + LINE_SIZE);
            if !client.compile_command(self, buffer, command, &mut args) {
                stream_error_at(
                    command.line_number(),
                    self.filename.bytes(),
                    &cmsg!(b"in command '", command.cstr(), b"'\n"),
                );
                return false;
            }
            if args.peek() != 0 {
                stream_error_at(
                    command.line_number(),
                    self.filename.bytes(),
                    &cmsg!(
                        b"Garbage after '",
                        command.cstr(),
                        b"' command: '",
                        args.cstr(),
                        b"'\n"
                    ),
                );
                return false;
            }
            *source = args.plus(1);
        }
        buffer.append_byte(EOS);
        true
    }

    /// `checkUnused`: a handler nobody asked for is an unknown handler.
    pub fn check_unused(&self) -> bool {
        for v in &self.variables {
            if !v.used {
                if v.name.get(0) == b'@' {
                    stream_error(&cmsg!(
                        b"Unknown handler ",
                        v.name.cstr(),
                        b" defined in protocol file '",
                        self.filename,
                        format!("' line {}\n", v.line)
                    ));
                    return false;
                }
                debug!(
                    b"Unused variable ",
                    v.name.cstr(),
                    b" in protocol file '",
                    self.filename,
                    format!("' line {}\n", v.line)
                );
            }
        }
        true
    }
}

/// The constant tokens of `compileString`.
static CODES: [(&[u8], u8); 38] = [
    (b"skip", SKIP),
    (b"?", SKIP),
    (b"nul", 0x00),
    (b"soh", 0x01),
    (b"stx", 0x02),
    (b"etx", 0x03),
    (b"eot", 0x04),
    (b"enq", 0x05),
    (b"ack", 0x06),
    (b"bel", 0x07),
    (b"bs", 0x08),
    (b"ht", 0x09),
    (b"tab", 0x09),
    (b"lf", 0x0A),
    (b"nl", 0x0A),
    (b"vt", 0x0B),
    (b"ff", 0x0C),
    (b"np", 0x0C),
    (b"cr", 0x0D),
    (b"so", 0x0E),
    (b"si", 0x0F),
    (b"dle", 0x10),
    (b"dc1", 0x11),
    (b"dc2", 0x12),
    (b"dc3", 0x13),
    (b"dc4", 0x14),
    (b"nak", 0x15),
    (b"syn", 0x16),
    (b"etb", 0x17),
    (b"can", 0x18),
    (b"em", 0x19),
    (b"sub", 0x1A),
    (b"esc", 0x1B),
    (b"fs", 0x1C),
    (b"gs", 0x1D),
    (b"rs", 0x1E),
    (b"us", 0x1F),
    (b"del", 0x7F),
];

/// `printString(buffer, s)`: append compiled string `s` in readable form;
/// returns the pointer after its terminator.
pub fn print_string<'a>(buffer: &mut StreamBuffer, s: CPtr<'a>) -> CPtr<'a> {
    let mut s = s;
    while s.peek() != 0 {
        match s.peek() {
            ESC => {
                s.advance(1);
                buffer.print(format_args!("\\x{:02x}", s.peek()));
            }
            b'\r' => {
                buffer.append(b"\\r");
            }
            b'\n' => {
                buffer.append(b"\\n");
            }
            SKIP => {
                buffer.append(b"\\?");
            }
            WHITESPACE => {
                buffer.append(b"\\_");
            }
            b'"' => {
                buffer.append(b"\\\"");
            }
            b'\\' => {
                buffer.append(b"\\\\");
            }
            c @ (FORMAT_FIELD | FORMAT) => {
                if c == FORMAT_FIELD {
                    // <format_field> field <eos> addrLength AddressStructure
                    // formatstr <eos> StreamFormat [info <eos>]
                    s.advance(1);
                    buffer.append(b"%(").append(s.cstr()).append(b")");
                    s.advance(s.strlen() + 1);
                    let field_size = u16::from_ne_bytes([s.at(0), s.at(1)]);
                    s.advance(2 + field_size as usize); // skip fieldAddress
                } else {
                    // <format> formatstr <eos> StreamFormat [info <eos>]
                    buffer.append_byte(b'%');
                    s.advance(1);
                }
                s = print_string(buffer, s);
                StreamFormat::extract(&mut s);
                continue;
            }
            c => {
                if (c & 0x7f) < 0x20 || (c & 0x7f) == 0x7f {
                    buffer.print(format_args!("\\x{c:02x}"));
                } else {
                    buffer.append_byte(c);
                }
            }
        }
        s.advance(1);
    }
    s.plus(1)
}

/// `getc` / `ungetc` over the file's bytes; pushed-back bytes come back
/// last-in first-out, as glibc's do.
struct Reader {
    data: Vec<u8>,
    pos: usize,
    pushback: Vec<u8>,
}

impl Reader {
    fn getc(&mut self) -> i32 {
        if let Some(c) = self.pushback.pop() {
            return c as i32;
        }
        match self.data.get(self.pos) {
            Some(&c) => {
                self.pos += 1;
                c as i32
            }
            None => EOF,
        }
    }

    fn ungetc(&mut self, c: i32) {
        if c != EOF {
            self.pushback.push(c as u8);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Global,
    Pending,
}

/// `StreamProtocolParser`: one parsed protocol file.
pub struct StreamProtocolParser {
    filename: StreamBuffer,
    file: Reader,
    line: i32,
    quote: i32,
    global_settings: Protocol,
    protocols: Vec<Protocol>,
    /// The protocol being parsed, until it is complete.
    pending: Option<Protocol>,
    valid: bool,
}

fn parsers() -> &'static Mutex<Vec<StreamProtocolParser>> {
    static PARSERS: OnceLock<Mutex<Vec<StreamProtocolParser>>> = OnceLock::new();
    PARSERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// `StreamProtocolParser::path`: directories separated by `:` or `;`.
static PATH: RwLock<Option<String>> = RwLock::new(None);

/// Set `StreamProtocolParser::path`.
pub fn set_path(path: Option<&str>) {
    *PATH.write() = path.map(str::to_string);
}

pub fn path() -> Option<String> {
    PATH.read().clone()
}

/// `fopen(name, "r")` and read it all; a read error ends the file, as it
/// makes `getc` return EOF.
fn read_whole(name: &str) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(name).ok()?;
    let mut data = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match f.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => data.extend_from_slice(&chunk[..n]),
        }
    }
    Some(data)
}

/// `StreamProtocolParser::getProtocol(file, protocolAndParams)`: read the
/// file unless already parsed, and return a copy of the protocol, whose
/// name may carry 0x00-separated arguments for `$1`...`$9`.
pub fn get_protocol(filename: &str, protocol_and_params: &StreamBuffer) -> Option<Box<Protocol>> {
    let mut parsers = parsers().lock();
    // Have we already seen this file?
    let i = match parsers
        .iter()
        .position(|p| p.filename.startswith(Some(filename.as_bytes())))
    {
        Some(i) => {
            if !parsers[i].valid {
                stream_error(&cmsg!(
                    b"Protocol file '",
                    filename,
                    b"' is invalid (see above)\n"
                ));
                return None;
            }
            i
        }
        None => {
            // If not, read it.
            let parser = StreamProtocolParser::read_file(filename)?;
            // A parser that failed is kept, so the file is not read again.
            let valid = parser.valid;
            parsers.push(parser);
            if !valid {
                return None;
            }
            parsers.len() - 1
        }
    };
    parsers[i].get_protocol(protocol_and_params)
}

/// `StreamProtocolParser::free()`: forget every parsed file.
pub fn free() {
    parsers().lock().clear();
}

/// `report()` of a parsed file, if it has been read.
pub fn report(filename: &str) {
    if let Some(p) = parsers()
        .lock()
        .iter()
        .find(|p| p.filename.startswith(Some(filename.as_bytes())))
    {
        p.report();
    }
}

impl StreamProtocolParser {
    fn new(data: Vec<u8>, filename: &str) -> Self {
        let mut parser = Self {
            filename: StreamBuffer::from_bytes(filename.as_bytes()),
            file: Reader {
                data,
                pos: 0,
                pushback: Vec::new(),
            },
            line: 1,
            quote: 0,
            global_settings: Protocol::new_global(filename.as_bytes()),
            protocols: Vec::new(),
            pending: None,
            valid: false,
        };
        // start parsing in global context
        parser.valid = parser.parse_protocol(Ctx::Global, 0);
        parser.file.data = Vec::new();
        parser
    }

    pub fn report(&self) {
        let mut out = Vec::new();
        out.extend(cmsg!(
            b"Report of protocol file '",
            self.filename,
            b"'\n GLOBAL:\n"
        ));
        let _ = std::io::stdout().write_all(&out);
        self.global_settings.report();
        let _ = std::io::stdout().write_all(b" PROTOCOLS:\n");
        for p in &self.protocols {
            p.report();
        }
    }

    /// `readFile`: find the file (as given when it is absolute or no path
    /// is set, else in each path directory) and parse it. A parser is
    /// returned even when the file is invalid.
    fn read_file(filename: &str) -> Option<Self> {
        let path = path();
        let fb = filename.as_bytes();
        let absolute = fb.first() == Some(&b'/')
            || (cfg!(windows)
                && (fb.first() == Some(&b'\\')
                    || (fb.first().is_some_and(u8::is_ascii_alphabetic)
                        && fb.get(1) == Some(&b':'))));
        let data = match path {
            Some(path) if !absolute => {
                // look for filename in every dir in search path
                let p = path.as_bytes();
                let mut k = 0;
                let mut found = None;
                while k < p.len() {
                    // allow ':' or ';' for OS independence
                    // we need to be careful with drive letters though
                    let mut n = p[k..]
                        .iter()
                        .position(|&c| c == b':' || c == b';')
                        .unwrap_or(p.len() - k);
                    let drive = |k: usize, n: usize| {
                        n == 2 && p.get(k + 1) == Some(&b':') && p[k].is_ascii_alphabetic()
                    };
                    if cfg!(windows)
                        && n == 1
                        && p.get(k + 1) == Some(&b':')
                        && p[k].is_ascii_alphabetic()
                    {
                        // drive letter
                        n = 2 + p[k + 2..]
                            .iter()
                            .position(|&c| c == b':' || c == b';')
                            .unwrap_or(p.len() - k - 2);
                    }
                    let mut dir = String::from_utf8_lossy(&p[k..k + n]).into_owned();
                    // append / after everything except empty path [or drive letter]
                    if n != 0 && !(cfg!(windows) && drive(k, n)) {
                        dir.push('/');
                    }
                    k += n;
                    if k < p.len() {
                        k += 1; // skip the path separator
                    }
                    dir.push_str(filename);
                    // try to read the file
                    debug!(
                        b"StreamProtocolParser::readFile: try '",
                        dir.as_str(),
                        b"'\n"
                    );
                    if let Some(data) = read_whole(&dir) {
                        debug!(
                            b"StreamProtocolParser::readFile: found '",
                            dir.as_str(),
                            b"'\n"
                        );
                        found = Some(data);
                        break;
                    }
                }
                match found {
                    Some(data) => data,
                    None => {
                        stream_error(&cmsg!(
                            b"Can't find readable file '",
                            filename,
                            b"' in '",
                            path.as_str(),
                            b"'\n"
                        ));
                        return None;
                    }
                }
            }
            _ => match read_whole(filename) {
                // no path or absolute file name
                Some(data) => {
                    debug!(b"StreamProtocolParser::readFile: found '", filename, b"'\n");
                    data
                }
                None => {
                    stream_error(&cmsg!(b"Can't find readable file '", filename, b"'\n"));
                    return None;
                }
            },
        };
        // file found; create a parser to read it
        Some(Self::new(data, filename))
    }

    /// `getProtocol(protocolAndParams)`: a copy of the protocol with its
    /// parameters set. The name is matched without case.
    fn get_protocol(&self, protocol_and_params: &StreamBuffer) -> Option<Box<Protocol>> {
        let mut name = protocol_and_params.clone();
        // make name case insensitive
        let n = name.cstr().len();
        name.bytes_mut()[..n].make_ascii_lowercase();
        // find and make a copy with parameters inserted
        if let Some(p) = self
            .protocols
            .iter()
            .find(|p| p.protocolname.startswith(Some(name.cstr())))
        {
            // constructor also replaces parameters
            return Some(Box::new(Protocol::copy_from(p, &name, 0)));
        }
        stream_error(&cmsg!(
            b"Protocol '",
            protocol_and_params.cstr(),
            b"' not found in protocol file '",
            self.filename,
            b"'\n"
        ));
        None
    }

    fn proto(&mut self, which: Ctx) -> &mut Protocol {
        match which {
            Ctx::Global => &mut self.global_settings,
            Ctx::Pending => self.pending.as_mut().expect("a protocol is being parsed"),
        }
    }

    fn commands(&mut self, which: Ctx, commands: usize) -> &mut StreamBuffer {
        &mut self.proto(which).variables[commands].value
    }

    fn error_here(&self, msg: &[u8]) {
        stream_error_at(self.line, self.filename.bytes(), msg);
    }

    /// `parseProtocol(protocol, commands)`: `commands` is the variable the
    /// commands go to: 0 for the protocol's own (the global context when
    /// `which` is `Global`), else a handler.
    fn parse_protocol(&mut self, which: Ctx, commands: usize) -> bool {
        let mut token = StreamBuffer::new();
        let global_context = which == Ctx::Global && commands == 0;
        let handler_context = commands != 0;

        self.commands(which, commands).clear();
        loop {
            // assignment and protocol parsing loop
            // expect command name, protocol name,
            // handler name, or variable name
            token.clear();
            let startline = self.line;
            if !self.read_token(&mut token, Some(b" ,;{}=()$'\""), global_context) {
                return false;
            }
            debug2!(
                b"StreamProtocolParser::parseProtocol: token='",
                token.expand(),
                b"'\n"
            );
            let t0 = token.get(0);
            if t0 == 0 {
                // terminate at EOF in global context
                return true;
            }
            if t0 == b' ' || t0 == b';' {
                // skip whitespaces, comments and ';'
                continue;
            }
            if t0 == b'}' {
                if !global_context {
                    // end of protocol or handler definition
                    return true;
                }
                self.error_here(b"Unexpected '}' (no matching '{') in global context\n");
                return false;
            }
            if t0 == b'{' {
                self.error_here(&cmsg!(
                    b"Expect ",
                    if global_context {
                        "protocol"
                    } else {
                        "handler"
                    },
                    b" name before '",
                    Ch(t0 as i32),
                    b"'\n"
                ));
                return false;
            }
            if t0 == b'=' {
                self.error_here(&cmsg!(
                    b"Expect variable name before '",
                    Ch(t0 as i32),
                    b"'\n"
                ));
                return false;
            }
            if t0 != b'@' && !c_isalpha(t0 as i32) {
                self.error_here(&cmsg!(b"Unexpected '", token.cstr(), b"'\n"));
                return false;
            }
            // what comes after token?
            let op = loop {
                let op = self.read_char();
                if op != b' ' as i32 {
                    break op;
                }
            };
            if op == b'=' as i32 {
                // variable assignment
                if handler_context {
                    self.error_here(&cmsg!(
                        b"Variables are not allowed in handlers: ",
                        token.cstr(),
                        b"\n"
                    ));
                    return false;
                }
                if t0 == b'@' || t0.is_ascii_digit() {
                    self.error_here(&cmsg!(
                        b"Variable name cannot start with '",
                        Ch(t0 as i32),
                        b"': ",
                        token.cstr(),
                        b"\n"
                    ));
                    return false;
                }
                let name = token.cstr().to_vec();
                if !self.parse_assignment(&name, which) {
                    self.line = startline;
                    self.error_here(&cmsg!(b"in variable assignment '", name, b" = ...'\n"));
                    return false;
                }
                continue;
            }
            if op == b'{' as i32 {
                // protocol or handler definition
                token.truncate(-1 - LINE_SIZE as isize);
                if token.get(0) == b'@' {
                    // handler
                    if handler_context {
                        self.error_here(&cmsg!(
                            b"Handlers are not allowed in handlers: ",
                            token.cstr(),
                            b"\n"
                        ));
                        return false;
                    }
                    let line = self.line;
                    let handler = self.proto(which).create_variable(token.cstr(), line);
                    self.commands(which, handler).clear(); // override existing handler
                    if !self.parse_protocol(which, handler) {
                        self.line = startline;
                        self.error_here(&cmsg!(b"in handler '", token.cstr(), b"'\n"));
                        return false;
                    }
                    continue;
                }
                // protocol definition
                if !global_context {
                    self.error_here(&cmsg!(
                        b"Definition of '",
                        token.cstr(),
                        b"' not in global context (missing '}' ?)\n"
                    ));
                    return false;
                }
                if self
                    .protocols
                    .iter()
                    .any(|p| p.protocolname.startswith(Some(token.cstr())))
                {
                    self.error_here(&cmsg!(b"Protocol '", token.cstr(), b"' redefined\n"));
                    return false;
                }
                self.pending = Some(Protocol::copy_from(
                    &self.global_settings,
                    &token,
                    startline,
                ));
                if !self.parse_protocol(Ctx::Pending, 0) {
                    self.line = startline;
                    self.error_here(&cmsg!(b"in protocol '", token.cstr(), b"'\n"));
                    self.pending = None;
                    return false;
                }
                // append new protocol to parser
                let p = self.pending.take().unwrap();
                self.protocols.push(p);
                continue;
            }
            if t0 == b'@' {
                self.error_here(&cmsg!(b"Expect '{' after handler '", token.cstr(), b"'\n"));
                return false;
            }
            // Must be a command or a protocol reference.
            if global_context {
                self.error_here(&cmsg!(
                    b"Expect '=' or '{' instead after '",
                    token.cstr(),
                    b"'\n"
                ));
                return false;
            }
            if op == b';' as i32 || op == b'}' as i32 {
                // no arguments
                // Check for protocol reference
                let referenced = self
                    .protocols
                    .iter()
                    .find(|p| p.protocolname.startswith(Some(token.cstr())))
                    .map(|p| p.variables[0].value.clone());
                if let Some(referenced) = referenced {
                    self.commands(which, commands).append_buffer(&referenced);
                    if op == b'}' as i32 {
                        self.file.ungetc(op);
                    }
                    continue;
                }
                // Fall through for commands without arguments
            }
            // must be a command (validity will be checked later)
            self.commands(which, commands).append_buffer(&token); // is null separated
            self.file.ungetc(op); // put back first char after command
            let mut value = std::mem::take(self.commands(which, commands));
            let ok = self.parse_value(&mut value, true);
            *self.commands(which, commands) = value;
            if !ok {
                self.line = startline;
                self.error_here(&cmsg!(b"after command '", token.cstr(), b"'\n"));
                return false;
            }
            let expanded = self.commands(which, commands).expand();
            debug2!(b"parseProtocol: command '", expanded, b"'\n");
            self.commands(which, commands).append_byte(0); // terminate value with null byte
        }
    }

    /// `readChar`: a byte, with each run of whitespace and `#` comments read
    /// as one space; counts lines.
    fn read_char(&mut self) -> i32 {
        let mut c = self.file.getc();
        if c_isspace(c) || c == b'#' as i32 {
            // blanks or comments
            loop {
                if c == b'#' as i32 {
                    // comments
                    loop {
                        c = self.file.getc();
                        if c == EOF || c == b'\n' as i32 {
                            break;
                        }
                    }
                }
                if c == b'\n' as i32 {
                    self.line += 1; // count newlines
                }
                c = self.file.getc();
                if !(c_isspace(c) || c == b'#' as i32) {
                    break;
                }
            }
            if c != EOF {
                self.file.ungetc(c); // put back non-blank
            }
            c = b' ' as i32; // return one space for all spaces and comments
        }
        c
    }

    /// `readToken`: append the next token to `buffer`.
    ///
    /// A token is a string in "" or '' (with escaped \" or \' inside), one of
    /// `specialchars`, EOF, a variable reference starting with $ (quoted or
    /// unquoted), or a word of other characters mapped to lowercase. Words,
    /// strings and variables are terminated and followed by the line they
    /// started on; EOF is a single 0x00.
    fn read_token(
        &mut self,
        buffer: &mut StreamBuffer,
        specialchars: Option<&[u8]>,
        eof_allowed: bool,
    ) -> bool {
        let specialchars = specialchars.unwrap_or(SPECIAL_CHARS);
        let token = buffer.length();
        let l = self.line;

        let mut c = self.read_char();
        if c == b'$' as i32 {
            // a variable
            debug2!(b"StreamProtocolParser::readToken: Variable\n");
            buffer.append_byte(b'$');
            if self.quote != 0 {
                buffer.append_byte(b'"'); // mark as quoted variable
            }
            c = self.file.getc();
            if (b'0' as i32..=b'9' as i32).contains(&c) {
                // positional parameter $0 ... $9
                buffer.append_byte(c as u8);
                buffer.append_byte(0).append(&line_bytes(l)); // append line number
                return true;
            }
            if c == b'{' as i32 {
                let q = self.quote;
                self.quote = 0;
                // variable like ${xyz}
                if !self.read_token(buffer, Some(b"{}=;"), false) {
                    return false;
                }
                debug2!(
                    b"StreamProtocolParser::readToken: Variable '",
                    buffer.ptr(token as isize).cstr(),
                    b"' in {}\n"
                );
                c = self.file.getc();
                if c != b'}' as i32 {
                    self.error_here(&cmsg!(
                        b"Expect '}' instead of '",
                        Ch(c),
                        b"' after: ",
                        buffer.ptr(token as isize).cstr(),
                        b"\n"
                    ));
                    return false;
                }
                self.quote = q;
                return true;
            }
            if c == EOF {
                self.error_here(b"Unexpected end of file after '$' (looking for '}')\n");
                return false;
            }
            if c_strchr(specialchars, c) {
                // The ',' after the newline is in the C format string.
                self.error_here(&cmsg!(b"Unexpected '", Ch(c), b"' after '$'\n,"));
                return false;
            }
            // variable like $xyz handled as word token
        } else if self.quote != 0 || c == b'\'' as i32 || c == b'"' as i32 {
            debug2!(b"StreamProtocolParser::readToken: Quoted string\n");
            // quoted string
            if self.quote == 0 {
                self.quote = c;
                c = self.file.getc();
            }
            buffer.append_byte(self.quote as u8);
            while self.quote != 0 {
                if c == EOF || c == b'\n' as i32 {
                    self.error_here(&cmsg!(
                        b"Unterminated quoted string: ",
                        buffer.ptr(token as isize).cstr(),
                        b"\n"
                    ));
                    return false;
                }
                buffer.append_byte(c as u8);
                if c == self.quote {
                    self.quote = 0;
                    break;
                }
                if c == b'\\' as i32 {
                    c = self.file.getc();
                    if c == b'$' as i32 {
                        // quoted variable reference
                        // terminate string here and do variable in next pass
                        buffer.set_byte(-1, self.quote as u8);
                        self.file.ungetc(c);
                        break;
                    }
                    if c == EOF || c == b'\n' as i32 {
                        self.error_here(&cmsg!(
                            b"Backslash at end of line: ",
                            buffer.ptr(token as isize).cstr(),
                            b"\n"
                        ));
                        return false;
                    }
                    buffer.append_byte(c as u8);
                }
                c = self.file.getc();
            }
            buffer.append_byte(0).append(&line_bytes(l)); // append line number
            return true;
        } else if c == EOF {
            // end of file
            if !eof_allowed {
                self.error_here(b"Unexpected end of file (looking for '}')\n");
                return false;
            }
            buffer.append_byte(0);
            return true;
        } else if c_strchr(specialchars, c) {
            debug2!(b"StreamProtocolParser::readToken: Special '", Ch(c), b"'\n");
            // a single character
            buffer.append_byte(c as u8);
            return true;
        }
        // a word or (variable name)
        debug2!(b"StreamProtocolParser::readToken: word\n");
        loop {
            buffer.append_byte(c_tolower(c) as u8);
            c = self.read_char();
            if c == EOF {
                break;
            }
            if c_strchr(specialchars, c) {
                self.file.ungetc(c); // put back char of next token
                break;
            }
        }
        debug2!(
            b"StreamProtocolParser::readToken: word='",
            buffer.ptr(token as isize).cstr(),
            b"' c='",
            Ch(c),
            b"'\n"
        );
        buffer.append_byte(0).append(&line_bytes(l)); // append line number
        true
    }

    /// `parseAssignment(name, protocol)`
    fn parse_assignment(&mut self, name: &[u8], which: Ctx) -> bool {
        let mut value = StreamBuffer::new();
        if !self.parse_value(&mut value, false) {
            return false;
        }
        let line = self.line;
        let i = self.proto(which).create_variable(name, line);
        self.proto(which).variables[i].value = value; // transfer value
        true
    }

    /// `parseValue(buffer, lazy)`: read tokens up to `;` or `}`; unless
    /// `lazy`, variable references are replaced from the global settings.
    fn parse_value(&mut self, buffer: &mut StreamBuffer, lazy: bool) -> bool {
        // skip leading spaces
        let c = loop {
            let c = self.read_char();
            if c != b' ' as i32 {
                break c;
            }
        };
        self.file.ungetc(c);
        loop {
            let token = buffer.length(); // start of next token
            if !self.read_token(buffer, None, false) {
                return false;
            }
            debug2!(
                format!("StreamProtocolParser::parseValue:{}: ", self.line),
                buffer.expand_from(token as isize),
                b"\n"
            );
            let c = buffer.get(token as isize);
            if c == b'$' {
                // a variable
                let mut varname = token + 1;
                if buffer.get(varname as isize) == b'"' {
                    varname += 1; // quoted variable
                }
                if lazy || buffer.get(varname as isize).is_ascii_digit() {
                    // replace later
                    continue;
                }
                let mut value = StreamBuffer::new();
                if !self
                    .global_settings
                    .replace_variable(&mut value, buffer.ptr(token as isize))
                {
                    return false;
                }
                let len = buffer.length() as isize;
                buffer.replace(token as isize, len, value.bytes());
                continue;
            }
            if c == b'{' || c == b'=' {
                self.error_here(&cmsg!(
                    b"Unexpected '",
                    Ch(c as i32),
                    b"' (missing ';' or '\"' ?)\n"
                ));
                return false;
            }
            if c_strchr(b";}", c as i32) {
                buffer.truncate(-1);
                if c != b';' {
                    // let's be generous with missing ';' before '}'
                    self.file.ungetc(c as i32);
                }
                return true;
            }
        }
    }
}
