//! `recDynLink.c`: a record's own CA channels, opened by name at runtime.
//!
//! C runs two threads (`recDynInp`, `recDynOut`) that drain a message queue
//! into libca and call the record's callbacks from CA's threads. Here each
//! link is one task on the database's CA client reactor
//! ([`CaClientContext::spawn`]); what it learns reaches the record as a
//! [`LinkEvent`] through the record's [`Deliver`], which takes the record's
//! lock ([`AsyncDbHandle::with_record`]) and hands the event to
//! [`RecDynLink::apply`]. `apply` updates the link the way the C callback
//! updates `dynLinkPvt` and answers which of the record's callbacks to run.
//!
//! So the link state lives in the record and every accessor here is plain
//! field access under the record's lock, which is where C's `dynLinkPvt`
//! lock was taken anyway.
//!
//! A generation number orders the link's lifetimes: [`RecDynLink::clear`]
//! and every add start a new one, and an event of an older lifetime is
//! dropped, which is what `ca_set_puser(chid, NULL)` does for C.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use epics_base_rs::runtime::task::TaskHandle;
use epics_base_rs::server::database::{
    AsyncDbHandle, CaClientContext, ClientChannel, ClientMonitor, Connection,
};
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DBR_CTRL_DOUBLE, DBR_TIME_DOUBLE, DBR_TIME_FLOAT};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use tokio::sync::mpsc;

/// iocsh `recDynLinkDebug`. Stored for an `st.cmd` that sets it; nothing
/// here prints by level.
pub static REC_DYN_LINK_DEBUG: AtomicI32 = AtomicI32::new(0);
/// iocsh `recDynLinkQsize`, C's message-queue depth. Stored only: commands
/// go to each link's own task over an unbounded channel, so there is no
/// queue to size. C's unexported `recDynINPCallPend*` poll knobs have no
/// counterpart either — no thread here polls.
pub static REC_DYN_LINK_QSIZE: AtomicI32 = AtomicI32::new(256);

/// The iocsh variables `sscanSupport.dbd` declares for `recDynLink.c`.
pub(crate) fn iocsh_vars() -> Vec<epics_base_rs::server::iocsh::vars::VarDef> {
    use epics_base_rs::server::iocsh::vars::{VarAccess, VarDef};
    vec![
        VarDef {
            name: "recDynLinkDebug",
            access: VarAccess::Int {
                get: || i64::from(REC_DYN_LINK_DEBUG.load(Ordering::Relaxed)),
                set: |v| REC_DYN_LINK_DEBUG.store(v as i32, Ordering::Relaxed),
            },
        },
        VarDef {
            name: "recDynLinkQsize",
            access: VarAccess::Int {
                get: || i64::from(REC_DYN_LINK_QSIZE.load(Ordering::Relaxed)),
                set: |v| REC_DYN_LINK_QSIZE.store(v as i32, Ordering::Relaxed),
            },
        },
    ]
}

/// `recDynLinkPutCallback`: a notify is already outstanding.
pub const NOTIFY_IN_PROGRESS: i64 = 1;
/// The command could not be queued.
pub const RINGBUFF_PUT_ERROR: i64 = 2;
/// The request failed and no CA callback will follow; the link's callback
/// runs with this in [`RecDynLink::status`].
pub const FATAL_ERROR: i16 = 3;

/// `recDynLinkCheckReadWriteAccess` bits.
pub const ACCESS_READ: i32 = 1;
pub const ACCESS_WRITE: i32 = 2;

/// The `dbrType` a link carries, `DBR_FLOAT` or `DBR_DOUBLE`: the only two
/// sscan asks for. A float link's values are rounded through `f32`, as
/// libca's conversion to `DBR_FLOAT` rounds them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaType {
    Float,
    Double,
}

impl CaType {
    fn dbf(self) -> DbFieldType {
        match self {
            CaType::Float => DbFieldType::Float,
            CaType::Double => DbFieldType::Double,
        }
    }

    fn time_dbr(self) -> u16 {
        match self {
            CaType::Float => DBR_TIME_FLOAT,
            CaType::Double => DBR_TIME_DOUBLE,
        }
    }

    fn value(self, v: f64) -> EpicsValue {
        match self {
            CaType::Float => EpicsValue::Float(v as f32),
            CaType::Double => EpicsValue::Double(v),
        }
    }

