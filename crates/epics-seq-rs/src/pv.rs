//! The pv layer (`pv.h`, `pvCa.c`): what seq calls to reach a named pv.
//!
//! The system is the IOC's CA client context
//! ([`epics_base_rs::server::database::CaClientContext`]), libca with
//! `dbContext` installed: a name this IOC serves opens an in-process
//! channel, any other goes to the network client epics-ca-rs registered,
//! so a program reaches remote pvs as C's `seq_ca.c` does.
//!
//! C's `(pvEventFunc, void *arg)` pairs are closures here, one per request.
//!
//! An in-process channel's request is made on the caller's thread, as
//! `dbContext` makes it: the channel is connected inside
//! [`PvVar::create`], and a get's callback, or a put callback's when the
//! record's processing ends there, runs before the call returns, so the
//! caller holds no lock that callback takes. A network channel's request
//! goes to the system's one request queue, which issues each in the order
//! made, whatever its channel, as libca queues each on its circuit when it
//! is called; its completion runs on the database reactor, where C's runs
//! on a CA auxiliary thread. A network channel's
//! monitor events and connection changes come from the reactor too; an
//! in-process channel's monitor events come from the system's one queue,
//! in post order, on its `CAC-event` thread. Either kind of monitor event
//! then passes the [`EventGate`] its monitor-on named. None runs inside a
//! call, so a caller may hold seq's locks across [`PvVar::monitor_on`], as
//! C's connection handler holds `sp->lock` across `pvVarMonitorOn`.
//!
//! What libca refuses at once (disconnected, no access, too many elements)
//! is refused here at once, before anything is queued, and returns
//! [`PvStat::Ok`] as `statFromCA` makes of those warning-class codes; the
//! callback is then never called. A request libca accepted is completed
//! exactly once: by the server, or with `ECA_DISCONN` when the channel
//! disconnects first, ahead of the connection handler's `false`.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::task::{Context, Poll, Waker};

use epics_base_rs::error::CaError;
use epics_base_rs::runtime::general_time::{EPICS_EPOCH_UNIX_SECS, get_current};
use epics_base_rs::runtime::log::{ErrlogSevEnum, errlog_sev_printf};
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::runtime::task::TaskHandle;
use epics_base_rs::server::database::{
    CaClientContext, ClientChannel, ClientMonitor, Connection, InProcessChannel, InProcessEvents,
    InProcessSubscription, PutCompletion, PvDatabase,
};
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{
    DBR_CHAR, DBR_DOUBLE, DBR_FLOAT, DBR_LONG, DBR_SHORT, DBR_STRING, DBR_TIME_CHAR,
    DBR_TIME_DOUBLE, DBR_TIME_FLOAT, DBR_TIME_LONG, DBR_TIME_SHORT, DBR_TIME_STRING, DbFieldType,
    EpicsValue,
};
use parking_lot::{Mutex, ReentrantMutex};

use crate::types::{PvSevr, PvStat, PvType};

/// `pvEventType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PvEventType {
    Get,
    Put,
    Monitor,
}

/// What a get, put or monitor callback receives (`pvEventFunc`'s
/// arguments past `arg`). `value` is `None` for a put, and for a get or
/// monitor that failed.
pub struct PvEvent<'a> {
    pub evt: PvEventType,
    pub ty: PvType,
    pub count: u32,
    pub value: Option<&'a Snapshot>,
    /// `statFromCA` of the request's CA status.
    pub status: PvStat,
    /// `sevrFromCA` of the request's CA status; the value's own alarm is in
    /// `value`.
    pub severity: PvSevr,
    /// `ca_message` of the request's CA status, which C also leaves in
    /// `pvVarGetMess`.
    pub message: Option<&'a str>,
}

/// `pvSystem`: one per IOC, shared by every program (`seq_cmd.c`
/// `createOrAttachPvSystem`).
#[derive(Clone)]
pub struct PvSystem {
    ctx: CaClientContext,
    events: InProcessEvents,
    /// The network channels' request queue, its [`Dispatcher`] started
    /// with the first network channel.
    requests: Arc<Mutex<Option<mpsc::UnboundedSender<Request>>>>,
}

impl PvSystem {
    /// `pvSysCreate`.
    pub fn create(db: &PvDatabase) -> PvSystem {
        PvSystem {
            ctx: db.ca_client(),
            events: InProcessEvents::default(),
            requests: Arc::default(),
        }
    }

    /// `pvSysAttach`: make the calling thread a client of this system. The
    /// context is not per thread here, so there is nothing to attach.
    pub fn attach(&self) -> PvStat {
        PvStat::Ok
    }

    /// `pvSysFlush`: send what the calling thread has queued. The
    /// [`Dispatcher`] sends each request as it takes it, so nothing waits
    /// for a flush.
    pub fn flush(&self) -> PvStat {
        PvStat::Ok
    }

    /// The network request queue; `None` when the database captured no
    /// reactor to run its [`Dispatcher`] on.
    fn requests(&self) -> Option<mpsc::UnboundedSender<Request>> {
        let mut requests = self.requests.lock();
        if requests.is_none() {
            let (tx, rx) = mpsc::unbounded_channel();
            let dispatcher = Dispatcher {
                ctx: self.ctx.clone(),
                chans: HashMap::new(),
            };
            self.ctx.spawn(dispatcher.run(rx))?;
            *requests = Some(tx);
        }
        requests.clone()
    }

    /// The CA client context underneath.
    pub fn context(&self) -> &CaClientContext {
        &self.ctx
    }

    /// The in-process monitor events queued now, each through its gate, on
    /// the calling thread, which holds no lock a monitor callback takes.
    pub fn deliver(&self) {
        self.events.deliver();
    }

    /// A new [`EventGate`] over this system's queue, open until a state set
    /// runs.
    pub fn gate(&self) -> Arc<EventGate> {
        Arc::new(EventGate {
            events: self.events.clone(),
            state: Mutex::default(),
        })
    }
}

/// The monitor events of one client's vars, held while any of its state
/// sets runs and let through, in the order they came, at a state set's
/// delivery point while no other one runs: a block, which takes all of
/// them and, as it ends, those posted before it ends, the return of a
/// [`Self::call`], which takes those posted before the call began, or a
/// [`Self::pass`], which takes what the last call's return did. C's
/// `CAC-event` delivers on its own CPU within
/// microseconds, so a post lands by the client's next call or wait, but
/// never before the put that made it returns; this makes that the rule. A
/// held event whose monitor is turned off or whose var is destroyed before
/// it is let through is dropped, as `db_cancel_event` drops a queued one.
#[derive(Default)]
pub struct EventGate {
    events: InProcessEvents,
    state: Mutex<GateState>,
}

#[derive(Default)]
struct GateState {
    /// The state sets running, each with the cut of its last
    /// [`EventGate::call`] since it last blocked.
    running: Vec<(usize, Option<u64>)>,
    /// How many events were ever taken in: the next one's number.
    taken: u64,
    held: VecDeque<(u64, Box<dyn FnOnce() + Send>)>,
}

impl EventGate {
    /// State set `ss` runs the client's code.
    pub fn run(&self, ss: usize) {
        let mut st = self.state.lock();
        if !st.running.iter().any(|&(r, _)| r == ss) {
            st.running.push((ss, None));
        }
    }

    /// State set `ss` ends.
    pub fn stop(&self, ss: usize) {
        self.blocked(ss);
    }

    /// `wait` as a block point of state set `ss`, which is back as it was
    /// after. What was posted before `wait` returned is taken in before
    /// `ss` runs again: `CAC-event` ends its pass over the queue chain
    /// before a thread one of its entries woke runs, and a later queue's
    /// entry, posted first, comes after the entry that woke it.
    pub fn block<R>(&self, ss: usize, wait: impl FnOnce() -> R) -> R {
        let ran = self.blocked(ss);
        let r = wait();
        if ran {
            self.events.deliver();
            self.run(ss);
        }
        r
    }

