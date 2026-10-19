//! `StreamEpics.cc`: the record layer, C's `Stream` subclass of
//! `StreamCore`, and the device support functions `devStream.h` declares
//! for the record types' `dev*Stream.c`.
//!
//! A record type's device support builds a [`StreamDevice`] with its
//! [`StreamRecordType`], which carries its `initRecord` and the `readData`
//! / `writeData` it hands C's `streamInitRecord` in [`StreamInitArgs`];
//! those reach the record's value through the record
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
use std::sync::atomic::{AtomicBool, AtomicI16, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak, mpsc};
use std::thread::ThreadId;
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::background::{CallbackBody, CallbackCell};
use epics_base_rs::runtime::task::{
    CallbackPriority, request_cell, sleep_background, spawn_background,
};
use epics_base_rs::server::database::{
    AsyncDbHandle, DeviceDone, DeviceDoneSource, ProcessCallback,
};
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DevicePost, DeviceReadOutcome, DeviceSupport, IoIntrCmd, ReadStart,
    WriteStart,
};
use epics_base_rs::server::driver_support::{DriverSupport, register_driver_support};
use epics_base_rs::server::ioc_app::{InitHookState, init_hook_register};
use epics_base_rs::server::recgbl::alarm_status::{
    CALC_ALARM, COMM_ALARM, NO_ALARM, READ_ALARM, TIMEOUT_ALARM, UDF_ALARM, WRITE_ALARM,
};
use epics_base_rs::server::recgbl::{
    AlarmResetResult, EventMask, alarm_condition_string, rec_gbl_set_sevr,
};
use epics_base_rs::server::record::{
    AlarmSeverity, CommonFields, HwLink, ParsedLink, Record, ScanType, parse_link_v2,
};
use epics_base_rs::types::EpicsValue;
use parking_lot::{Condvar, Mutex};

