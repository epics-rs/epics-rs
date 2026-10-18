//! `StreamEpics.cc`: the record layer, C's `Stream` subclass of
//! `StreamCore`, and the device support functions `devStream.h` declares
//! for the record types' `dev*Stream.c`.
//!
//! A record type's device support builds a [`StreamDevice`] with its
//! [`StreamRecordType`], which carries its `initRecord` and the `readData`
//! / `writeData` it hands [`Stream::init_record`] (C's
//! `streamInitRecord`); those reach the record's value through the record
//! and its `dbCommon` and print or scan it through the [`StreamIo`] they are
//! given (C's `streamPrintf` / `streamScanf`).
//!
//! # Locking
//!
//! C's bus, timer and `exec` callbacks take only the stream's mutex and
//! write the record unlocked. Here the record is taken first, everywhere:
//! an entry into the core from another thread passes the gate, which takes
//! the record with [`AsyncDbHandle::with_instance`] and lends it to the
//! hooks for the entry's run. A device support call into the stream
//! already holds the record and lends it the same way; a bus answering
//! inline, on the thread that holds the core, finds it lent. While
//! `initRecord` waits for its `@init` run, holding the record as iocInit
//! hands it over, it runs the entries itself.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicI16, Ordering};
use std::sync::{Arc, OnceLock, Weak, mpsc};
use std::thread::ThreadId;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{AsyncDbHandle, DeviceDoneSource};
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, ReadStart, WriteStart,
};
use epics_base_rs::server::recgbl::alarm_status::{
    CALC_ALARM, COMM_ALARM, NO_ALARM, READ_ALARM, TIMEOUT_ALARM, UDF_ALARM, WRITE_ALARM,
};
use epics_base_rs::server::record::{
    AlarmSeverity, CommonFields, HwLink, ParsedLink, Record, parse_link_v2,
};
use parking_lot::Mutex;

use crate::StreamBuffer;
use crate::bus::StreamIoStatus;
use crate::core::{
    Entry, INIT_RUN, ProtocolResult, SCAN_TRIED, StartMode, StreamCore, StreamCoreHooks,
    StreamCoreLock,
};
use crate::error::{STREAM_ERROR, stream_error};
use crate::format::{DOUBLE_FORMAT, ENUM_FORMAT, SIGNED_FORMAT, StreamFormat, UNSIGNED_FORMAT};
use crate::{cmsg, debug, protocol};

/// devStream.h's return values.
pub const OK: i64 = 0;
pub const ERROR: i64 = -1;
pub const DO_NOT_CONVERT: i64 = 2;

/// devSup.h `S_dev_badInitRet`
pub const S_DEV_BAD_INIT_RET: i64 = (514 << 16) | 11;
/// devLib.h `S_dev_noDevice`
pub const S_DEV_NO_DEVICE: i64 = (503 << 16) | 18;

/// More flags: 0x00FFFFFF used by StreamCore
const VALUE_RECEIVED: u64 = 0x0200000;

/// The DBF type of a [`Format`], with its `dbFldTypes.h` code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dbf {
    String = 0,
    Char = 1,
    Long = 5,
    Ulong = 6,
    Double = 10,
    Enum = 11,
}

impl Dbf {
    /// `pamapdbfType[type].strvalue`
    pub fn name(self) -> &'static str {
        match self {
            Dbf::String => "DBF_STRING",
            Dbf::Char => "DBF_CHAR",
            Dbf::Long => "DBF_LONG",
            Dbf::Ulong => "DBF_ULONG",
            Dbf::Double => "DBF_DOUBLE",
            Dbf::Enum => "DBF_ENUM",
        }
    }

    /// `dbfMapping[format.type]`: C's table maps the type 0 to its 0,
    /// DBF_STRING, and has no row for a pseudo format, which never reaches
    /// a record.
    fn of(format_type: i32) -> Dbf {
        match format_type {
            UNSIGNED_FORMAT => Dbf::Ulong,
            SIGNED_FORMAT => Dbf::Long,
            ENUM_FORMAT => Dbf::Enum,
            DOUBLE_FORMAT => Dbf::Double,
            _ => Dbf::String,
        }
    }
}

/// `format_t`: the DBF type the record reads or writes, and the format.
pub struct Format<'a> {
    pub type_: Dbf,
    pub priv_: &'a StreamFormat<'a>,
}

