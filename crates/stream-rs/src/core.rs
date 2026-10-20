//! `StreamCore.h` / `StreamCore.cc`: the protocol engine, a client of both
//! the protocol parser and a bus, and the boundary to the record layer
//! that subclasses it (`StreamEpics`'s `Stream`).
//!
//! C's `StreamCore` is a base class whose protected members the record
//! layer reads and writes, and whose virtual methods it implements. Here
//! [`StreamCore`] holds those members and the record layer's
//! [`StreamCoreHooks`] implementation, and every hook is handed the core.
//! The core sits in a [`StreamCoreLock`], C's recursive `lockMutex`: a bus
//! may call back from inside a request, on the requesting thread, so the
//! members are cells and no borrow of one is held across a bus or hook call.
//!
//! The hooks, C's virtual methods:
//!
//! - `get_field_address(fieldname)`: for `%(FIELD)` formats, the field's
//!   address, `None` when there is no such field. C appends the address
//!   structure to the compiled code; here the code carries its index into
//!   the addresses its [`Code`] holds, and the core hands the address back
//!   to `format_value` / `match_value`.
//! - `format_value(format, fieldaddress)`: take the value from the field
//!   (or the default location for `format.type_`) and print it with the
//!   matching `print_value_*`, once per element of an array (the separator
//!   is added by the core).
//! - `match_value(format, fieldaddress)`: scan with the matching
//!   `scan_value_*` and store the value, moving `consumed_input` past it;
//!   for arrays, once per element until a scan fails (the separator is
//!   matched by the core).
//! - `start_timer` (answered with [`StreamCoreLock::timer_callback`]),
//!   `execute` (answered with [`StreamCoreLock::exec_callback`]), and the
//!   start / input / finish hooks.
//!
//! The record layer keeps C's list of all streams (`first` / `next`): only
//! its reports and reload walk it.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use epics_libcom_rs::cfmt::CFloat;
use parking_lot::{Condvar, Mutex, ReentrantMutex, ReentrantMutexGuard};

use crate::buffer::StreamBuffer;
use crate::bus::{self, BusInterface, ClientBus, ReadInput, StreamIoStatus};
use crate::cmsg;
use crate::converter::{self, ESC};
use crate::cstr::{CPtr, c_isspace};
use crate::debug;
use crate::error::{AnsiMode, Ch, ansi_escape, stream_error, stream_error_at};
use crate::format::{
    COMPARE_FLAG, DEFAULT_FLAG, DOUBLE_FORMAT, ENUM_FORMAT, FIX_WIDTH_FLAG, NEEDS_ORIGINAL_FORMAT,
    PSEUDO_FORMAT, SIGNED_FORMAT, SKIP_FLAG, STREAM_FORMAT_TYPE_STR, STRING_FORMAT, StreamFormat,
    UNSIGNED_FORMAT,
};
use crate::protocol::{self, FormatType, Protocol, codes};

/// `streamErrorDeadTime`: seconds in which repeated timeout errors of a
/// stream are counted instead of printed.
pub static STREAM_ERROR_DEAD_TIME: AtomicI32 = AtomicI32::new(0);

// Flags: 0x00FFFFFF reserved for StreamCore
pub const NONE: u64 = 0x0000;
pub const IGNORE_EXTRA_INPUT: u64 = 0x0001;
pub const INIT_RUN: u64 = 0x0002;
pub const ASYNC_MODE: u64 = 0x0004;
pub const GOT_VALUE: u64 = 0x0008;
pub const BUS_OWNER: u64 = 0x0010;
pub const SEPARATOR: u64 = 0x0020;
pub const SCAN_TRIED: u64 = 0x0040;
pub const ACCEPT_INPUT: u64 = 0x0100;
pub const ACCEPT_EVENT: u64 = 0x0200;
pub const LOCK_PENDING: u64 = 0x0400;
pub const WRITE_PENDING: u64 = 0x0800;
pub const WAIT_PENDING: u64 = 0x1000;
pub const ABORTED: u64 = 0x2000;
pub const BUS_PENDING: u64 = LOCK_PENDING | WRITE_PENDING | WAIT_PENDING;
pub const CLEAR_ON_START: u64 = INIT_RUN
    | ASYNC_MODE
    | GOT_VALUE
    | ABORTED
    | BUS_OWNER
    | SEPARATOR
    | SCAN_TRIED
    | ACCEPT_INPUT
    | ACCEPT_EVENT
    | BUS_PENDING;

/// `ENUM(ProtocolResult, ...)`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolResult {
    Success,
    LockTimeout,
    WriteTimeout,
    ReplyTimeout,
    ReadTimeout,
    ScanError,
    FormatError,
    Abort,
    Fault,
    Offline,
}

impl ProtocolResult {
    /// `toStr`
    pub fn to_str(self) -> &'static str {
        match self {
            Self::Success => "Success",
            Self::LockTimeout => "LockTimeout",
            Self::WriteTimeout => "WriteTimeout",
            Self::ReplyTimeout => "ReplyTimeout",
            Self::ReadTimeout => "ReadTimeout",
            Self::ScanError => "ScanError",
            Self::FormatError => "FormatError",
            Self::Abort => "Abort",
            Self::Fault => "Fault",
            Self::Offline => "Offline",
        }
    }
}

/// `ENUM(StartMode, ...)`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartMode {
    StartNormal,
    StartInit,
    StartAsync,
}

impl StartMode {
    /// `toStr`
    pub fn to_str(self) -> &'static str {
        match self {
            Self::StartNormal => "StartNormal",
            Self::StartInit => "StartInit",
            Self::StartAsync => "StartAsync",
        }
    }
}

/// `ENUM(Commands, ...)`: the first byte of each compiled command.
pub mod commands {
    pub const END: u8 = 0;
    pub const IN: u8 = 1;
    pub const OUT: u8 = 2;
    pub const WAIT: u8 = 3;
    pub const EVENT: u8 = 4;
    pub const EXEC: u8 = 5;
    pub const CONNECT: u8 = 6;
    pub const DISCONNECT: u8 = 7;

    /// `toStr`
    pub fn to_str(c: u8) -> &'static str {
        match c {
            END => "end",
            IN => "in",
            OUT => "out",
            WAIT => "wait",
            EVENT => "event",
            EXEC => "exec",
            CONNECT => "connect",
            DISCONNECT => "disconnect",
            _ => "invalid",
        }
    }
}
use commands::*;

/// StreamCore's virtual methods, implemented by the record layer. Each is
/// called with the core's lock held.
pub trait StreamCoreHooks: Send + Sized + 'static {
    /// What a `%(FIELD)` format names (C's `DBADDR`).
    type FieldAddress: Send + Sync + 'static;
    /// What a matchValue that ends in a put the record layer makes outside
    /// the core keeps for its status ([`ValueMatch::Put`]).
    type LinkPut: Send;
    fn protocol_start_hook(&self, _core: &StreamCore<Self>) {}
    fn input_hook(&self, _core: &StreamCore<Self>, _input: &[u8]) {}
    fn protocol_finish_hook(&self, _core: &StreamCore<Self>, _result: ProtocolResult) {}
    fn start_timer(&self, core: &StreamCore<Self>, timeout_ms: u64);
    fn format_value(
        &self,
        core: &StreamCore<Self>,
        format: &StreamFormat,
        fieldaddress: Option<&Self::FieldAddress>,
    ) -> bool;
    fn match_value(
        &self,
        core: &StreamCore<Self>,
        format: &StreamFormat,
        fieldaddress: Option<&Self::FieldAddress>,
    ) -> ValueMatch<Self::LinkPut>;
    /// The rest of a matchValue that ended in [`ValueMatch::Put`], given the
    /// put's `status` ([`Entry::LinkPut`]): whether the value matched.
    fn link_put_done(&self, core: &StreamCore<Self>, put: Self::LinkPut, status: i32) -> bool;
    /// `execute`: start the `exec` command in `core.output_line`. The
    /// default is C's: not supported.
    fn execute(&self, core: &StreamCore<Self>) -> bool {
        stream_error(&cmsg!(
            core.name(),
            b": Command 'exec' not implemented on this system\n"
        ));
        false
    }
    /// `getFieldAddress`, for the parser.
    fn get_field_address(
        &self,
        core: &StreamCore<Self>,
        fieldname: &[u8],
    ) -> Option<Self::FieldAddress>;
    /// `priority`, for the bus's queue.
    fn priority(&self, _core: &StreamCore<Self>) -> i64 {
        0
    }
    /// `interruptAccept`, for the bus's asynchronous input.
    fn interrupt_accept(&self, core: &StreamCore<Self>) -> bool;
}

/// What [`StreamCoreHooks::match_value`] made of a format.
pub enum ValueMatch<P> {
    /// C's `true`: the value matched, its input consumed.
    Matched,
    /// C's `false`.
    Failed,
    /// The value goes to a put C makes inside matchValue, which the record
    /// layer makes holding neither the record nor the core: the run waits
    /// for its status ([`Entry::LinkPut`]).
    Put(P),
}

/// `class MutexLock`: the core, locked.
pub type MutexLock<'a, H> = ReentrantMutexGuard<'a, StreamCore<H>>;

/// A [`StreamCore`] behind C's recursive `lockMutex`. The bus is given a
/// [`Weak`] to this, and the record layer's timer and `exec` answer here.
pub struct StreamCoreLock<H: StreamCoreHooks> {
    me: Weak<Self>,
    core: ReentrantMutex<StreamCore<H>>,
    gate: Option<EntryGate<H>>,
    owner: Arc<RunOwner>,
}

/// An entry into the core from outside it, each a StreamCore method that
/// takes the lock in C: a bus callback, the record layer's timer expiring,
/// or its `exec` command finishing.
#[derive(Clone, Copy, Debug)]
pub enum Entry<'a> {
    Lock(StreamIoStatus),
    Write(StreamIoStatus),
    Read(StreamIoStatus, ReadInput<'a>),
    Event(StreamIoStatus),
    Connect(StreamIoStatus),
    Disconnect(StreamIoStatus),
    Timer,
    Exec(StreamIoStatus),
    /// The status of the put a suspended run waits for, 0 for success.
    LinkPut(i32),
}

/// The run an [`Entry`] belongs to: the one in progress when it was made,
/// none between runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunId(Option<NonZeroU64>);

/// The run the entries made now belong to, which the core sets under its
/// lock and an entry reads outside it, and the suspensions it ended, which
/// an entry waits on outside it.
#[derive(Default)]
struct RunOwner {
    run: AtomicU64,
    /// [`RunState::Suspended`]s ended so far.
    resumes: Mutex<u64>,
    resumed: Condvar,
}

impl RunOwner {
    fn now(&self) -> RunId {
        RunId(NonZeroU64::new(self.run.load(Ordering::Acquire)))
    }

    fn set(&self, run: RunId) {
        self.run
            .store(run.0.map_or(0, NonZeroU64::get), Ordering::Release);
    }

    /// Whether `run` has ended; no run never does.
    fn has_ended(&self, run: RunId) -> bool {
        run.0.is_some() && run != self.now()
    }

    /// The suspension in progress, read under the core's lock.
    fn suspension(&self) -> Resume {
        Resume(*self.resumes.lock())
    }

    fn end_suspension(&self) {
        *self.resumes.lock() += 1;
        self.resumed.notify_all();
    }

    /// Until `suspension` has ended.
    fn wait(&self, suspension: Resume) {
        let mut resumes = self.resumes.lock();
        while *resumes == suspension.0 {
            self.resumed.wait(&mut resumes);
        }
    }
}

/// [`StreamCoreLock::dispatch`]'s answer.
#[must_use]
pub enum Dispatch {
    /// The entry ran, or was dropped: what the bus is answered.
    Done(isize),
    /// The run is suspended at a put ([`ValueMatch::Put`]), which C makes
    /// holding the core: the entry goes in once that is over, the gate
    /// holding nothing until then.
    Wait(Resume),
}

/// The suspension a [`Dispatch::Wait`] waits out.
pub struct Resume(u64);

/// Where the run stands between entries.
enum RunState<P> {
    /// As its flags and command say.
    Running,
    /// In matchInput, at a put the record layer makes outside the core
    /// ([`ValueMatch::Put`]): only that put's [`Entry::LinkPut`] goes on
    /// from here.
    Suspended(Suspension<P>),
}

/// A run's matchInput, stopped at a format whose put it waits for.
struct Suspension<P> {
    /// The format's offset in the running list's code.
    format: usize,
    /// matchInput's `delta` there.
    delta: isize,
    caller: MatchCaller,
    put: P,
}

/// Where a matchInput goes back to once it is done.
#[derive(Clone, Copy)]
enum MatchCaller {
    /// readCallback: `line` bytes of the input are the line, with its
    /// terminator.
    Read {
        status: StreamIoStatus,
        line: usize,
        command_start: usize,
    },
    /// finishProtocol, reparsing the input for a `@mismatch` handler that
    /// starts with `in`.
    Reparse,
}

/// What a matchInput came to.
enum Matching<P> {
    Done(bool),
    /// The format at `format` waits for `put`, `delta` there.
    Put {
        format: usize,
        delta: isize,
        put: P,
    },
}

