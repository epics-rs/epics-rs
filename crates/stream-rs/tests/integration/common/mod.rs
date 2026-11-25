//! A record layer and a mock bus, so tests drive the port's `StreamCore`
//! as a C stream record drives StreamCore.

#![allow(dead_code)]

use std::sync::{Arc, Once, Weak};

use parking_lot::Mutex;

use stream_rs::bus::{self, StreamBusInterface};
use stream_rs::core::{ProtocolResult, StreamCore, StreamCoreHooks, StreamCoreLock, ValueMatch};
use stream_rs::error::stream_error;
use stream_rs::format::*;

/// The error output settings of the C oracle run: no colour, no
/// timestamp, thread `_main_`.
pub fn setup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
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
    type FieldAddress = ();
    type LinkPut = std::convert::Infallible;

    fn start_timer(&self, _: &StreamCore<Self>, _: u64) {}
    fn interrupt_accept(&self, _: &StreamCore<Self>) -> bool {
        true
    }
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
    fn format_value(&self, core: &StreamCore<Self>, fmt: &StreamFormat, _: Option<&()>) -> bool {
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
    fn match_value(
        &self,
        core: &StreamCore<Self>,
        fmt: &StreamFormat,
        _: Option<&()>,
    ) -> ValueMatch<Self::LinkPut> {
        let consumed = match (&self.value, fmt.type_) {
            (Value::Longin(_), UNSIGNED_FORMAT | SIGNED_FORMAT | ENUM_FORMAT)
            | (Value::Ai, UNSIGNED_FORMAT | SIGNED_FORMAT) => core.scan_value_long(fmt, &mut 0),
            (Value::Ai, DOUBLE_FORMAT) => core.scan_value_double(fmt, &mut 0.0),
            (Value::Stringin, STRING_FORMAT) => {
                let mut sval = [0u8; 40];
                let mut size = sval.len();
                core.scan_value_string(fmt, &mut sval, &mut size)
            }
            _ => return ValueMatch::Failed,
        };
        if consumed < 0 {
            return ValueMatch::Failed;
        }
        core.consumed_input
            .set(core.consumed_input.get() + consumed as usize);
        ValueMatch::Matched
    }
    /// No field takes a put here.
    fn link_put_done(&self, _: &StreamCore<Self>, put: Self::LinkPut, _: i32) -> bool {
        match put {}
    }

    /// `Stream::getFieldAddress`: `RECORD.FIELD`, else a field of this
    /// (stringout) record, else `FIELD.VAL`.
    fn get_field_address(&self, _: &StreamCore<Self>, fieldname: &[u8]) -> Option<()> {
        let name = String::from_utf8_lossy(fieldname);
        let found = match name.split_once('.') {
            Some((rec, field)) => {
                self.records.iter().any(|r| r == rec) && (field == "VAL" || field == "RVAL")
            }
            None => name == "VAL" || self.records.iter().any(|r| *r == name),
        };
        found.then_some(())
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
    let ok = parse(&core, file, protocol.as_bytes());
    (core, ok)
}

/// `StreamCore::parse`, which C compiles into the core: what it wrote
/// installed, and whether the protocol compiled.
pub fn parse(core: &Core, file: &str, protocol: &[u8]) -> bool {
    let core = core.lock();
    let mut parsed = core.installed();
    let compiled = core.parse(&mut parsed, file, protocol);
    core.install(parsed);
    compiled
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