    /// `count` elements of `value`, converted the way libca converts to this
    /// type.
    fn elements(self, value: EpicsValue) -> Vec<f64> {
        match value.into_type(self.dbf()) {
            EpicsValue::DoubleArray(a) => a.iter().copied().collect(),
            EpicsValue::FloatArray(a) => a.iter().map(|&v| f64::from(v)).collect(),
            v => v.to_f64().into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Io {
    Input,
    Output,
}

/// `stateType`. `Searching` is never entered by C either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Starting,
    Getting,
    Connected,
}

/// What a link's task learned, in the order C's callbacks learn it.
#[derive(Debug)]
pub enum LinkEvent {
    /// `connectCallback` with `cs_conn`: the `ca_get_callback(DBR_CTRL_DOUBLE)`
    /// is in flight.
    Up { nelem: u32, access: (bool, bool) },
    /// `getCallback`: the channel's metadata.
    Ctrl(Metadata),
    /// `connectCallback` without `cs_conn`.
    Down,
    /// `monitorCallback`.
    Monitor(Values),
    /// `userGetCallback`, or the error C answers with `FATAL_ERROR`.
    GetDone(Result<Values, String>),
    /// `notifyCallback`, or the error C answers with `FATAL_ERROR`.
    PutDone(Result<(), String>),
}

/// The `DBR_CTRL_DOUBLE` fields `getCallback` keeps.
#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub graphic_low: f64,
    pub graphic_high: f64,
    pub control_low: f64,
    pub control_high: f64,
    pub units: String,
    pub precision: i16,
}

/// One `DBR_TIME_*` delivery.
#[derive(Debug, Clone)]
pub struct Values {
    pub values: Vec<f64>,
    pub status: u16,
    pub severity: u16,
}

/// How a link's task reaches the record: the record's lock, then
/// [`RecDynLink::apply`] with the generation the task was started for.
pub type Deliver = Arc<dyn Fn(u64, LinkEvent) + Send + Sync>;

enum Cmd {
    Put(EpicsValue),
    PutCallback(EpicsValue),
    GetCallback(u32),
}

/// `dynLinkPvt`.
struct Pvt<K> {
    io: Io,
    ca_type: CaType,
    scalar: bool,
    state: State,
    /// `ca_state(chid) == cs_conn`.
    connected: bool,
    nelem: u32,
    access: (bool, bool),
    buffer: Vec<f64>,
    n_request: usize,
    status: u16,
    severity: u16,
    meta: Metadata,
    notify_in_progress: bool,
    search_callback: Option<K>,
    monitor_callback: Option<K>,
    notify_callback: Option<K>,
    user_get_callback: Option<K>,
    tx: mpsc::UnboundedSender<Cmd>,
}

/// `recDynLink`, generic over the record's callback names `K` — C's
/// `recDynCallback` function pointers.
pub struct RecDynLink<K> {
    /// `precDynLink->status`: [`FATAL_ERROR`] when a callback ran because a
    /// request failed.
    pub status: i16,
    pub get_callback_in_progress: bool,
    generation: u64,
    pvt: Option<Pvt<K>>,
}

impl<K> Default for RecDynLink<K> {
    fn default() -> Self {
        Self {
            status: 0,
            get_callback_in_progress: false,
            generation: 0,
            pvt: None,
        }
    }
}

/// Where a link's task is started: the record's database and name.
pub struct LinkHost<'a> {
    pub handle: &'a AsyncDbHandle,
    pub record: &'a [u8],
}

/// The arguments C's `recDynLinkAddInput`/`recDynLinkAddOutput` take.
pub struct LinkSpec<K> {
    pub io: Io,
    pub ca_type: CaType,
    /// `rdlSCALAR`.
    pub scalar: bool,
    pub search_callback: Option<K>,
    /// Inputs only.
    pub monitor_callback: Option<K>,
}