    /// `call`, a request of state set `ss`, whose return is a delivery
    /// point for the events posted before it began. The caller holds no
    /// lock a monitor callback takes.
    pub fn call<R>(&self, ss: usize, call: impl FnOnce() -> R) -> R {
        self.events.deliver();
        let cut = {
            let mut st = self.state.lock();
            let cut = st.taken;
            st.called(ss, cut);
            cut
        };
        let r = call();
        let mut st = self.state.lock();
        // Again: a block inside `call` cleared it.
        st.called(ss, cut);
        st.let_through(ss, cut);
        r
    }

    /// A delivery point of state set `ss` that does not block, a wait that
    /// finds its event raised or a sleep that returns at once: it lets
    /// through what the return of its last [`Self::call`] since it last
    /// blocked did, or, with none, everything taken in. The action's own
    /// put is too recent for C's `CAC-event` to have landed its monitor, a
    /// post from before that put is not.
    pub fn pass(&self, ss: usize) {
        self.events.deliver();
        let mut st = self.state.lock();
        let cut = st
            .running
            .iter()
            .find(|&&(r, _)| r == ss)
            .and_then(|&(_, c)| c);
        st.let_through(ss, cut.unwrap_or(u64::MAX));
    }

    /// `ss` runs no more, and whether it did: the system's queued
    /// in-process events come in first, and when no state set runs on,
    /// everything held is delivered here before any runs again. The caller
    /// holds no lock a monitor callback takes.
    fn blocked(&self, ss: usize) -> bool {
        self.events.deliver();
        let mut st = self.state.lock();
        let ran = st.running.iter().position(|&(r, _)| r == ss);
        if let Some(i) = ran {
            st.running.swap_remove(i);
        }
        if st.running.is_empty() {
            while let Some((_, event)) = st.held.pop_front() {
                event();
            }
        }
        ran.is_some()
    }

    /// `call` now, under the gate, when no state set runs; else at a
    /// delivery point.
    fn admit(&self, call: impl FnOnce() + Send + 'static) {
        let mut st = self.state.lock();
        let n = st.taken;
        st.taken += 1;
        if st.running.is_empty() {
            call();
        } else {
            st.held.push_back((n, Box::new(call)));
        }
    }
}

impl GateState {
    /// `cut` is the cut of state set `ss`'s last call, when it runs.
    fn called(&mut self, ss: usize, cut: u64) {
        if let Some((_, last)) = self.running.iter_mut().find(|(r, _)| *r == ss) {
            *last = Some(cut);
        }
    }

    /// The held events numbered below `cut` let through, when no state set
    /// but `ss` runs.
    fn let_through(&mut self, ss: usize, cut: u64) {
        if self.running.iter().all(|&(r, _)| r == ss) {
            while let Some((_, event)) = self.held.pop_front_if(|(n, _)| *n < cut) {
                event();
            }
        }
    }
}

/// The libca statuses this layer produces (`caerr.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Eca {
    Normal,
    GetFail,
    PutFail,
    BadCount,
    Disconn,
    NoRdAccess,
    NoWtAccess,
    BadChid,
    Timeout,
}

impl Eca {
    /// `ca_message`.
    fn text(self) -> &'static str {
        match self {
            Eca::Normal => "Normal successful completion",
            Eca::GetFail => "Channel read request failed",
            Eca::PutFail => "Channel write request failed",
            Eca::BadCount => "Invalid element count requested",
            Eca::Disconn => "Virtual circuit disconnect",
            Eca::NoRdAccess => "Read access denied",
            Eca::NoWtAccess => "Write access denied",
            Eca::BadChid => "Invalid channel identifier",
            Eca::Timeout => "User specified timeout on IO operation expired",
        }
    }

    /// `ca_message` as the shared string `pvVarGetMess` hands out, so an
    /// event does not allocate one.
    fn message(self) -> Arc<str> {
        static TEXTS: LazyLock<[Arc<str>; 9]> = LazyLock::new(|| {
            [
                Eca::Normal,
                Eca::GetFail,
                Eca::PutFail,
                Eca::BadCount,
                Eca::Disconn,
                Eca::NoRdAccess,
                Eca::NoWtAccess,
                Eca::BadChid,
                Eca::Timeout,
            ]
            .map(|e| Arc::from(e.text()))
        });
        TEXTS[self as usize].clone()
    }

    /// `sevrFromCA`: `ECA_NORMAL` is `CA_K_SUCCESS`, `ECA_BADCHID`
    /// `CA_K_ERROR`, every other one here `CA_K_WARNING`.
    fn severity(self) -> PvSevr {
        match self {
            Eca::Normal => PvSevr::None,
            Eca::BadChid => PvSevr::Major,
            _ => PvSevr::Minor,
        }
    }

    /// `statFromCA`: a warning is still [`PvStat::Ok`].
    fn stat(self) -> PvStat {
        match self.severity() {
            PvSevr::None | PvSevr::Minor => PvStat::Ok,
            _ => PvStat::Error,
        }
    }

    /// The errlog severity `INVOKE` prints with, `sevrFromCA` again.
    fn errlog_severity(self) -> ErrlogSevEnum {
        match self.severity() {
            PvSevr::Major => ErrlogSevEnum::Major,
            _ => ErrlogSevEnum::Minor,
        }
    }
}

/// `typeToCA`.
fn type_to_ca(ty: PvType) -> u16 {
    match ty {
        PvType::Char => DBR_CHAR,
        PvType::Short => DBR_SHORT,
        PvType::Long => DBR_LONG,
        PvType::Float => DBR_FLOAT,
        PvType::Double => DBR_DOUBLE,
        PvType::String => DBR_STRING,
        PvType::TimeChar => DBR_TIME_CHAR,
        PvType::TimeShort => DBR_TIME_SHORT,
        PvType::TimeLong => DBR_TIME_LONG,
        PvType::TimeFloat => DBR_TIME_FLOAT,
        PvType::TimeDouble => DBR_TIME_DOUBLE,
        PvType::TimeString => DBR_TIME_STRING,
        PvType::Error => unreachable!("pv_is_valid_type is asserted first"),
    }
}

/// `pv_size_n` (`pvType.h`) over `pv.c`'s `pv_sizes` and `pv_value_sizes`:
/// the bytes of a `ty` value of `count` elements, the first inside the
/// `dbr_time_*` struct.
pub fn pv_size_n(ty: PvType, count: u32) -> usize {
    let (size, value_size) = match ty {
        PvType::Char => (1, 1),
        PvType::Short => (2, 2),
        PvType::Long => (4, 4),
        PvType::Float => (4, 4),
        PvType::Double => (8, 8),
        PvType::String => (40, 40),
        PvType::TimeChar => (16, 1),
        PvType::TimeShort => (16, 2),
        PvType::TimeLong => (16, 4),
        PvType::TimeFloat => (16, 4),
        PvType::TimeDouble => (24, 8),
        PvType::TimeString => (52, 40),
        PvType::Error => unreachable!("pv_size_n asserts pv_is_valid_type"),
    };
    match count {
        0 => size,
        n => size + (n as usize - 1) * value_size,
    }
}