use crate::StreamBuffer;
use crate::bus::{OwnedReadInput, StreamIoStatus};
use crate::core::{
    ABORTED, ACCEPT_INPUT, Entry, INIT_RUN, ProtocolResult, SCAN_TRIED, StartMode, StreamCore,
    StreamCoreHooks, StreamCoreLock,
};
use crate::error::{STREAM_ERROR, stream_debug_print, stream_error};
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

    /// `streamPrintf(record, format, long)`: [`OK`] or [`ERROR`].
    pub fn printf_long(&self, format: &Format<'_>, value: i64) -> i64 {
        self.print(format, |c| match format.type_ {
            Dbf::Ulong | Dbf::Long | Dbf::Enum => c.print_value_long(format.priv_, value),
            _ => self.illegal_print(format),
        })
    }

    /// `streamPrintf(record, format, double)`
    pub fn printf_double(&self, format: &Format<'_>, value: f64) -> i64 {
        self.print(format, |c| match format.type_ {
            Dbf::Double => c.print_value_double(format.priv_, value),
            _ => self.illegal_print(format),
        })
    }

    /// `streamPrintf(record, format, char*)`: `value` up to its first 0x00.
    pub fn printf_string(&self, format: &Format<'_>, value: &[u8]) -> i64 {
        self.print(format, |c| match format.type_ {
            Dbf::String => c.print_value_string(format.priv_, value),
            _ => self.illegal_print(format),
        })
    }

    /// `record->pact`: a protocol `process` started is running.
    pub fn pact(&self) -> bool {
        self.core.hooks.run.get() == Run::Active
    }

    /// `db_post_events(record, &field, mask)` with the field's `value` now,
    /// posted by whoever holds the record before it lets go.
    pub fn post_events(&self, field: &'static str, value: EpicsValue, mask: EventMask) {
        self.post(DevicePost { field, mask, value });
    }

    /// The posts `recGblResetAlarms` made for `reset`, as
    /// [`Self::post_events`] does.
    pub fn post_alarm_reset(&self, common: &CommonFields, reset: &AlarmResetResult) {
        for post in DevicePost::alarm_reset(common, reset) {
            self.post(post);
        }
    }

    fn post(&self, post: DevicePost) {
        let shared = &self.core.hooks.shared;
        // No database, no monitors.
        if shared.db.get().is_some() {
            shared.posts.lock().push(post);
        }
    }

    /// `INIT_RUN`, `!interruptAccept`: the record is processed by its
    /// `@init` run, before iocInit ends.
    pub fn init_run(&self) -> bool {
        !self
            .core
            .hooks
            .shared
            .db
            .get()
            .is_some_and(AsyncDbHandle::interrupt_accept)
    }

    /// `streamPrintf`, `Stream::print`'s type switch given as `print`.
    fn print(
        &self,
        format: &Format<'_>,
        print: impl FnOnce(&StreamCore<StreamHooks>) -> bool,
    ) -> i64 {
        debug!(format!(
            "streamPrintf({},format=%{})\n",
            self.name(),
            format.priv_.conv as char
        ));
        if print(self.core) { OK } else { ERROR }
    }

    /// C reads the `va_arg` its format's type names, whatever the caller
    /// passed: a type no `dev*Stream.c` passes is refused here.
    fn illegal_print(&self, format: &Format<'_>) -> bool {
        stream_error(&cmsg!(format!(
            "INTERNAL ERROR ({}): Illegal format type {}\n",
            self.name(),
            format.type_ as i32
        )));
        false
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

/// What a record type's `initRecord` passes `streamInitRecord`, and how it
/// returns the status.
pub struct StreamInitArgs {
    /// The text of the record's INP or OUT.
    pub link: String,
    pub read_data: Option<StreamIoFunction>,
    pub write_data: Option<StreamIoFunction>,
    /// `streamInitRecord(...) == ERROR ? ERROR : OK`; else the status as is.
    pub map_error: bool,
}

impl StreamInitArgs {
    fn status(&self, status: i64) -> i64 {
        if !self.map_error || status == ERROR {
            status
        } else {
            OK
        }
    }
}

/// A record type's `dev*Stream.c`.
pub trait StreamRecordType: Send + Sync + 'static {
    /// The dset's `init_record`: C's `initRecord` up to its call of
    /// `streamInitRecord`, which the caller makes after it, with what it
    /// returns. A record type's own init (`mask <<= shft`, `sevr = 0`)
    /// thus runs before `streamInitRecord` prints SEVR, on every
    /// `init_record`, `streamReload`'s too.
    fn init_record(&self, record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs;

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
// `RecordSlot::with`). Each `lend` takes `&mut` of the record, so the
// record lock is held: the slot holds one thread's entries at a time.
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
        assert!(
            previous
                .as_ref()
                .is_none_or(|p| p.thread == std::thread::current().id()),
            "a record lent on two threads at once"
        );
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
    Read(StreamIoStatus, OwnedReadInput),
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
            Entry::Read(s, input) => Self::Read(s, input.to_owned()),
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
            Self::Read(s, input) => Entry::Read(*s, input.as_input()),
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
    /// The core, for the callbacks that enter it.
    lock: OnceLock<Weak<StreamCoreLock<StreamHooks>>>,
    /// The record type's entries: an output's completing pass calls its I/O
    /// entry as C's `rset->process` does.
    record_type: OnceLock<Arc<dyn StreamRecordType>>,
    /// The completion the pass that started the protocol was given.
    device_done: Mutex<Option<DeviceDone>>,
    /// C's `processCallback`: [`RecordProcess`] on a callback band.
    process_callback: OnceLock<Arc<CallbackCell<RecordProcess>>>,
    /// The record's re-entry, an input's completing pass.
    reprocess: OnceLock<ProcessCallback>,
    /// The timer's current start; an earlier start's expiry is dropped, as
    /// C's `epicsTimer::start` replaces it.
    timer_start: AtomicU64,
    /// `initDone`, set by the finish of an `@init` run.
    init_done: Mutex<bool>,
    init_signal: Condvar,
    /// The process callbacks requested, and how many of those requests a
    /// run has started after: `streamReload` waits for the second.
    process_requested: AtomicU64,
    process_served: Mutex<u64>,
    process_signal: Condvar,
    /// The `db_post_events` calls of the record's `readData`, for whoever
    /// holds the record to post before it lets go.
    posts: Mutex<Vec<DevicePost>>,
}

impl Shared {
    /// [`AsyncDbHandle::with_instance`], the posts `f` made delivered after
    /// the record is let go: the bus, timer and `exec` callbacks this takes
    /// the record for run unlocked in C.
    fn with_instance<R>(
        &self,
        db: &AsyncDbHandle,
        f: impl FnOnce(&mut dyn Record, &mut CommonFields) -> R,
    ) -> Option<R> {
        let (r, posts) = db.with_instance(&self.name, |record, common| {
            let r = f(record, common);
            (r, std::mem::take(&mut *self.posts.lock()))
        })?;
        db.post_events(&self.name, posts);
        Some(r)
    }

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
        self.with_instance(db, |record, common| {
            self.slot.lend(record, common, || lock.dispatch(entry))
        })
        .unwrap_or(0)
    }

    /// Every process callback requested up to `request` has run.
    fn served(&self, request: u64) {
        let mut served = self.process_served.lock();
        *served = (*served).max(request);
        self.process_signal.notify_all();
    }

    /// Wait until a run of the process callback has finished after every
    /// request made so far.
    fn wait_process_callbacks(&self) {
        let requested = self.process_requested.load(Ordering::Acquire);
        let mut served = self.process_served.lock();
        while *served < requested {
            self.process_signal.wait(&mut served);
        }
    }

    /// Run `f` with the record taken and lent, given its SCAN; `None` when
    /// the stream has no database or the record is gone.
    fn held<R>(&self, f: impl FnOnce(ScanType) -> R) -> Option<R> {
        self.with_instance(self.db.get()?, |record, common| {
            let scan = common.scan;
            self.slot.lend(record, common, || f(scan))
        })
    }

    /// `Stream::process()`, the record lent: whether the record goes on.
    fn process(&self, lock: &StreamCoreLock<StreamHooks>) -> bool {
        let core = lock.lock();
        let hooks = &core.hooks;
        let name = self.name.as_str();
        debug!(format!("Stream::process({name})\n"));
        let Some((completing, proc_field, tpro)) = self.slot.with(|_, common| {
            (
                hooks.run.get() == Run::Active || common.scan == ScanType::IoIntr,
                common.proc_field,
                common.tpro,
            )
        }) else {
            return false;
        };
        if completing {
            // C's record clears PACT at the end of this pass.
            hooks.run.set(Run::Idle);
            return self.complete(hooks);
        }
        debug!(format!("Stream::process({name}) start\n"));
        hooks.status.set(NO_ALARM as i64);
        hooks.convert.set(OK);
        if tpro != 0 {
            let msg = cmsg!(b"start protocol '", &*core.protocolname.borrow(), b"'\n");
            stream_debug_print(Some(name), 0, &msg);
        }
        let init = proc_field == 2;
        hooks.run.set(Run::Starting);
        let started = lock.start_protocol(if init {
            StartMode::StartInit
        } else {
            StartMode::StartNormal
        });
        let run = hooks.run.replace(Run::Idle);
        if !started {
            let status = hooks.status.get();
            debug!(format!(
                "Stream::process({name}): could not start {}protocol, status={} ({status})\n",
                if init { "@init " } else { "" },
                alarm_text(status)
            ));
            self.slot.with(|_, common| {
                let stat = if status != 0 {
                    status as u16
                } else {
                    UDF_ALARM
                };
                rec_gbl_set_sevr(common, stat, AlarmSeverity::Invalid);
                common.proc_field = 0;
            });
            return false;
        }
        debug!(format!("Stream::process({name}): protocol started\n"));
        if run == Run::FinishedInline {
            // C sets PACT here (StreamEpics.cc:884-885) after a synchronous
            // bus already finished the protocol, and protocolFinishHook
            // (:1036-1041) requested no processCallback as PACT was still
            // 0, so the record stays active for good: this pass completes.
            return self.complete(hooks);
        }
        hooks.run.set(Run::Active);
        true
    }

    /// `Stream::process()`'s completing pass, the record lent: whether the
    /// record goes on.
    fn complete(&self, hooks: &StreamHooks) -> bool {
        let name = self.name.as_str();
        let status = hooks.status.get();
        self.slot.with(|_, common| {
            common.proc_field = 0;
            if status != NO_ALARM as i64 {
                rec_gbl_set_sevr(common, status as u16, AlarmSeverity::Invalid);
            }
        });
        if status != NO_ALARM as i64 {
            debug!(format!(
                "Stream::process({name}) error status={} ({status})\n",
                alarm_text(status)
            ));
            return false;
        }
        debug!(format!(
            "Stream::process({name}) ready. {}\n",
            convert_text(hooks.convert.get())
        ));
        true
    }
}