/// `streamIoFunction`: a record type's `readData` or `writeData`, returning
/// [`OK`], [`ERROR`] or [`DO_NOT_CONVERT`].
pub type StreamIoFunction =
    fn(&StreamIo<'_>, &mut dyn Record, &mut CommonFields, &Format<'_>) -> i64;

/// What `readData` and `writeData` print and scan the value with.
pub struct StreamIo<'a> {
    core: &'a StreamCore<StreamHooks>,
}

impl StreamIo<'_> {
    /// The record's name.
    pub fn name(&self) -> &str {
        self.core.name()
    }

    /// `streamScanf(record, format, &long)`, `Stream::scan`'s long case.
    pub fn scanf_long(&self, format: &Format<'_>, value: &mut i64) -> i64 {
        match format.type_ {
            Dbf::Ulong | Dbf::Long | Dbf::Enum => self
                .scan(format, |c| c.scan_value_long(format.priv_, value))
                .min(OK as isize) as i64,
            _ => self.illegal_scan(format),
        }
    }

    /// `streamScanf(record, format, &double)`
    pub fn scanf_double(&self, format: &Format<'_>, value: &mut f64) -> i64 {
        match format.type_ {
            Dbf::Double => self
                .scan(format, |c| c.scan_value_double(format.priv_, value))
                .min(OK as isize) as i64,
            _ => self.illegal_scan(format),
        }
    }

    /// `streamScanfN(record, format, value, value.len())`: the bytes
    /// written, the terminator included, or [`ERROR`].
    pub fn scanf_string(&self, format: &Format<'_>, value: &mut [u8]) -> isize {
        if format.type_ != Dbf::String {
            return self.illegal_scan(format) as isize;
        }
        let mut size = value.len();
        let n = self.scan(format, |c| {
            c.scan_value_string(format.priv_, value, &mut size)
        });
        if n < 0 { n } else { size as isize }
    }

    /// `Stream::scan`, the scan given: [`ERROR`] or the bytes it took.
    fn scan(
        &self,
        _format: &Format<'_>,
        scan: impl FnOnce(&StreamCore<StreamHooks>) -> isize,
    ) -> isize {
        let core = self.core;
        let hooks = &core.hooks;
        // first remove old value from inputLine (in case we are scanning arrays)
        core.consumed_input
            .set(core.consumed_input.get() + hooks.current_value_length.get() as usize);
        hooks.current_value_length.set(0);
        let n = scan(core);
        hooks.current_value_length.set(n);
        debug!(format!("Stream::scan() currentValueLength={n}\n"));
        if n < 0 {
            // important for arrays with less than NELM elements
            hooks.current_value_length.set(0);
            return ERROR as isize;
        }
        // Don't remove scanned value from inputLine yet, because
        // we might need the string in a later error message.
        n
    }

    fn illegal_scan(&self, format: &Format<'_>) -> i64 {
        stream_error(&cmsg!(format!(
            "INTERNAL ERROR ({}): Illegal format type {}\n",
            self.name(),
            format.type_ as i32
        )));
        ERROR
    }
}

/// A record type's `dev*Stream.c`.
pub trait StreamRecordType: Send + Sync + 'static {
    /// The dset's `init_record`: C's `initRecord`, which calls
    /// [`Stream::init_record`] with the record's link and its `readData`
    /// and `writeData`, and maps the status.
    fn init_record(
        &self,
        stream: &Stream,
        record: &mut dyn Record,
        common: &mut CommonFields,
    ) -> i64;

    /// Whether the dset's I/O entry writes (`streamWrite`); else it reads.
    fn is_output(&self) -> bool;

    /// The dset's I/O entry: `streamRead` / `streamWrite`, unless the record
    /// type wraps it.
    fn read_write(
        &self,
        stream: &Stream,
        record: &mut dyn Record,
        common: &mut CommonFields,
    ) -> i64 {
        stream.read_write(record, common)
    }
}

/// The record a hook may reach: lent by the thread that holds it, for one
/// run of the core, and handed only to that thread.
#[derive(Default)]
struct RecordSlot(Mutex<Option<Lent>>);

struct Lent {
    record: NonNull<dyn Record>,
    common: NonNull<CommonFields>,
    thread: ThreadId,
}

// SAFETY: a `Lent` is dereferenced only on the thread that lent it (see
// `RecordSlot::with`).
unsafe impl Send for Lent {}

