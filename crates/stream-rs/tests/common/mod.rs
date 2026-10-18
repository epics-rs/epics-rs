//! Stand-in converters and a record layer, so tests drive the port's
//! `StreamCore` as a C stream record drives StreamCore.

#![allow(dead_code)]

use std::ffi::CString;
use std::sync::{Arc, Once, Weak};

use parking_lot::Mutex;

use stream_rs::StreamBuffer;
use stream_rs::bus::{self, StreamBusInterface};
use stream_rs::converter::ESC;
use stream_rs::converter::{StreamFormatConverter, register_converter};
use stream_rs::core::{ProtocolResult, StreamCore, StreamCoreHooks, StreamCoreLock};
use stream_rs::cstr::CPtr;
use stream_rs::error::stream_error;
use stream_rs::format::*;

/// A converter that takes nothing after its conversion character.
struct Stub(i32);

impl StreamFormatConverter for Stub {
    fn parse(&self, _: &StreamFormat, _: &mut StreamBuffer, _: &mut CPtr<'_>, _: bool) -> i32 {
        self.0
    }
}

/// The printf format C's standard converters build into their info in
/// `parse` (`copyFormatString` and the conversion, `l` for a long).
fn printf_format(fmt: &StreamFormat, long: bool) -> CString {
    let mut f = String::from("%");
    for (flag, c) in [
        (LEFT_FLAG, '-'),
        (SIGN_FLAG, '+'),
        (SPACE_FLAG, ' '),
        (ALT_FLAG, '#'),
        (ZERO_FLAG, '0'),
    ] {
        if fmt.flags & flag != 0 {
            f.push(c);
        }
    }
    if fmt.width != 0 {
        f += &fmt.width.to_string();
    }
    if fmt.prec >= 0 {
        f += &format!(".{}", fmt.prec);
    }
    if long {
        f.push('l');
    }
    f.push(fmt.conv as char);
    CString::new(f).unwrap()
}

/// `StreamBuffer::print(fmt.info, value)`, through C's snprintf.
fn c_print<T: Copy>(output: &mut StreamBuffer, format: &CString, value: T) {
    let mut buf = vec![0u8; 512];
    // SAFETY: the format takes exactly one argument of type T.
    let n = unsafe {
        libc::snprintf(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            format.as_ptr(),
            value,
        )
    };
    output.append(&buf[..n as usize]);
}

/// Stand-ins for StdLongConverter, StdDoubleConverter and
/// StdStringConverter: what they print (StreamFormatConverter.cc).
struct Std(i32);

impl StreamFormatConverter for Std {
    fn parse(&self, _: &StreamFormat, _: &mut StreamBuffer, _: &mut CPtr<'_>, _: bool) -> i32 {
        self.0
    }

    fn print_long(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: i64) -> bool {
        let mut value = value;
        // limits %x/%X formats to number of half bytes in width.
        if fmt.width != 0 && (fmt.conv == b'x' || fmt.conv == b'X') && fmt.width < 16 {
            value &= !(-1i64 << (fmt.width * 4));
        }
        c_print(output, &printf_format(fmt, true), value as libc::c_long);
        true
    }

    fn print_double(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: f64) -> bool {
        c_print(output, &printf_format(fmt, false), value);
        true
    }

    fn print_string(&self, fmt: &StreamFormat, output: &mut StreamBuffer, value: &[u8]) -> bool {
        if fmt.flags & ZERO_FLAG != 0 && fmt.width != 0 {
            let l = if fmt.prec > -1 {
                value.len().min(fmt.prec as usize)
            } else {
                value.len()
            };
            let pad = fmt.width as isize - l as isize;
            if fmt.flags & LEFT_FLAG == 0 {
                output.append_repeat(0, pad);
            }
            output.append(&value[..l]);
            if fmt.flags & LEFT_FLAG != 0 {
                output.append_repeat(0, pad);
            }
        } else {
            let value = CString::new(value).unwrap();
            c_print(output, &printf_format(fmt, false), value.as_ptr());
        }
        true
    }