impl<K: Copy> RecDynLink<K> {
    /// `recDynLinkAddInput` / `recDynLinkAddOutput`. A blank name is refused
    /// with -1, as C refuses it; a link already added is cleared first.
    /// The channel is created when the database is running (C's tasks wait
    /// for `interruptAccept`).
    pub fn add(
        &mut self,
        host: LinkHost<'_>,
        pvname: &str,
        spec: LinkSpec<K>,
        deliver: Deliver,
    ) -> i64 {
        if pvname.is_empty() {
            return -1;
        }
        if self.pvt.is_some() {
            self.clear();
        }
        self.generation += 1;
        let (tx, rx) = mpsc::unbounded_channel();
        self.pvt = Some(Pvt {
            io: spec.io,
            ca_type: spec.ca_type,
            scalar: spec.scalar,
            state: State::Starting,
            connected: false,
            nelem: 0,
            access: (false, false),
            buffer: Vec::new(),
            n_request: 0,
            status: 0,
            severity: 0,
            meta: Metadata::default(),
            notify_in_progress: false,
            search_callback: spec.search_callback,
            monitor_callback: if spec.io == Io::Input {
                spec.monitor_callback
            } else {
                None
            },
            notify_callback: None,
            user_get_callback: None,
            tx,
        });
        let generation = self.generation;
        let deliver: Arc<dyn Fn(LinkEvent) + Send + Sync> =
            Arc::new(move |event| deliver(generation, event));
        let ctx = host.handle.ca_client();
        let task = run_link(
            ctx.clone(),
            pvname.to_owned(),
            spec.io,
            spec.ca_type,
            spec.scalar,
            rx,
            deliver,
        );
        host.handle.schedule_record_init(host.record, async move {
            // Detached: the task ends when this link's sender is dropped.
            drop(ctx.spawn(task));
        });
        0
    }

    /// `recDynLinkClear`.
    pub fn clear(&mut self) {
        self.generation += 1;
        self.pvt = None;
        self.status = 0;
    }

    pub fn is_added(&self) -> bool {
        self.pvt.is_some()
    }

    /// `recDynLinkConnectionStatus`: 0 connected, -1 not.
    pub fn connection_status(&self) -> i64 {
        match &self.pvt {
            Some(p) if p.connected => 0,
            _ => -1,
        }
    }

    /// `recDynLinkGetNelem`.
    pub fn nelem(&self) -> Option<usize> {
        self.pvt
            .as_ref()
            .filter(|p| p.connected)
            .map(|p| p.nelem as usize)
    }

    fn metadata(&self) -> Option<&Metadata> {
        self.pvt
            .as_ref()
            .filter(|p| p.state == State::Connected)
            .map(|p| &p.meta)
    }

    /// `recDynLinkGetControlLimits` as `(low, high)`.
    pub fn control_limits(&self) -> Option<(f64, f64)> {
        self.metadata().map(|m| (m.control_low, m.control_high))
    }

    /// `recDynLinkGetGraphicLimits` as `(low, high)`.
    pub fn graphic_limits(&self) -> Option<(f64, f64)> {
        self.metadata().map(|m| (m.graphic_low, m.graphic_high))
    }

    /// `recDynLinkGetPrecision`.
    pub fn precision(&self) -> Option<i16> {
        self.metadata().map(|m| m.precision)
    }

    /// `recDynLinkGetUnits(.., maxlen)`: at most `maxlen - 1` bytes.
    pub fn units(&self, maxlen: usize) -> Option<String> {
        self.metadata().map(|m| {
            let mut end = m.units.len().min(maxlen.saturating_sub(1));
            while !m.units.is_char_boundary(end) {
                end -= 1;
            }
            m.units[..end].to_owned()
        })
    }

    /// `recDynLinkCheckReadWriteAccess`.
    pub fn check_access(&self) -> i32 {
        match &self.pvt {
            Some(p) if p.connected => {
                (if p.access.0 { ACCESS_READ } else { 0 })
                    | (if p.access.1 { ACCESS_WRITE } else { 0 })
            }
            _ => 0,
        }
    }

    /// `recDynLinkGet`: the latest value, at most `n_request` elements and at
    /// most what the link holds. `None` while not connected.
    pub fn get(&mut self, n_request: usize) -> Option<&[f64]> {
        self.status = 0;
        let p = self.pvt.as_ref().filter(|p| p.connected)?;
        let n = n_request.min(p.n_request).min(p.buffer.len());
        Some(&p.buffer[..n])
    }

    /// `recDynLinkGetCallback`. `n_request` is clamped to the channel's
    /// element count and becomes the count later monitors copy, as in C.
    pub fn get_callback(&mut self, n_request: &mut usize, callback: K) -> i64 {
        self.status = 0;
        self.get_callback_in_progress = true;
        let Some(p) = self.pvt.as_mut() else {
            return -1;
        };
        if p.io != Io::Input || p.state != State::Connected || !p.connected {
            return -1;
        }
        p.user_get_callback = Some(callback);
        *n_request = (*n_request).min(p.nelem as usize);
        p.n_request = *n_request;
        if p.tx.send(Cmd::GetCallback(*n_request as u32)).is_err() {
            return RINGBUFF_PUT_ERROR;
        }
        0
    }