/// How an [`Entry`] from a thread that does not hold the core reaches it:
/// the record layer takes what it must hold before the core (StreamEpics
/// takes the record), then calls [`StreamCoreLock::dispatch`].
pub type EntryGate<H> =
    Box<dyn Fn(&StreamCoreLock<H>, RunId, Entry<'_>) -> Dispatch + Send + Sync + 'static>;

impl<H: StreamCoreHooks> StreamCoreLock<H> {
    /// `StreamCore()` for the stream `name` (C's `streamname`, which the
    /// record layer sets to the record name). `hooks` is given the handle
    /// the record layer's timer and `exec` callbacks answer to.
    pub fn new(name: &str, hooks: impl FnOnce(&Weak<Self>) -> H) -> Arc<Self> {
        Self::build(name, hooks, None)
    }

    /// [`Self::new`] with every [`Entry`] from another thread passed
    /// through `gate`.
    pub fn new_gated(
        name: &str,
        hooks: impl FnOnce(&Weak<Self>) -> H,
        gate: EntryGate<H>,
    ) -> Arc<Self> {
        Self::build(name, hooks, Some(gate))
    }

    fn build(
        name: &str,
        hooks: impl FnOnce(&Weak<Self>) -> H,
        gate: Option<EntryGate<H>>,
    ) -> Arc<Self> {
        let owner = Arc::new(RunOwner::default());
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            core: ReentrantMutex::new(StreamCore::new(name, hooks(me), owner.clone())),
            gate,
            owner,
        })
    }

    /// `entry`, made now.
    fn enter(&self, entry: Entry<'_>) -> isize {
        self.enter_as(self.owner.now(), entry)
    }

    fn enter_as(&self, run: RunId, entry: Entry<'_>) -> isize {
        if self.core.is_owned_by_current_thread() {
            // A bus answering inside a request, on the requesting thread,
            // is behind the gate already; that thread has no suspension
            // to wait out but its own, which no entry of its can end.
            return match self.dispatch(run, entry) {
                Dispatch::Done(r) => r,
                Dispatch::Wait(_) => 0,
            };
        }
        loop {
            let dispatched = match &self.gate {
                Some(gate) => gate(self, run, entry),
                None => self.dispatch(run, entry),
            };
            match dispatched {
                Dispatch::Done(r) => return r,
                Dispatch::Wait(suspension) => self.owner.wait(suspension),
            }
        }
    }

    /// Run `entry` on the core, the lock taken; nothing when the protocol
    /// was aborted or `run` has ended. C tests `Aborted` before it takes
    /// the lock; here the flags are read under it, as they are only shared
    /// through it. The gate can wait out the end of `run` and the start of
    /// the next, which C's window (AsynDriverInterface.cc:930 to
    /// StreamCore.cc:961) is too short for.
    pub fn dispatch(&self, run: RunId, entry: Entry<'_>) -> Dispatch {
        let core = self.lock();
        if core.has(ABORTED) || self.owner.has_ended(run) {
            return Dispatch::Done(0);
        }
        match entry {
            Entry::LinkPut(status) => core.link_put_callback(status),
            // only its put's status takes a suspended run on
            _ if core.suspended() => return Dispatch::Wait(self.owner.suspension()),
            Entry::Lock(status) => core.lock_callback(status),
            Entry::Write(status) => core.write_callback(status),
            Entry::Read(status, input) => return Dispatch::Done(core.read_callback(status, input)),
            Entry::Event(status) => core.event_callback(status),
            Entry::Connect(status) => core.connect_callback(status),
            Entry::Disconnect(status) => core.disconnect_callback(status),
            Entry::Timer => core.timer_callback(),
            Entry::Exec(status) => core.exec_callback(status),
        }
        Dispatch::Done(0)
    }

    /// `MutexLock lock(this)`
    pub fn lock(&self) -> MutexLock<'_, H> {
        self.core.lock()
    }

    /// `attachBus(busname, addr, param)`
    pub fn attach_bus(&self, busname: &str, addr: i32, param: &str) -> bool {
        let core = self.lock();
        core.release_bus();
        let Some(me) = self.me.upgrade() else {
            return false;
        };
        let client: Arc<dyn bus::Client> = me;
        let Some(bus) = bus::find(&client, busname, addr, param) else {
            stream_error(&cmsg!(
                b"Cannot find a bus named '",
                busname,
                b"' for '",
                core.name(),
                b"'\n"
            ));
            return false;
        };
        let bus = Arc::new(bus);
        debug!(format!(
            "StreamCore::attachBus(busname=\"{busname}\", addr={addr}, param=\"{param}\") businterface={:p}\n",
            Arc::as_ptr(&bus)
        ));
        *core.businterface.borrow_mut() = Some(bus);
        true
    }

    /// `startProtocol(startMode)`
    pub fn start_protocol(&self, start_mode: StartMode) -> bool {
        self.lock().start_protocol(start_mode)
    }

    /// `timerCallback`: the record layer's timer started by `start_timer`
    /// has expired.
    pub fn timer_callback(&self) {
        self.enter(Entry::Timer);
    }

    /// `execCallback(status)`: the command started by `execute` is done.
    pub fn exec_callback(&self, status: StreamIoStatus) {
        self.enter(Entry::Exec(status));
    }

    /// The status of the put `run` was suspended at ([`ValueMatch::Put`]).
    pub fn link_put_callback(&self, run: RunId, status: i32) {
        self.enter_as(run, Entry::LinkPut(status));
    }
}

/// The bus callbacks.
impl<H: StreamCoreHooks> bus::Client for StreamCoreLock<H> {
    fn lock_callback(&self, status: StreamIoStatus) {
        self.enter(Entry::Lock(status));
    }

    fn write_callback(&self, status: StreamIoStatus) {
        self.enter(Entry::Write(status));
    }

    fn read_callback(&self, status: StreamIoStatus, input: ReadInput<'_>) -> isize {
        self.enter(Entry::Read(status, input))
    }

    fn event_callback(&self, status: StreamIoStatus) {
        self.enter(Entry::Event(status));
    }

    fn connect_callback(&self, status: StreamIoStatus) {
        self.enter(Entry::Connect(status));
    }

    fn async_connect_callback(&self, status: StreamIoStatus) {
        self.enter_as(RunId(None), Entry::Connect(status));
    }

    fn disconnect_callback(&self, status: StreamIoStatus) {
        self.enter(Entry::Disconnect(status));
    }

    fn priority(&self) -> i64 {
        let core = self.lock();
        core.hooks.priority(&core)
    }

    fn interrupt_accept(&self) -> bool {
        let core = self.lock();
        core.hooks.interrupt_accept(&core)
    }

    fn get_in_terminator(&self) -> Option<Vec<u8>> {
        self.lock().get_in_terminator()
    }

    fn get_out_terminator(&self) -> Option<Vec<u8>> {
        self.lock().get_out_terminator()
    }

    fn name(&self) -> String {
        self.lock().name().to_string()
    }
}

/// A compiled command list (see [`StreamCore::commands`]).
pub type CommandList<A> = RefCell<Arc<Code<A>>>;

/// A compiled command list, with the field addresses its `%(FIELD)`
/// formats name by index. C appends each address to the code itself: a run
/// holding its list holds the addresses its code was compiled with.
pub struct Code<A> {
    buffer: StreamBuffer,
    field_addresses: Arc<[A]>,
}

impl<A> Code<A> {
    /// The addresses of the protocol the list was compiled with, shared by
    /// its other lists.
    pub fn field_addresses(&self) -> &[A] {
        &self.field_addresses
    }
}

impl<A> Default for Code<A> {
    fn default() -> Self {
        Self {
            buffer: StreamBuffer::new(),
            field_addresses: Arc::new([]),
        }
    }
}

impl<A> std::ops::Deref for Code<A> {
    type Target = StreamBuffer;

    fn deref(&self) -> &StreamBuffer {
        &self.buffer
    }
}

/// The members C's `parse` and `compile` write. [`StreamCore::parse`]
/// writes a copy of the core's ([`StreamCore::installed`]), which nothing
/// runs until [`StreamCore::install`] makes it the core's.
pub struct Parsed<A> {
    protocolname: StreamBuffer,
    ignore_extra_input: bool,
    lock_timeout: u64,
    write_timeout: u64,
    reply_timeout: u64,
    read_timeout: u64,
    poll_period: u64,
    max_input: u64,
    in_terminator_defined: bool,
    out_terminator_defined: bool,
    in_terminator: StreamBuffer,
    out_terminator: StreamBuffer,
    separator: StreamBuffer,
    /// `commands`, `onInit`, `onWriteTimeout`, `onReplyTimeout`,
    /// `onReadTimeout`, `onMismatch`.
    lists: [Arc<Code<A>>; 6],
}

impl<A> Parsed<A> {
    /// The addresses the lists' `%(FIELD)` formats name. A failed compile
    /// leaves the lists after the one it failed in as they were.
    pub fn field_addresses(&self) -> impl Iterator<Item = &A> {
        self.lists.iter().flat_map(|code| code.field_addresses())
    }
}

/// The address a compiled `%(FIELD)` format names: `address` is its index
/// into the addresses of the list it was read from.
fn field_address<'a, A>(field_addresses: &'a [A], address: &[u8]) -> &'a A {
    &field_addresses[u32::from_ne_bytes(address.try_into().unwrap()) as usize]
}

/// `StreamCore`'s members. A member the record layer reads or writes in C
/// is public.
pub struct StreamCore<H: StreamCoreHooks> {
    streamname: String,
    pub flags: Cell<u64>,
    businterface: RefCell<Option<Arc<BusInterface>>>,

    pub protocolname: RefCell<StreamBuffer>,
    pub lock_timeout: Cell<u64>,
    pub write_timeout: Cell<u64>,
    pub reply_timeout: Cell<u64>,
    pub read_timeout: Cell<u64>,
    pub poll_period: Cell<u64>,
    pub max_input: Cell<u64>,
    in_terminator_defined: Cell<bool>,
    out_terminator_defined: Cell<bool>,
    pub in_terminator: RefCell<StreamBuffer>,
    pub out_terminator: RefCell<StreamBuffer>,
    pub separator: RefCell<StreamBuffer>,
    /// The compiled command lists: the protocol and its handlers. A
    /// running protocol holds its own reference, so a reload cannot pull
    /// the code from under it.
    pub commands: CommandList<H::FieldAddress>,
    pub on_init: CommandList<H::FieldAddress>,
    pub on_write_timeout: CommandList<H::FieldAddress>,
    pub on_reply_timeout: CommandList<H::FieldAddress>,
    pub on_read_timeout: CommandList<H::FieldAddress>,
    pub on_mismatch: CommandList<H::FieldAddress>,
    active_command: Cell<u8>,
    /// `commandIndex`: the list running and the offset in it. The list is
    /// held, so a reload cannot pull it from under a running protocol.
    code: RefCell<Arc<Code<H::FieldAddress>>>,
    command_index: Cell<usize>,
    running_handler: Cell<ProtocolResult>,
    previous_result: Cell<ProtocolResult>,
    number_of_errors: Cell<u32>,
    last_error_time: Cell<i64>,
    last_input_status: Cell<StreamIoStatus>,
    unparsed_input: Cell<bool>,
    pub output_line: RefCell<StreamBuffer>,
    pub input_buffer: RefCell<StreamBuffer>,
    pub input_line: RefCell<StreamBuffer>,
    pub consumed_input: Cell<usize>,
    /// The protocol starts so far.
    runs: Cell<u64>,
    owner: Arc<RunOwner>,
    run_state: RefCell<RunState<H::LinkPut>>,

    pub hooks: H,
}

/// `extract<unsigned long>(c)`
fn extract_ulong(c: &mut CPtr<'_>) -> u64 {
    let mut b = [0u8; 8];
    for (i, x) in b.iter_mut().enumerate() {
        *x = c.at(i);
    }
    c.advance(8);
    u64::from_ne_bytes(b)
}

/// `strcspn(s, reject)`
fn c_strcspn(s: CPtr<'_>, reject: &[u8]) -> usize {
    let mut n = 0;
    while s.at(n) != 0 && !reject.contains(&s.at(n)) {
        n += 1;
    }
    n
}

impl<H: StreamCoreHooks> StreamCore<H> {
    fn new(name: &str, hooks: H, owner: Arc<RunOwner>) -> Self {
        Self {
            streamname: name.to_string(),
            flags: Cell::new(NONE),
            businterface: RefCell::new(None),
            protocolname: RefCell::new(StreamBuffer::new()),
            lock_timeout: Cell::new(0),
            write_timeout: Cell::new(0),
            reply_timeout: Cell::new(0),
            read_timeout: Cell::new(0),
            poll_period: Cell::new(0),
            max_input: Cell::new(0),
            in_terminator_defined: Cell::new(false),
            out_terminator_defined: Cell::new(false),
            in_terminator: RefCell::new(StreamBuffer::new()),
            out_terminator: RefCell::new(StreamBuffer::new()),
            separator: RefCell::new(StreamBuffer::new()),
            commands: RefCell::new(Arc::default()),
            on_init: RefCell::new(Arc::default()),
            on_write_timeout: RefCell::new(Arc::default()),
            on_reply_timeout: RefCell::new(Arc::default()),
            on_read_timeout: RefCell::new(Arc::default()),
            on_mismatch: RefCell::new(Arc::default()),
            active_command: Cell::new(END),
            code: RefCell::new(Arc::default()),
            command_index: Cell::new(0),
            running_handler: Cell::new(ProtocolResult::Success),
            previous_result: Cell::new(ProtocolResult::Success),
            number_of_errors: Cell::new(0),
            last_error_time: Cell::new(0),
            last_input_status: Cell::new(StreamIoStatus::StreamIoSuccess),
            unparsed_input: Cell::new(false),
            output_line: RefCell::new(StreamBuffer::new()),
            input_buffer: RefCell::new(StreamBuffer::new()),
            input_line: RefCell::new(StreamBuffer::new()),
            consumed_input: Cell::new(0),
            runs: Cell::new(0),
            owner,
            run_state: RefCell::new(RunState::Running),
            hooks,
        }
    }

