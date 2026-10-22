//! The pv layer (`pv.h`, `pvCa.c`): what seq calls to reach a named pv.
//!
//! The system is the IOC's CA client context
//! ([`epics_base_rs::server::database::CaClientContext`]), libca with
//! `dbContext` installed: a name this IOC serves opens an in-process
//! channel, any other goes to the network client epics-ca-rs registered,
//! so a program reaches remote pvs as C's `seq_ca.c` does. A request's
//! completion runs on the database reactor, where C's runs on a CA
//! auxiliary thread; it must not block.
//!
//! C's `(pvEventFunc, void *arg)` pairs are closures here, one per request.
//!
//! No call here runs a callback before it returns, [`PvVar::create`]
//! included: a callback is queued for the reactor even when the channel is
//! local and the answer is already known. A caller may therefore hold
//! seq's locks across a request, as C's connection handler holds
//! `sp->lock` across `pvVarMonitorOn`, and a callback may take them.
//!
//! What libca refuses at once (disconnected, no access, too many elements)
//! is refused here at once, before anything is queued, and returns
//! [`PvStat::Ok`] as `statFromCA` makes of those warning-class codes; the
//! callback is then never called. A request libca accepted is completed
//! exactly once: by the server, or with `ECA_DISCONN` when the channel
//! disconnects first, ahead of the connection handler's `false`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, OnceLock};
use std::task::Poll;

use epics_base_rs::error::CaError;
use epics_base_rs::runtime::general_time::{EPICS_EPOCH_UNIX_SECS, get_current};
use epics_base_rs::runtime::log::{ErrlogSevEnum, errlog_sev_printf};
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::runtime::task::TaskHandle;
use epics_base_rs::server::database::{
    CaClientContext, ClientChannel, ClientMonitor, Connection, PvDatabase,
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
}

impl PvSystem {
    /// `pvSysCreate`.
    pub fn create(db: &PvDatabase) -> PvSystem {
        PvSystem {
            ctx: db.ca_client(),
        }
    }

    /// `pvSysAttach`: make the calling thread a client of this system. The
    /// context is not per thread here, so there is nothing to attach.
    pub fn attach(&self) -> PvStat {
        PvStat::Ok
    }

    /// `pvSysFlush`: send what the calling thread has queued. A request is
    /// sent when it is issued, so nothing is ever queued.
    pub fn flush(&self) -> PvStat {
        PvStat::Ok
    }