/// Where the protocol a `process` pass starts stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Run {
    /// None runs: C's `record->pact` 0.
    Idle,
    /// `process` is in `startProtocol`.
    Starting,
    /// The protocol finished before `startProtocol` returned.
    FinishedInline,
    /// The protocol `process` started is running: C's `record->pact` 1.
    Active,
}

/// `epicsAlarmConditionStrings[status]`, or "ERROR" outside it.
fn alarm_text(status: i64) -> &'static str {
    match u16::try_from(status).map(alarm_condition_string) {
        Ok(text) if !text.is_empty() => text,
        _ => "ERROR",
    }
}

/// Whether the record converts the value it was given. C prints "convert"
/// for DO_NOT_CONVERT and "don't convert" for the rest (StreamEpics.cc
/// `initRecord` and `process`), the wrong way round.
fn convert_text(convert: i64) -> &'static str {
    if convert == DO_NOT_CONVERT {
        "don't convert"
    } else {
        "convert"
    }
}

/// C's `recordProcessCallback`, the body of the stream's `processCallback`.
struct RecordProcess(Weak<Shared>);

impl CallbackBody for RecordProcess {
    fn run(&self) {
        let Some(shared) = self.0.upgrade() else {
            return;
        };
        struct Served<'a>(&'a Shared, u64);
        impl Drop for Served<'_> {
            fn drop(&mut self) {
                self.0.served(self.1);
            }
        }
        let _served = Served(&shared, shared.process_requested.load(Ordering::Acquire));
        let name = shared.name.as_str();
        // process record
        // This will call streamReadWrite.
        debug!(format!("recordProcessCallback({name}) processing record\n"));
        let output = shared.record_type.get().is_some_and(|t| t.is_output());
        let pending = shared.device_done.lock().take();
        if let (true, Some(done)) = (output, pending) {
            // An output's pass ends with the device's completion, which runs
            // no device code: the record type's I/O entry runs here. With no
            // pass pending (I/O Intr) the record processes, as C's
            // rset->process does for every record.
            let stream = shared
                .lock
                .get()
                .and_then(Weak::upgrade)
                .map(|lock| Stream {
                    lock,
                    shared: shared.clone(),
                });
            if let (Some(stream), Some(db), Some(record_type)) =
                (stream, shared.db.get(), shared.record_type.get())
            {
                shared.with_instance(db, |record, common| {
                    record_type.read_write(&stream, record, common);
                });
            }
            // C's rset->process completes the write before the "done" line.
            done.complete_now();
        } else if let Some(reprocess) = shared.reprocess.get() {
            reprocess.process_now();
        }
        debug!(format!(
            "recordProcessCallback({name}) processing record done\n"
        ));
        let Some(lock) = shared.lock.get().and_then(Weak::upgrade) else {
            return;
        };
        shared.held(|scan| {
            if scan == ScanType::IoIntr && lock.lock().flags.get() & ABORTED == 0 {
                // restart protocol for next turn
                debug!(format!(
                    "recordProcessCallback({name}) restart async protocol\n"
                ));
                if !lock.start_protocol(StartMode::StartAsync) {
                    stream_error(&cmsg!(name, b": Can't restart \"I/O Intr\" protocol\n"));
                }
            }
        });
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
    /// The protocol a `process` pass started: C's `record->pact` as
    /// `process` sets it.
    run: Cell<Run>,
}

impl StreamHooks {
    fn with_record<R>(&self, f: impl FnOnce(&mut dyn Record, &mut CommonFields) -> R) -> Option<R> {
        self.shared.slot.with(f)
    }
}

impl StreamCoreHooks for StreamHooks {
    fn input_hook(&self, core: &StreamCore<Self>, input: &[u8]) {
        if self.with_record(|_, c| c.tpro).unwrap_or(0) > 1 {
            let msg = cmsg!(
                b"received \"",
                StreamBuffer::from_bytes(input).expand(),
                b"\"\n"
            );
            stream_debug_print(Some(core.name()), 0, &msg);
        }
    }