    pub fn name(&self) -> &str {
        &self.streamname
    }

    fn has(&self, f: u64) -> bool {
        self.flags.get() & f != 0
    }

    fn set_flags(&self, f: u64) {
        self.flags.set(self.flags.get() | f);
    }

    fn clear_flags(&self, f: u64) {
        self.flags.set(self.flags.get() & !f);
    }

    fn bus(&self) -> Option<Arc<BusInterface>> {
        self.businterface.borrow().clone()
    }

    fn bus_supports_event(&self) -> bool {
        self.bus().bus_supports_event()
    }

    /// `releaseBus`
    pub fn release_bus(&self) {
        if self.businterface.borrow().is_some() {
            if self.has(BUS_OWNER) {
                self.bus().bus_unlock();
            }
            let mut bus = self.businterface.borrow_mut().take();
            bus.bus_release();
        }
    }

    // debug functions /////////////////////////////////////////////

    /// `printCommands`
    fn print_commands(buffer: &mut StreamBuffer, code: &StreamBuffer) {
        let mut c = code.ptr(0);
        loop {
            let cmd = c.peek();
            c.advance(1);
            match cmd {
                END => return,
                IN => {
                    buffer.append(b"    in \"");
                    c = protocol::print_string(buffer, c);
                    buffer.append(b"\";\n");
                }
                OUT => {
                    buffer.append(b"    out \"");
                    c = protocol::print_string(buffer, c);
                    buffer.append(b"\";\n");
                }
                WAIT => {
                    let timeout = extract_ulong(&mut c);
                    buffer.print(format_args!("    wait {}; # ms\n", timeout as i64));
                }
                EVENT => {
                    let eventnumber = extract_ulong(&mut c);
                    let timeout = extract_ulong(&mut c);
                    buffer.print(format_args!(
                        "    event({}) {}; # ms\n",
                        eventnumber as i64, timeout as i64
                    ));
                }
                EXEC => {
                    buffer.append(b"    exec \"");
                    c = protocol::print_string(buffer, c);
                    buffer.append(b"\";\n");
                }
                CONNECT => {
                    let timeout = extract_ulong(&mut c);
                    buffer.print(format_args!("    connect {}; # ms\n", timeout as i64));
                }
                DISCONNECT => {
                    buffer.append(b"    disconnect;\n");
                }
                _ => {
                    buffer
                        .append(ansi_escape(AnsiMode::RedBold).as_bytes())
                        .append(b"GARBAGE: ");
                    c = protocol::print_string(buffer, CPtr::at_offset(c.base(), c.offset() - 1));
                    buffer
                        .append(ansi_escape(AnsiMode::Reset).as_bytes())
                        .append(b"\n");
                }
            }
        }
    }

    /// `printProtocol(file)`
    pub fn print_protocol(&self, file: &mut dyn Write) -> std::io::Result<()> {
        let mut buffer = StreamBuffer::new();
        let mut out = Vec::new();
        out.extend_from_slice(&cmsg!(self.protocolname.borrow().cstr(), b" {\n"));
        out.extend_from_slice(
            format!(
                "  extraInput    = {};\n",
                if self.has(IGNORE_EXTRA_INPUT) {
                    "ignore"
                } else {
                    "error"
                }
            )
            .as_bytes(),
        );
        for (label, value, unit) in [
            ("lockTimeout  ", self.lock_timeout.get(), "ms"),
            ("readTimeout  ", self.read_timeout.get(), "ms"),
            ("replyTimeout ", self.reply_timeout.get(), "ms"),
            ("writeTimeout ", self.write_timeout.get(), "ms"),
            ("pollPeriod   ", self.poll_period.get(), "ms"),
            ("maxInput     ", self.max_input.get(), "bytes"),
        ] {
            out.extend_from_slice(format!("  {label} = {}; # {unit}\n", value as i64).as_bytes());
        }
        for (label, value) in [
            ("inTerminator ", &self.in_terminator),
            ("outTerminator", &self.out_terminator),
            ("separator    ", &self.separator),
        ] {
            protocol::print_string(buffer.clear(), value.borrow().ptr(0));
            out.extend_from_slice(&cmsg!(b"  ", label, b" = \"", buffer.cstr(), b"\";\n"));
        }
        for (label, handler) in [
            ("@Init", &self.on_init),
            ("@ReplyTimeout", &self.on_reply_timeout),
            ("@ReadTimeout", &self.on_read_timeout),
            ("@WriteTimeout", &self.on_write_timeout),
            ("@Mismatch", &self.on_mismatch),
        ] {
            let handler = handler.borrow().clone();
            if handler.is_set() {
                Self::print_commands(buffer.clear(), &handler);
                out.extend_from_slice(&cmsg!(b"  ", label, b" {\n", buffer.cstr(), b"  }\n"));
            }
        }
        let commands = self.commands.borrow().clone();
        Self::print_commands(buffer.clear(), &commands);
        out.extend_from_slice(&cmsg!(b"\n", buffer.cstr(), b"}\n"));
        file.write_all(&out)
    }

    ///////////////////////////////////////////////////////////////////////

    // Parse the protocol

    /// `parse(filename, protocolname)`: `protocolname` may carry
    /// substitutions, `name(sub1, sub2)`. C writes the core's members under
    /// a protocol still running; here `parsed`, for [`Self::install`], as
    /// far as C writes them when it fails.
    pub fn parse(
        &self,
        parsed: &mut Parsed<H::FieldAddress>,
        filename: &str,
        protocolname_arg: &[u8],
    ) -> bool {
        let mut protocolname = StreamBuffer::from_bytes(protocolname_arg);
        // extract substitutions from protocolname "name ( sub1, sub2 ) "
        let mut i = protocolname.find_byte(b'(', 0);
        if i < 0 {
            i = 0;
        }
        while protocolname.get(i - 1) == b' ' {
            i -= 1;
            protocolname.remove(i, 1);
        }
        if protocolname.get(i) == b'(' {
            while i < protocolname.length() as isize {
                if protocolname.get(i - 1) == b' ' {
                    i -= 1;
                    protocolname.remove(i, 1); // remove trailing space
                }
                protocolname.set_byte(i, 0); // replace initial '(' and separating ',' with '\0'
                if protocolname.get(i + 1) == b' ' {
                    protocolname.remove(i + 1, 1); // remove leading space
                }
                let mut brackets = 0;
                loop {
                    i += 1;
                    i += c_strcspn(protocolname.ptr(i), b",()\\") as isize;
                    let c = protocolname.get(i);
                    if c == b'(' {
                        brackets += 1;
                    } else if c == b')' {
                        brackets -= 1;
                    } else if c == b',' && brackets <= 0 {
                        break;
                    } else if c == b'\\' {
                        if protocolname.get(i + 1) == b'\\' {
                            i += 1; // keep '\\'
                        } else {
                            protocolname.remove(i, 1); // else skip over next char
                        }
                    }
                    if i >= protocolname.length() as isize {
                        break;
                    }
                }
            }
            // should have closing parentheses
            if protocolname.get(-1) != b')' {
                stream_error(&cmsg!(
                    b"Missing ')' after substitutions '",
                    protocolname_arg,
                    b"'\n"
                ));
                parsed.protocolname = protocolname;
                return false;
            }
            protocolname.truncate(-1); // remove ')'
            if protocolname.get(-1) == b' ' {
                protocolname.truncate(-1); // remove trailing space
            }
            debug!(
                b"StreamCore::parse \"",
                protocolname_arg,
                b"\" -> \"",
                protocolname.expand(),
                b"\"\n"
            );
        }
        parsed.protocolname = protocolname.clone();
        let Some(mut protocol) = protocol::get_protocol(filename, &protocolname) else {
            stream_error(&cmsg!(
                b"while reading protocol '",
                protocolname.cstr(),
                b"' for '",
                self.name(),
                b"'\n"
            ));
            return false;
        };
        if !self.compile(&mut protocol, parsed) {
            stream_error(&cmsg!(
                b"while compiling protocol '",
                protocolname_arg,
                b"' for '",
                self.name(),
                b"'\n"
            ));
            return false;
        }
        true
    }

    /// `compile(protocol)`
    fn compile(&self, protocol: &mut Protocol, parsed: &mut Parsed<H::FieldAddress>) -> bool {
        let extra_input_names: [&[u8]; 2] = [b"error", b"ignore"];
        const MAX: u64 = 0xFFFF_FFFF;

        // default values for protocol variables
        parsed.ignore_extra_input = false;
        let mut lock_timeout = 5000;
        let mut read_timeout = 100;
        let mut reply_timeout = 1000;
        let mut write_timeout = 100;
        let mut max_input = 0;
        let mut poll_period = 1000;
        let (mut in_defined, mut out_defined) = (false, false);
        let store = |parsed: &mut Parsed<H::FieldAddress>, lock, read, reply, write, max, poll| {
            parsed.lock_timeout = lock;
            parsed.read_timeout = read;
            parsed.reply_timeout = reply;
            parsed.write_timeout = write;
            parsed.max_input = max;
            parsed.poll_period = poll;
            parsed.in_terminator_defined = in_defined;
            parsed.out_terminator_defined = out_defined;
        };
        store(
            parsed,
            lock_timeout,
            read_timeout,
            reply_timeout,
            write_timeout,
            max_input,
            poll_period,
        );

        let mut ignore_extra_input = 0u16;
        if !protocol.get_enum_variable(b"extrainput", &mut ignore_extra_input, &extra_input_names) {
            return false;
        }
        parsed.ignore_extra_input = ignore_extra_input != 0;

        let numbers_ok = protocol.get_number_variable(b"locktimeout", &mut lock_timeout, MAX)
            && protocol.get_number_variable(b"readtimeout", &mut read_timeout, MAX)
            && protocol.get_number_variable(b"replytimeout", &mut reply_timeout, MAX)
            && protocol.get_number_variable(b"writetimeout", &mut write_timeout, MAX)
            && protocol.get_number_variable(b"maxinput", &mut max_input, MAX)
            // use replyTimeout as default for pollPeriod
            && protocol.get_number_variable(b"replytimeout", &mut poll_period, MAX)
            && protocol.get_number_variable(b"pollperiod", &mut poll_period, MAX);
        store(
            parsed,
            lock_timeout,
            read_timeout,
            reply_timeout,
            write_timeout,
            max_input,
            poll_period,
        );
        if !numbers_ok {
            return false;
        }

        let mut in_terminator = StreamBuffer::new();
        let mut out_terminator = StreamBuffer::new();
        let mut separator = StreamBuffer::new();
        let strings_ok = protocol.get_string_variable(
            b"interminator",
            &mut in_terminator,
            Some(&mut in_defined),
        ) && protocol.get_string_variable(
            b"outterminator",
            &mut out_terminator,
            Some(&mut out_defined),
        ) && (in_defined
            || protocol.get_string_variable(
                b"terminator",
                &mut in_terminator,
                Some(&mut in_defined),
            ))
            && (out_defined
                || protocol.get_string_variable(
                    b"terminator",
                    &mut out_terminator,
                    Some(&mut out_defined),
                ))
            && protocol.get_string_variable(b"separator", &mut separator, None);
        parsed.in_terminator_defined = in_defined;
        parsed.out_terminator_defined = out_defined;
        parsed.in_terminator = in_terminator;
        parsed.out_terminator = out_terminator;
        parsed.separator = separator;
        if !strings_ok {
            return false;
        }

        let mut client = Compiler {
            core: self,
            field_addresses: Vec::new(),
        };
        let handlers: [Option<&[u8]>; 6] = [
            None,
            Some(b"@init"),
            Some(b"@writetimeout"),
            Some(b"@replytimeout"),
            Some(b"@readtimeout"),
            Some(b"@mismatch"),
        ];
        let mut codes = Vec::new();
        let mut ok = true;
        for handler in handlers {
            let mut code = StreamBuffer::new();
            ok = protocol.get_commands(handler, &mut code, &mut client);
            codes.push(code);
            if !ok {
                break;
            }
        }
        let field_addresses: Arc<[H::FieldAddress]> = client.field_addresses.into();
        for (list, buffer) in parsed.lists.iter_mut().zip(codes) {
            *list = Arc::new(Code {
                buffer,
                field_addresses: field_addresses.clone(),
            });
        }

        ok && protocol.check_unused()
    }

    /// The members [`Self::parse`] writes, the core's.
    pub fn installed(&self) -> Parsed<H::FieldAddress> {
        Parsed {
            protocolname: self.protocolname.borrow().clone(),
            ignore_extra_input: self.flags.get() & IGNORE_EXTRA_INPUT != 0,
            lock_timeout: self.lock_timeout.get(),
            write_timeout: self.write_timeout.get(),
            reply_timeout: self.reply_timeout.get(),
            read_timeout: self.read_timeout.get(),
            poll_period: self.poll_period.get(),
            max_input: self.max_input.get(),
            in_terminator_defined: self.in_terminator_defined.get(),
            out_terminator_defined: self.out_terminator_defined.get(),
            in_terminator: self.in_terminator.borrow().clone(),
            out_terminator: self.out_terminator.borrow().clone(),
            separator: self.separator.borrow().clone(),
            lists: [
                &self.commands,
                &self.on_init,
                &self.on_write_timeout,
                &self.on_reply_timeout,
                &self.on_read_timeout,
                &self.on_mismatch,
            ]
            .map(|list| list.borrow().clone()),
        }
    }