impl RecordSlot {
    /// Run `f` with `record` and `common` lent to it.
    fn lend<R>(
        &self,
        record: &mut dyn Record,
        common: &mut CommonFields,
        f: impl FnOnce() -> R,
    ) -> R {
        let record: NonNull<dyn Record + '_> = NonNull::from(record);
        // SAFETY: only the trait object's lifetime bound changes. The
        // pointer is handed out by `with`, on this thread, until `f`
        // returns, while the borrow it came from is held by this call.
        let record: NonNull<dyn Record> = unsafe { std::mem::transmute(record) };
        let lent = Lent {
            record,
            common: NonNull::from(common),
            thread: std::thread::current().id(),
        };
        let previous = self.0.lock().replace(lent);
        struct Restore<'a>(&'a RecordSlot, Option<Lent>);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                *self.0.0.lock() = self.1.take();
            }
        }
        let _restore = Restore(self, previous);
        f()
    }

    /// The lent record, on the thread that lent it; `None` elsewhere, or
    /// while it is out to a `with` further up this thread's stack.
    fn with<R>(&self, f: impl FnOnce(&mut dyn Record, &mut CommonFields) -> R) -> Option<R> {
        let mut lent = {
            let mut slot = self.0.lock();
            match slot.as_ref() {
                Some(l) if l.thread == std::thread::current().id() => slot.take()?,
                _ => return None,
            }
        };
        struct PutBack<'a>(&'a RecordSlot, Option<Lent>);
        impl Drop for PutBack<'_> {
            fn drop(&mut self) {
                *self.0.0.lock() = self.1.take();
            }
        }
        // SAFETY: see `lend`; taken out of the slot, so no second `with`
        // hands out the same borrow while this one is out.
        let (record, common) = unsafe { (lent.record.as_mut(), lent.common.as_mut()) };
        let _put_back = PutBack(self, None);
        let r = f(record, common);
        drop(_put_back);
        *self.0.lock() = Some(lent);
        Some(r)
    }
}

/// An [`Entry`] the `@init` waiter runs for the thread that sent it.
struct Job {
    entry: OwnedEntry,
    reply: mpsc::Sender<isize>,
}

enum OwnedEntry {
    Lock(StreamIoStatus),
    Write(StreamIoStatus),
    Read(StreamIoStatus, Vec<u8>),
    Event(StreamIoStatus),
    Connect(StreamIoStatus),
    Disconnect(StreamIoStatus),
    Timer,
    Exec(StreamIoStatus),
}

impl OwnedEntry {
    fn of(entry: Entry<'_>) -> Self {
        match entry {
            Entry::Lock(s) => Self::Lock(s),
            Entry::Write(s) => Self::Write(s),
            Entry::Read(s, input) => Self::Read(s, input.to_vec()),
            Entry::Event(s) => Self::Event(s),
            Entry::Connect(s) => Self::Connect(s),
            Entry::Disconnect(s) => Self::Disconnect(s),
            Entry::Timer => Self::Timer,
            Entry::Exec(s) => Self::Exec(s),
        }
    }

    fn entry(&self) -> Entry<'_> {
        match self {
            Self::Lock(s) => Entry::Lock(*s),
            Self::Write(s) => Entry::Write(*s),
            Self::Read(s, input) => Entry::Read(*s, input),
            Self::Event(s) => Entry::Event(*s),
            Self::Connect(s) => Entry::Connect(*s),
            Self::Disconnect(s) => Entry::Disconnect(*s),
            Self::Timer => Entry::Timer,
            Self::Exec(s) => Entry::Exec(*s),
        }
    }
}

/// What the gate and the device support share outside the core.
struct Shared {
    name: String,
    slot: RecordSlot,
    db: OnceLock<AsyncDbHandle>,
    /// Set while `initRecord` waits for its `@init` run holding the record:
    /// every entry goes to it.
    init_waiter: Mutex<Option<mpsc::Sender<Job>>>,
    /// `record->prio`, which the bus reads from its own thread.
    prio: AtomicI16,
}

impl Shared {
    /// The gate: the record, then the core.
    fn enter(&self, lock: &StreamCoreLock<StreamHooks>, entry: Entry<'_>) -> isize {
        let waiter = self.init_waiter.lock().clone();
        if let Some(waiter) = waiter {
            let (reply, answer) = mpsc::channel();
            let job = Job {
                entry: OwnedEntry::of(entry),
                reply,
            };
            if waiter.send(job).is_ok()
                && let Ok(r) = answer.recv()
            {
                return r;
            }
            // the wait ended before it took this one: the record is free
        }
        let Some(db) = self.db.get() else {
            return lock.dispatch(entry);
        };
        db.with_instance(&self.name, |record, common| {
            self.slot.lend(record, common, || lock.dispatch(entry))
        })
        .unwrap_or(0)
    }
}