    fn protocol_finish_hook(&self, core: &StreamCore<Self>, result: ProtocolResult) {
        debug!(format!(
            "Stream::protocolFinishHook({}, {})\n",
            core.name(),
            result.to_str()
        ));
        let run = self.run.get();
        let pact = run == Run::Active;
        let (io_intr, proc_field, tpro) = self
            .with_record(|_, c| (c.scan == ScanType::IoIntr, c.proc_field, c.tpro))
            .unwrap_or((false, 0, 0));
        if tpro != 0 {
            let msg = cmsg!(
                result.to_str(),
                b". out=\"",
                core.output_line.borrow().expand(),
                b"\", in=\"",
                core.input_line.borrow().expand(),
                b"\"\n"
            );
            stream_debug_print(Some(core.name()), 0, &msg);
        }
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
            ProtocolResult::Abort | ProtocolResult::Fault => {
                if pact || io_intr {
                    stream_error(&cmsg!(core.name(), b": Protocol aborted\n"));
                }
                UDF_ALARM
            }
        };
        self.status.set(status as i64);
        if flags & (INIT_RUN | ABORTED) == INIT_RUN && proc_field != 2 {
            debug!(format!(
                "Stream::protocolFinishHook {}: signalling init done\n",
                core.name()
            ));
            *self.shared.init_done.lock() = true;
            self.shared.init_signal.notify_all();
            return;
        }
        if result != ProtocolResult::Abort && io_intr {
            // re-enable early input
            core.flags.set(core.flags.get() | ACCEPT_INPUT);
        }
        if run == Run::Starting {
            // process completes the record when startProtocol returns
            self.run.set(Run::FinishedInline);
        } else if (pact || io_intr) && !TERMINATING.load(Ordering::Acquire) {
            // process record in callback thread to break possible recursion
            if let Some(cell) = self.shared.process_callback.get() {
                let prio =
                    CallbackPriority::from_record_prio(self.shared.prio.load(Ordering::Relaxed));
                let request = self.shared.process_requested.fetch_add(1, Ordering::AcqRel) + 1;
                if request_cell(prio, cell).is_err() {
                    // refused: no run will come for it
                    self.shared.served(request);
                }
            }
        }
    }

    fn start_timer(&self, core: &StreamCore<Self>, timeout_ms: u64) {
        debug!(format!(
            "Stream::startTimer(stream={}, timeout={timeout_ms}) = {:.6} seconds\n",
            core.name(),
            timeout_ms as f64 * 0.001
        ));
        let start = self.shared.timer_start.fetch_add(1, Ordering::AcqRel) + 1;
        let Some(lock) = self.shared.lock.get().cloned() else {
            return;
        };
        let shared = Arc::downgrade(&self.shared);
        let prio = CallbackPriority::from_record_prio(self.shared.prio.load(Ordering::Relaxed));
        drop(spawn_background(prio, async move {
            sleep_background(Duration::from_millis(timeout_ms)).await;
            let current = shared
                .upgrade()
                .is_some_and(|s| s.timer_start.load(Ordering::Acquire) == start);
            if let (true, Some(lock)) = (current, lock.upgrade()) {
                lock.timer_callback();
            }
        }));
    }

    fn format_value(
        &self,
        core: &StreamCore<Self>,
        format: &StreamFormat,
        fieldaddress: Option<&[u8]>,
    ) -> bool {
        // C's line has no closing parenthesis
        debug!(format!(
            "Stream::formatValue({}, format=%{}, fieldaddr={}\n",
            core.name(),
            format.conv as char,
            c_ptr(fieldaddress.map(|a| a.as_ptr().cast()))
        ));
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

    fn interrupt_accept(&self, _core: &StreamCore<Self>) -> bool {
        self.shared
            .db
            .get()
            .is_some_and(AsyncDbHandle::interrupt_accept)
    }
}

/// `drvStreamInit` has run.
static DRV_INIT: OnceLock<()> = OnceLock::new();

/// Set by `streamExitHook`: no record is processed after the protocol ends.
static TERMINATING: AtomicBool = AtomicBool::new(false);

/// stream.dbd's `driver(stream)`: the `stream` drvet (StreamEpics.cc:276-280),
/// whose `drvStreamInit` iocInit runs in `initDrvSup` (iocInit.c:167) whether
/// or not a record uses it.
pub fn register_driver() {
    register_driver_support("stream", Arc::new(Drvet));
}

/// `StreamVersion` (StreamVersion.c) as a build without git or `.VERSION`
/// metadata spells it: the StreamDevice release this crate ports, with no
/// commit date or hash, those of a C build naming its StreamDevice
/// checkout.
pub const STREAM_VERSION: &str = "StreamDevice 2.8.26";

/// `struct drvet stream`
struct Drvet;

impl DriverSupport for Drvet {
    fn report(&self, interest: i32) -> Option<Vec<u8>> {
        Some(report(interest))
    }

    fn init(&self) {
        drv_init();
    }
}