    /// Make `parsed` the core's members. A protocol still running keeps the
    /// list it holds.
    pub fn install(&self, parsed: Parsed<H::FieldAddress>) {
        *self.protocolname.borrow_mut() = parsed.protocolname;
        if parsed.ignore_extra_input {
            self.set_flags(IGNORE_EXTRA_INPUT);
        } else {
            self.clear_flags(IGNORE_EXTRA_INPUT);
        }
        self.lock_timeout.set(parsed.lock_timeout);
        self.write_timeout.set(parsed.write_timeout);
        self.reply_timeout.set(parsed.reply_timeout);
        self.read_timeout.set(parsed.read_timeout);
        self.poll_period.set(parsed.poll_period);
        self.max_input.set(parsed.max_input);
        self.in_terminator_defined.set(parsed.in_terminator_defined);
        self.out_terminator_defined
            .set(parsed.out_terminator_defined);
        *self.in_terminator.borrow_mut() = parsed.in_terminator;
        *self.out_terminator.borrow_mut() = parsed.out_terminator;
        *self.separator.borrow_mut() = parsed.separator;
        let [
            commands,
            on_init,
            on_write_timeout,
            on_reply_timeout,
            on_read_timeout,
            on_mismatch,
        ] = parsed.lists;
        *self.commands.borrow_mut() = commands;
        *self.on_init.borrow_mut() = on_init;
        *self.on_write_timeout.borrow_mut() = on_write_timeout;
        *self.on_reply_timeout.borrow_mut() = on_reply_timeout;
        *self.on_read_timeout.borrow_mut() = on_read_timeout;
        *self.on_mismatch.borrow_mut() = on_mismatch;
    }

    /// A protocol is running: C's `printStatus` shows its `activeCommand`.
    pub fn running(&self) -> bool {
        self.active_command.get() != END
    }

    /// The run an entry made now belongs to.
    pub fn run_id(&self) -> RunId {
        self.owner.now()
    }

    fn suspended(&self) -> bool {
        matches!(*self.run_state.borrow(), RunState::Suspended(_))
    }

    /// The run's suspension, ended: only here does a run leave one, for
    /// its put's status or at the run's end.
    fn end_suspension(&self) -> Option<Suspension<H::LinkPut>> {
        let RunState::Suspended(suspension) = self.run_state.replace(RunState::Running) else {
            return None;
        };
        self.owner.end_suspension();
        Some(suspension)
    }

    // Protocol execution /////////////////////////////////////////////////

    /// The byte at the command index, 0 (`end`) past the list, as C reads
    /// the terminating zero of an empty list.
    fn command_byte(&self) -> u8 {
        let code = self.code.borrow();
        code.bytes()
            .get(self.command_index.get())
            .copied()
            .unwrap_or(END)
    }

    /// `commandIndex = list()`
    fn run_list(&self, list: &CommandList<H::FieldAddress>) {
        *self.code.borrow_mut() = list.borrow().clone();
        self.command_index.set(0);
    }

    /// `startProtocol(startMode)`, the lock held.
    pub fn start_protocol(&self, start_mode: StartMode) -> bool {
        debug!(format!(
            "StreamCore::startProtocol({}, startMode={})\n",
            self.name(),
            start_mode.to_str()
        ));
        if self.businterface.borrow().is_none() {
            stream_error(&cmsg!(self.name(), b": No businterface attached\n"));
            return false;
        }
        // entries of the run before this one are dropped from here on
        self.owner.set(RunId(None));
        self.end_suspension();
        self.clear_flags(CLEAR_ON_START);
        match start_mode {
            StartMode::StartInit => {
                if !self.on_init.borrow().is_set() {
                    return false;
                }
                self.set_flags(INIT_RUN);
                self.run_list(&self.on_init);
            }
            StartMode::StartAsync | StartMode::StartNormal => {
                if start_mode == StartMode::StartAsync {
                    if !self.bus().bus_supports_async_read() {
                        stream_error(&cmsg!(
                            self.name(),
                            b": Businterface does not support async mode\n"
                        ));
                        return false;
                    }
                    self.set_flags(ASYNC_MODE);
                }
                if !self.commands.borrow().is_set() {
                    return false;
                }
                self.run_list(&self.commands);
            }
        }
        self.runs.set(self.runs.get() + 1);
        self.owner.set(RunId(NonZeroU64::new(self.runs.get())));
        self.running_handler.set(ProtocolResult::Success);
        self.hooks.protocol_start_hook(self);
        self.eval_command()
    }

    /// `finishProtocol(status)`
    pub fn finish_protocol(&self, status: ProtocolResult) {
        let mut status = status;
        debug!(format!(
            "StreamCore::finishProtocol({}, {}) {}bus owner\n",
            self.name(),
            status.to_str(),
            if self.has(BUS_OWNER) { "" } else { "not " }
        ));
        // a run that ends waits for no put
        self.end_suspension();

        if status == ProtocolResult::Success && self.has(BUS_PENDING) {
            stream_error(&cmsg!(format!(
                "StreamCore::finishProtocol({}, {}): Still waiting for {}{}{}\n",
                self.name(),
                status.to_str(),
                if self.has(LOCK_PENDING) {
                    "lockSuccess() "
                } else {
                    ""
                },
                if self.has(WRITE_PENDING) {
                    "writeSuccess() "
                } else {
                    ""
                },
                if self.has(WAIT_PENDING) {
                    "timerCallback()"
                } else {
                    ""
                }
            )));
            status = ProtocolResult::Fault;
        }
        self.active_command.set(END);

        if self.running_handler.get() != ProtocolResult::Success || self.has(INIT_RUN) {
            // get original error status
            if status == ProtocolResult::Success {
                status = self.running_handler.get();
            }
        } else {
            // save original error status
            self.running_handler.set(status);
            // look for error handler; an empty one runs too, as C's
            // `onX()` of an empty buffer is never NULL
            let handler = match status {
                ProtocolResult::Success => {
                    self.previous_result.set(ProtocolResult::Success);
                    self.number_of_errors.set(0);
                    None
                }
                ProtocolResult::WriteTimeout => Some(&self.on_write_timeout),
                ProtocolResult::ReplyTimeout => Some(&self.on_reply_timeout),
                ProtocolResult::ReadTimeout => Some(&self.on_read_timeout),
                ProtocolResult::ScanError => {
                    let handler = self.on_mismatch.borrow().clone();
                    // reparse old input if first command in handler is 'in'
                    if handler.get(0) == IN {
                        debug!(
                            b"reparsing input \"",
                            self.input_line.borrow().expand(),
                            b"\"\n"
                        );
                        *self.code.borrow_mut() = handler;
                        self.command_index.set(1);
                        if let Some(matches) = self.match_input(MatchCaller::Reparse) {
                            self.reparse_done(matches);
                        }
                        return;
                    } else {
                        Some(&self.on_mismatch)
                    }
                }
                _ => {
                    if status == ProtocolResult::Abort {
                        self.set_flags(ABORTED);
                    }
                    // get rid of all the rubbish whe might have collected
                    self.unparsed_input.set(false);
                    self.input_buffer.borrow_mut().clear();
                    None
                }
            };
            if let Some(handler) = handler {
                debug!(b"starting exception handler\n");
                // execute handler
                self.run_list(handler);
                self.eval_command();
                return;
            }
        }
        self.finish_unhandled(status);
    }

    /// finishProtocol's end, with no handler to run.
    fn finish_unhandled(&self, status: ProtocolResult) {
        if status != ProtocolResult::Abort {
            // what comes from here on belongs to no run, as an I/O Intr
            // record's early input; an aborted run keeps its own, which C
            // drops (Aborted)
            self.owner.set(RunId(None));
        }
        if self.has(BUS_OWNER) {
            self.bus().bus_unlock();
            self.clear_flags(BUS_OWNER);
        }
        self.bus().bus_finish();
        self.clear_flags(ACCEPT_INPUT | ACCEPT_EVENT);
        self.hooks.protocol_finish_hook(self, status);
    }

    /// finishProtocol after the `@mismatch` handler's `in` reparsed the
    /// input.
    fn reparse_done(&self, matches: bool) {
        if matches {
            self.eval_command();
        } else {
            self.finish_unhandled(ProtocolResult::ScanError);
        }
    }

    /// `evalCommand`
    fn eval_command(&self) -> bool {
        if self.has(BUS_PENDING) {
            // C's message has no newline.
            stream_error(&cmsg!(format!(
                "StreamCore::evalCommand({}): Still waiting for {}{}{}",
                self.name(),
                if self.has(LOCK_PENDING) {
                    "lockSuccess() "
                } else {
                    ""
                },
                if self.has(WRITE_PENDING) {
                    "writeSuccess() "
                } else {
                    ""
                },
                if self.has(WAIT_PENDING) {
                    "timerCallback()"
                } else {
                    ""
                }
            )));
            return false;
        }
        let command = self.command_byte();
        self.active_command.set(command);
        debug!(format!(
            "StreamCore::evalCommand({}): activeCommand = {}\n",
            self.name(),
            commands::to_str(command)
        ));
        self.command_index.set(self.command_index.get() + 1);
        match command {
            OUT => self.eval_out(),
            IN => self.eval_in(),
            WAIT => self.eval_wait(),
            EVENT => self.eval_event(),
            EXEC => self.eval_exec(),
            END => {
                self.finish_protocol(ProtocolResult::Success);
                true
            }
            CONNECT => self.eval_connect(),
            DISCONNECT => self.eval_disconnect(),
            _ => {
                stream_error(&cmsg!(format!(
                    "INTERNAL ERROR ({}): illegal command code 0x{:02x}\n",
                    self.name(),
                    command
                )));
                self.clear_flags(BUS_PENDING);
                self.finish_protocol(ProtocolResult::Fault);
                false
            }
        }
    }

    // Handle 'out' command

    /// `evalOut`
    fn eval_out(&self) -> bool {
        // flush all unread input
        self.unparsed_input.set(false);
        self.input_buffer.borrow_mut().clear();
        self.input_line.borrow_mut().clear();
        if !self.format_output() {
            self.finish_protocol(ProtocolResult::FormatError);
            return false;
        }
        self.output_line
            .borrow_mut()
            .append_buffer(&self.out_terminator.borrow());
        debug!(
            b"StreamCore::evalOut: outputLine = \"",
            self.output_line.borrow().expand(),
            b"\"\n"
        );
        if self.command_byte() == IN {
            // prepare for early input
            self.set_flags(ACCEPT_INPUT);
        }
        if self.command_byte() == EVENT {
            // prepare for early event
            self.set_flags(ACCEPT_EVENT);
        }
        if !self.has(BUS_OWNER) {
            let timeout = if self.has(INIT_RUN) {
                0
            } else {
                self.lock_timeout.get()
            };
            debug!(format!(
                "StreamCore::evalOut({}): lockRequest({})\n",
                self.name(),
                timeout as i64
            ));
            self.set_flags(LOCK_PENDING);
            if !self.bus().bus_lock_request(timeout) {
                self.clear_flags(LOCK_PENDING);
                debug!(format!(
                    "StreamCore::evalOut({}): lockRequest failed. Device is offline.\n",
                    self.name()
                ));
                self.finish_protocol(ProtocolResult::Offline);
                return false;
            }
            return true;
        }
        self.set_flags(WRITE_PENDING);
        self.write_request()
    }

    /// `busWriteRequest(outputLine(), outputLine.length(), writeTimeout)`;
    /// the line is copied, so a write answered from inside the request
    /// finds it unborrowed.
    fn write_request(&self) -> bool {
        let line = self.output_line.borrow().bytes().to_vec();
        self.bus()
            .bus_write_request(&line, self.write_timeout.get())
    }

    /// `formatOutput`
    fn format_output(&self) -> bool {
        let code = self.code.borrow().clone();
        let mut c = CPtr::at_offset(code.bytes(), self.command_index.get());
        self.output_line.borrow_mut().clear();
        let ok = self.format_output_from(&mut c, code.field_addresses());
        self.command_index.set(c.offset());
        ok
    }