    /// The CA client context underneath.
    pub fn context(&self) -> &CaClientContext {
        &self.ctx
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
        }
    }

    /// `ca_message` as the shared string `pvVarGetMess` hands out, so an
    /// event does not allocate one.
    fn message(self) -> Arc<str> {
        static TEXTS: LazyLock<[Arc<str>; 8]> = LazyLock::new(|| {
            [
                Eca::Normal,
                Eca::GetFail,
                Eca::PutFail,
                Eca::BadCount,
                Eca::Disconn,
                Eca::NoRdAccess,
                Eca::NoWtAccess,
                Eca::BadChid,
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

enum Cmd {
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
    MonitorOn {
        monid: u64,
        ty: PvType,
        count: u32,
        event: MonitorFn,
    },
    MonitorOff,
    /// `ca_clear_channel`.
    Clear,
}

/// Why [`PvVar::create`] fails: the only way it can.
pub(crate) const NO_CONTEXT: &str = "the IOC's CA client context is gone";

/// `pvVar`: one channel.
pub struct PvVar {
    name: Arc<str>,
    shared: Arc<Shared>,
    cmds: mpsc::UnboundedSender<Cmd>,
}

impl PvVar {
    /// `pvVarCreate`: start connecting `name`; `conn` runs with `true` on
    /// each connect and `false` on each disconnect.
    pub fn create(
        sys: &PvSystem,
        name: &str,
        conn: impl Fn(bool) + Send + Sync + 'static,
    ) -> Result<PvVar, PvStat> {
        let name: Arc<str> = Arc::from(name);
        let shared = Arc::new(Shared::new());
        let (cmds, rx) = mpsc::unbounded_channel();
        let run = run(
            sys.ctx.clone(),
            name.clone(),
            shared.clone(),
            Arc::new(conn),
            rx,
        );
        if sys.ctx.spawn(run).is_none() {
            errlog_sev_printf(
                ErrlogSevEnum::Major,
                &format!(
                    "ca_create_channel(name, pvCaConnectionHandler, var, \
                     CA_PRIORITY_DEFAULT, &var->chid): {NO_CONTEXT}"
                ),
            );
            return Err(PvStat::Error);
        }
        Ok(PvVar { name, shared, cmds })
    }

    /// `pvVarDestroy`: no callback of this pv runs after it returns, and a
    /// later request fails. It waits for a running callback, so the caller
    /// holds no seq lock across it, as C unlocks `sp->lock` around it.
    pub fn destroy(&self) -> PvStat {
        let _cb = self.shared.callback.lock();
        self.shared.close();
        // `ca_clear_channel`: the run task drops the channel and its monitor.
        let _ = self.cmds.send(Cmd::Clear);
        PvStat::Ok
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `pvVarGetCount`: the server's element count, 0 before the first
    /// connect.
    pub fn count(&self) -> u32 {
        self.shared
            .chan
            .get()
            .and_then(|c| c.element_count())
            .unwrap_or(0)
    }

    /// `pvVarGetMess`.
    pub fn message(&self) -> Option<Arc<str>> {
        self.shared.state.lock().message.clone()
    }

    /// `pvMonIsDefined`.
    pub(crate) fn monitor_is_on(&self) -> bool {
        self.shared.state.lock().monitor != 0
    }

    /// A var whose channel never connects and whose requests go nowhere.
    #[cfg(test)]
    pub(crate) fn unconnected(name: &str) -> PvVar {
        PvVar {
            name: Arc::from(name),
            shared: Arc::new(Shared::new()),
            cmds: mpsc::unbounded_channel().0,
        }
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
        let chan = match self.shared.chan.get() {
            Some(chan) if connected => chan,
            _ => return Err(Eca::Disconn),
        };
        let (read, written) = chan.access();
        if write && !written {
            return Err(Eca::NoWtAccess);
        }
        if !write && !read {
            return Err(Eca::NoRdAccess);
        }
        if count > chan.element_count().unwrap_or(0) {
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
        let _ = self.cmds.send(Cmd::Get {
            id,
            dbr: type_to_ca(ty),
            count,
        });
        PvStat::Ok
    }

    /// `pvVarPutNoBlock`.
    pub fn put_no_block(&self, ty: PvType, count: u32, value: EpicsValue) -> PvStat {
        assert!(ty.is_simple(), "pvVarPutNoBlock: {ty:?}");
        if let Err(eca) = self.admit(true, count) {
            return self
                .shared
                .refuse(eca, "ca_array_put(typeToCA(type), count, var->chid, value)");
        }
        let _ = self.cmds.send(Cmd::Put {
            id: None,
            value: put_value(value, ty, count),
        });
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
            Err(eca) => {
                return self.shared.refuse(
                    eca,
                    "ca_array_put_callback( typeToCA(type), count, var->chid, value, pvCaPutHandler, arg)",
                );
            }
        };
        let _ = self.cmds.send(Cmd::Put {
            id: Some(id),
            value: put_value(value, ty, count),
        });
        PvStat::Ok
    }

    /// `pvVarMonitorOn`: a no-op while a monitor is on. Allowed while
    /// disconnected; the first event comes after the connect.
    pub fn monitor_on(
        &self,
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
        let _ = self.cmds.send(Cmd::MonitorOn {
            monid,
            ty,
            count,
            event: Arc::new(event),
        });
        PvStat::Ok
    }

    /// `pvVarMonitorOff`: once this returns no event of the monitor runs, so
    /// it must not be called holding a lock the monitor callback takes.
    pub fn monitor_off(&self) -> PvStat {
        let _cb = self.shared.callback.lock();
        let was_on = std::mem::take(&mut self.shared.state.lock().monitor) != 0;
        if was_on {
            let _ = self.cmds.send(Cmd::MonitorOff);
        }
        PvStat::Ok
    }
}

impl Drop for PvVar {
    /// Closes the channel without waiting for a callback in flight, so a
    /// var may be dropped from anywhere; [`PvVar::destroy`] waits.
    fn drop(&mut self) {
        self.shared.close();
    }
}

const MONITOR_EXPR: &str = "ca_create_subscription(typeToCA(type), count, var->chid, \
     DBE_VALUE | DBE_ALARM, pvCaMonitorHandler, arg, &var->monid)";

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

/// One channel's lifetime: `ca_create_channel`, then the requests in the
/// order they were made, until the var is gone.
async fn run(
    ctx: CaClientContext,
    name: Arc<str>,
    shared: Arc<Shared>,
    conn: ConnFn,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let Some(chan) = ctx.create_channel(&name).await else {
        // A search nobody answers: the channel never connects, so every
        // request was refused as disconnected; only monitors queue.
        while rx.recv().await.is_some() {}
        return;
    };
    serve(ctx, chan, shared, conn, rx).await;
}

/// The requests to `chan` in the order they were made, until the var is
/// gone.
async fn serve(
    ctx: CaClientContext,
    chan: Arc<dyn ClientChannel>,
    shared: Arc<Shared>,
    conn: ConnFn,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let _ = shared.chan.set(chan.clone());
    // Held here, not by the connection task: once connected, a local
    // channel's next connection change never comes and registers no waker,
    // and the exec backend drops such a task with everything it owns.
    let _connection = AbortOnDrop(ctx.spawn(connection_task(chan.clone(), shared.clone(), conn)));
    let mut monitor: Option<AbortOnDrop> = None;
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Get { id, dbr, count } => {
                if !shared.is_pending(id) {
                    continue;
                }
                // Sent on the first poll, so a later put cannot overtake it;
                // the answer is awaited off this task, so it holds up nothing.
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
            Cmd::Put { id, value } => {
                if id.is_some_and(|id| !shared.is_pending(id)) {
                    continue;
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
                            errlog_sev_printf(
                                ErrlogSevEnum::Minor,
                                &format!(
                                    "ca_array_put(typeToCA(type), count, var->chid, value): {e}"
                                ),
                            );
                        }
                    }
                }
            }
            Cmd::MonitorOn {
                monid,
                ty,
                count,
                event,
            } => {
                if shared.state.lock().monitor != monid {
                    continue;
                }
                let mask = (EventMask::VALUE | EventMask::ALARM).bits();
                match chan.subscribe(type_to_ca(ty), count, mask).await {
                    Ok(sub) => {
                        drop(monitor.replace(AbortOnDrop(ctx.spawn(monitor_task(
                            sub,
                            shared.clone(),
                            monid,
                            ty,
                            count,
                            event,
                        )))));
                    }
                    Err(e) => {
                        // `monid` stays NULL, so the next monitor-on retries.
                        let mut st = shared.state.lock();
                        if st.monitor == monid {
                            st.monitor = 0;
                        }
                        drop(st);
                        errlog_sev_printf(ErrlogSevEnum::Minor, &format!("{MONITOR_EXPR}: {e}"));
                    }
                }
            }
            Cmd::MonitorOff => drop(monitor.take()),
            Cmd::Clear => break,
        }
    }
}

/// `pvCaConnectionHandler`, for every connection change.
async fn connection_task(chan: Arc<dyn ClientChannel>, shared: Arc<Shared>, conn: ConnFn) {
    while let Some(state) = chan.next_connection().await {
        shared.connection_change(state == Connection::Connected, &*conn);
    }
}

/// `pvCaMonitorHandler`, for each event of monitor `monid` while it is the
/// channel's monitor.
async fn monitor_task(
    mut sub: Box<dyn ClientMonitor>,
    shared: Arc<Shared>,
    monid: u64,
    ty: PvType,
    count: u32,
    event: MonitorFn,
) {
    while let Some(snapshot) = sub.next().await {
        shared.callback(
            Some(Eca::Normal),
            |st| st.monitor == monid,
            || {
                event(PvEvent {
                    evt: PvEventType::Monitor,
                    ty,
                    count: event_count(count, Some(&snapshot)),
                    value: Some(&snapshot),
                    status: PvStat::Ok,
                    severity: PvSevr::None,
                    message: Some(Eca::Normal.text()),
                })
            },
        );
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
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// A var whose run task is the returned receiver, never connected.
    fn var() -> (PvVar, mpsc::UnboundedReceiver<Cmd>) {
        let (cmds, rx) = mpsc::unbounded_channel();
        let var = PvVar {
            name: Arc::from("T"),
            shared: Arc::new(Shared::new()),
            cmds,
        };
        (var, rx)
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
        assert_eq!(var.monitor_on(PvType::TimeDouble, 1, |_| {}), PvStat::Ok);
        waits_for_a_running_callback(&var, PvVar::monitor_off);
        assert!(!var.monitor_is_on());
        waits_for_a_running_callback(&var, PvVar::destroy);
        assert!(var.shared.state.lock().closed);
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

    /// Run `cmds` against a [`Refusing`] channel, connected.
    fn serve_refused(var: &PvVar, rx: mpsc::UnboundedReceiver<Cmd>, cmds: Vec<Cmd>) -> Vec<String> {
        var.shared.state.lock().connected = true;
        for cmd in cmds.into_iter().chain([Cmd::Clear]) {
            let _ = var.cmds.send(cmd);
        }
        let db = PvDatabase::new();
        logged("refused", || {
            epics_base_rs::runtime::task::test_block_on(serve(
                db.ca_client(),
                Arc::new(Refusing),
                var.shared.clone(),
                Arc::new(|_| {}),
                rx,
            ))
        })
    }

    #[test]
    fn a_failed_subscribe_leaves_the_monitor_off_for_the_next_monitor_on() {
        let (var, rx) = var();
        assert_eq!(var.monitor_on(PvType::TimeDouble, 1, |_| {}), PvStat::Ok);
        assert_eq!(
            serve_refused(&var, rx, Vec::new()),
            [format!("sevr=minor {MONITOR_EXPR}: refused")]
        );
        assert!(!var.monitor_is_on());
        assert_eq!(var.monitor_on(PvType::TimeDouble, 1, |_| {}), PvStat::Ok);
        assert!(var.monitor_is_on(), "retried, not a no-op");
    }

    #[test]
    fn a_refused_put_logs_the_libca_call() {
        let (var, rx) = var();
        let put = Cmd::Put {
            id: None,
            value: EpicsValue::Double(1.0),
        };
        assert_eq!(
            serve_refused(&var, rx, vec![put]),
            ["sevr=minor ca_array_put(typeToCA(type), count, var->chid, value): refused"]
        );
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
        assert_eq!(var.monitor_on(PvType::TimeDouble, 1, |_| {}), PvStat::Ok);
        assert_eq!(var.monitor_on(PvType::TimeDouble, 1, |_| {}), PvStat::Ok);
        assert_eq!(var.monitor_off(), PvStat::Ok);
        assert_eq!(var.monitor_off(), PvStat::Ok);
        assert_eq!(var.monitor_on(PvType::TimeDouble, 1, |_| {}), PvStat::Ok);
        let mut seen = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            seen.push(match cmd {
                Cmd::MonitorOn { monid, .. } => format!("on {monid}"),
                Cmd::MonitorOff => "off".to_string(),
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
        assert!(matches!(rx.try_recv(), Ok(Cmd::Clear)));
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
        assert_eq!(var.monitor_on(PvType::TimeDouble, 1, |_| {}), PvStat::Error);
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