/// `Stream::report(interest)`, its `printf`s' bytes.
fn report(interest: i32) -> Vec<u8> {
    use std::io::Write as _;
    let mut out = Vec::new();
    let _ = writeln!(out, "  {STREAM_VERSION}");
    out.extend_from_slice(b"  (C) 1999 Dirk Zimoch (dirk.zimoch@psi.ch)\n");
    if interest == 100 {
        let _ = write!(out, "\n{}", crate::core::license());
        return out;
    }
    out.extend_from_slice(b"  Use interest level 100 for license information\n");
    out.extend_from_slice(b"  registered bus interfaces:\n");
    for name in crate::bus::stream_bus_interface_names() {
        let _ = writeln!(out, "    {name}");
    }
    if interest < 1 {
        return out;
    }
    out.extend_from_slice(b"  registered converters:\n");
    for c in 0..=255u8 {
        if let Some(converter) = crate::converter::find(c) {
            out.extend_from_slice(b"    %");
            out.push(c);
            let _ = writeln!(out, " {}", converter.name);
        }
    }
    out.extend_from_slice(b"  connected records:\n");
    for stream in all_streams() {
        if interest == 2 {
            let _ = writeln!(out, "\n{}: {}", stream.name(), stream.link());
            let _ = stream.lock.lock().print_protocol(&mut out);
        } else {
            let _ = writeln!(out, "    {}: {}", stream.name(), stream.link());
            if interest == 3 {
                let mut buffer = StreamBuffer::new();
                stream.lock.lock().print_status(&mut buffer);
                out.extend_from_slice(b"      ");
                out.extend_from_slice(buffer.bytes());
                out.push(b'\n');
            }
        }
    }
    out
}

/// `Stream::drvInit`, the driver's `init`.
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
        init_hook_register(Arc::new(init_hook));
        epics_base_rs::runtime::exit::at_exit("streamExitHook", || {
            TERMINATING.store(true, Ordering::Release);
        });
    });
}

/// `Stream::initHook(state)`
fn init_hook(state: InitHookState) {
    let all: Vec<Arc<Shared>> = streams()
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .map(|lock| lock.lock().hooks.shared.clone())
        .collect();
    let lock_of = |shared: &Shared| shared.lock.get().and_then(Weak::upgrade);
    match state {
        InitHookState::AtIocRun => {
            // re-run @init handlers after restart
            static IN_IOC_INIT: AtomicBool = AtomicBool::new(true);
            if IN_IOC_INIT.swap(false, Ordering::AcqRel) {
                // We don't want to run @init twice in iocInit
                return;
            }
            for shared in all {
                let Some(lock) = lock_of(&shared) else {
                    continue;
                };
                if !lock.lock().on_init.borrow().is_set() {
                    continue;
                }
                let name = shared.name.as_str();
                debug!(format!("{name}: running @init handler\n"));
                *shared.init_done.lock() = false;
                if !shared
                    .held(|_| lock.start_protocol(StartMode::StartInit))
                    .unwrap_or(false)
                {
                    stream_error(&cmsg!(name, b": @init handler failed.\n"));
                }
                let mut done = shared.init_done.lock();
                while !*done {
                    shared.init_signal.wait(&mut done);
                }
            }
        }
        InitHookState::AtIocPause => {
            // stop polling I/O Intr
            for shared in all {
                let Some(lock) = lock_of(&shared) else {
                    continue;
                };
                shared.held(|scan| {
                    if scan == ScanType::IoIntr {
                        debug!(format!("{}: stopping \"I/O Intr\"\n", shared.name));
                        lock.lock().finish_protocol(ProtocolResult::Abort);
                    }
                });
            }
        }
        InitHookState::AfterIocRunning => {
            // start polling I/O Intr
            for shared in all {
                let Some(lock) = lock_of(&shared) else {
                    continue;
                };
                shared.held(|scan| {
                    if scan == ScanType::IoIntr {
                        let name = shared.name.as_str();
                        debug!(format!("{name}: starting \"I/O Intr\"\n"));
                        if !lock.start_protocol(StartMode::StartAsync) {
                            stream_error(&cmsg!(name, b": Can't start \"I/O Intr\" protocol\n"));
                        }
                    }
                });
            }
        }
        _ => {}
    }
}