/// `Stream`'s members, C's beside StreamCore's: the core's hooks.
pub struct StreamHooks {
    shared: Arc<Shared>,
    link: RefCell<String>,
    read_data: Cell<Option<StreamIoFunction>>,
    write_data: Cell<Option<StreamIoFunction>>,
    /// The alarm status of the last run, or [`ERROR`] before the record
    /// was initialized.
    status: Cell<i64>,
    convert: Cell<i64>,
    current_value_length: Cell<isize>,
    /// `ioscanpvt`: set once the record has been initialized.
    initialized: Cell<bool>,
    /// `initDone`, signalled by the finish of an `@init` run.
    init_done: Cell<bool>,
}

impl StreamHooks {
    fn with_record<R>(&self, f: impl FnOnce(&mut dyn Record, &mut CommonFields) -> R) -> Option<R> {
        self.shared.slot.with(f)
    }
}

impl StreamCoreHooks for StreamHooks {
    fn protocol_finish_hook(&self, core: &StreamCore<Self>, result: ProtocolResult) {
        debug!(format!(
            "Stream::protocolFinishHook({}, {})\n",
            core.name(),
            result.to_str()
        ));
        let flags = core.flags.get();
        let status = match result {
            ProtocolResult::Success => {
                if flags & VALUE_RECEIVED != 0 {
                    self.with_record(|_, common| {
                        common.udf = 0;
                        if flags & INIT_RUN != 0 {
                            // records start with UDF/INVALID,
                            // but now this record has a value
                            common.sevr = AlarmSeverity::NoAlarm;
                            common.stat = NO_ALARM;
                        }
                    });
                }
                NO_ALARM
            }
            ProtocolResult::LockTimeout | ProtocolResult::ReplyTimeout => TIMEOUT_ALARM,
            ProtocolResult::WriteTimeout => WRITE_ALARM,
            ProtocolResult::ReadTimeout => READ_ALARM,
            ProtocolResult::ScanError | ProtocolResult::FormatError => CALC_ALARM,
            ProtocolResult::Offline => COMM_ALARM,
            ProtocolResult::Abort | ProtocolResult::Fault => UDF_ALARM,
        };
        self.status.set(status as i64);
        if flags & (INIT_RUN | crate::core::ABORTED) == INIT_RUN {
            debug!(format!(
                "Stream::protocolFinishHook {}: signalling init done\n",
                core.name()
            ));
            self.init_done.set(true);
        }
    }

    fn start_timer(&self, _core: &StreamCore<Self>, _timeout_ms: u64) {}

    fn format_value(
        &self,
        core: &StreamCore<Self>,
        format: &StreamFormat,
        _fieldaddress: Option<&[u8]>,
    ) -> bool {
        let fmt = Format {
            type_: Dbf::of(format.type_),
            priv_: format,
        };
        debug!(format!(
            "Stream::formatValue({}) format=%{} type={}\n",
            core.name(),
            format.conv as char,
            fmt.type_.name()
        ));
        let Some(write_data) = self.write_data.get() else {
            stream_error(&cmsg!(core.name(), b": No writeData() function provided\n"));
            return false;
        };
        let io = StreamIo { core };
        let status = self
            .with_record(|record, common| write_data(&io, record, common, &fmt))
            .unwrap_or(ERROR);
        if status == ERROR {
            debug!(format!(
                "Stream::formatValue({}): writeData failed\n",
                core.name()
            ));
            return false;
        }
        true
    }

    fn match_value(
        &self,
        core: &StreamCore<Self>,
        format: &StreamFormat,
        _fieldaddress: Option<&[u8]>,
    ) -> bool {
        let fmt = Format {
            type_: Dbf::of(format.type_),
            priv_: format,
        };
        // no fieldaddress (the "normal" case)
        let Some(read_data) = self.read_data.get() else {
            stream_error(&cmsg!(core.name(), b": No readData() function provided\n"));
            return false;
        };
        self.current_value_length.set(0);
        let io = StreamIo { core };
        // this will call scan()
        let convert = self
            .with_record(|record, common| read_data(&io, record, common, &fmt))
            .unwrap_or(ERROR);
        self.convert.set(convert);
        if convert == ERROR {
            debug!(format!(
                "Stream::matchValue({}): readData failed\n",
                core.name()
            ));
            if self.current_value_length.get() > 0 {
                let line = core.input_line.borrow();
                let consumed = core.consumed_input.get();
                stream_error(&cmsg!(
                    core.name(),
                    b": Record does not accept input \"",
                    line.expand_range(consumed as isize, 19),
                    if line.length() - consumed > 20 {
                        "..."
                    } else {
                        ""
                    },
                    b"\"\n"
                ));
                core.flags.set(core.flags.get() & !SCAN_TRIED);
            }
            return false;
        }
        core.flags.set(core.flags.get() | VALUE_RECEIVED);
        core.consumed_input
            .set(core.consumed_input.get() + self.current_value_length.get() as usize);
        debug!(format!(
            "Stream::matchValue({}): success, {} bytes consumed\n",
            core.name(),
            self.current_value_length.get()
        ));
        true
    }