    fn format_output_from(&self, c: &mut CPtr<'_>, fields: &[H::FieldAddress]) -> bool {
        loop {
            let mut command = c.peek();
            c.advance(1);
            if command == codes::EOS {
                return true;
            }
            match command {
                codes::FORMAT_FIELD | codes::FORMAT => {
                    // code layout:
                    // [field <eos> addrlen AddressStructure] formatstring <eos> StreamFormat [info]
                    let mut field: Option<(&[u8], &H::FieldAddress)> = None;
                    if command == codes::FORMAT_FIELD {
                        debug!(format!(
                            "StreamCore::formatOutput({}): StreamProtocolParser::redirect_format\n",
                            self.name()
                        ));
                        let field_name = c.cstr();
                        c.advance(field_name.len() + 1);
                        let addrlen = u16::from_ne_bytes([c.at(0), c.at(1)]) as usize;
                        c.advance(2);
                        let start = c.offset().min(c.base().len());
                        let end = (c.offset() + addrlen).min(c.base().len());
                        field = Some((field_name, field_address(fields, &c.base()[start..end])));
                        c.advance(addrlen);
                    }
                    let formatstring = c.offset();
                    // jump after <eos>
                    while c.peek() != 0 {
                        if c.peek() == ESC {
                            c.advance(1);
                        }
                        c.advance(1);
                    }
                    let formatstr = StreamBuffer::from_bytes(
                        &c.base()[formatstring.min(c.base().len())..c.offset().min(c.base().len())],
                    );
                    c.advance(1);

                    let fmt = StreamFormat::extract(c);
                    debug!(
                        format!("StreamCore::formatOutput({}): format = %", self.name()),
                        formatstr.expand(),
                        b"\n"
                    );

                    if fmt.type_ == PSEUDO_FORMAT {
                        let printed = converter::find(fmt.conv).is_some_and(|r| {
                            r.converter
                                .print_pseudo(&fmt, &mut self.output_line.borrow_mut())
                        });
                        if !printed {
                            // C prints the raw format string, up to its <eos>.
                            stream_error(&cmsg!(
                                self.name(),
                                b": Can't print pseudo value '%",
                                formatstr.cstr(),
                                b"'\n"
                            ));
                            return false;
                        }
                        continue;
                    }
                    self.clear_flags(SEPARATOR);
                    if !self
                        .hooks
                        .format_value(self, &fmt, field.map(|(_, address)| address))
                    {
                        match field {
                            Some((field_name, _)) => stream_error(&cmsg!(
                                self.name(),
                                b": Cannot format field '",
                                field_name,
                                b"' with '%",
                                formatstr.expand(),
                                b"'\n"
                            )),
                            None => stream_error(&cmsg!(
                                self.name(),
                                b": Cannot format value with '%",
                                formatstr.expand(),
                                b"'\n"
                            )),
                        }
                        return false;
                    }
                }
                codes::WHITESPACE => {
                    self.output_line.borrow_mut().append_byte(b' ');
                }
                codes::SKIP => {}
                _ => {
                    if command == ESC {
                        // escaped literal byte
                        command = c.peek();
                        c.advance(1);
                    }
                    // literal byte
                    self.output_line.borrow_mut().append_byte(command);
                }
            }
        }
    }

    /// `printSeparator`
    fn print_separator(&self) {
        if !self.has(SEPARATOR) {
            self.set_flags(SEPARATOR);
            return;
        }
        let separator = self.separator.borrow();
        let mut out = self.output_line.borrow_mut();
        let sep = separator.bytes();
        let mut i = 0;
        while i < sep.len() {
            match sep[i] {
                codes::WHITESPACE => {
                    out.append_byte(b' '); // print single space
                }
                codes::SKIP => {}
                ESC => {
                    // escaped literal byte
                    i += 1;
                    out.append_byte(separator.get(i as isize));
                }
                b => {
                    // literal byte
                    out.append_byte(b);
                }
            }
            i += 1;
        }
    }

    /// `printValue(fmt, long)`
    pub fn print_value_long(&self, fmt: &StreamFormat, value: i64) -> bool {
        if fmt.type_ != UNSIGNED_FORMAT && fmt.type_ != SIGNED_FORMAT && fmt.type_ != ENUM_FORMAT {
            stream_error(&cmsg!(
                self.name(),
                b": printValue(long) called with %",
                Ch(fmt.conv as i32),
                b" format\n"
            ));
            return false;
        }
        self.print_separator();
        let printed = converter::find(fmt.conv).is_some_and(|r| {
            r.converter
                .print_long(fmt, &mut self.output_line.borrow_mut(), value)
        });
        if !printed {
            stream_error(&cmsg!(format!(
                "{}: Formatting value {} failed\n",
                self.name(),
                value
            )));
            return false;
        }
        debug!(
            format!(
                "StreamCore::printValue({}, %{}, {} = 0x{:x}): \"",
                self.name(),
                fmt.conv as char,
                value,
                value
            ),
            self.output_line.borrow().expand(),
            b"\"\n"
        );
        true
    }

    /// `printValue(fmt, double)`
    pub fn print_value_double(&self, fmt: &StreamFormat, value: f64) -> bool {
        if fmt.type_ != DOUBLE_FORMAT {
            stream_error(&cmsg!(
                self.name(),
                b": printValue(double) called with %",
                Ch(fmt.conv as i32),
                b" format\n"
            ));
            return false;
        }
        self.print_separator();
        let printed = converter::find(fmt.conv).is_some_and(|r| {
            r.converter
                .print_double(fmt, &mut self.output_line.borrow_mut(), value)
        });
        if !printed {
            stream_error(&cmsg!(format!(
                "{}: Formatting value {:#} failed\n",
                self.name(),
                CFloat(value).g()
            )));
            return false;
        }
        debug!(
            format!(
                "StreamCore::printValue({}, %{}, {:#}): \"",
                self.name(),
                fmt.conv as char,
                CFloat(value).g()
            ),
            self.output_line.borrow().expand(),
            b"\"\n"
        );
        true
    }

    /// `printValue(fmt, char*)`: `value` up to its first 0x00.
    pub fn print_value_string(&self, fmt: &StreamFormat, value: &[u8]) -> bool {
        let value = &value[..value.iter().position(|&b| b == 0).unwrap_or(value.len())];
        if fmt.type_ != STRING_FORMAT {
            stream_error(&cmsg!(
                self.name(),
                b": printValue(char*) called with %",
                Ch(fmt.conv as i32),
                b" format\n"
            ));
            return false;
        }
        self.print_separator();
        let printed = converter::find(fmt.conv).is_some_and(|r| {
            r.converter
                .print_string(fmt, &mut self.output_line.borrow_mut(), value)
        });
        if !printed {
            stream_error(&cmsg!(
                self.name(),
                b": Formatting value \"",
                StreamBuffer::from_bytes(value).expand(),
                b"\" failed\n"
            ));
            return false;
        }
        debug!(
            format!(
                "StreamCore::printValue({}, %{}, \"",
                self.name(),
                fmt.conv as char
            ),
            value,
            b"\"): \"",
            self.output_line.borrow().expand(),
            b"\"\n"
        );
        true
    }

    /// `lockCallback(status)`, the lock held.
    fn lock_callback(&self, status: StreamIoStatus) {
        debug!(format!(
            "StreamCore::lockCallback({}, {})\n",
            self.name(),
            status.to_str()
        ));
        if !self.has(LOCK_PENDING) {
            stream_error(&cmsg!(format!(
                "{}: StreamCore::lockCallback({}) called unexpectedly\n",
                self.name(),
                status.to_str()
            )));
            return;
        }
        self.clear_flags(LOCK_PENDING);
        self.set_flags(BUS_OWNER);
        match status {
            StreamIoStatus::StreamIoSuccess => {}
            StreamIoStatus::StreamIoTimeout => {
                debug!(format!(
                    "{}: Cannot lock device within {} ms, device seems to be busy\n",
                    self.name(),
                    self.lock_timeout.get() as i64
                ));
                self.clear_flags(BUS_OWNER);
                self.finish_protocol(ProtocolResult::LockTimeout);
                return;
            }
            StreamIoStatus::StreamIoFault => {
                stream_error(&cmsg!(
                    self.name(),
                    b": Locking failed because of a device fault\n"
                ));
                self.clear_flags(BUS_OWNER);
                self.finish_protocol(ProtocolResult::LockTimeout);
                return;
            }
            _ => {
                stream_error(&cmsg!(format!(
                    "StreamCore::lockCallback({}) unexpected status {}\n",
                    self.name(),
                    status.to_str()
                )));
                self.clear_flags(BUS_OWNER);
                self.finish_protocol(ProtocolResult::Fault);
                return;
            }
        }
        self.set_flags(WRITE_PENDING);
        if !self.write_request() {
            self.finish_protocol(ProtocolResult::Fault);
        }
    }

    /// `writeCallback(status)`, the lock held.
    fn write_callback(&self, status: StreamIoStatus) {
        debug!(format!(
            "StreamCore::writeCallback({}, {})\n",
            self.name(),
            status.to_str()
        ));
        if !self.has(WRITE_PENDING) {
            stream_error(&cmsg!(format!(
                "{}: StreamCore::writeCallback({}) called unexpectedly\n",
                self.name(),
                status.to_str()
            )));
            return;
        }
        self.clear_flags(WRITE_PENDING);
        if status != StreamIoStatus::StreamIoSuccess {
            self.finish_protocol(ProtocolResult::WriteTimeout);
            return;
        }
        self.eval_command();
    }

    // Handle 'in' command

    /// `evalIn`
    fn eval_in(&self) -> bool {
        self.set_flags(ACCEPT_INPUT);
        let mut expected_input = self.max_input.get() as isize;
        if self.unparsed_input.get() {
            // handle early input
            debug!(
                format!("StreamCore::evalIn({}): early input: ", self.name()),
                self.input_buffer.borrow().expand(),
                b"\n"
            );
            expected_input =
                self.read_callback(self.last_input_status.get(), ReadInput::Bytes(&[]));
            if expected_input == 0 {
                return true;
            }
            if expected_input == -1 {
                // don't know how much
                expected_input = 0;
            }
        }
        if self.has(ASYNC_MODE) {
            // release bus
            if self.has(BUS_OWNER) {
                debug!(format!(
                    "StreamCore::evalIn({}): unlocking bus\n",
                    self.name()
                ));
                self.bus().bus_unlock();
                self.clear_flags(BUS_OWNER);
            }
            return self.bus().bus_read_request(
                self.poll_period.get(),
                self.read_timeout.get(),
                expected_input,
                true,
            );
        }
        // continue with readCallback() in another thread
        self.bus().bus_read_request(
            self.reply_timeout.get(),
            self.read_timeout.get(),
            expected_input,
            false,
        )
    }

    /// `readCallback(status, input, size)`, the lock held: the number of
    /// bytes to read additionally, -1 when not known. C's check for an
    /// illegal status has no counterpart; the enum holds none.
    fn read_callback(&self, status: StreamIoStatus, input: ReadInput<'_>) -> isize {
        use StreamIoStatus::*;
        let size = input.size();
        let input = input.bytes();
        let mut status = status;
        if self.has(ABORTED) {
            return 0;
        }
        self.last_input_status.set(status);

        debug!(
            format!(
                "StreamCore::readCallback({}, {} input=\"",
                self.name(),
                status.to_str()
            ),
            StreamBuffer::from_bytes(input).expand(),
            // C prints the size_t: a take-back of 1 is 18446744073709551615
            format!("\", size={})\n", size as usize)
        );

        if !self.has(ACCEPT_INPUT) {
            stream_error(&cmsg!(
                format!(
                    "{}: StreamCore::readCallback({}, \"",
                    self.name(),
                    status.to_str()
                ),
                StreamBuffer::from_bytes(input).expand(),
                b"\") called unexpectedly\n"
            ));
            return 0;
        }
        match status {
            StreamIoTimeout => {
                // timeout is valid end if we have no terminator
                // and number of input bytes is not limited
                if !self.in_terminator.borrow().is_set() && self.max_input.get() == 0 {
                    status = StreamIoEnd;
                }
                // else timeout might be ok if we find a terminator
            }
            StreamIoSuccess | StreamIoEnd => {}
            StreamIoNoReply => {
                if self.has(ASYNC_MODE) {
                    // just restart in asyn mode
                    debug!(format!(
                        "StreamCore::readCallback({}) no async input: just restart\n",
                        self.name()
                    ));
                    self.eval_in();
                    return 0;
                }
                if self.check_should_print(ProtocolResult::ReplyTimeout) {
                    stream_error(&cmsg!(
                        format!(
                            "{}: No reply within {} ms to \"",
                            self.name(),
                            self.reply_timeout.get() as i64
                        ),
                        self.output_line.borrow().expand(),
                        b"\"\n"
                    ));
                }
                self.input_buffer.borrow_mut().clear();
                self.finish_protocol(ProtocolResult::ReplyTimeout);
                return 0;
            }
            StreamIoFault => {
                let msg = {
                    let buffer = self.input_buffer.borrow();
                    let n = buffer.length();
                    cmsg!(
                        format!(
                            "{}: I/O error after reading {} byte{}: \"{}",
                            self.name(),
                            n,
                            if n == 1 { "" } else { "s" },
                            if n > 20 { "..." } else { "" }
                        ),
                        buffer.expand_range(-20, 20),
                        b"\"\n"
                    )
                };
                stream_error(&msg);
                self.finish_protocol(ProtocolResult::Fault);
                return 0;
            }
        }
        self.hooks.input_hook(self, input);
        {
            // StreamBuffer::append: a negative size deletes from the end
            let mut buffer = self.input_buffer.borrow_mut();
            if size < 0 {
                let len = buffer.length();
                buffer.truncate((len - size.unsigned_abs().min(len)) as isize);
            } else {
                buffer.append(input);
            }
        }
        debug!(
            format!("StreamCore::readCallback({}) inputBuffer=\"", self.name()),
            self.input_buffer.borrow().expand(),
            format!("\", size {}\n", self.input_buffer.borrow().length())
        );
        if self.active_command.get() != IN {
            // early input, stop here and wait for in command
            // -- Should we limit size of inputBuffer? --
            if self.input_buffer.borrow().is_set() {
                self.unparsed_input.set(true);
            }
            return 0;
        }

        // prepare to parse the input
        let command_start = self.command_index.get();
        let mut end: isize = -1;
        let mut termlen = 0;
        let max_input = self.max_input.get();
        let length = self.input_buffer.borrow().length();

        {
            let terminator = self.in_terminator.borrow();
            if terminator.is_set() {
                // look for terminator
                // performance issue for long inputs that come in chunks:
                // do not parse old chunks again or performance decreases to O(n^2)
                // but make sure to get all terminators in multi-line input
                let start = if self.unparsed_input.get() {
                    // multi-line input sets 'unparsedInput' and removes the line
                    // remaining unparsed lines are left at start of inputBuffer
                    0
                } else {
                    // long line does not set 'unparsedInput' but keeps
                    // already parsed chunks in inputBuffer
                    // start parsing at beginning of new data
                    // but beware of split terminators
                    (length as isize - size - terminator.length() as isize).max(0)
                };
                end = self.input_buffer.borrow().find(terminator.bytes(), start);
                if end >= 0 {
                    termlen = terminator.length();
                    debug!(
                        format!("StreamCore::readCallback({}) inTerminator ", self.name()),
                        terminator.expand(),
                        format!(" at position {end}\n")
                    );
                } else {
                    debug!(
                        format!("StreamCore::readCallback({}) inTerminator ", self.name()),
                        terminator.expand(),
                        b" not found\n"
                    );
                }
            }
        }
        if status == StreamIoEnd && end < 0 {
            // no terminator but end flag
            debug!(format!(
                "StreamCore::readCallback({}) end flag received\n",
                self.name()
            ));
            end = length as isize;
        }
        if max_input != 0 && end < 0 && max_input <= length as u64 {
            // no terminator but maxInput bytes read
            debug!(format!(
                "StreamCore::readCallback({}) maxInput size {} reached\n",
                self.name(),
                max_input
            ));
            end = max_input as isize;
        }
        if max_input != 0 && end > max_input as isize {
            // limit input length to maxInput (ignore terminator)
            end = max_input as isize;
            termlen = 0;
        }
        if end >= 0 {
            // be forgiving with timeout because end is found
            if status == StreamIoTimeout {
                status = StreamIoEnd;
            }
        }
        if end < 0 {
            // no end found
            if status != StreamIoTimeout {
                // input is incomplete - wait for more
                debug!(format!(
                    "StreamCore::readCallback({}) wait for more input\n",
                    self.name()
                ));
                self.set_flags(ACCEPT_INPUT);
                return if max_input != 0 {
                    max_input as isize - length as isize
                } else {
                    -1 // We don't know for how much to wait
                };
            }
            // try to parse what we got
            end = length as isize;
            if self.has(ASYNC_MODE) {
                debug!(format!(
                    "StreamCore::readCallback({}) async timeout: just restart\n",
                    self.name()
                ));
                self.unparsed_input.set(false);
                self.input_buffer.borrow_mut().clear();
                self.command_index.set(command_start);
                self.eval_in();
                return 0;
            } else if self.check_should_print(ProtocolResult::ReadTimeout) {
                let msg = cmsg!(
                    format!(
                        "{}: Timeout after reading {} byte{} \"{}",
                        self.name(),
                        end,
                        if end == 1 { "" } else { "s" },
                        if end > 20 { "..." } else { "" }
                    ),
                    self.input_buffer.borrow().expand_from(-20),
                    b"\"\n"
                );
                stream_error(&msg);
            }
        }

        self.input_line
            .borrow_mut()
            .set(&self.input_buffer.borrow().bytes()[..end as usize]);
        debug!(
            format!("StreamCore::readCallback({}) input line: \"", self.name()),
            self.input_line.borrow().expand(),
            b"\"\n"
        );
        let caller = MatchCaller::Read {
            status,
            line: end as usize + termlen,
            command_start,
        };
        if let Some(matches) = self.match_input(caller) {
            self.match_done(caller, matches);
        }
        0
    }