    fn scan_long(&self, fmt: &StreamFormat, input: CPtr<'_>, value: &mut i64) -> isize {
        let Some((input, mut consumed, neg)) = prepareval(fmt, input) else {
            return -1;
        };
        let base = match fmt.conv {
            b'd' => 10,
            // allow negative hex and oct numbers with - flag
            b'o' | b'x' | b'X' if neg && fmt.flags & LEFT_FLAG == 0 => return -1,
            b'o' => 8,
            b'x' | b'X' => 16,
            b'u' if neg => return -1,
            b'u' => 10,
            _ => 0,
        };
        let mut end = std::ptr::null_mut();
        // SAFETY: input is a NUL-terminated string.
        let v = unsafe { libc::strtoul(input.as_ptr(), &mut end, base) } as i64;
        let n = end as usize - input.as_ptr() as usize;
        if n == 0 {
            return -1;
        }
        consumed += n as isize;
        *value = if neg { v.wrapping_neg() } else { v };
        consumed
    }

    fn scan_double(&self, fmt: &StreamFormat, input: CPtr<'_>, value: &mut f64) -> isize {
        let Some((input, mut consumed, neg)) = prepareval(fmt, input) else {
            return -1;
        };
        let mut end = std::ptr::null_mut();
        // SAFETY: input is a NUL-terminated string.
        let v = unsafe { libc::strtod(input.as_ptr(), &mut end) };
        *value = if neg { -v } else { v };
        let n = end as usize - input.as_ptr() as usize;
        if n == 0 {
            return -1;
        }
        consumed += n as isize;
        consumed
    }

    fn scan_string(
        &self,
        fmt: &StreamFormat,
        input: CPtr<'_>,
        value: &mut [u8],
        size: &mut usize,
    ) -> isize {
        let mut consumed = 0;
        let mut space_left = (*size).min(value.len());
        if fmt.flags & SKIP_FLAG != 0 || value.is_empty() {
            space_left = 0;
        }
        // if user does not specify width assume "infinity" (-1)
        let mut width: i64 = match fmt.width {
            0 if fmt.conv == b'c' => 1,
            0 => -1,
            w => w as i64,
        };
        let mut v = 0;
        let mut i = 0;
        while input.at(i).is_c_space() && width != 0 {
            // normally leading whitespace does not count to width
            // but do so if space flag is present
            if fmt.flags & SPACE_FLAG != 0 {
                if space_left > 1 {
                    value[v] = input.at(i);
                    v += 1;
                    space_left -= 1;
                }
                width -= 1;
            }
            consumed += 1;
            i += 1;
        }
        while input.at(i) != 0 && width != 0 {
            // normally whitespace ends string
            // but don't end if # flag is present
            if fmt.flags & ALT_FLAG == 0 && input.at(i).is_c_space() {
                break;
            }
            if space_left > 1 {
                value[v] = input.at(i);
                v += 1;
                space_left -= 1;
            }
            consumed += 1;
            width -= 1;
            i += 1;
        }
        if space_left != 0 {
            value[v] = 0;
            space_left -= 1;
            *size -= space_left; // update number of bytes copied to value
        }
        consumed
    }
}

/// `isspace`, the C locale's: VT too.
trait CSpace {
    fn is_c_space(&self) -> bool;
}

impl CSpace for u8 {
    fn is_c_space(&self) -> bool {
        matches!(self, b' ' | b'\t'..=b'\r')
    }
}