    fn get_field_address(
        &self,
        _core: &StreamCore<Self>,
        _fieldname: &[u8],
        _address: &mut StreamBuffer,
    ) -> bool {
        false
    }

    fn priority(&self, _core: &StreamCore<Self>) -> i64 {
        self.shared.prio.load(Ordering::Relaxed) as i64
    }
}

/// `drvStreamInit` has run.
static DRV_INIT: OnceLock<()> = OnceLock::new();

/// `Stream::drvInit`, the driver's `init`, which iocInit runs before the
/// records': here before the first record's.
fn drv_init() {
    DRV_INIT.get_or_init(|| {
        debug!(b"drvStreamInit()\n");
        match std::env::var("STREAM_PROTOCOL_PATH") {
            Ok(path) => protocol::set_path(Some(&path)),
            Err(_) => {
                eprintln!("drvStreamInit: Warning! STREAM_PROTOCOL_PATH not set.");
            }
        }
        debug!(format!(
            "StreamProtocolParser::path = {}\n",
            protocol::path().as_deref().unwrap_or("(null)")
        ));
        crate::error::set_print_timestamp_function(|| {
            chrono::Local::now()
                .format("%Y/%m/%d %H:%M:%S%.6f")
                .to_string()
        });
        // epicsThreadGetNameSelf never returns NULL: a thread it did not
        // create is "non-EPICS_<tid>" (osdThread.c createImplicit).
        crate::error::set_get_thread_name_function(Some(|| {
            let thread = std::thread::current();
            Some(match thread.name() {
                Some(name) => name.to_string(),
                None => format!("non-EPICS_{:?}", thread.id()),
            })
        }));
    });
}

/// `streamInit(after)`, the dset's `init`: errors are on while the
/// protocol files are read.
fn stream_init(after: bool) {
    static OLD_STREAM_ERROR: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    static BEFORE: AtomicBool = AtomicBool::new(true);
    static AFTER: AtomicBool = AtomicBool::new(true);
    if after {
        if AFTER.swap(false, Ordering::AcqRel) {
            // restore error filtering to previous setting
            STREAM_ERROR.store(OLD_STREAM_ERROR.load(Ordering::Relaxed), Ordering::Relaxed);
            protocol::free();
        }
    } else if BEFORE.swap(false, Ordering::AcqRel) {
        // enable errors while reading protocol files
        OLD_STREAM_ERROR.store(STREAM_ERROR.load(Ordering::Relaxed), Ordering::Relaxed);
        STREAM_ERROR.store(1, Ordering::Relaxed);
    }
}

/// The parts of an INST_IO link: `filename protocol bus [addr [params]]`.
struct Link<'a> {
    filename: &'a str,
    protocol: &'a str,
    busname: &'a str,
    addr: i64,
    busparam: &'a str,
}