    /// matchInput's `caller`, after it.
    fn match_done(&self, caller: MatchCaller, matches: bool) {
        match caller {
            MatchCaller::Read {
                status,
                line,
                command_start,
            } => self.read_done(status, line, command_start, matches),
            MatchCaller::Reparse => self.reparse_done(matches),
        }
    }

    /// readCallback after matchInput: `line` bytes of the input were the
    /// line, with its terminator.
    fn read_done(&self, status: StreamIoStatus, line: usize, command_start: usize, matches: bool) {
        use StreamIoStatus::*;
        self.input_buffer.borrow_mut().remove_front(line);
        if self.input_buffer.borrow().is_set() {
            debug!(
                format!(
                    "StreamCore::readCallback({}) unparsed input left: \"",
                    self.name()
                ),
                self.input_buffer.borrow().expand(),
                b"\"\n"
            );
            self.unparsed_input.set(true);
        } else {
            self.unparsed_input.set(false);
        }
        if !matches {
            if status == StreamIoTimeout {
                // we have not forgotten the timeout
                self.finish_protocol(ProtocolResult::ReadTimeout);
                return;
            }
            if self.has(ASYNC_MODE) {
                debug!(format!(
                    "StreamCore::readCallback({}) async match failure: just restart\n",
                    self.name()
                ));
                self.command_index.set(command_start);
                self.eval_in();
                return;
            }
            debug!(format!(
                "StreamCore::readCallback({}) match failure\n",
                self.name()
            ));
            self.finish_protocol(ProtocolResult::ScanError);
            return;
        }
        if status == StreamIoTimeout {
            // we have not forgotten the timeout
            self.finish_protocol(ProtocolResult::ReadTimeout);
            return;
        }
        // end input mode and do next command
        //// flags &= ~(AsyncMode|AcceptInput);
        // -- should we tell someone that input has finished? --
        self.eval_command();
    }

    /// Whether `matchInput` reports a mismatch: not in asynchronous mode
    /// (then we just wait for new matching input), nor when a `@mismatch`
    /// handler is installed that starts with `in` (then we reparse the
    /// input).
    fn report_mismatch(&self) -> bool {
        !self.has(ASYNC_MODE) && self.on_mismatch.borrow().get(0) != IN
    }

    /// `matchInput`: `None` when the run is suspended at a put
    /// ([`ValueMatch::Put`]), which goes on to `caller` once its status is
    /// in ([`Self::link_put_callback`]).
    fn match_input(&self, caller: MatchCaller) -> Option<bool> {
        self.consumed_input.set(0);
        self.match_input_at(self.command_index.get(), 0, None, caller)
    }

    /// matchInput from the command at `at`, `delta` there; `put` is the
    /// put the format there waits for, with its status.
    fn match_input_at(
        &self,
        at: usize,
        delta: isize,
        put: Option<(H::LinkPut, i32)>,
        caller: MatchCaller,
    ) -> Option<bool> {
        let code = self.code.borrow().clone();
        let mut c = CPtr::at_offset(code.bytes(), at);
        match self.match_input_from(&mut c, code.field_addresses(), delta, put) {
            Matching::Done(matches) => {
                self.command_index.set(c.offset());
                Some(matches)
            }
            Matching::Put { format, delta, put } => {
                self.run_state.replace(RunState::Suspended(Suspension {
                    format,
                    delta,
                    caller,
                    put,
                }));
                None
            }
        }
    }

    /// The status of the put the run is suspended at: its format's
    /// matchValue ends, then matchInput and its caller go on from there.
    fn link_put_callback(&self, status: i32) {
        let Some(Suspension {
            format,
            delta,
            caller,
            put,
        }) = self.end_suspension()
        else {
            return;
        };
        if let Some(matches) = self.match_input_at(format, delta, Some((put, status)), caller) {
            self.match_done(caller, matches);
        }
    }

    fn match_input_from(
        &self,
        c: &mut CPtr<'_>,
        fields: &[H::FieldAddress],
        mut delta: isize,
        mut put: Option<(H::LinkPut, i32)>,
    ) -> Matching<H::LinkPut> {
        loop {
            let at = c.offset();
            let mut command = c.peek();
            c.advance(1);
            if command == codes::EOS {
                break;
            }
            match command {
                codes::FORMAT_FIELD | codes::FORMAT => {
                    let mut field: Option<(&[u8], &H::FieldAddress)> = None;
                    if command == codes::FORMAT_FIELD {
                        // code layout:
                        // field <eos> addrlen AddressStructure formatstring <eos> StreamFormat [info]
                        let field_name = c.cstr();
                        c.advance(field_name.len() + 1);
                        let addrlen = u16::from_ne_bytes([c.at(0), c.at(1)]) as usize;
                        c.advance(2);
                        let start = c.offset().min(c.base().len());
                        let end = (c.offset() + addrlen).min(c.base().len());
                        field = Some((field_name, field_address(fields, &c.base()[start..end])));
                        c.advance(addrlen);
                    }
                    // code layout:
                    // formatstring <eos> StreamFormat [info]
                    let mut formatstring = StreamBuffer::new();
                    *c = protocol::print_string(&mut formatstring, *c);

                    let fmt = StreamFormat::extract(c);
                    if put.is_none() {
                        // printed when the format was first come to
                        debug!(
                            format!("StreamCore::matchInput({}): format = \"%", self.name()),
                            formatstring.bytes(),
                            b"\"\n"
                        );
                    }

                    if fmt.flags & SKIP_FLAG != 0
                        || fmt.type_ == PSEUDO_FORMAT
                        || fmt.type_ == NEEDS_ORIGINAL_FORMAT
                    {
                        let Some(conv) = converter::find(fmt.conv).map(|r| &*r.converter) else {
                            return Matching::Done(false);
                        };
                        let mut consumed = match fmt.type_ {
                            UNSIGNED_FORMAT | SIGNED_FORMAT | ENUM_FORMAT => {
                                let line = self.input_line.borrow();
                                conv.scan_long(
                                    &fmt,
                                    line.ptr(self.consumed_input.get() as isize),
                                    &mut 0,
                                )
                            }
                            DOUBLE_FORMAT => {
                                let line = self.input_line.borrow();
                                conv.scan_double(
                                    &fmt,
                                    line.ptr(self.consumed_input.get() as isize),
                                    &mut 0.0,
                                )
                            }
                            STRING_FORMAT => {
                                let line = self.input_line.borrow();
                                conv.scan_string(
                                    &fmt,
                                    line.ptr(self.consumed_input.get() as isize),
                                    &mut [],
                                    &mut 0,
                                )
                            }
                            PSEUDO_FORMAT => {
                                // pass complete input line for scan and/or re-write
                                let mut line = self.input_line.borrow_mut();
                                let size = line.length() as isize;
                                let mut cursor = self.consumed_input.get();
                                let consumed = conv.scan_pseudo(&fmt, &mut line, &mut cursor);
                                self.consumed_input.set(cursor);
                                delta += line.length() as isize - size; // track length changes
                                debug!(format!("after rewrite delta={delta}\n"));
                                consumed
                            }
                            NEEDS_ORIGINAL_FORMAT => {
                                // pass original input with adjusted current position
                                debug!(format!("before checksum delta={delta}\n"));
                                // correct for length changes
                                let mut cursor =
                                    (self.consumed_input.get() as isize - delta) as usize;
                                let consumed = conv.scan_pseudo(
                                    &fmt,
                                    &mut self.input_buffer.borrow_mut(),
                                    &mut cursor,
                                );
                                self.consumed_input.set((cursor as isize + delta) as usize);
                                consumed
                            }
                            _ => {
                                stream_error(&cmsg!(format!(
                                    "INTERNAL ERROR ({}): illegal format.type 0x{:02x}\n",
                                    self.name(),
                                    fmt.type_
                                )));
                                return Matching::Done(false);
                            }
                        };
                        if consumed < 0 {
                            if fmt.flags & DEFAULT_FLAG != 0 {
                                consumed = 0;
                            } else {
                                if self.report_mismatch() {
                                    self.input_mismatch(formatstring.bytes(), None);
                                }
                                return Matching::Done(false);
                            }
                        }
                        self.consumed_input
                            .set(self.consumed_input.get() + consumed as usize);
                        continue;
                    }
                    if fmt.flags & COMPARE_FLAG != 0 {
                        self.output_line.borrow_mut().clear();
                        self.clear_flags(SEPARATOR);
                        if !self
                            .hooks
                            .format_value(self, &fmt, field.map(|(_, address)| address))
                        {
                            match field {
                                Some((field_name, _)) => stream_error(&cmsg!(
                                    self.name(),
                                    b": Cannot format variable \"",
                                    field_name,
                                    b"\" with \"%",
                                    formatstring.bytes(),
                                    b"\"\n"
                                )),
                                None => stream_error(&cmsg!(
                                    self.name(),
                                    b": Cannot format value with \"%",
                                    formatstring.bytes(),
                                    b"\"\n"
                                )),
                            }
                            return Matching::Done(false);
                        }
                        let consumed = self.consumed_input.get();
                        let line = self.input_line.borrow();
                        let output = self.output_line.borrow();
                        debug!(
                            format!("StreamCore::matchInput({}): compare \"", self.name()),
                            line.expand_range(consumed as isize, output.length() as isize),
                            b"\" with \"",
                            output.expand(),
                            b"\"\n"
                        );
                        if line.length().wrapping_sub(consumed) < output.length() {
                            if self.report_mismatch() {
                                stream_error(&cmsg!(
                                    self.name(),
                                    b": Input \"",
                                    if line.length() > 20 { "..." } else { "" },
                                    line.expand_from(-20),
                                    b"\" too short. No match for format \"%",
                                    formatstring.bytes(),
                                    b"\" (\"",
                                    output.expand(),
                                    b"\")\n"
                                ));
                            }
                            return Matching::Done(false);
                        }
                        if !output
                            .bytes()
                            .iter()
                            .enumerate()
                            .all(|(i, &b)| line.get((consumed + i) as isize) == b)
                        {
                            if self.report_mismatch() {
                                let output = output.clone();
                                drop(line);
                                self.input_mismatch(formatstring.bytes(), Some(&output));
                            }
                            return Matching::Done(false);
                        }
                        self.consumed_input.set(consumed + output.length());
                        continue;
                    }
                    let matched = match put.take() {
                        Some((put, status)) => self.hooks.link_put_done(self, put, status),
                        None => {
                            self.clear_flags(SEPARATOR);
                            match self.hooks.match_value(
                                self,
                                &fmt,
                                field.map(|(_, address)| address),
                            ) {
                                ValueMatch::Matched => true,
                                ValueMatch::Failed => false,
                                ValueMatch::Put(put) => {
                                    return Matching::Put {
                                        format: at,
                                        delta,
                                        put,
                                    };
                                }
                            }
                        }
                    };
                    if !matched {
                        if self.report_mismatch() {
                            if self.has(SCAN_TRIED) {
                                self.input_mismatch(formatstring.bytes(), None);
                            } else {
                                stream_error(&cmsg!(
                                    self.name(),
                                    b": Format \"%",
                                    formatstring.bytes(),
                                    b"\" has data type ",
                                    STREAM_FORMAT_TYPE_STR
                                        .get(fmt.type_ as usize)
                                        .copied()
                                        .unwrap_or(""),
                                    b" which is not supported by \"",
                                    match field {
                                        Some((field_name, _)) => field_name,
                                        None => self.name().as_bytes(),
                                    },
                                    b"\".\n"
                                ));
                            }
                        }
                        return Matching::Done(false);
                    }
                    // matchValue() has already removed consumed bytes from inputBuffer
                }
                codes::SKIP => {
                    // ignore next input byte (if exists)
                    let consumed = self.consumed_input.get();
                    if consumed < self.input_line.borrow().length() {
                        self.consumed_input.set(consumed + 1);
                    }
                }
                codes::WHITESPACE => {
                    // any number of whitespace (including 0)
                    let line = self.input_line.borrow();
                    let mut consumed = self.consumed_input.get();
                    while consumed < line.length()
                        && c_isspace(line.get(consumed as isize) as i8 as i32)
                    {
                        consumed += 1;
                    }
                    self.consumed_input.set(consumed);
                }
                _ => {
                    if command == ESC {
                        // escaped literal byte
                        command = c.peek();
                        c.advance(1);
                    }
                    // literal byte
                    let consumed = self.consumed_input.get();
                    let line = self.input_line.borrow();
                    // the literal up to the next control byte; C's
                    // `char` is signed, so bytes from 0x80 on end it too
                    let expected = || {
                        let mut i = 0;
                        while c.at(i) as i8 >= b' ' as i8 {
                            i += 1;
                        }
                        let start = c.offset() - 1;
                        StreamBuffer::from_bytes(
                            &c.base()[start..(start + i + 1).min(c.base().len())],
                        )
                        .expand()
                    };
                    if consumed >= line.length() {
                        if self.report_mismatch() {
                            stream_error(&cmsg!(
                                self.name(),
                                b": Input \"",
                                if line.length() > 20 { "..." } else { "" },
                                line.expand_from(-20),
                                b"\" too short.\n"
                            ));
                            stream_error(&cmsg!(b"No match for \"", expected(), b"\"\n"));
                        }
                        return Matching::Done(false);
                    }
                    if command != line.get(consumed as isize) {
                        if self.report_mismatch() {
                            let rest = line.length() - consumed;
                            stream_error(&cmsg!(
                                self.name(),
                                b": Input \"",
                                if consumed > 20 { "..." } else { "" },
                                line.expand_range(
                                    if consumed > 20 {
                                        consumed as isize - 20
                                    } else {
                                        0
                                    },
                                    40
                                ),
                                if rest > 20 { "..." } else { "" },
                                b"\"\n"
                            ));
                            stream_error(&cmsg!(
                                format!(
                                    "{}: mismatch after {} byte{} \"{}",
                                    self.name(),
                                    consumed,
                                    if consumed == 1 { "" } else { "s" },
                                    if consumed > 10 { "..." } else { "" }
                                ),
                                line.expand_range(
                                    if consumed > 10 {
                                        consumed as isize - 10
                                    } else {
                                        0
                                    },
                                    if consumed > 10 { 10 } else { consumed as isize }
                                ),
                                b"\"\n"
                            ));
                            stream_error(&cmsg!(
                                self.name(),
                                b": got \"",
                                line.expand_range(consumed as isize, 10),
                                if rest > 10 { "..." } else { "" },
                                b"\" where \"",
                                expected(),
                                b"\" was expected\n"
                            ));
                        }
                        return Matching::Done(false);
                    }
                    self.consumed_input.set(consumed + 1);
                }
            }
        }
        let consumed = self.consumed_input.get();
        let line = self.input_line.borrow();
        let surplus = line.length().wrapping_sub(consumed);
        if surplus > 0 && !self.has(IGNORE_EXTRA_INPUT) {
            if self.report_mismatch() {
                stream_error(&cmsg!(
                    format!(
                        "{}: {} byte{} surplus input \"",
                        self.name(),
                        surplus,
                        if surplus == 1 { "" } else { "s" }
                    ),
                    line.expand_range(consumed as isize, 20),
                    if surplus > 20 { "..." } else { "" },
                    b"\"\n"
                ));

                if consumed > 20 {
                    stream_error(&cmsg!(
                        format!(
                            "{}: after {} byte{} \"...",
                            self.name(),
                            consumed,
                            if consumed == 1 { "" } else { "s" }
                        ),
                        line.expand_range(consumed as isize - 20, 20),
                        b"\"\n"
                    ));
                } else {
                    stream_error(&cmsg!(
                        format!(
                            "{}: after {} byte{}: \"",
                            self.name(),
                            consumed,
                            if consumed == 1 { "" } else { "s" }
                        ),
                        line.expand_range(0, consumed as isize),
                        b"\"\n"
                    ));
                }
            }
            return Matching::Done(false);
        }
        Matching::Done(true)
    }