/// C's `prepareval`: the number, cut to the width, as strtoul and strtod
/// read it, the bytes taken before it, and its sign.
fn prepareval(fmt: &StreamFormat, input: CPtr<'_>) -> Option<(CString, isize, bool)> {
    let mut input = input;
    let mut consumed = 0;
    let mut neg = false;
    while input.peek().is_c_space() {
        input.advance(1);
        consumed += 1;
    }
    let mut s = input.cstr().to_vec();
    if fmt.width != 0 {
        // take local copy because strto* don't have width parameter
        let mut width = fmt.width as usize;
        if fmt.flags & SPACE_FLAG != 0 {
            // normally whitespace does not count to width
            // but do so if space flag is present
            width = width.wrapping_sub(consumed as usize);
        }
        s.truncate(width);
    }
    if s.first() == Some(&b'+') || s.first() == Some(&b'-') {
        neg = s[0] == b'-';
        s.remove(0);
        consumed += 1;
    }
    if s.first().is_some_and(|c| c.is_c_space()) {
        // allow space after sign only if # flag is set
        if fmt.flags & ALT_FLAG == 0 {
            return None;
        }
    }
    Some((CString::new(s).unwrap(), consumed, neg))
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
        register_converter("Long", b"di", Box::new(Std(SIGNED_FORMAT)));
        register_converter("Long", b"ouxX", Box::new(Std(UNSIGNED_FORMAT)));
        register_converter("Long", b"c", Box::new(Stub(SIGNED_FORMAT)));
        register_converter("Double", b"feEgG", Box::new(Std(DOUBLE_FORMAT)));
        register_converter("String", b"s", Box::new(Std(STRING_FORMAT)));
        register_converter("Binary", b"b", Box::new(Stub(UNSIGNED_FORMAT)));
        register_converter("Raw", b"rD", Box::new(Stub(SIGNED_FORMAT)));
        register_converter("Double", b"Rm", Box::new(Stub(DOUBLE_FORMAT)));
        register_converter("Consuming", b"<{[BT", Box::new(Consuming));
        stream_rs::error::set_stream_msg_time_stamped(0);
        stream_rs::error::set_stream_debug_colored(false);
        stream_rs::error::set_get_thread_name_function(Some(|| Some("_main_".into())));
        bus::register_stream_bus_interface("Mock", mock_bus);
    });
}

/// How the [`MockBus`] answers, set by a test before it attaches.
pub struct MockConfig {
    pub lock_ok: bool,
    pub events: bool,
    pub async_read: bool,
}

pub static MOCK: Mutex<MockConfig> = Mutex::new(MockConfig {
    lock_ok: true,
    events: false,
    async_read: false,
});

/// The bus "P": it takes every request (the lock, event and async read
/// support as configured) and answers none; a test plays the answers.
struct MockBus;

impl StreamBusInterface for MockBus {
    fn lock_request(&self, _: u64) -> bool {
        MOCK.lock().lock_ok
    }
    fn unlock(&self) -> bool {
        true
    }
    fn write_request(&self, _: &[u8], _: u64) -> bool {
        true
    }
    fn read_request(&self, _: u64, _: u64, _: isize, _: bool) -> bool {
        true
    }
    fn connect_request(&self, _: u64) -> bool {
        true
    }
    fn disconnect_request(&self) -> bool {
        true
    }
    fn supports_event(&self) -> bool {
        MOCK.lock().events
    }
    fn supports_async_read(&self) -> bool {
        MOCK.lock().async_read
    }
    fn accept_event(&self, _: u64, _: u64) -> bool {
        true
    }
}

/// The client last attached to "P", for a test to play the bus's answers.
pub static CLIENT: Mutex<Option<Weak<dyn bus::Client>>> = Mutex::new(None);

fn mock_bus(
    client: Weak<dyn bus::Client>,
    busname: &str,
    _: i32,
    _: &str,
) -> Option<Box<dyn StreamBusInterface>> {
    (busname == "P").then(|| {
        *CLIENT.lock() = Some(client);
        Box::new(MockBus) as Box<dyn StreamBusInterface>
    })
}

/// A record's VAL, and what its device support's `writeData` prints it as
/// for each format type (dev*Stream.c), `false` where it returns ERROR; an
/// input record's `readData` scans each format type it takes (the value is
/// not kept).
#[derive(Clone, Debug, Default)]
pub enum Value {
    #[default]
    None,
    Longout(i64),
    Ao(f64),
    Stringout(Vec<u8>),
    Longin(i64),
    Ai,
    Stringin,
}

/// A record layer for parsing only: `records` are the other records
/// `%(NAME.VAL)` and `%(NAME.RVAL)` can name, as `ai` records in the C
/// oracle.
#[derive(Default)]
pub struct TestHooks {
    pub records: Vec<String>,
    pub value: Value,
}