/// `Stream::initRecord`'s link parse.
fn parse_link(s: &str) -> Link<'_> {
    let b = s.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let space = |c: u8| crate::cstr::c_isspace(c as i32);
    let mut p = 0;

    while space(at(p)) {
        p += 1;
    }
    let filename_start = p;
    while at(p) != 0 && !space(at(p)) {
        p += 1;
    }
    let filename = &s[filename_start..p];
    if at(p) != 0 {
        p += 1;
    }

    while space(at(p)) {
        p += 1;
    }
    let protocol_start = p;
    while at(p) != 0 && !space(at(p)) && at(p) != b'(' {
        p += 1;
    }
    let name_end = p;
    while space(at(p)) {
        p += 1;
    }
    let protocol = if at(p) == b'(' {
        let mut brackets = 0;
        loop {
            p += 1;
            match at(p) {
                0 => break,
                b'(' => brackets += 1,
                b')' => brackets -= 1,
                b'\\' => {
                    p += 1;
                    if at(p) == 0 {
                        break;
                    }
                }
                c if space(c) && brackets < 0 => break,
                _ => {}
            }
        }
        let protocol = &s[protocol_start..p];
        if at(p) != 0 {
            p += 1;
        }
        protocol
    } else {
        // C steps back one byte to end the name at the space before the
        // bus name, so the spaces before that one stay in the protocol name
        // ("p   " for "p    P", StreamEpics.cc initRecord); it ends at
        // the name here.
        &s[protocol_start..name_end]
    };

    while space(at(p)) {
        p += 1;
    }
    let bus_start = p;
    while at(p) != 0 && !space(at(p)) {
        p += 1;
    }
    let busname = &s[bus_start..p];
    if at(p) != 0 {
        p += 1;
    }

    let (addr, n) = crate::cstr::c_strtol(crate::cstr::CPtr::new(&b[p..]), 0);
    p += n;
    while space(at(p)) {
        p += 1;
    }
    Link {
        filename,
        protocol,
        busname,
        addr,
        busparam: &s[p..],
    }
}

/// The stream of one record: C's `Stream`, the record's `dpvt`.
pub struct Stream {
    lock: Arc<StreamCoreLock<StreamHooks>>,
    shared: Arc<Shared>,
}

/// Every stream, in creation order: C's `first` / `next`.
fn streams() -> &'static Mutex<Vec<Weak<StreamCoreLock<StreamHooks>>>> {
    static STREAMS: OnceLock<Mutex<Vec<Weak<StreamCoreLock<StreamHooks>>>>> = OnceLock::new();
    STREAMS.get_or_init(Default::default)
}

impl Stream {
    /// `new Stream(record, ...)`
    fn new(name: &str, prio: i16, db: Option<AsyncDbHandle>) -> Self {
        let shared = Arc::new(Shared {
            name: name.to_string(),
            slot: RecordSlot::default(),
            db: OnceLock::new(),
            init_waiter: Mutex::new(None),
            prio: AtomicI16::new(prio),
        });
        if let Some(db) = db {
            let _ = shared.db.set(db);
        }
        let gate_shared = shared.clone();
        let lock = StreamCoreLock::new_gated(
            name,
            |_| StreamHooks {
                shared: shared.clone(),
                link: RefCell::new(String::new()),
                read_data: Cell::new(None),
                write_data: Cell::new(None),
                status: Cell::new(ERROR),
                convert: Cell::new(DO_NOT_CONVERT),
                current_value_length: Cell::new(0),
                initialized: Cell::new(false),
                init_done: Cell::new(false),
            },
            Box::new(move |lock, entry| gate_shared.enter(lock, entry)),
        );
        streams().lock().push(Arc::downgrade(&lock));
        Self { lock, shared }
    }

    /// The record's name.
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// `streamInitRecord(record, ioLink, readData, writeData)`: `link` is
    /// the text of the record's INP or OUT.
    pub fn init_record(
        &self,
        record: &mut dyn Record,
        common: &mut CommonFields,
        link: &str,
        read_data: Option<StreamIoFunction>,
        write_data: Option<StreamIoFunction>,
    ) -> i64 {
        let name = self.name();
        {
            let core = self.lock.lock();
            core.hooks.read_data.set(read_data);
            core.hooks.write_data.set(write_data);
        }
        let link = match parse_link_v2(link) {
            ParsedLink::Hw(HwLink::InstIo { string }) => string,
            // a field the .db leaves out is the device's INST_IO, empty
            ParsedLink::None => String::new(),
            other => {
                stream_error(&cmsg!(format!(
                    "{name}: Wrong I/O link type {}\n",
                    other.db_link_type().c_name()
                )));
                return S_DEV_BAD_INIT_RET;
            }
        };
        let link = link.as_str();
        if link.is_empty() {
            stream_error(&cmsg!(
                name,
                b": Empty I/O link. Forgot the leading '@' or confused INP with OUT or link is too long ?\n"
            ));
            return S_DEV_BAD_INIT_RET;
        }
        // (re)initialize bus and protocol
        debug!(format!("streamInitRecord({name}): calling initRecord\n"));
        *self.lock.lock().hooks.link.borrow_mut() = link.to_string();
        let status = self.shared.slot.lend(record, common, || self.init(link));
        if status != OK && status != DO_NOT_CONVERT {
            stream_error(&cmsg!(name, b": Record initialization failed\n"));
        } else {
            self.lock.lock().hooks.initialized.set(true);
        }
        debug!(format!(
            "streamInitRecord({name}) done status={}\n",
            c_alt_hex(status)
        ));
        status
    }