    /// `recDynLinkPut`.
    pub fn put(&mut self, value: f64) -> i64 {
        self.put_inner(value, None)
    }

    /// `recDynLinkPutCallback`. Non-zero means no callback will come.
    pub fn put_callback(&mut self, value: f64, callback: K) -> i64 {
        self.put_inner(value, Some(callback))
    }

    fn put_inner(&mut self, value: f64, callback: Option<K>) -> i64 {
        self.status = 0;
        let Some(p) = self.pvt.as_mut() else {
            return -1;
        };
        if p.io != Io::Output || p.state != State::Connected || !p.connected {
            return -1;
        }
        // C raises `notifyInProgress` when its out-task dispatches the put;
        // raised here, a second put-callback queued behind the first is
        // refused instead of overwriting the first one's buffer.
        if callback.is_some() {
            if p.notify_in_progress {
                return NOTIFY_IN_PROGRESS;
            }
            p.notify_callback = callback;
            p.notify_in_progress = true;
        }
        // Every sscan put is one element (`rdlSCALAR`, or `nRequest` 1).
        p.n_request = 1;
        let value = p.ca_type.value(value);
        let cmd = if callback.is_some() {
            Cmd::PutCallback(value)
        } else {
            Cmd::Put(value)
        };
        if p.tx.send(cmd).is_err() {
            p.notify_in_progress = false;
            return RINGBUFF_PUT_ERROR;
        }
        0
    }

    /// Apply what the link's task learned and answer the callback C would
    /// call from it. An event of an earlier lifetime of this link answers
    /// nothing.
    pub fn apply(&mut self, generation: u64, event: LinkEvent) -> Option<K> {
        if generation != self.generation {
            return None;
        }
        let p = self.pvt.as_mut()?;
        match event {
            LinkEvent::Up { nelem, access } => {
                p.connected = true;
                p.nelem = nelem;
                p.access = access;
                p.state = State::Getting;
                None
            }
            LinkEvent::Ctrl(meta) => {
                p.meta = meta;
                p.n_request = if p.scalar { 1 } else { p.nelem as usize };
                p.buffer = vec![0.0; p.n_request];
                p.state = State::Connected;
                p.search_callback
            }
            LinkEvent::Down => {
                // C leaves `state` at `stateConnected`: only `ca_state` moves.
                p.connected = false;
                p.search_callback
            }
            LinkEvent::Monitor(values) => {
                p.store(values);
                p.monitor_callback
            }
            LinkEvent::GetDone(result) => {
                match result {
                    Ok(values) => p.store(values),
                    // C's `userGetCallback` returns on a bad `eha.status`
                    // without calling back, which leaves sscan waiting on a
                    // get that never completes; answered as the queueing
                    // failure is answered instead.
                    Err(_) => self.status = FATAL_ERROR,
                }
                p.user_get_callback
            }
            LinkEvent::PutDone(result) => {
                p.notify_in_progress = false;
                // C's `notifyCallback` returns on a bad `eha.status` with
                // `notifyInProgress` still set, which hangs the scan and
                // refuses every later put-callback; answered as the issue
                // failure is answered instead.
                if result.is_err() {
                    self.status = FATAL_ERROR;
                }
                p.notify_callback
            }
        }
    }
}

impl<K> Pvt<K> {
    /// `monitorCallback`'s copy: at most `nRequest` elements into a buffer
    /// that exists once the link has connected.
    fn store(&mut self, values: Values) {
        if self.buffer.is_empty() {
            return;
        }
        let n = values
            .values
            .len()
            .min(self.n_request)
            .min(self.buffer.len());
        self.buffer[..n].copy_from_slice(&values.values[..n]);
        self.status = values.status;
        self.severity = values.severity;
    }
}