impl StreamCoreHooks for TestHooks {
    fn start_timer(&self, _: &StreamCore<Self>, _: u64) {}
    /// `Stream::execute` queues the command; a test plays the answer.
    fn execute(&self, _: &StreamCore<Self>) -> bool {
        true
    }
    /// `Stream::protocolFinishHook`'s message, for a record processing
    /// asynchronously (PACT set), as in the C runs.
    fn protocol_finish_hook(&self, core: &StreamCore<Self>, result: ProtocolResult) {
        if matches!(result, ProtocolResult::Abort | ProtocolResult::Fault) {
            stream_error(format!("{}: Protocol aborted\n", core.name()).as_bytes());
        }
    }
    fn format_value(&self, core: &StreamCore<Self>, fmt: &StreamFormat, _: Option<&[u8]>) -> bool {
        match (&self.value, fmt.type_) {
            // A longout's VAL is an epicsInt32, read as unsigned long.
            (Value::Longout(v), UNSIGNED_FORMAT) => {
                core.print_value_long(fmt, *v as i32 as u32 as i64)
            }
            (Value::Longout(v), SIGNED_FORMAT | ENUM_FORMAT) => core.print_value_long(fmt, *v),
            (Value::Ao(v), DOUBLE_FORMAT) => core.print_value_double(fmt, *v),
            (Value::Ao(v), UNSIGNED_FORMAT) => core.print_value_long(fmt, *v as u64 as i64),
            (Value::Ao(v), SIGNED_FORMAT) => core.print_value_long(fmt, *v as i64),
            (Value::Stringout(v), STRING_FORMAT) => core.print_value_string(fmt, v),
            // A longin's VAL, as unsigned long too.
            (Value::Longin(v), UNSIGNED_FORMAT | SIGNED_FORMAT | ENUM_FORMAT) => {
                core.print_value_long(fmt, *v)
            }
            _ => false,
        }
    }
    /// `Stream::matchValue` without a field: `readData` scans, then the
    /// value's bytes are consumed.
    fn match_value(&self, core: &StreamCore<Self>, fmt: &StreamFormat, _: Option<&[u8]>) -> bool {
        let consumed = match (&self.value, fmt.type_) {
            (Value::Longin(_), UNSIGNED_FORMAT | SIGNED_FORMAT | ENUM_FORMAT)
            | (Value::Ai, UNSIGNED_FORMAT | SIGNED_FORMAT) => core.scan_value_long(fmt, &mut 0),
            (Value::Ai, DOUBLE_FORMAT) => core.scan_value_double(fmt, &mut 0.0),
            (Value::Stringin, STRING_FORMAT) => {
                let mut sval = [0u8; 40];
                let mut size = sval.len();
                core.scan_value_string(fmt, &mut sval, &mut size)
            }
            _ => return false,
        };
        if consumed < 0 {
            return false;
        }
        core.consumed_input
            .set(core.consumed_input.get() + consumed as usize);
        true
    }

    /// `Stream::getFieldAddress`: `RECORD.FIELD`, else a field of this
    /// (stringout) record, else `FIELD.VAL`.
    fn get_field_address(
        &self,
        _: &StreamCore<Self>,
        fieldname: &[u8],
        address: &mut StreamBuffer,
    ) -> bool {
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
}

pub type Core = Arc<StreamCoreLock<TestHooks>>;

/// A record `name` that loads `protocol` (the link's `name` or
/// `name(param,...)`) from `file`, as `Stream::initRecord` does; the core,
/// and whether it parsed.
pub fn load(file: &str, protocol: &str, name: &str, records: &[String]) -> (Core, bool) {
    let core = StreamCoreLock::new(name, |_| TestHooks {
        records: records.to_vec(),
        ..Default::default()
    });
    let ok = core.lock().parse(file, protocol.as_bytes());
    (core, ok)
}

/// Each non-empty command list of a parsed core, with its handler
/// (`(null)` for the protocol's own), as C's `getCommands` debug output
/// has them.
pub fn compiled(core: &Core) -> Vec<(String, Vec<u8>)> {
    let c = core.lock();
    [
        ("(null)", &c.commands),
        ("@init", &c.on_init),
        ("@writetimeout", &c.on_write_timeout),
        ("@replytimeout", &c.on_reply_timeout),
        ("@readtimeout", &c.on_read_timeout),
        ("@mismatch", &c.on_mismatch),
    ]
    .into_iter()
    .filter(|(_, l)| l.borrow().is_set())
    .map(|(h, l)| (h.to_string(), l.borrow().bytes().to_vec()))
    .collect()
}