type Done = Box<dyn FnOnce(PvEvent<'_>) + Send>;
type MonitorFn = Arc<dyn Fn(PvEvent<'_>) + Send + Sync>;
type ConnFn = Arc<dyn Fn(bool) + Send + Sync>;

/// One monitor-on: its `monid`, its request, its callback and the gate its
/// events pass.
#[derive(Clone)]
struct Monitor {
    monid: u64,
    ty: PvType,
    count: u32,
    gate: Arc<EventGate>,
    event: MonitorFn,
}

impl Monitor {
    /// `pvCaMonitorHandler` for one event, once the gate lets it through
    /// and while this is still the channel's monitor.
    fn deliver(&self, shared: &Arc<Shared>, snapshot: Snapshot) {
        let (m, shared) = (self.clone(), shared.clone());
        self.gate.admit(move || {
            shared.callback(
                Some(Eca::Normal),
                |st| st.monitor == m.monid,
                || {
                    (m.event)(PvEvent {
                        evt: PvEventType::Monitor,
                        ty: m.ty,
                        count: event_count(m.count, Some(&snapshot)),
                        value: Some(&snapshot),
                        status: PvStat::Ok,
                        severity: PvSevr::None,
                        message: Some(Eca::Normal.text()),
                    })
                },
            )
        });
    }
}

/// A get or put callback libca accepted and has not completed.
struct Pending {
    evt: PvEventType,
    ty: PvType,
    count: u32,
    done: Done,
}

#[derive(Default)]
struct State {
    connected: bool,
    closed: bool,
    /// The live `monid`'s generation; 0 when there is none.
    monitor: u64,
    next_id: u64,
    /// libca's `eventq` of get and put callbacks.
    pending: HashMap<u64, Pending>,
    /// `pvVarGetMess`.
    message: Option<Arc<str>>,
}

struct Shared {
    chan: OnceLock<Arc<dyn ClientChannel>>,
    state: Mutex<State>,
    /// libca's `cbMutex`, per channel: held across every callback, and taken
    /// by destroy and monitor-off, so none runs once they return. Reentrant,
    /// so a callback may destroy its own channel.
    callback: ReentrantMutex<()>,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            chan: OnceLock::new(),
            state: Mutex::new(State {
                next_id: 1,
                ..State::default()
            }),
            callback: ReentrantMutex::new(()),
        }
    }

    /// One connection change. A disconnect first completes each accepted get
    /// and put with `ECA_DISCONN`, in the order they were made
    /// (`cac::disconnectAllIO`), then calls the handler.
    fn connection_change(&self, up: bool, conn: &dyn Fn(bool)) {
        let mut lost: Vec<_> = {
            let mut st = self.state.lock();
            st.connected = up;
            if up {
                Vec::new()
            } else {
                std::mem::take(&mut st.pending).into_iter().collect()
            }
        };
        lost.sort_by_key(|(id, _)| *id);
        for (_, p) in lost {
            self.deliver(p, None, Eca::Disconn);
        }
        self.callback(None, |_| true, || conn(up));
    }

    /// Refuse a request as libca does before queueing it (`INVOKE`).
    fn refuse(&self, eca: Eca, expr: &str) -> PvStat {
        self.state.lock().message = Some(eca.message());
        errlog_sev_printf(eca.errlog_severity(), &format!("{expr}: {}", eca.text()));
        eca.stat()
    }

    /// Run `call` as a libca callback, unless the channel was destroyed or
    /// `live` says the request is gone. An event callback's status is left
    /// in `pvVarGetMess` first, as `pvCaEventHandler` does.
    fn callback(
        &self,
        status: Option<Eca>,
        live: impl FnOnce(&State) -> bool,
        call: impl FnOnce(),
    ) {
        let _cb = self.callback.lock();
        {
            let mut st = self.state.lock();
            if st.closed || !live(&st) {
                return;
            }
            if let Some(eca) = status {
                st.message = Some(eca.message());
            }
        }
        call();
    }

    fn is_pending(&self, id: u64) -> bool {
        self.state.lock().pending.contains_key(&id)
    }

    /// The server's answer to request `id`, unless a disconnect or destroy
    /// took the request first.
    fn complete(&self, id: u64, result: Result<Option<Snapshot>, String>) {
        let (pending, connected) = {
            let mut st = self.state.lock();
            (st.pending.remove(&id), st.connected)
        };
        let Some(p) = pending else { return };
        // The client fails its waiters before it reports the disconnect, so
        // the handler may not have heard of it yet; only the error's text
        // says so, as `ClientChannel` errors are strings.
        static DISCONNECTED: LazyLock<String> = LazyLock::new(|| CaError::Disconnected.to_string());
        let (value, eca) = match result {
            Ok(value) => (value, Eca::Normal),
            Err(e) if !connected || e == *DISCONNECTED => (None, Eca::Disconn),
            Err(_) if p.evt == PvEventType::Get => (None, Eca::GetFail),
            Err(_) => (None, Eca::PutFail),
        };
        self.deliver(p, value.as_ref(), eca);
    }

    fn deliver(&self, p: Pending, value: Option<&Snapshot>, eca: Eca) {
        let count = event_count(p.count, value);
        self.callback(
            Some(eca),
            |_| true,
            || {
                (p.done)(PvEvent {
                    evt: p.evt,
                    ty: p.ty,
                    count,
                    value,
                    status: PvStat::Ok,
                    severity: eca.severity(),
                    message: Some(eca.text()),
                })
            },
        );
    }

    /// Mark the channel gone: no callback runs after this, and the accepted
    /// requests are dropped uncalled (`ECA_CHANDESTROY`).
    fn close(&self) {
        let pending = {
            let mut st = self.state.lock();
            st.closed = true;
            st.monitor = 0;
            std::mem::take(&mut st.pending)
        };
        drop(pending);
    }
}

/// `args.count`: what was asked for, or for a request of 0 elements (the
/// native count) what came back.
fn event_count(count: u32, value: Option<&Snapshot>) -> u32 {
    match (count, value) {
        (0, Some(s)) => s.value.count(),
        _ => count,
    }
}