    /// `Stream::initRecord(linkstring)`, the record lent.
    fn init(&self, linkstring: &str) -> i64 {
        let name = self.name();
        debug!(format!(
            "Stream::initRecord {name}: parse link string \"{linkstring}\"\n"
        ));
        let link = parse_link(linkstring);
        debug!(format!(
            "Stream::initRecord {name}: filename=\"{}\" protocol=\"{}\" bus=\"{}\" addr={} params=\"{}\"\n",
            link.filename, link.protocol, link.busname, link.addr, link.busparam
        ));
        if link.filename.is_empty() {
            stream_error(&cmsg!(name, b": Missing protocol file name\n"));
            return S_DEV_BAD_INIT_RET;
        }
        if link.protocol.is_empty() {
            stream_error(&cmsg!(name, b": Missing protocol name\n"));
            return S_DEV_BAD_INIT_RET;
        }
        if link.busname.is_empty() {
            stream_error(&cmsg!(name, b": Missing bus name\n"));
            return S_DEV_BAD_INIT_RET;
        }

        // attach to bus interface
        debug!(format!(
            "Stream::initRecord {name}: attachBus(\"{}\", {}, \"{}\")\n",
            link.busname, link.addr, link.busparam
        ));
        if !self
            .lock
            .attach_bus(link.busname, link.addr as i32, link.busparam)
        {
            stream_error(&cmsg!(format!(
                "{name}: Can't attach to bus {} {}\n",
                link.busname, link.addr
            )));
            return S_DEV_NO_DEVICE;
        }

        // parse protocol file
        debug!(format!(
            "Stream::initRecord {name}: parse(\"{}\", \"{}\")\n",
            link.filename, link.protocol
        ));
        if !self
            .lock
            .lock()
            .parse(link.filename, link.protocol.as_bytes())
        {
            stream_error(&cmsg!(name, b": Protocol parse error\n"));
            return S_DEV_NO_DEVICE;
        }

        // record is ready to use
        let has_init = {
            let core = self.lock.lock();
            core.hooks.status.set(NO_ALARM as i64);
            if core.hooks.initialized.get() {
                // we have been called by streamReload
                debug!(format!(
                    "Stream::initRecord {name}: re-initialize after streamReload\n"
                ));
            } else {
                debug!(format!(
                    "Stream::initRecord {name}: initialize the first time\n"
                ));
            }
            core.on_init.borrow().is_set()
        };
        if !has_init {
            return DO_NOT_CONVERT; // no @init handler, keep DOL
        }

        // initialize the record from hardware
        let (waiter, jobs) = mpsc::channel::<Job>();
        *self.shared.init_waiter.lock() = Some(waiter);
        self.lock.lock().hooks.init_done.set(false);
        if !self.lock.start_protocol(StartMode::StartInit) {
            self.end_init_wait(&jobs);
            stream_error(&cmsg!(name, b": Can't start @init handler\n"));
            return ERROR;
        }
        debug!(format!("Stream::initRecord {name}: waiting for initDone\n"));
        while !self.lock.lock().hooks.init_done.get() {
            let Ok(job) = jobs.recv() else { break };
            let _ = job.reply.send(self.lock.dispatch(job.entry.entry()));
        }
        self.end_init_wait(&jobs);
        debug!(format!("Stream::initRecord {name}: initDone\n"));

        // init run has set status and convert
        let core = self.lock.lock();
        let status = core.hooks.status.get();
        if status != NO_ALARM as i64 {
            self.shared
                .slot
                .with(|_, common| common.stat = status as u16);
            stream_error(&cmsg!(name, b": @init handler failed\n"));
            return ERROR;
        }
        let convert = core.hooks.convert.get();
        // C prints "convert" for DO_NOT_CONVERT and "don't convert" for the
        // rest (StreamEpics.cc initRecord), the wrong way round.
        debug!(format!(
            "Stream::initRecord {name}: initialized. {}\n",
            if convert == DO_NOT_CONVERT {
                "don't convert"
            } else {
                "convert"
            }
        ));
        convert
    }