    /// `"%s: Input \"%s%s\" does not match format \"%%%s\"[ (\"%s\")]\n"`
    fn input_mismatch(&self, formatstring: &[u8], output: Option<&StreamBuffer>) {
        let consumed = self.consumed_input.get();
        let line = self.input_line.borrow();
        let mut msg = cmsg!(
            self.name(),
            b": Input \"",
            line.expand_range(consumed as isize, 20),
            if line.length().wrapping_sub(consumed) > 20 {
                "..."
            } else {
                ""
            },
            b"\" does not match format \"%",
            formatstring,
            b"\""
        );
        if let Some(output) = output {
            msg.extend(cmsg!(b" (\"", output.expand(), b"\")"));
        }
        msg.push(b'\n');
        drop(line);
        stream_error(&msg);
    }

    /// `matchSeparator`
    fn match_separator(&self) -> bool {
        // called before value is read, first value has Separator flag cleared
        // for second and next value set Separator flag

        let separator = self.separator.borrow();
        if !separator.is_set() {
            // empty separator matches
            return true;
        }
        if !self.has(SEPARATOR) {
            // before first element, don't expect separator yet
            self.set_flags(SEPARATOR);
            return true;
        }
        let line = self.input_line.borrow();
        let mut j = self.consumed_input.get();
        let mut i = 0;
        while i < separator.length() {
            match separator.get(i as isize) {
                codes::SKIP => {
                    j += 1;
                }
                codes::WHITESPACE => {
                    while c_isspace(line.get(j as isize) as i8 as i32) {
                        j += 1;
                    }
                }
                s => {
                    let s = if s == ESC {
                        i += 1;
                        separator.get(i as isize)
                    } else {
                        s
                    };
                    if s != line.get(j as isize) {
                        // no match
                        // don't complain here, just return false
                        debug!(
                            format!("StreamCore::matchSeparator({}) separator \"", self.name()),
                            separator.expand(),
                            b"\" not found\n"
                        );
                        return false;
                    }
                    j += 1;
                }
            }
            i += 1;
        }
        // separator successfully read
        debug!(
            format!("StreamCore::matchSeparator({}) separator \"", self.name()),
            separator.expand(),
            b"\" found\n"
        );
        self.consumed_input.set(j);
        true
    }

    /// `scanValue(fmt, long&)`: the bytes the value takes, -1 when it does
    /// not match.
    pub fn scan_value_long(&self, fmt: &StreamFormat, value: &mut i64) -> isize {
        if fmt.type_ != UNSIGNED_FORMAT && fmt.type_ != SIGNED_FORMAT && fmt.type_ != ENUM_FORMAT {
            stream_error(&cmsg!(
                self.name(),
                b": scanValue(long&) called with %",
                Ch(fmt.conv as i32),
                b" format\n"
            ));
            return -1;
        }
        self.set_flags(SCAN_TRIED);
        if !self.match_separator() {
            return -1;
        }
        let consumed_input = self.consumed_input.get();
        let line = self.input_line.borrow();
        let mut consumed = converter::find(fmt.conv).map_or(-1, |r| {
            r.converter
                .scan_long(fmt, line.ptr(consumed_input as isize), value)
        });
        if consumed < 0 {
            // C's format ends "failed\\n" (StreamCore.cc scanValue(long&)):
            // a backslash, an n and no line end, which joins the next line.
            debug!(
                format!(
                    "StreamCore::scanValue({}, format=%{}, long) input=\"",
                    self.name(),
                    fmt.conv as char
                ),
                line.expand_from(consumed_input as isize),
                b"\" failed\n"
            );
            if fmt.flags & DEFAULT_FLAG != 0 {
                *value = 0;
                consumed = 0;
            } else {
                return -1;
            }
        }
        debug!(
            format!(
                "StreamCore::scanValue({}, format=%{}, long) input=\"",
                self.name(),
                fmt.conv as char
            ),
            line.expand_range(consumed_input as isize, consumed),
            format!("\" value={value}\n")
        );
        if fmt.flags & FIX_WIDTH_FLAG != 0 && consumed as u64 != fmt.width {
            return -1;
        }
        if consumed as usize > line.length().wrapping_sub(consumed_input) {
            return -1;
        }
        self.set_flags(GOT_VALUE);
        consumed
    }

    /// `scanValue(fmt, double&)`
    pub fn scan_value_double(&self, fmt: &StreamFormat, value: &mut f64) -> isize {
        if fmt.type_ != DOUBLE_FORMAT {
            stream_error(&cmsg!(
                self.name(),
                b": scanValue(double&) called with %",
                Ch(fmt.conv as i32),
                b" format\n"
            ));
            return -1;
        }
        self.set_flags(SCAN_TRIED);
        if !self.match_separator() {
            return -1;
        }
        let consumed_input = self.consumed_input.get();
        let line = self.input_line.borrow();
        let mut consumed = converter::find(fmt.conv).map_or(-1, |r| {
            r.converter
                .scan_double(fmt, line.ptr(consumed_input as isize), value)
        });
        if consumed < 0 {
            debug!(
                format!(
                    "StreamCore::scanValue({}, format=%{}, double) input=\"",
                    self.name(),
                    fmt.conv as char
                ),
                line.expand_from(consumed_input as isize),
                b"\" failed\n"
            );
            if fmt.flags & DEFAULT_FLAG != 0 {
                *value = 0.0;
                consumed = 0;
            } else {
                return -1;
            }
        }
        debug!(
            format!(
                "StreamCore::scanValue({}, format=%{}, double) input=\"",
                self.name(),
                fmt.conv as char
            ),
            line.expand_range(consumed_input as isize, consumed),
            format!("\" value={:#}\n", CFloat(*value).g())
        );
        if fmt.flags & FIX_WIDTH_FLAG != 0
            && consumed != fmt.width.wrapping_add(fmt.prec as u64).wrapping_add(1) as isize
        {
            return -1;
        }
        if consumed as usize > line.length().wrapping_sub(consumed_input) {
            return -1;
        }
        self.set_flags(GOT_VALUE);
        consumed
    }

    /// `scanValue(fmt, char*, size)`: `value` holds at most `value.len()`
    /// bytes, the terminating 0x00 included; `size` is set to the bytes
    /// written.
    pub fn scan_value_string(
        &self,
        fmt: &StreamFormat,
        value: &mut [u8],
        size: &mut usize,
    ) -> isize {
        if fmt.type_ != STRING_FORMAT {
            stream_error(&cmsg!(
                self.name(),
                b": scanValue(char*) called with %",
                Ch(fmt.conv as i32),
                b" format\n"
            ));
            return -1;
        }
        self.set_flags(SCAN_TRIED);
        if !self.match_separator() {
            return -1;
        }
        let consumed_input = self.consumed_input.get();
        let line = self.input_line.borrow();
        let mut consumed = converter::find(fmt.conv).map_or(-1, |r| {
            r.converter
                .scan_string(fmt, line.ptr(consumed_input as isize), value, size)
        });
        if consumed < 0 {
            debug!(
                format!(
                    "StreamCore::scanValue({}, format=%{}, char*, size={}) input=\"",
                    self.name(),
                    fmt.conv as char,
                    size
                ),
                line.expand_from(consumed_input as isize),
                b"\" failed\n"
            );
            if fmt.flags & DEFAULT_FLAG != 0 {
                if let Some(v) = value.first_mut() {
                    *v = 0;
                }
                consumed = 0;
            } else {
                return -1;
            }
        }
        debug!(
            format!(
                "StreamCore::scanValue({}, format=%{}, char*, size={}) input=\"",
                self.name(),
                fmt.conv as char,
                size
            ),
            line.expand_from(consumed_input as isize),
            b"\" value=\"",
            StreamBuffer::from_bytes(&value[..(*size).min(value.len())]).expand(),
            b"\"\n"
        );
        if fmt.flags & FIX_WIDTH_FLAG != 0 && consumed as u64 != fmt.width {
            return -1;
        }
        if consumed as usize > line.length().wrapping_sub(consumed_input) {
            return -1;
        }
        self.set_flags(GOT_VALUE);
        consumed
    }