/// `streamInit(after)`, the dset's `init`: errors are on while the
/// protocol files are read.
pub(crate) fn stream_init(after: bool) {
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
    fn new(
        name: &str,
        prio: i16,
        db: Option<AsyncDbHandle>,
        record_type: Arc<dyn StreamRecordType>,
    ) -> Self {
        let shared = Arc::new_cyclic(|me: &Weak<Shared>| Shared {
            name: name.to_string(),
            slot: RecordSlot::default(),
            db: OnceLock::new(),
            init_waiter: Mutex::new(None),
            prio: AtomicI16::new(prio),
            lock: OnceLock::new(),
            record_type: OnceLock::from(record_type),
            device_done: Mutex::new(None),
            process_callback: OnceLock::from(CallbackCell::new(RecordProcess(me.clone()))),
            reprocess: OnceLock::new(),
            timer_start: AtomicU64::new(0),
            init_done: Mutex::new(false),
            init_signal: Condvar::new(),
            process_requested: AtomicU64::new(0),
            process_served: Mutex::new(0),
            process_signal: Condvar::new(),
            posts: Mutex::new(Vec::new()),
        });
        if let Some(db) = db {
            let _ = shared.reprocess.set(db.process_callback(name));
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
                run: Cell::new(Run::Idle),
            },
            Box::new(move |lock, entry| gate_shared.enter(lock, entry)),
        );
        let _ = shared.lock.set(Arc::downgrade(&lock));
        streams().lock().push(Arc::downgrade(&lock));
        Self { lock, shared }
    }

    /// The record's name.
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// `streamInitRecord(record, ioLink, readData, writeData)` from its
    /// link checks on.
    fn init_record(
        &self,
        record: &mut dyn Record,
        common: &mut CommonFields,
        args: &StreamInitArgs,
    ) -> i64 {
        let name = self.name();
        {
            let core = self.lock.lock();
            core.hooks.read_data.set(args.read_data);
            core.hooks.write_data.set(args.write_data);
        }
        let link = match parse_link_v2(&args.link) {
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
        let reload = {
            let core = self.lock.lock();
            core.hooks.status.set(NO_ALARM as i64);
            core.hooks.initialized.get()
        };
        if reload {
            // we have been called by streamReload
            debug!(format!(
                "Stream::initRecord {name}: re-initialize after streamReload\n"
            ));
            let io_intr = self.shared.slot.with(|_, c| c.scan == ScanType::IoIntr);
            if io_intr == Some(true) {
                debug!(format!(
                    "Stream::initRecord {name}: restarting \"I/O Intr\" after reload\n"
                ));
                if !self.lock.start_protocol(StartMode::StartAsync) {
                    stream_error(&cmsg!(name, b": Can't restart \"I/O Intr\" protocol\n"));
                }
            }
        } else {
            debug!(format!(
                "Stream::initRecord {name}: initialize the first time\n"
            ));
        }
        let has_init = self.lock.lock().on_init.borrow().is_set();
        if !has_init {
            return DO_NOT_CONVERT; // no @init handler, keep DOL
        }

        // initialize the record from hardware
        let (waiter, jobs) = mpsc::channel::<Job>();
        *self.shared.init_waiter.lock() = Some(waiter);
        *self.shared.init_done.lock() = false;
        if !self.lock.start_protocol(StartMode::StartInit) {
            self.end_init_wait(&jobs);
            stream_error(&cmsg!(name, b": Can't start @init handler\n"));
            return ERROR;
        }
        debug!(format!("Stream::initRecord {name}: waiting for initDone\n"));
        while !*self.shared.init_done.lock() {
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
        debug!(format!(
            "Stream::initRecord {name}: initialized. {}\n",
            convert_text(convert)
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

    /// `streamReadWrite(record)`, the dset's I/O entry.
    pub fn read_write(&self, record: &mut dyn Record, common: &mut CommonFields) -> i64 {
        if self.lock.lock().hooks.status.get() == ERROR {
            rec_gbl_set_sevr(common, UDF_ALARM, AlarmSeverity::Invalid);
            return ERROR;
        }
        if self
            .shared
            .slot
            .lend(record, common, || self.shared.process(&self.lock))
        {
            self.lock.lock().hooks.convert.get()
        } else {
            ERROR
        }
    }

    /// A protocol `process` started is running: the record stays active.
    fn active(&self) -> bool {
        self.lock.lock().hooks.run.get() == Run::Active
    }

    /// `streamInitRecord`'s start for a record that has its stream.
    fn stop_running_protocol(&self, record: &mut dyn Record, common: &mut CommonFields) {
        // stop any running protocol
        debug!(format!(
            "streamInitRecord({}): stop running protocol\n",
            self.name()
        ));
        self.shared.slot.lend(record, common, || {
            self.lock.lock().finish_protocol(ProtocolResult::Abort)
        });
    }

    /// `record->dset->init_record(record)` as `streamReload` calls it: the
    /// record type's `initRecord`, which reaches `streamInitRecord` again.
    ///
    /// C holds no record lock here, so the process callback of the protocol
    /// it aborts runs while the record re-initializes: in practice before
    /// `initRecord` resets the status, and the callback finishes the record
    /// with the abort's alarm, leaving the restart to `initRecord`. Holding
    /// the record, rs waits for that callback between the two halves: run
    /// after `initRecord`, it would finish the record without the alarm and
    /// start a second I/O Intr protocol beside `initRecord`'s.
    fn reinit_record(&self, db: &AsyncDbHandle) -> Option<i64> {
        let name = self.name();
        let record_type = self.shared.record_type.get()?;
        let args = db.with_instance(name, |record, common| {
            let args = record_type.init_record(record, common);
            debug!(format!(
                "streamInitRecord({name}): SEVR={}\n",
                common.sevr as i32
            ));
            self.stop_running_protocol(record, common);
            args
        })?;
        // C races here: streamReload (StreamEpics.cc:193) runs init_record
        // unlocked against recordProcessCallback's dbScanLock (:1052); rs
        // fixes the order C showed in every run, callback first.
        self.shared.wait_process_callbacks();
        db.with_instance(name, |record, common| {
            args.status(self.init_record(record, common, &args))
        })
    }

    /// The link text after the `@`, as `ioLink->value.instio.string`.
    fn link(&self) -> String {
        self.lock.lock().hooks.link.borrow().clone()
    }
}

/// Every stream, in creation order, as C walks `Stream::first`.
fn all_streams() -> Vec<Stream> {
    streams()
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .map(|lock| {
            let shared = lock.lock().hooks.shared.clone();
            Stream { lock, shared }
        })
        .collect()
}

fn glob(name: &str, pattern: &str) -> bool {
    epics_base_rs::runtime::epics_string::epics_str_glob_match(name.as_bytes(), pattern.as_bytes())
}

/// `streamReload(recordname)`: run the `init_record` of each stream's
/// record, all of them for no name or an empty one, which stops its
/// protocol and reads the protocol file again; `out` is the shell's stdout.
pub fn stream_reload(recordname: Option<&str>, out: &mut dyn std::io::Write) -> i64 {
    let old_stream_error = STREAM_ERROR.swap(1, Ordering::Relaxed);
    // The shell that runs this has a database: C's "No database has been
    // loaded" (no pdbbase) cannot happen here.
    debug!(format!(
        "streamReload({})\n",
        recordname.unwrap_or("(null)")
    ));
    for stream in all_streams() {
        let name = stream.name();
        if let Some(pattern) = recordname
            && !pattern.is_empty()
            && !glob(name, pattern)
        {
            continue;
        }
        // This cancels any running protocol and reloads
        // the protocol file
        let status = stream
            .shared
            .db
            .get()
            .and_then(|db| stream.reinit_record(db));
        if matches!(status, Some(OK | DO_NOT_CONVERT)) {
            let _ = writeln!(out, "{name}: Protocol reloaded");
        } else {
            stream_error(&cmsg!(name, b": Protocol reload failed\n"));
        }
    }
    protocol::free();
    STREAM_ERROR.store(old_stream_error, Ordering::Relaxed);
    OK
}

/// `streamReportRecord(recordname)`: each matching stream's link, status
/// and protocol; all of them for no name.
pub fn stream_report_record(recordname: Option<&str>, out: &mut dyn std::io::Write) -> i64 {
    for stream in all_streams() {
        let name = stream.name();
        if recordname.is_some_and(|pattern| !glob(name, pattern)) {
            continue;
        }
        let _ = writeln!(out, "{name}: {}", stream.link());
        let core = stream.lock.lock();
        let mut buffer = StreamBuffer::new();
        core.print_status(&mut buffer);
        let _ = out.write_all(buffer.bytes());
        let _ = out.write_all(b"\n");
        let _ = core.print_protocol(out);
        let _ = out.write_all(b"\n");
    }
    OK
}

/// `streamSetLogfile`: debug and error lines also go to `filename`,
/// truncated; none closes the file.
pub fn stream_set_logfile(filename: Option<&str>, _: &mut dyn std::io::Write) -> i64 {
    let file = match filename.map(std::fs::File::create).transpose() {
        Ok(file) => file,
        Err(e) => {
            // strerror's text, without Rust's " (os error N)"
            let text = e.to_string();
            let text = text.rfind(" (os error ").map_or(&*text, |at| &text[..at]);
            eprintln!(
                "Opening file {} failed: {text}",
                filename.unwrap_or_default()
            );
            return ERROR;
        }
    };
    crate::error::set_stream_debug_file(
        file.map(|f| Box::new(f) as Box<dyn std::io::Write + Send>),
    );
    OK
}

/// `streamRegistrar`'s iocsh commands `streamReload`,
/// `streamReportRecord` and `streamSetLogfile`.
pub fn iocsh_commands() -> Vec<epics_base_rs::server::iocsh::registry::CommandDef> {
    use epics_base_rs::server::iocsh::registry::{
        ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
    };
    type Command = fn(Option<&str>, &mut dyn std::io::Write) -> i64;
    let def = |name: &'static str, arg: &'static str, run: Command| {
        CommandDef::new(
            name,
            vec![ArgDesc {
                name: arg,
                arg_type: ArgType::String,
            }],
            // C's iocshFuncDef has no usage: help prints the synopsis only
            "",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let arg = match args.first() {
                    Some(ArgValue::String(s)) => Some(s.to_str_lossy().into_owned()),
                    _ => None,
                };
                let mut out = Vec::new();
                run(arg.as_deref(), &mut out);
                // println_bytes adds the newline the last line has
                if let Some(text) = out.strip_suffix(b"\n") {
                    ctx.println_bytes(text);
                }
                Ok(CommandOutcome::Continue)
            },
        )
    };
    vec![
        def("streamReload", "recordname", stream_reload),
        def("streamReportRecord", "recordname", stream_report_record),
        def("streamSetLogfile", "filename", stream_set_logfile),
    ]
}

/// C's `%p`: glibc prints a null pointer as "(nil)".
fn c_ptr(p: Option<*const ()>) -> String {
    p.map_or_else(|| "(nil)".to_string(), |p| format!("{p:p}"))
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
pub struct StreamDevice {
    record_type: Arc<dyn StreamRecordType>,
    stream: Option<Stream>,
    db: Option<AsyncDbHandle>,
    name: String,
    /// What the pass's I/O entry returned, for the read that follows it.
    status: i64,
    /// `ioscanpvt`, held so the record stays on the I/O Intr list: nothing
    /// requests a scan on it, the stream processes its record itself.
    ioscanpvt: Option<epics_base_rs::runtime::sync::mpsc::Sender<()>>,
}

impl StreamDevice {
    pub fn new(record_type: impl StreamRecordType) -> Self {
        Self {
            record_type: Arc::new(record_type),
            stream: None,
            db: None,
            name: String::new(),
            status: ERROR,
            ioscanpvt: None,
        }
    }

    /// The record's stream, once the record was initialized.
    pub fn stream(&self) -> Option<&Stream> {
        self.stream.as_ref()
    }

    /// The pass's call of the dset's I/O entry. A protocol it started ends
    /// the pass; the completion gets the record's `done`.
    fn read_write(
        &mut self,
        record: &mut dyn Record,
        common: &mut CommonFields,
        done: &DeviceDoneSource<'_>,
    ) -> bool {
        let Some(stream) = self.stream.as_ref() else {
            rec_gbl_set_sevr(common, UDF_ALARM, AlarmSeverity::Invalid);
            self.status = ERROR;
            return false;
        };
        self.status = self.record_type.read_write(stream, record, common);
        let started = stream.active();
        if started {
            *stream.shared.device_done.lock() = Some(done.device_done());
        }
        started
    }
}

/// A dset status the record's `init_record` passes on.
fn init_error(status: i64) -> CaError {
    CaError::InvalidValue(format!("init_record status {status:#x}"))
}

impl DeviceSupport for StreamDevice {
    fn bind_database(&mut self, db: AsyncDbHandle) {
        self.db = Some(db);
    }

    fn set_record_info(&mut self, name: &str, _scan: ScanType) {
        self.name = name.to_string();
    }

    fn init_common(
        &mut self,
        record: &mut dyn Record,
        common: &mut CommonFields,
    ) -> CaResult<DeviceInitOutcome> {
        let args = self.record_type.init_record(record, common);
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
            self.stream = Some(Stream::new(
                &name,
                common.prio,
                self.db.clone(),
                self.record_type.clone(),
            ));
        } else if let Some(stream) = self.stream.as_ref() {
            stream.stop_running_protocol(record, common);
        }
        let stream = self.stream.as_ref().expect("created above");
        match args.status(stream.init_record(record, common, &args)) {
            OK => Ok(DeviceInitOutcome::ok()),
            DO_NOT_CONVERT => Ok(DeviceInitOutcome::DoNotConvert(Default::default())),
            status => Err(init_error(status)),
        }
    }

    fn read_begin_common(
        &mut self,
        record: &mut dyn Record,
        common: &mut CommonFields,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        Ok(if self.read_write(record, common, done) {
            ReadStart::Pending
        } else {
            ReadStart::Synchronous
        })
    }

    fn read_common(
        &mut self,
        _record: &mut dyn Record,
        _common: &mut CommonFields,
    ) -> CaResult<DeviceReadOutcome> {
        // The alarm of a failure is the stream's, raised in the pass.
        Ok(match self.status {
            OK => DeviceReadOutcome::ok(),
            DO_NOT_CONVERT => DeviceReadOutcome::computed(Default::default()),
            _ => DeviceReadOutcome::failed(),
        })
    }

    fn write_begin_common(
        &mut self,
        record: &mut dyn Record,
        common: &mut CommonFields,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        // A failure's alarm is the stream's, raised in the pass, so it is
        // Completed, never the framework's write failure: C's record raises
        // nothing for the dset's ERROR.
        Ok(if self.read_write(record, common, done) {
            WriteStart::Pending
        } else {
            WriteStart::Completed
        })
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        unreachable!("write_begin_common never asks for the write")
    }

    /// `streamGetIointInfo(cmd, record, ppvt)`
    fn get_ioint_info(
        &mut self,
        record: &mut dyn Record,
        common: &mut CommonFields,
        cmd: IoIntrCmd,
    ) -> bool {
        let name = self.name.as_str();
        let stream = self.stream.as_ref();
        let ioscanpvt = stream.filter(|s| s.lock.lock().hooks.initialized.get());
        debug!(format!(
            "streamGetIointInfo({name},cmd={}): stream={}, ioscanpvt={}\n",
            cmd as i32,
            c_ptr(stream.map(|s| Arc::as_ptr(&s.shared).cast::<()>())),
            c_ptr(ioscanpvt.map(|s| Arc::as_ptr(&s.lock).cast::<()>()))
        ));
        let Some(stream) = stream else {
            stream_error(b"streamGetIointInfo called without stream instance\n");
            return false;
        };
        match cmd {
            IoIntrCmd::Add => {
                if !self
                    .db
                    .as_ref()
                    .is_some_and(AsyncDbHandle::interrupt_accept)
                {
                    // We will start polling later in initHook.
                    debug!(format!("streamGetIointInfo({name}): start later...\n"));
                    return true;
                }
                debug!(format!("streamGetIointInfo({name}): start protocol\n"));
                // SCAN has been set to "I/O Intr"
                if !stream.shared.slot.lend(record, common, || {
                    stream.lock.start_protocol(StartMode::StartAsync)
                }) {
                    stream_error(&cmsg!(name, b": Can't start \"I/O Intr\" protocol\n"));
                }
            }
            IoIntrCmd::Delete => {
                // SCAN is no longer "I/O Intr"
                debug!(format!("streamGetIointInfo({name}): abort protocol\n"));
                stream.shared.slot.lend(record, common, || {
                    stream.lock.lock().finish_protocol(ProtocolResult::Abort)
                });
            }
        }
        true
    }

    /// The `ioscanpvt` `streamGetIointInfo` hands `scanAdd`: none before the
    /// record was initialized.
    fn io_intr_receiver(&mut self) -> Option<epics_base_rs::runtime::sync::mpsc::Receiver<()>> {
        let initialized = self
            .stream
            .as_ref()
            .is_some_and(|s| s.lock.lock().hooks.initialized.get());
        if !initialized {
            return None;
        }
        let (tx, rx) = epics_base_rs::runtime::sync::mpsc::channel(1);
        self.ioscanpvt = Some(tx);
        Some(rx)
    }

    fn dtyp(&self) -> &str {
        "stream"
    }

    fn posts_from_device(&self) -> bool {
        true
    }

    fn take_device_posts(&mut self) -> Vec<DevicePost> {
        self.stream
            .as_ref()
            .map(|s| std::mem::take(&mut *s.shared.posts.lock()))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::records::ai::AiRecord;

    #[test]
    #[should_panic(expected = "a record lent on two threads at once")]
    fn a_lend_while_another_thread_has_the_slot_panics() {
        let slot = RecordSlot::default();
        let (mut a, mut b) = (AiRecord::default(), AiRecord::default());
        let (mut ca, mut cb) = (CommonFields::default(), CommonFields::default());
        slot.lend(&mut a, &mut ca, || {
            std::thread::scope(|s| {
                let lent = s.spawn(|| slot.lend(&mut b, &mut cb, || ()));
                if let Err(panic) = lent.join() {
                    std::panic::resume_unwind(panic);
                }
            })
        });
    }
}