    /// Stop sending entries to the `@init` waiter, and run those it was
    /// sent.
    fn end_init_wait(&self, jobs: &mpsc::Receiver<Job>) {
        *self.shared.init_waiter.lock() = None;
        while let Ok(job) = jobs.try_recv() {
            let _ = job.reply.send(self.lock.dispatch(job.entry.entry()));
        }
    }

    /// `streamReadWrite(record)`
    pub fn read_write(&self, _record: &mut dyn Record, common: &mut CommonFields) -> i64 {
        if self.lock.lock().hooks.status.get() == ERROR {
            epics_base_rs::server::recgbl::rec_gbl_set_sevr(
                common,
                UDF_ALARM,
                AlarmSeverity::Invalid,
            );
            return ERROR;
        }
        ERROR
    }
}

/// C's `%#lx` of a `long`.
fn c_alt_hex(v: i64) -> String {
    if v == 0 {
        "0".to_string()
    } else {
        format!("{:#x}", v as u64)
    }
}

/// A record's `DTYP stream` device support: C's dset for the record type,
/// whose `dpvt` is the [`Stream`].
pub struct StreamDevice<T: StreamRecordType> {
    record_type: T,
    stream: Option<Stream>,
    db: Option<AsyncDbHandle>,
    name: String,
}

impl<T: StreamRecordType> StreamDevice<T> {
    pub fn new(record_type: T) -> Self {
        Self {
            record_type,
            stream: None,
            db: None,
            name: String::new(),
        }
    }

    /// The record's stream, once the record was initialized.
    pub fn stream(&self) -> Option<&Stream> {
        self.stream.as_ref()
    }
}

/// A dset status the record's `init_record` passes on.
fn init_error(status: i64) -> CaError {
    CaError::InvalidValue(format!("init_record status {status:#x}"))
}

impl<T: StreamRecordType> DeviceSupport for StreamDevice<T> {
    fn bind_database(&mut self, db: AsyncDbHandle) {
        self.db = Some(db);
    }

    fn set_record_info(&mut self, name: &str, _scan: epics_base_rs::server::record::ScanType) {
        self.name = name.to_string();
    }

    fn set_callback_priority(&mut self, _prio: epics_base_rs::runtime::task::CallbackPriority) {}

    fn init_common(
        &mut self,
        record: &mut dyn Record,
        common: &mut CommonFields,
    ) -> CaResult<DeviceInitOutcome> {
        drv_init();
        stream_init(false);
        let name = self.name.clone();
        debug!(format!(
            "streamInitRecord({name}): SEVR={}\n",
            common.sevr as i32
        ));
        if self.stream.is_none() {
            // initialize the first time
            debug!(format!(
                "streamInitRecord({name}): create new Stream object\n"
            ));
            self.stream = Some(Stream::new(&name, common.prio, self.db.clone()));
        } else if let Some(stream) = self.stream.as_ref() {
            // stop any running protocol
            debug!(format!("streamInitRecord({name}): stop running protocol\n"));
            stream.shared.slot.lend(record, common, || {
                stream.lock.lock().finish_protocol(ProtocolResult::Abort)
            });
        }
        let stream = self.stream.as_ref().expect("created above");
        match self.record_type.init_record(stream, record, common) {
            OK => Ok(DeviceInitOutcome::ok()),
            DO_NOT_CONVERT => Ok(DeviceInitOutcome::DoNotConvert(Default::default())),
            status => Err(init_error(status)),
        }
    }

    fn read_begin_common(
        &mut self,
        _record: &mut dyn Record,
        _common: &mut CommonFields,
        _done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        Ok(ReadStart::Synchronous)
    }

    fn read_common(
        &mut self,
        record: &mut dyn Record,
        common: &mut CommonFields,
    ) -> CaResult<DeviceReadOutcome> {
        let Some(stream) = self.stream.as_ref() else {
            return Ok(DeviceReadOutcome::failed());
        };
        match self.record_type.read_write(stream, record, common) {
            OK => Ok(DeviceReadOutcome::ok()),
            DO_NOT_CONVERT => Ok(DeviceReadOutcome::computed(Default::default())),
            _ => Ok(DeviceReadOutcome::failed()),
        }
    }

    fn write_begin_common(
        &mut self,
        _record: &mut dyn Record,
        _common: &mut CommonFields,
        _done: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        Ok(WriteStart::Synchronous)
    }

    fn write_common(&mut self, record: &mut dyn Record, common: &mut CommonFields) -> CaResult<()> {
        if let Some(stream) = self.stream.as_ref() {
            self.record_type.read_write(stream, record, common);
        }
        Ok(())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "stream"
    }
}