    /// `checkShouldPrint(newErrorType)`: whether to print an error of this
    /// type, from the type and time of the last one; counts the errors
    /// left unprinted and reports them when it prints again.
    fn check_should_print(&self, new_error_type: ProtocolResult) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let dead_time = STREAM_ERROR_DEAD_TIME.load(Ordering::Relaxed);
        if self.previous_result.get() != new_error_type {
            self.previous_result.set(new_error_type);
            self.number_of_errors.set(0);
            self.last_error_time.set(now);
            true
        } else if (now - self.last_error_time.get()) as i32 > dead_time {
            self.last_error_time.set(now);
            if self.number_of_errors.get() != 0 {
                stream_error(&cmsg!(format!(
                    "{}: {} additional errors of the following type seen in the last {} seconds\n",
                    self.name(),
                    self.number_of_errors.get(),
                    dead_time
                )));
            }
            self.number_of_errors.set(0);
            true
        } else {
            self.number_of_errors.set(self.number_of_errors.get() + 1);
            false
        }
    }

    /// `extract<unsigned long>(commandIndex)`
    fn extract_command_ulong(&self) -> u64 {
        let code = self.code.borrow().clone();
        let mut c = CPtr::at_offset(code.bytes(), self.command_index.get());
        let value = extract_ulong(&mut c);
        self.command_index.set(c.offset());
        value
    }

    // Handle 'event' command

    /// `evalEvent`
    fn eval_event(&self) -> bool {
        // code layout:
        // eventmask timeout
        let event_mask = self.extract_command_ulong();
        let event_timeout = self.extract_command_ulong();
        if self.has(ASYNC_MODE) && event_timeout == 0 && self.has(BUS_OWNER) {
            self.bus().bus_unlock();
            self.clear_flags(BUS_OWNER);
        }
        self.set_flags(ACCEPT_EVENT);
        self.bus().bus_accept_event(event_mask, event_timeout);
        true
    }

    /// `eventCallback(status)`, the lock held.
    fn event_callback(&self, status: StreamIoStatus) {
        debug!(format!(
            "StreamCore::eventCallback({}, {}) activeCommand: {}\n",
            self.name(),
            status.to_str(),
            commands::to_str(self.active_command.get())
        ));

        if !self.has(ACCEPT_EVENT) {
            stream_error(&cmsg!(format!(
                "{}: StreamCore::eventCallback({}) called unexpectedly\n",
                self.name(),
                status.to_str()
            )));
            return;
        }
        self.clear_flags(ACCEPT_EVENT);
        match status {
            StreamIoStatus::StreamIoTimeout => {
                stream_error(&cmsg!(self.name(), b": No event from device\n"));
                self.finish_protocol(ProtocolResult::ReplyTimeout);
            }
            StreamIoStatus::StreamIoSuccess => {
                self.eval_command();
            }
            _ => {
                stream_error(&cmsg!(format!(
                    "{}: Event error from device: {}\n",
                    self.name(),
                    status.to_str()
                )));
                self.finish_protocol(ProtocolResult::Fault);
            }
        }
    }

    // Handle 'wait' command

    /// `evalWait`
    fn eval_wait(&self) -> bool {
        let wait_timeout = self.extract_command_ulong();
        self.set_flags(WAIT_PENDING);
        self.hooks.start_timer(self, wait_timeout);
        true
    }

    /// `timerCallback`, the lock held.
    fn timer_callback(&self) {
        debug!(format!("StreamCore::timerCallback({})\n", self.name()));
        if !self.has(WAIT_PENDING) {
            stream_error(&cmsg!(
                self.name(),
                b": StreamCore::timerCallback() called unexpectedly\n"
            ));
            return;
        }
        self.clear_flags(WAIT_PENDING);
        self.eval_command();
    }

    /// `evalExec`
    fn eval_exec(&self) -> bool {
        if !self.format_output() {
            self.finish_protocol(ProtocolResult::FormatError);
            return false;
        }
        debug!(
            b"StreamCore::evalExec: command = \"",
            self.output_line.borrow().expand(),
            b"\"\n"
        );
        // release bus
        if self.has(BUS_OWNER) {
            debug!(format!(
                "StreamCore::evalExec({}): unlocking bus\n",
                self.name()
            ));
            self.bus().bus_unlock();
            self.clear_flags(BUS_OWNER);
        }
        if !self.hooks.execute(self) {
            stream_error(&cmsg!(
                self.name(),
                b": executing command \"",
                self.output_line.borrow().cstr(),
                b"\"\n"
            ));
            return false;
        }
        true
    }

    /// `execCallback(status)`, the lock held.
    fn exec_callback(&self, status: StreamIoStatus) {
        let active = self.active_command.get();
        debug!(format!(
            "StreamCore::execCallback({}, {}) activeCommand: {}\n",
            self.name(),
            status.to_str(),
            commands::to_str(active)
        ));

        if active != EXEC {
            stream_error(&cmsg!(format!(
                "{}: execCallback ({}) called unexpectedly during command {}\n",
                self.name(),
                status.to_str(),
                commands::to_str(active)
            )));
        } else if status != StreamIoStatus::StreamIoSuccess {
            stream_error(&cmsg!(
                self.name(),
                b": Shell command \"",
                self.output_line.borrow().cstr(),
                b"\" failed\n"
            ));
            self.finish_protocol(ProtocolResult::Fault);
        } else {
            self.eval_command();
        }
    }

    /// `evalConnect`
    fn eval_connect(&self) -> bool {
        let connect_timeout = self.extract_command_ulong();
        if !self.bus().bus_connect_request(connect_timeout) {
            stream_error(&cmsg!(
                self.name(),
                b": Connect not supported for this bus\n"
            ));
            return false;
        }
        true
    }

    /// `connectCallback(status)`, the lock held.
    fn connect_callback(&self, status: StreamIoStatus) {
        let active = self.active_command.get();
        debug!(format!(
            "StreamCore::connectCallback({}, {}) activeCommand: {}\n",
            self.name(),
            status.to_str(),
            commands::to_str(active)
        ));

        if active == END {
            // asynchronous connect
            self.start_protocol(StartMode::StartInit);
        } else if active != CONNECT {
            stream_error(&cmsg!(format!(
                "{}: connectCallback({}) called unexpectedly during command {}\n",
                self.name(),
                status.to_str(),
                commands::to_str(active)
            )));
        } else if status != StreamIoStatus::StreamIoSuccess {
            stream_error(&cmsg!(self.name(), b": Connect failed\n"));
            self.finish_protocol(ProtocolResult::Fault);
        } else {
            self.eval_command();
        }
    }

    /// `evalDisconnect`
    fn eval_disconnect(&self) -> bool {
        if !self.bus().bus_disconnect() {
            stream_error(&cmsg!(self.name(), b": Cannot disconnect from this bus\n"));
            self.finish_protocol(ProtocolResult::Fault);
            return false;
        }
        true
    }

    /// `disconnectCallback(status)`, the lock held.
    fn disconnect_callback(&self, status: StreamIoStatus) {
        let active = self.active_command.get();
        debug!(format!(
            "StreamCore::disconnectCallback({}, {}) activeCommand: {}\n",
            self.name(),
            status.to_str(),
            commands::to_str(active)
        ));

        if active != DISCONNECT {
            // asynchronous disconnect
            self.clear_flags(BUS_PENDING);
            self.finish_protocol(ProtocolResult::Offline);
        } else if status != StreamIoStatus::StreamIoSuccess {
            stream_error(&cmsg!(self.name(), b": Disconnect failed\n"));
            self.finish_protocol(ProtocolResult::Fault);
        } else {
            self.eval_command();
        }
    }

    /// `printStatus(buffer)`
    pub fn print_status(&self, buffer: &mut StreamBuffer) {
        let active = self.active_command.get();
        buffer.print(format_args!(
            "active command={} ",
            if active != 0 {
                commands::to_str(active)
            } else {
                "none"
            }
        ));
        let flags = self.flags.get();
        buffer.print(format_args!("flags=0x{flags:04x}"));
        for (f, name) in [
            (IGNORE_EXTRA_INPUT, " IgnoreExtraInput"),
            (INIT_RUN, " InitRun"),
            (ASYNC_MODE, " AsyncMode"),
            (GOT_VALUE, " GotValue"),
            (BUS_OWNER, " BusOwner"),
            (SEPARATOR, " Separator"),
            (SCAN_TRIED, " ScanTried"),
            (ACCEPT_INPUT, " AcceptInput"),
            (ACCEPT_EVENT, " AcceptEvent"),
            (LOCK_PENDING, " LockPending"),
            (WRITE_PENDING, " WritePending"),
            (WAIT_PENDING, " WaitPending"),
            (ABORTED, " Aborted"),
        ] {
            if flags & f != 0 {
                buffer.append(name.as_bytes());
            }
        }
        self.bus().bus_print_status(buffer);
    }

    /// `getInTerminator`: `None` when no terminator is defined.
    pub fn get_in_terminator(&self) -> Option<Vec<u8>> {
        self.in_terminator_defined
            .get()
            .then(|| self.in_terminator.borrow().bytes().to_vec())
    }

    /// `getOutTerminator`: `None` when no terminator is defined.
    pub fn get_out_terminator(&self) -> Option<Vec<u8>> {
        self.out_terminator_defined
            .get()
            .then(|| self.out_terminator.borrow().bytes().to_vec())
    }
}

impl<H: StreamCoreHooks> Drop for StreamCore<H> {
    /// `~StreamCore()`
    fn drop(&mut self) {
        debug!(format!(
            "~StreamCore({}) {:p}\n",
            self.name(),
            self as *const Self
        ));
        self.release_bus();
    }
}

/// `license()`
pub fn license() -> &'static str {
    concat!(
        "StreamDevice is free software: You can redistribute it and/or modify\n",
        "it under the terms of the GNU Lesser General Public License as published\n",
        "by the Free Software Foundation, either version 3 of the License, or\n",
        "(at your option) any later version.\n",
        "\n",
        "StreamDevice is distributed in the hope that it will be useful,\n",
        "but WITHOUT ANY WARRANTY; without even the implied warranty of\n",
        "MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the\n",
        "GNU Lesser General Public License for more details\n",
        "\n",
        "You should have received a copy of the GNU Lesser General Public License\n",
        "along with StreamDevice. If not, see https://www.gnu.org/licenses/.\n",
    )
}

/// The core as `StreamProtocolParser::Client`, while it compiles: the
/// addresses its `%(FIELD)` formats name, in the order their indices count.
struct Compiler<'a, H: StreamCoreHooks> {
    core: &'a StreamCore<H>,
    field_addresses: Vec<H::FieldAddress>,
}

impl<H: StreamCoreHooks> protocol::Client for Compiler<'_, H> {
    /// `compileCommand`
    fn compile_command(
        &mut self,
        protocol: &mut Protocol,
        buffer: &mut StreamBuffer,
        command: CPtr<'_>,
        args: &mut CPtr<'_>,
    ) -> bool {
        const MAX: u64 = 0xFFFF_FFFF;
        let mut timeout = 0u64;
        let name = command.cstr();

        match name {
            b"in" | b"out" | b"exec" => {
                let (code, format_type) = match name {
                    b"in" => (IN, FormatType::ScanFormat),
                    b"out" => (OUT, FormatType::PrintFormat),
                    _ => (EXEC, FormatType::PrintFormat),
                };
                buffer.append_byte(code);
                if !protocol.compile_string(buffer, args, format_type, Some(self), 0) {
                    return false;
                }
                buffer.append_byte(codes::EOS);
                true
            }
            b"wait" | b"connect" => {
                buffer.append_byte(if name == b"wait" { WAIT } else { CONNECT });
                if !protocol.compile_number(&mut timeout, args, MAX) {
                    return false;
                }
                buffer.append(&timeout.to_ne_bytes());
                true
            }
            b"event" => {
                if !self.core.bus_supports_event() {
                    stream_error_at(
                        command.line_number(),
                        protocol.filename(),
                        b"Events not supported by businterface.\n",
                    );
                    return false;
                }
                let mut eventmask = 0xffff_ffffu64;
                buffer.append_byte(EVENT);
                if args.peek() == b'(' {
                    args.advance(1);
                    if !protocol.compile_number(&mut eventmask, args, MAX) {
                        return false;
                    }
                    if args.peek() != b')' {
                        stream_error_at(
                            command.line_number(),
                            protocol.filename(),
                            &cmsg!(b"Expect ')' instead of: '", args.cstr(), b"'\n"),
                        );
                        return false;
                    }
                    args.advance(1);
                    while c_isspace(args.peek() as i8 as i32) {
                        args.advance(1);
                    }
                }
                buffer.append(&eventmask.to_ne_bytes());
                if args.peek() != 0 && !protocol.compile_number(&mut timeout, args, MAX) {
                    return false;
                }
                buffer.append(&timeout.to_ne_bytes());
                true
            }
            b"disconnect" => {
                buffer.append_byte(DISCONNECT);
                true
            }
            _ => {
                stream_error_at(
                    command.line_number(),
                    protocol.filename(),
                    &cmsg!(b"Unknown command name '", name, b"'\n"),
                );
                false
            }
        }
    }

    fn get_field_address(&mut self, fieldname: &[u8], address: &mut StreamBuffer) -> bool {
        let Some(field) = self.core.hooks.get_field_address(self.core, fieldname) else {
            return false;
        };
        address.append(&(self.field_addresses.len() as u32).to_ne_bytes());
        self.field_addresses.push(field);
        true
    }

    fn name(&self) -> String {
        self.core.name().to_string()
    }
}