/// Aborts its task when dropped, so a cleared link's monitor dies with it.
struct AbortOnDrop(Option<TaskHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

fn values_of(ca_type: CaType, snapshot: Snapshot) -> Values {
    Values {
        status: snapshot.alarm.status,
        severity: snapshot.alarm.severity,
        values: ca_type.elements(snapshot.value),
    }
}

fn metadata_of(snapshot: &Snapshot) -> Metadata {
    let mut meta = Metadata::default();
    if let Some(d) = &snapshot.display {
        meta.graphic_low = d.lower_disp_limit;
        meta.graphic_high = d.upper_disp_limit;
        meta.precision = d.precision;
        meta.units = d.units.as_str_lossy().into_owned();
    }
    if let Some(c) = &snapshot.control {
        meta.control_low = c.lower_ctrl_limit;
        meta.control_high = c.upper_ctrl_limit;
    }
    meta
}

/// One link's lifetime: `ca_create_channel`, then commands until the record
/// drops the link's sender.
async fn run_link(
    ctx: CaClientContext,
    pvname: String,
    io: Io,
    ca_type: CaType,
    scalar: bool,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    deliver: Arc<dyn Fn(LinkEvent) + Send + Sync>,
) {
    let Some(channel) = ctx.create_channel(&pvname).await else {
        // A search nobody answers: requests are refused at the record by the
        // never-connected state, so the queue only has to be drained.
        while rx.recv().await.is_some() {}
        return;
    };
    // The monitor is held here, not by `connection_task`: once connected, a
    // local channel's next connection change never comes and registers no
    // waker, and the exec backend drops such a task with everything it owns.
    let monitor = MonitorSlot::default();
    let _connection = AbortOnDrop(ctx.spawn(connection_task(
        ctx.clone(),
        channel.clone(),
        io,
        ca_type,
        scalar,
        deliver.clone(),
        monitor.clone(),
    )));
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Put(value) => {
                if let Err(e) = channel.put(value, false).await {
                    tracing::error!("recDynLinkTask pv={pvname} CA Error {e}");
                }
            }
            Cmd::PutCallback(value) => match channel.put(value, true).await {
                Ok(Some(done)) => {
                    let deliver = deliver.clone();
                    drop(ctx.spawn(async move { deliver(LinkEvent::PutDone(done.await)) }));
                }
                Ok(None) => deliver(LinkEvent::PutDone(Ok(()))),
                Err(e) => {
                    tracing::error!("recDynLinkTask pv={pvname} CA Error {e}");
                    deliver(LinkEvent::PutDone(Err(e)));
                }
            },
            Cmd::GetCallback(count) => {
                let channel = channel.clone();
                let deliver = deliver.clone();
                drop(ctx.spawn(async move {
                    let got = channel.get(ca_type.time_dbr(), count).await;
                    deliver(LinkEvent::GetDone(got.map(|s| values_of(ca_type, s))));
                }));
            }
        }
    }
    monitor.lock().unwrap().take();
}

type MonitorSlot = Arc<std::sync::Mutex<Option<AbortOnDrop>>>;

/// `connectCallback` → `getCallback` → `ca_add_array_event`, for every
/// connection change. The monitor is added once per link: C adds another on
/// every reconnect, each delivering the same value.
async fn connection_task(
    ctx: CaClientContext,
    channel: Arc<dyn ClientChannel>,
    io: Io,
    ca_type: CaType,
    scalar: bool,
    deliver: Arc<dyn Fn(LinkEvent) + Send + Sync>,
    monitor: MonitorSlot,
) {
    while let Some(state) = channel.next_connection().await {
        match state {
            Connection::Disconnected => deliver(LinkEvent::Down),
            Connection::Connected => {
                let nelem = channel.element_count().unwrap_or(1);
                deliver(LinkEvent::Up {
                    nelem,
                    access: channel.access(),
                });
                // C's `getCallback` returns on a failed get and leaves the
                // link in `stateGetting` for good; the link connects with
                // empty metadata instead.
                let meta = match channel.get(DBR_CTRL_DOUBLE, 1).await {
                    Ok(snapshot) => metadata_of(&snapshot),
                    Err(e) => {
                        tracing::warn!("recDynLink:getCallback: {e}");
                        Metadata::default()
                    }
                };
                deliver(LinkEvent::Ctrl(meta));
                if io == Io::Input && monitor.lock().unwrap().is_none() {
                    let count = if scalar { 1 } else { nelem };
                    let mask = (EventMask::VALUE | EventMask::ALARM).bits();
                    match channel.subscribe(ca_type.time_dbr(), count, mask).await {
                        Ok(sub) => {
                            *monitor.lock().unwrap() = Some(AbortOnDrop(ctx.spawn(monitor_task(
                                sub,
                                ca_type,
                                deliver.clone(),
                            ))))
                        }
                        Err(e) => tracing::error!("ca_add_array_event: {e}"),
                    }
                }
            }
        }
    }
}

async fn monitor_task(
    mut sub: Box<dyn ClientMonitor>,
    ca_type: CaType,
    deliver: Arc<dyn Fn(LinkEvent) + Send + Sync>,
) {
    while let Some(snapshot) = sub.next().await {
        deliver(LinkEvent::Monitor(values_of(ca_type, snapshot)));
    }
}