/// Aborts its task when dropped.
struct AbortOnDrop(Option<TaskHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

/// A network var's request to the [`Dispatcher`], under the var's key.
type Request = (u64, Cmd);

enum Cmd {
    /// `ca_create_channel`.
    Create {
        name: Arc<str>,
        shared: Arc<Shared>,
        conn: ConnFn,
    },
    Chan(ChanCmd),
    /// `ca_clear_channel`.
    Clear,
}

/// A request to a created channel.
enum ChanCmd {
    Get {
        id: u64,
        dbr: u16,
        count: u32,
    },
    /// `id` is the pending callback; `None` for `ca_array_put`.
    Put {
        id: Option<u64>,
        value: EpicsValue,
    },
    MonitorOn(Monitor),
    MonitorOff,
}

/// Why [`PvVar::create`] fails: the only way it can.
pub(crate) const NO_CONTEXT: &str = "the IOC's CA client context is gone";

/// `pvVar`: one channel.
pub struct PvVar {
    name: Arc<str>,
    shared: Arc<Shared>,
    link: Link,
}

/// Where a var's requests run.
enum Link {
    InProcess(InProcess),
    Network(Network),
}

/// A network var's way to its system's [`Dispatcher`].
struct Network {
    key: u64,
    requests: mpsc::UnboundedSender<Request>,
}

impl Network {
    fn send(&self, cmd: Cmd) {
        let _ = self.requests.send((self.key, cmd));
    }
}

/// An in-process channel, whose requests run on the caller's thread.
struct InProcess {
    chan: InProcessChannel,
    ctx: CaClientContext,
    events: InProcessEvents,
    monitor: Mutex<Option<InProcessSubscription>>,
}

impl InProcess {
    /// `dbChannelIO::subscribe` for monitor `m`, kept unless a monitor-off
    /// came in meanwhile.
    fn monitor_on(&self, shared: &Arc<Shared>, m: Monitor) {
        let mut slot = self.monitor.lock();
        let mask = (EventMask::VALUE | EventMask::ALARM).bits();
        let (monid, ty, count) = (m.monid, m.ty, m.count);
        let handler = shared.clone();
        let sub = self
            .chan
            .subscribe(&self.events, type_to_ca(ty), count, mask, move |snapshot| {
                m.deliver(&handler, snapshot)
            });
        match sub {
            Ok(sub) if shared.state.lock().monitor == monid => drop(slot.replace(sub)),
            Ok(_) => {}
            Err(e) => subscribe_refused(shared, monid, &e),
        }
    }

    /// The put callback's completion: here, when the record's processing
    /// has already ended, as `dbNotify`'s `callDone` runs it before
    /// `dbProcessNotify` returns; else on the reactor once it does.
    fn complete_put(&self, shared: &Arc<Shared>, id: u64, mut done: PutCompletion) {
        let mut cx = Context::from_waker(Waker::noop());
        match done.as_mut().poll(&mut cx) {
            Poll::Ready(result) => shared.complete(id, result.map(|()| None)),
            Poll::Pending => {
                let shared = shared.clone();
                drop(
                    self.ctx
                        .spawn(async move { shared.complete(id, done.await.map(|()| None)) }),
                );
            }
        }
    }
}

impl PvVar {
    /// `pvVarCreate`: start connecting `name`; `conn` runs with `true` on
    /// each connect and `false` on each disconnect.
    ///
    /// An in-process channel is connected when this returns and stays so,
    /// as `dbChannelIO::initiateConnect` connects one inside
    /// `ca_create_channel`; `conn` never runs for it. C's handler ran there
    /// with the var already stored, so a caller that needs that call makes
    /// it once it has stored the var, when [`Self::in_process`] says so.
    pub fn create(
        sys: &PvSystem,
        name: &str,
        conn: impl Fn(bool) + Send + Sync + 'static,
    ) -> Result<PvVar, PvStat> {
        let name: Arc<str> = Arc::from(name);
        let shared = Arc::new(Shared::new());
        if let Some(chan) = sys.ctx.create_in_process(&name) {
            shared.state.lock().connected = true;
            let link = Link::InProcess(InProcess {
                chan,
                ctx: sys.ctx.clone(),
                events: sys.events.clone(),
                monitor: Mutex::new(None),
            });
            return Ok(PvVar { name, shared, link });
        }
        let Some(requests) = sys.requests() else {
            errlog_sev_printf(
                ErrlogSevEnum::Major,
                &format!(
                    "ca_create_channel(name, pvCaConnectionHandler, var, \
                     CA_PRIORITY_DEFAULT, &var->chid): {NO_CONTEXT}"
                ),
            );
            return Err(PvStat::Error);
        };
        static NEXT_KEY: AtomicU64 = AtomicU64::new(1);
        let net = Network {
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            requests,
        };
        net.send(Cmd::Create {
            name: name.clone(),
            shared: shared.clone(),
            conn: Arc::new(conn),
        });
        let link = Link::Network(net);
        Ok(PvVar { name, shared, link })
    }

    /// Whether this is an in-process channel: connected inside
    /// [`Self::create`] and never changing after.
    pub fn in_process(&self) -> bool {
        matches!(self.link, Link::InProcess(_))
    }

    /// `pvVarDestroy`: no callback of this pv runs after it returns, and a
    /// later request fails. It waits for a running callback, so the caller
    /// holds no seq lock across it, as C unlocks `sp->lock` around it.
    pub fn destroy(&self) -> PvStat {
        let _cb = self.shared.callback.lock();
        self.shared.close();
        // `ca_clear_channel`: the channel's monitor goes with it.
        match &self.link {
            Link::InProcess(l) => drop(l.monitor.lock().take()),
            Link::Network(net) => net.send(Cmd::Clear),
        }
        PvStat::Ok
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `pvVarGetCount`: the server's element count, 0 before the first
    /// connect.
    pub fn count(&self) -> u32 {
        self.channel().map_or(0, |(count, _)| count)
    }

    /// `ca_element_count` and the channel's read and write access, `None`
    /// before the first connect.
    fn channel(&self) -> Option<(u32, (bool, bool))> {
        match &self.link {
            Link::InProcess(l) => Some((l.chan.element_count(), (true, true))),
            Link::Network(_) => {
                let chan = self.shared.chan.get()?;
                Some((chan.element_count().unwrap_or(0), chan.access()))
            }
        }
    }

    /// `pvVarGetMess`.
    pub fn message(&self) -> Option<Arc<str>> {
        self.shared.state.lock().message.clone()
    }

    /// `pvMonIsDefined`.
    pub(crate) fn monitor_is_on(&self) -> bool {
        self.shared.state.lock().monitor != 0
    }

    /// A network var whose requests go to `requests` under `key`.
    #[cfg(test)]
    fn queued(name: &str, key: u64, requests: mpsc::UnboundedSender<Request>) -> PvVar {
        PvVar {
            name: Arc::from(name),
            shared: Arc::new(Shared::new()),
            link: Link::Network(Network { key, requests }),
        }
    }

    /// A var whose channel never connects and whose requests go nowhere.
    #[cfg(test)]
    pub(crate) fn unconnected(name: &str) -> PvVar {
        PvVar::queued(name, 0, mpsc::unbounded_channel().0)
    }

    /// [`Self::unconnected`] on `chan`, whose element count it reports.
    #[cfg(test)]
    pub(crate) fn on_channel(name: &str, chan: Arc<dyn ClientChannel>) -> PvVar {
        let var = PvVar::unconnected(name);
        let _ = var.shared.chan.set(chan);
        var
    }

    /// The checks `nciu::read` and `nciu::write` make before queueing.
    fn admit(&self, write: bool, count: u32) -> Result<(), Eca> {
        let connected = {
            let st = self.shared.state.lock();
            if st.closed {
                return Err(Eca::BadChid);
            }
            st.connected
        };
        let Some((elements, (read, written))) = self.channel().filter(|_| connected) else {
            return Err(Eca::Disconn);
        };
        if write && !written {
            return Err(Eca::NoWtAccess);
        }
        if !write && !read {
            return Err(Eca::NoRdAccess);
        }
        if count > elements {
            return Err(Eca::BadCount);
        }
        Ok(())
    }

    /// Queue a get or put callback as libca's `eventq` holds it, unless a
    /// destroy came in after [`Self::admit`].
    fn accept(&self, evt: PvEventType, ty: PvType, count: u32, done: Done) -> Result<u64, Eca> {
        let mut st = self.shared.state.lock();
        if st.closed {
            return Err(Eca::BadChid);
        }
        let id = st.next_id;
        st.next_id += 1;
        st.pending.insert(
            id,
            Pending {
                evt,
                ty,
                count,
                done,
            },
        );
        Ok(id)
    }

    /// `pvVarGetCallback`.
    pub fn get_callback(
        &self,
        ty: PvType,
        count: u32,
        done: impl FnOnce(PvEvent<'_>) + Send + 'static,
    ) -> PvStat {
        assert!(ty.is_valid(), "pvVarGetCallback: {ty:?}");
        let id = match self
            .admit(false, count)
            .and_then(|()| self.accept(PvEventType::Get, ty, count, Box::new(done)))
        {
            Ok(id) => id,
            Err(eca) => {
                return self.shared.refuse(
                    eca,
                    "ca_array_get_callback( typeToCA(type), count, var->chid, pvCaGetHandler, arg)",
                );
            }
        };
        let dbr = type_to_ca(ty);
        match &self.link {
            // `callReadNotify`: the callback runs before this returns.
            Link::InProcess(l) => self.shared.complete(id, l.chan.get(dbr, count).map(Some)),
            Link::Network(net) => net.send(Cmd::Chan(ChanCmd::Get { id, dbr, count })),
        }
        PvStat::Ok
    }

    /// `pvVarPutNoBlock`.
    pub fn put_no_block(&self, ty: PvType, count: u32, value: EpicsValue) -> PvStat {
        assert!(ty.is_simple(), "pvVarPutNoBlock: {ty:?}");
        if let Err(eca) = self.admit(true, count) {
            return self.shared.refuse(eca, PUT_EXPR);
        }
        let value = put_value(value, ty, count);
        match &self.link {
            // `dbChannelIO::write` throws a refused `dbChannel_put`, which
            // `ca_array_put` answers with ECA_PUTFAIL.
            Link::InProcess(l) => {
                if l.chan.put(value).is_err() {
                    return self.shared.refuse(Eca::PutFail, PUT_EXPR);
                }
            }
            Link::Network(net) => net.send(Cmd::Chan(ChanCmd::Put { id: None, value })),
        }
        PvStat::Ok
    }

    /// `pvVarPutCallback`.
    pub fn put_callback(
        &self,
        ty: PvType,
        count: u32,
        value: EpicsValue,
        done: impl FnOnce(PvEvent<'_>) + Send + 'static,
    ) -> PvStat {
        assert!(ty.is_simple(), "pvVarPutCallback: {ty:?}");
        let id = match self
            .admit(true, count)
            .and_then(|()| self.accept(PvEventType::Put, ty, count, Box::new(done)))
        {
            Ok(id) => id,
            Err(eca) => return self.shared.refuse(eca, PUT_CALLBACK_EXPR),
        };
        let value = put_value(value, ty, count);
        match &self.link {
            Link::InProcess(l) => match l.chan.put_callback(value) {
                Ok(done) => l.complete_put(&self.shared, id, done),
                // `ca_array_put_callback` answers a throw at once and never
                // calls the callback.
                Err(e) => {
                    drop(self.shared.state.lock().pending.remove(&id));
                    let eca = match e {
                        CaError::Timeout => Eca::Timeout,
                        _ => Eca::PutFail,
                    };
                    return self.shared.refuse(eca, PUT_CALLBACK_EXPR);
                }
            },
            Link::Network(net) => net.send(Cmd::Chan(ChanCmd::Put {
                id: Some(id),
                value,
            })),
        }
        PvStat::Ok
    }

    /// `pvVarMonitorOn`: a no-op while a monitor is on. Allowed while
    /// disconnected; the first event comes after the connect. Each event
    /// reaches `event` through `gate`.
    pub fn monitor_on(
        &self,
        gate: &Arc<EventGate>,
        ty: PvType,
        count: u32,
        event: impl Fn(PvEvent<'_>) + Send + Sync + 'static,
    ) -> PvStat {
        assert!(ty.is_valid(), "pvVarMonitorOn: {ty:?}");
        let monid = {
            let mut st = self.shared.state.lock();
            if st.closed {
                drop(st);
                return self.shared.refuse(Eca::BadChid, MONITOR_EXPR);
            }
            if st.monitor != 0 {
                return PvStat::Ok;
            }
            st.monitor = st.next_id;
            st.next_id += 1;
            st.monitor
        };
        let m = Monitor {
            monid,
            ty,
            count,
            gate: gate.clone(),
            event: Arc::new(event),
        };
        match &self.link {
            Link::InProcess(l) => l.monitor_on(&self.shared, m),
            Link::Network(net) => net.send(Cmd::Chan(ChanCmd::MonitorOn(m))),
        }
        PvStat::Ok
    }

    /// `pvVarMonitorOff`: once this returns no event of the monitor runs, so
    /// it must not be called holding a lock the monitor callback takes.
    pub fn monitor_off(&self) -> PvStat {
        let _cb = self.shared.callback.lock();
        let was_on = std::mem::take(&mut self.shared.state.lock().monitor) != 0;
        if was_on {
            match &self.link {
                Link::InProcess(l) => drop(l.monitor.lock().take()),
                Link::Network(net) => net.send(Cmd::Chan(ChanCmd::MonitorOff)),
            }
        }
        PvStat::Ok
    }
}

impl Drop for PvVar {
    /// Closes the channel without waiting for a callback in flight, so a
    /// var may be dropped from anywhere; [`PvVar::destroy`] waits.
    fn drop(&mut self) {
        self.shared.close();
        if let Link::Network(net) = &self.link {
            net.send(Cmd::Clear);
        }
    }
}

const MONITOR_EXPR: &str = "ca_create_subscription(typeToCA(type), count, var->chid, \
     DBE_VALUE | DBE_ALARM, pvCaMonitorHandler, arg, &var->monid)";
const PUT_EXPR: &str = "ca_array_put(typeToCA(type), count, var->chid, value)";
const PUT_CALLBACK_EXPR: &str =
    "ca_array_put_callback( typeToCA(type), count, var->chid, value, pvCaPutHandler, arg)";

/// A subscription the server refused: `monid` stays NULL, so the next
/// monitor-on retries.
fn subscribe_refused(shared: &Shared, monid: u64, e: &str) {
    let mut st = shared.state.lock();
    if st.monitor == monid {
        st.monitor = 0;
    }
    drop(st);
    errlog_sev_printf(ErrlogSevEnum::Minor, &format!("{MONITOR_EXPR}: {e}"));
}

/// The `count` elements `ca_array_put(typeToCA(type), ...)` sends of
/// `value`: the variable's bits as `type`, so an unsigned short 0xffff goes
/// as DBR_SHORT -1, and a char as the unsigned byte `dbChannel_put` stores.
fn put_value(mut value: EpicsValue, ty: PvType, count: u32) -> EpicsValue {
    value.truncate(count as usize);
    let dbr = DbFieldType::from_u16(type_to_ca(ty)).expect("a simple pv type");
    value.into_type(dbr.wire_carrier())
}

/// Poll `fut` once: its output if it finished, else `None` with the
/// request it makes on its first poll already sent.
async fn poll_once<F: Future + Unpin>(fut: &mut F) -> Option<F::Output> {
    std::future::poll_fn(|cx| {
        Poll::Ready(match Pin::new(&mut *fut).poll(cx) {
            Poll::Ready(out) => Some(out),
            Poll::Pending => None,
        })
    })
    .await
}

/// A system's network channels and their requests, issued one at a time in
/// the order they were made, whatever the channel, as libca queues each on
/// its circuit when it is called: a get made after a put to the same
/// server reads what the put did.
struct Dispatcher {
    ctx: CaClientContext,
    chans: HashMap<u64, Served>,
}

/// A created network channel and the tasks delivering its events.
struct Served {
    chan: Arc<dyn ClientChannel>,
    shared: Arc<Shared>,
    _connection: AbortOnDrop,
    monitor: Option<AbortOnDrop>,
}

impl Dispatcher {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Request>) {
        while let Some((key, cmd)) = rx.recv().await {
            self.handle(key, cmd).await;
        }
    }

    /// Issue one request; it is on its way when this returns.
    async fn handle(&mut self, key: u64, cmd: Cmd) {
        match cmd {
            Cmd::Create { name, shared, conn } => {
                // A search nobody answers: the channel never connects, so
                // every request is refused as disconnected; only monitors
                // queue, and are dropped here.
                if let Some(chan) = self.ctx.create_network(&name).await {
                    self.attach(key, chan, shared, conn);
                }
            }
            Cmd::Chan(cmd) => {
                if let Some(served) = self.chans.get_mut(&key) {
                    served.serve(&self.ctx, cmd).await;
                }
            }
            Cmd::Clear => drop(self.chans.remove(&key)),
        }
    }

    fn attach(
        &mut self,
        key: u64,
        chan: Arc<dyn ClientChannel>,
        shared: Arc<Shared>,
        conn: ConnFn,
    ) {
        let _ = shared.chan.set(chan.clone());
        // Held here, not by the connection task: once connected, a local
        // channel's next connection change never comes and registers no waker,
        // and the exec backend drops such a task with everything it owns.
        let connection = connection_task(chan.clone(), shared.clone(), conn);
        let served = Served {
            _connection: AbortOnDrop(self.ctx.spawn(connection)),
            chan,
            shared,
            monitor: None,
        };
        self.chans.insert(key, served);
    }
}

impl Served {
    async fn serve(&mut self, ctx: &CaClientContext, cmd: ChanCmd) {
        let (chan, shared) = (&self.chan, &self.shared);
        match cmd {
            ChanCmd::Get { id, dbr, count } => {
                if !shared.is_pending(id) {
                    return;
                }
                // Sent on the first poll; the answer is awaited off the
                // dispatcher, so it holds up nothing.
                let chan = chan.clone();
                let mut got = Box::pin(async move { chan.get(dbr, count).await });
                match poll_once(&mut got).await {
                    Some(result) => shared.complete(id, result.map(Some)),
                    None => {
                        let shared = shared.clone();
                        drop(ctx.spawn(async move { shared.complete(id, got.await.map(Some)) }));
                    }
                }
            }
            ChanCmd::Put { id, value } => {
                if id.is_some_and(|id| !shared.is_pending(id)) {
                    return;
                }
                match (chan.put(value, id.is_some()).await, id) {
                    (Ok(Some(done)), Some(id)) => {
                        let shared = shared.clone();
                        drop(
                            ctx.spawn(
                                async move { shared.complete(id, done.await.map(|()| None)) },
                            ),
                        );
                    }
                    (Ok(_), Some(id)) => shared.complete(id, Ok(None)),
                    (Ok(_), None) => {}
                    (Err(e), Some(id)) => shared.complete(id, Err(e)),
                    (Err(e), None) => {
                        // A put the disconnect overtook is lost unreported, as
                        // one queued on a dead circuit is.
                        if shared.state.lock().connected {
                            errlog_sev_printf(ErrlogSevEnum::Minor, &format!("{PUT_EXPR}: {e}"));
                        }
                    }
                }
            }
            ChanCmd::MonitorOn(m) => {
                if shared.state.lock().monitor != m.monid {
                    return;
                }
                let mask = (EventMask::VALUE | EventMask::ALARM).bits();
                match chan.subscribe(type_to_ca(m.ty), m.count, mask).await {
                    Ok(sub) => {
                        let task = monitor_task(sub, shared.clone(), m);
                        drop(self.monitor.replace(AbortOnDrop(ctx.spawn(task))));
                    }
                    Err(e) => subscribe_refused(shared, m.monid, &e),
                }
            }
            ChanCmd::MonitorOff => drop(self.monitor.take()),
        }
    }
}

/// `pvCaConnectionHandler`, for every connection change.
async fn connection_task(chan: Arc<dyn ClientChannel>, shared: Arc<Shared>, conn: ConnFn) {
    while let Some(state) = chan.next_connection().await {
        shared.connection_change(state == Connection::Connected, &*conn);
    }
}

/// `pvCaMonitorHandler`, for each event of monitor `m` while it is the
/// channel's monitor.
async fn monitor_task(mut sub: Box<dyn ClientMonitor>, shared: Arc<Shared>, m: Monitor) {
    while let Some(snapshot) = sub.next().await {
        m.deliver(&shared, snapshot);
    }
}

/// `pvTimeGetCurrentDouble`: seconds since the EPICS epoch.
pub fn pv_time_get_current_double() -> f64 {
    let now = get_current();
    now.unix_secs().saturating_sub(EPICS_EPOCH_UNIX_SECS) as f64
        + f64::from(now.subsec_nanos()) / 1e9
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Duration;

    use super::*;

    /// A var whose dispatcher is the returned receiver, never connected.
    fn var() -> (PvVar, mpsc::UnboundedReceiver<Request>) {
        let (requests, rx) = mpsc::unbounded_channel();
        (PvVar::queued("T", 1, requests), rx)
    }

    type Log = Arc<Mutex<Vec<String>>>;

    fn record(log: &Log, tag: &'static str) -> Done {
        let log = log.clone();
        Box::new(move |ev: PvEvent<'_>| {
            assert_eq!(ev.status, PvStat::Ok);
            log.lock()
                .push(format!("{tag} {:?} {}", ev.severity, ev.message.unwrap()));
        })
    }

    fn accept(var: &PvVar, evt: PvEventType, log: &Log, tag: &'static str) -> u64 {
        var.accept(evt, PvType::TimeDouble, 1, record(log, tag))
            .unwrap_or_else(|eca| panic!("{eca:?}"))
    }

    #[test]
    fn a_disconnect_completes_accepted_requests_before_the_handler() {
        let (var, _rx) = var();
        let log = Log::default();
        accept(&var, PvEventType::Get, &log, "get");
        accept(&var, PvEventType::Put, &log, "put");
        let conn_log = log.clone();
        var.shared
            .connection_change(false, &move |up| conn_log.lock().push(format!("conn {up}")));
        assert_eq!(
            *log.lock(),
            [
                "get Minor Virtual circuit disconnect",
                "put Minor Virtual circuit disconnect",
                "conn false",
            ]
        );
        assert_eq!(var.message().as_deref(), Some("Virtual circuit disconnect"));
    }

    #[test]
    fn a_completion_after_the_disconnect_runs_nothing() {
        let (var, _rx) = var();
        let log = Log::default();
        let id = accept(&var, PvEventType::Get, &log, "get");
        var.shared.connection_change(false, &|_| {});
        var.shared.complete(id, Ok(None));
        assert_eq!(*log.lock(), ["get Minor Virtual circuit disconnect"]);
    }

    #[test]
    fn nothing_runs_once_closed() {
        let (var, _rx) = var();
        let log = Log::default();
        let id = accept(&var, PvEventType::Put, &log, "put");
        let ran = Arc::new(AtomicBool::new(false));
        var.shared.close();
        assert!(
            var.shared.state.lock().pending.is_empty(),
            "dropped uncalled"
        );
        var.shared.complete(id, Ok(None));
        var.shared
            .connection_change(true, &|_| ran.store(true, Ordering::SeqCst));
        var.shared.callback(
            Some(Eca::Normal),
            |_| true,
            || ran.store(true, Ordering::SeqCst),
        );
        assert!(log.lock().is_empty());
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[test]
    fn a_stale_monitor_delivers_nothing() {
        let (var, _rx) = var();
        var.shared.state.lock().monitor = 7;
        let ran = AtomicBool::new(false);
        var.shared.callback(
            Some(Eca::Normal),
            |st| st.monitor == 3,
            || ran.store(true, Ordering::SeqCst),
        );
        assert!(!ran.load(Ordering::SeqCst));
        var.shared.callback(
            Some(Eca::Normal),
            |st| st.monitor == 7,
            || ran.store(true, Ordering::SeqCst),
        );
        assert!(ran.load(Ordering::SeqCst));
    }

    /// `call` made on another thread while a callback of `var` runs does
    /// not return until the callback has.
    fn waits_for_a_running_callback(var: &PvVar, call: impl FnOnce(&PvVar) -> PvStat + Send) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(move || {
                var.shared.callback(
                    None,
                    |_| true,
                    || {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    },
                )
            });
            entered_rx.recv().unwrap();
            s.spawn(move || done_tx.send(call(var)).unwrap());
            assert!(
                done_rx
                    .recv_timeout(std::time::Duration::from_millis(200))
                    .is_err(),
                "returned while the callback ran"
            );
            release_tx.send(()).unwrap();
            assert_eq!(done_rx.recv().unwrap(), PvStat::Ok);
        });
    }

    #[test]
    fn destroy_and_monitor_off_wait_for_a_callback_in_flight() {
        let (var, _rx) = var();
        assert_eq!(
            var.monitor_on(&Arc::default(), PvType::TimeDouble, 1, |_| {}),
            PvStat::Ok
        );
        waits_for_a_running_callback(&var, PvVar::monitor_off);
        assert!(!var.monitor_is_on());
        waits_for_a_running_callback(&var, PvVar::destroy);
        assert!(var.shared.state.lock().closed);
    }

    fn admitted(gate: &EventGate) -> Arc<AtomicBool> {
        let landed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&landed);
        gate.admit(move || flag.store(true, Ordering::SeqCst));
        landed
    }

    /// A block by a state set that is not running lets nothing through
    /// while another runs, and leaves it not running.
    #[test]
    fn a_block_of_a_state_set_not_running_keeps_events_held() {
        let gate = EventGate::default();
        gate.run(0);
        let landed = admitted(&gate);
        gate.block(1, || ());
        assert!(!landed.load(Ordering::SeqCst));
        gate.stop(0);
        assert!(landed.load(Ordering::SeqCst));
        gate.block(1, || ());
        assert!(admitted(&gate).load(Ordering::SeqCst));
    }

    /// A running state set's block lets held events through during the
    /// wait, and it runs again after.
    #[test]
    fn a_block_lets_events_through_and_runs_again_after() {
        let gate = EventGate::default();
        gate.run(0);
        let landed = admitted(&gate);
        assert!(gate.block(0, || landed.load(Ordering::SeqCst)));
        assert!(!admitted(&gate).load(Ordering::SeqCst));
    }

    /// A call's return lets through what came before it began, not what
    /// came during it, though that is in before the return: a put's own
    /// monitor waits for the next delivery point.
    #[test]
    fn a_call_delivers_only_what_came_before_it_began() {
        let gate = EventGate::default();
        gate.run(0);
        let before = admitted(&gate);
        let during = gate.call(0, || admitted(&gate));
        assert!(before.load(Ordering::SeqCst));
        assert!(!during.load(Ordering::SeqCst));
        gate.call(0, || ());
        assert!(during.load(Ordering::SeqCst));
    }

    /// A call's return lets nothing through while another state set runs,
    /// and leaves its own running.
    #[test]
    fn a_call_while_another_state_set_runs_keeps_events_held() {
        let gate = EventGate::default();
        gate.run(0);
        gate.run(1);
        let landed = admitted(&gate);
        gate.call(0, || ());
        assert!(!landed.load(Ordering::SeqCst));
        gate.stop(1);
        assert!(!landed.load(Ordering::SeqCst));
        gate.call(0, || ());
        assert!(landed.load(Ordering::SeqCst));
        assert!(!admitted(&gate).load(Ordering::SeqCst));
    }

    /// A pass lets through what the last call's return did, not what came
    /// during that call, though a block inside the call cleared its cut;
    /// with no call since a block, it takes everything.
    #[test]
    fn a_pass_takes_the_cut_of_the_last_call_since_the_last_block() {
        let gate = EventGate::default();
        gate.run(0);
        let during = gate.call(0, || admitted(&gate));
        gate.pass(0);
        assert!(!during.load(Ordering::SeqCst));
        let after_its_block = gate.call(0, || {
            gate.block(0, || ());
            admitted(&gate)
        });
        assert!(during.load(Ordering::SeqCst));
        gate.pass(0);
        assert!(!after_its_block.load(Ordering::SeqCst));
        gate.block(0, || ());
        assert!(after_its_block.load(Ordering::SeqCst));
        let after = admitted(&gate);
        gate.pass(0);
        assert!(after.load(Ordering::SeqCst));
    }

    /// ao records `names` at 0, on a reactor that outlives the test.
    fn served(names: &'static [&'static str]) -> PvDatabase {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            epics_base_rs::runtime::task::test_block_on(async move {
                let db = PvDatabase::new();
                for name in names {
                    db.add_record(
                        name,
                        Box::new(epics_base_rs::server::records::ao::AoRecord::new(0.0)),
                    )
                    .await
                    .unwrap();
                }
                db.mark_pini_done();
                tx.send(db).unwrap();
                std::future::pending::<()>().await
            })
        });
        rx.recv().unwrap()
    }

    /// A post made before a state set's wait returns lands before it runs
    /// again, though `CAC-event` hands it over after the wakeup: here the
    /// callback of the post that wakes the state set holds the pass up
    /// 100 ms, as a preemption would.
    #[test]
    fn a_block_takes_in_what_was_posted_before_it_returned() {
        let db = served(&["g:a", "g:b"]);
        let sys = PvSystem::create(&db);
        let gate = sys.gate();
        let posted = Arc::new(AtomicBool::new(false));
        let (wake, woken) = std::sync::mpsc::channel();
        let (wake, first, all_posted) = (Mutex::new(wake), AtomicBool::new(true), posted.clone());
        let _a = sys
            .ctx
            .create_in_process("g:a")
            .unwrap()
            .subscribe(
                &sys.events,
                type_to_ca(PvType::Double),
                1,
                EventMask::VALUE.bits(),
                move |_| {
                    if !first.swap(false, Ordering::SeqCst) {
                        while !all_posted.load(Ordering::SeqCst) {
                            std::thread::yield_now();
                        }
                        wake.lock().send(()).unwrap();
                        std::thread::sleep(Duration::from_millis(100));
                    }
                },
            )
            .unwrap();
        let b = PvVar::create(&sys, "g:b", |_| {}).unwrap();
        let landed = Arc::new(AtomicU32::new(0));
        let count = landed.clone();
        b.monitor_on(&gate, PvType::Double, 1, move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        });
        sys.deliver();
        assert_eq!(landed.load(Ordering::SeqCst), 1, "the first event");
        gate.run(0);
        let poster = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let other = PvSystem::create(&db);
            for name in ["g:a", "g:b"] {
                let pv = PvVar::create(&other, name, |_| {}).unwrap();
                let put = pv.put_no_block(PvType::Double, 1, EpicsValue::Double(1.0));
                assert_eq!(put, PvStat::Ok);
            }
            posted.store(true, Ordering::SeqCst);
        });
        gate.block(0, || woken.recv().unwrap());
        assert_eq!(landed.load(Ordering::SeqCst), 2);
        poster.join().unwrap();
    }

    /// A channel whose server refuses every subscription and put.
    struct Refusing;

    #[async_trait::async_trait]
    impl ClientChannel for Refusing {
        fn native_type(&self) -> Option<DbFieldType> {
            Some(DbFieldType::Double)
        }

        fn element_count(&self) -> Option<u32> {
            Some(1)
        }

        fn access(&self) -> (bool, bool) {
            (true, true)
        }

        async fn next_connection(&self) -> Option<Connection> {
            None
        }

        async fn get(&self, _: u16, _: u32) -> Result<Snapshot, String> {
            unreachable!()
        }

        async fn subscribe(
            &self,
            _: u16,
            _: u32,
            _: u16,
        ) -> Result<Box<dyn ClientMonitor>, String> {
            Err("refused".into())
        }

        async fn put(
            &self,
            _: EpicsValue,
            _: bool,
        ) -> Result<Option<epics_base_rs::server::database::PutCompletion>, String> {
            Err("refused".into())
        }
    }

    /// The errlog lines containing `needle` that `f` logs.
    fn logged(needle: &str, f: impl FnOnce()) -> Vec<String> {
        use epics_base_rs::runtime::log::{
            errlog_add_listener, errlog_flush, errlog_remove_listener,
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let id = {
            let (seen, needle) = (seen.clone(), needle.to_owned());
            errlog_add_listener(move |m| {
                if m.contains(&needle) {
                    seen.lock().push(m.to_owned());
                }
            })
        };
        f();
        errlog_flush();
        errlog_remove_listener(id);
        std::mem::take(&mut *seen.lock())
    }

    /// Run what `var` queued, then `cmds`, against a [`Refusing`] channel,
    /// connected.
    fn serve_refused(
        var: &PvVar,
        mut rx: mpsc::UnboundedReceiver<Request>,
        cmds: Vec<ChanCmd>,
    ) -> Vec<String> {
        var.shared.state.lock().connected = true;
        let Link::Network(net) = &var.link else {
            unreachable!("var() is a network var")
        };
        for cmd in cmds {
            net.send(Cmd::Chan(cmd));
        }
        let db = PvDatabase::new();
        let mut d = Dispatcher {
            ctx: db.ca_client(),
            chans: HashMap::new(),
        };
        d.attach(
            net.key,
            Arc::new(Refusing),
            var.shared.clone(),
            Arc::new(|_| {}),
        );
        logged("refused", || {
            epics_base_rs::runtime::task::test_block_on(async {
                while let Ok((key, cmd)) = rx.try_recv() {
                    d.handle(key, cmd).await;
                }
            })
        })
    }

    #[test]
    fn a_failed_subscribe_leaves_the_monitor_off_for_the_next_monitor_on() {
        let (var, rx) = var();
        assert_eq!(
            var.monitor_on(&Arc::default(), PvType::TimeDouble, 1, |_| {}),
            PvStat::Ok
        );
        assert_eq!(
            serve_refused(&var, rx, Vec::new()),
            [format!("sevr=minor {MONITOR_EXPR}: refused")]
        );
        assert!(!var.monitor_is_on());
        assert_eq!(
            var.monitor_on(&Arc::default(), PvType::TimeDouble, 1, |_| {}),
            PvStat::Ok
        );
        assert!(var.monitor_is_on(), "retried, not a no-op");
    }

    #[test]
    fn a_refused_put_logs_the_libca_call() {
        let (var, rx) = var();
        let put = ChanCmd::Put {
            id: None,
            value: EpicsValue::Double(1.0),
        };
        assert_eq!(
            serve_refused(&var, rx, vec![put]),
            ["sevr=minor ca_array_put(typeToCA(type), count, var->chid, value): refused"]
        );
    }

    /// A channel that logs each get and put as it sends it, a put on its
    /// second poll.
    struct Recording {
        name: &'static str,
        log: Log,
    }

    #[async_trait::async_trait]
    impl ClientChannel for Recording {
        fn native_type(&self) -> Option<DbFieldType> {
            Some(DbFieldType::Double)
        }

        fn element_count(&self) -> Option<u32> {
            Some(1)
        }

        fn access(&self) -> (bool, bool) {
            (true, true)
        }

        async fn next_connection(&self) -> Option<Connection> {
            None
        }

        async fn get(&self, _: u16, _: u32) -> Result<Snapshot, String> {
            self.log.lock().push(format!("get {}", self.name));
            std::future::pending().await
        }

        async fn subscribe(
            &self,
            _: u16,
            _: u32,
            _: u16,
        ) -> Result<Box<dyn ClientMonitor>, String> {
            unreachable!()
        }

        async fn put(
            &self,
            _: EpicsValue,
            _: bool,
        ) -> Result<Option<epics_base_rs::server::database::PutCompletion>, String> {
            let mut polled = false;
            std::future::poll_fn(|cx| {
                if std::mem::replace(&mut polled, true) {
                    return Poll::Ready(());
                }
                cx.waker().wake_by_ref();
                Poll::Pending
            })
            .await;
            self.log.lock().push(format!("put {}", self.name));
            Ok(None)
        }
    }

    /// pvGetSync's `pvPut(p, ASYNC)` then `pvGet(x, SYNC)`: the get reads
    /// the record the put processed only if it leaves after the put.
    #[test]
    fn a_request_leaves_after_those_made_before_it_on_other_channels() {
        let db = PvDatabase::new();
        let mut d = Dispatcher {
            ctx: db.ca_client(),
            chans: HashMap::new(),
        };
        let (requests, mut rx) = mpsc::unbounded_channel();
        let log = Log::default();
        let [p, x] = [(1, "p"), (2, "x")].map(|(key, name)| {
            let var = PvVar::queued(name, key, requests.clone());
            var.shared.state.lock().connected = true;
            let chan = Arc::new(Recording {
                name,
                log: log.clone(),
            });
            d.attach(key, chan, var.shared.clone(), Arc::new(|_| {}));
            var
        });
        let put = p.put_callback(PvType::Double, 1, EpicsValue::Double(1.0), |_| {});
        assert_eq!(put, PvStat::Ok);
        assert_eq!(x.get_callback(PvType::Double, 1, |_| {}), PvStat::Ok);
        epics_base_rs::runtime::task::test_block_on(async {
            while let Ok((key, cmd)) = rx.try_recv() {
                d.handle(key, cmd).await;
            }
        });
        assert_eq!(*log.lock(), ["put p", "get x"]);
    }

    #[test]
    fn a_create_failure_logs_the_libca_call() {
        let sys = PvSystem::create(&PvDatabase::new());
        let lines = logged("ca_create_channel", || {
            assert!(PvVar::create(&sys, "SEQ:X", |_| {}).is_err());
        });
        assert_eq!(
            lines,
            [format!(
                "sevr=major ca_create_channel(name, pvCaConnectionHandler, var, \
                 CA_PRIORITY_DEFAULT, &var->chid): {NO_CONTEXT}"
            )]
        );
    }

    #[test]
    fn a_failed_request_reports_its_own_direction() {
        let (var, _rx) = var();
        var.shared.state.lock().connected = true;
        let log = Log::default();
        let get = accept(&var, PvEventType::Get, &log, "get");
        let put = accept(&var, PvEventType::Put, &log, "put");
        var.shared.complete(get, Err("x".into()));
        var.shared.complete(put, Err("x".into()));
        assert_eq!(
            *log.lock(),
            [
                "get Minor Channel read request failed",
                "put Minor Channel write request failed",
            ]
        );
    }

    #[test]
    fn a_callback_may_destroy_its_own_channel() {
        let (var, _rx) = var();
        let shared = var.shared.clone();
        shared.callback(
            None,
            |_| true,
            move || {
                assert_eq!(var.monitor_off(), PvStat::Ok);
                assert_eq!(var.destroy(), PvStat::Ok);
            },
        );
        assert!(shared.state.lock().closed);
    }

    #[test]
    fn monitor_on_while_on_is_a_no_op() {
        let (var, mut rx) = var();
        assert_eq!(
            var.monitor_on(&Arc::default(), PvType::TimeDouble, 1, |_| {}),
            PvStat::Ok
        );
        assert_eq!(
            var.monitor_on(&Arc::default(), PvType::TimeDouble, 1, |_| {}),
            PvStat::Ok
        );
        assert_eq!(var.monitor_off(), PvStat::Ok);
        assert_eq!(var.monitor_off(), PvStat::Ok);
        assert_eq!(
            var.monitor_on(&Arc::default(), PvType::TimeDouble, 1, |_| {}),
            PvStat::Ok
        );
        let mut seen = Vec::new();
        while let Ok((_, cmd)) = rx.try_recv() {
            seen.push(match cmd {
                Cmd::Chan(ChanCmd::MonitorOn(m)) => format!("on {}", m.monid),
                Cmd::Chan(ChanCmd::MonitorOff) => "off".to_string(),
                _ => "other".to_string(),
            });
        }
        assert_eq!(seen, ["on 1", "off", "on 2"]);
    }

    #[test]
    fn a_refused_request_queues_nothing_and_drops_its_callback() {
        let (var, mut rx) = var();
        let called = Arc::new(AtomicBool::new(false));
        let c = called.clone();
        let get = var.get_callback(PvType::TimeDouble, 1, move |_| {
            c.store(true, Ordering::SeqCst)
        });
        assert_eq!(get, PvStat::Ok, "statFromCA(ECA_DISCONN)");
        assert_eq!(var.message().as_deref(), Some("Virtual circuit disconnect"));
        let c = called.clone();
        let put = var.put_callback(PvType::Double, 1, EpicsValue::Double(1.0), move |_| {
            c.store(true, Ordering::SeqCst)
        });
        assert_eq!(put, PvStat::Ok);
        assert_eq!(
            var.put_no_block(PvType::Double, 1, EpicsValue::Double(1.0)),
            PvStat::Ok
        );
        assert_eq!(Arc::strong_count(&called), 1, "both callbacks dropped");
        assert!(!called.load(Ordering::SeqCst));
        assert!(rx.try_recv().is_err());
        assert_eq!(var.count(), 0);
    }

    #[test]
    fn a_request_after_destroy_fails() {
        let (var, mut rx) = var();
        assert_eq!(var.destroy(), PvStat::Ok);
        assert!(matches!(rx.try_recv(), Ok((_, Cmd::Clear))));
        let log = Log::default();
        assert_eq!(
            var.get_callback(PvType::TimeDouble, 1, record(&log, "get")),
            PvStat::Error
        );
        assert_eq!(var.message().as_deref(), Some("Invalid channel identifier"));
        assert_eq!(
            var.put_callback(
                PvType::Double,
                1,
                EpicsValue::Double(1.0),
                record(&log, "put")
            ),
            PvStat::Error
        );
        assert_eq!(
            var.put_no_block(PvType::Double, 1, EpicsValue::Double(1.0)),
            PvStat::Error
        );
        assert_eq!(
            var.monitor_on(&Arc::default(), PvType::TimeDouble, 1, |_| {}),
            PvStat::Error
        );
        // One that passed the checks before the destroy is refused too.
        assert!(matches!(
            var.accept(
                PvEventType::Get,
                PvType::TimeDouble,
                1,
                record(&log, "late")
            ),
            Err(Eca::BadChid)
        ));
        assert!(log.lock().is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn pv_size_n_counts_the_first_element_in_the_struct() {
        assert_eq!(pv_size_n(PvType::TimeChar, 0), 16);
        assert_eq!(pv_size_n(PvType::TimeChar, 1), 16);
        assert_eq!(pv_size_n(PvType::TimeChar, 4), 19);
        assert_eq!(pv_size_n(PvType::Short, 3), 6);
        assert_eq!(pv_size_n(PvType::TimeDouble, 2), 32);
        assert_eq!(pv_size_n(PvType::TimeString, 2), 92);
    }

    #[test]
    fn the_time_is_past_the_epics_epoch() {
        // 2020-01-01 is 946_684_800 s past 1990-01-01.
        assert!(pv_time_get_current_double() > 946_684_800.0);
    }
}
