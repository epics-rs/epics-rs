//! The `ca` link set, as C keeps dbCa in base: a CA link to a name this
//! database serves is an in-process channel, and only a name it refuses
//! reaches the network half another crate registers.
//!
//! `dbServiceIOInit` (`dbContext.cpp:59-66`, from `dbCaLinkInit`,
//! `dbCa.c:344`) installs `dbContext` as libca's default service, so every
//! `ca_create_channel` dbCa makes first tries `dbChannel_create`
//! (`dbContext.cpp:87-121`). [`CaLinks`] is that first try, decided once
//! per name: a name the database serves gets a [`LocalLink`], any other
//! goes to the registered network lset — or, with none registered, stays a
//! link that never connects, as a C link whose search nobody answers.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use parking_lot::RwLock;

use super::channel_io::{self, ChannelTarget, DbAccessChannel};
use super::dbr_delivery::{DeliveryStage, prepare_read, with_live_options};
use super::filters::FilterChain;
use super::link_set::{
    DynLinkSet, EnumString, LinkDbfType, LinkDiagnostics, LinkMetadata, LinkPutDone, LinkPutOp,
    LinkRef, LinkSerial, LinkSet, LinkShape, PutAdmission,
};
use super::{CaLinkInit, ExternalCpProcess, LinkHolder, PvDatabase, PvDatabaseInner};
use crate::runtime::task::{BlockingBridge, TaskHandle};
use crate::server::event_queue::{
    AttachId, DeliveryContext, EvQue, EventReader, EventUser, no_context,
};
use crate::server::pv::{MonitorEvent, PutStarted, SubscribeRefused, WriteContext, WriteEnd};
use crate::server::record::ProcessCompletion;
use crate::server::snapshot::{AlarmInfo, Snapshot};
use crate::types::{
    DBR_CTRL_DOUBLE, DBR_TIME_STRING, DbFieldType, EpicsValue, PvString, RecordName, WallTime,
    dbr_buffer_size, decode_dbr, encode_dbr_into, native_type_for_dbr,
};

/// dbCa's monitor mask, `ca_add_array_event`'s `DBE_VALUE | DBE_ALARM`
/// (`dbCa.c:1225-1229`, `cadef.h:2004-2012`): never `DBE_LOG`.
pub const CA_LINK_EVENT_MASK: u16 =
    crate::server::recgbl::EventMask::VALUE.bits() | crate::server::recgbl::EventMask::ALARM.bits();

/// How long a put callback waits for the channel's previous one before it
/// gives up — `dbPutNotifyBlocker::initiatePutNotify`'s 30 s
/// (`dbPutNotifyBlocker.cpp:146-149`).
const PUT_NOTIFY_BLOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Where `ca_create_channel` sent a link.
enum Route {
    Local(Arc<LocalLink>),
    Network,
}

/// The `ca` scheme's link set: every `ca` lset call goes through here, and
/// the network lset registered under `ca` is reached only for a link the
/// database does not serve.
pub(crate) struct CaLinks {
    db: Weak<PvDatabaseInner>,
    /// Per link, as each C `caLink` has its own channel: two links naming
    /// one PV are two channels.
    routes: RwLock<HashMap<LinkSerial, (RecordName, Route)>>,
    network: RwLock<Option<DynLinkSet>>,
    /// [`Self::isolate`] ran: `network` stays empty.
    isolated: AtomicBool,
    /// `dbCaTask` connects no channel before `dbCaRun` (`dbCa.c:1171-1173`),
    /// so until then every link, local or network, reads as unconnected
    /// (`dbCa.c:459`), whatever its channel has already delivered.
    ca_run: Arc<super::processing::CaRunGate>,
}

impl CaLinks {
    pub(crate) fn new(
        db: Weak<PvDatabaseInner>,
        ca_run: Arc<super::processing::CaRunGate>,
    ) -> Self {
        Self {
            db,
            routes: RwLock::new(HashMap::new()),
            network: RwLock::new(None),
            isolated: AtomicBool::new(false),
            ca_run,
        }
    }

    /// Install the half that serves names this database does not; refused
    /// once [`Self::isolate`] ran.
    pub(crate) fn set_network(&self, lset: DynLinkSet) {
        if !self.isolated.load(Ordering::Acquire) {
            *self.network.write() = Some(lset);
        }
    }

    /// C `dbServiceIsolate` (`dbCa.c:343`): a name this database does not
    /// serve gets `dbChannelNOOP`, a channel that never connects
    /// (`dbContext.cpp:95-97`), instead of the network context.
    pub(crate) fn isolate(&self) {
        self.isolated.store(true, Ordering::Release);
        *self.network.write() = None;
    }

    /// Whether `link` has anything to connect to: a name this database
    /// serves, or any name once a network half is registered. An opened link
    /// answers from its route, as a scan cycle's put gate asks it.
    pub(crate) fn addresses(&self, link: LinkRef<'_>) -> bool {
        self.network.read().is_some()
            || self
                .settled_local(link)
                .unwrap_or_else(|| self.has_name(link.name))
    }

    /// Whether `link` is a local link — C's `dbCaPutAsync` to one completes
    /// through dbNotify rather than libca.
    pub(crate) fn is_local(&self, link: LinkRef<'_>) -> bool {
        self.serves(link)
    }

    fn has_name(&self, name: &[u8]) -> bool {
        self.db
            .upgrade()
            .is_some_and(|inner| (PvDatabase { inner }).has_name_no_resolve(name))
    }

    /// `link` is, or on open would be, a local link: `dbChannelTest`
    /// (`dbLink.c:125`) until the open has decided it. A link to a name the
    /// database does not serve is decided here, once, as C decides it once
    /// per link.
    fn serves(&self, link: LinkRef<'_>) -> bool {
        if let Some(local) = self.settled_local(link) {
            return local;
        }
        let name = link.name;
        if self.has_name(name) {
            return true;
        }
        let mut routes = self.routes.write();
        if !link.is_removed() {
            routes
                .entry(link.serial())
                .or_insert_with(|| (name.into(), Route::Network));
        }
        false
    }

    /// Whether the route `link`'s open settled is local; `None` before it.
    fn settled_local(&self, link: LinkRef<'_>) -> Option<bool> {
        self.routes
            .read()
            .get(&link.serial())
            .map(|(_, route)| matches!(route, Route::Local(_)))
    }

    fn network(&self) -> Option<DynLinkSet> {
        self.network.read().clone()
    }

    /// The network half's CA client, for a channel a record opens itself.
    pub(super) fn client_service(&self) -> Option<Arc<dyn super::ca_client::CaClientService>> {
        self.network.read().as_ref()?.ca_client_service()
    }

    /// `local` over the link's local channel, else `net` over the network
    /// half (`None` with none registered, or before `dbCaRun`).
    fn answer<R>(
        &self,
        link: LinkRef<'_>,
        local: impl FnOnce(&LocalLink) -> R,
        net: impl FnOnce(&dyn LinkSet) -> Option<R>,
    ) -> Option<R> {
        if !self.ca_run.has_run() {
            return None;
        }
        self.answer_paused(link, local, net)
    }

    /// [`Self::answer`] before `dbCaRun` too: for the wait `dbCaRun` itself
    /// makes on the links' first events.
    fn answer_paused<R>(
        &self,
        link: LinkRef<'_>,
        local: impl FnOnce(&LocalLink) -> R,
        net: impl FnOnce(&dyn LinkSet) -> Option<R>,
    ) -> Option<R> {
        if let Some((_, Route::Local(local_link))) = self.routes.read().get(&link.serial()) {
            return Some(local(local_link));
        }
        net(self.network.read().as_deref()?)
    }

    /// The link's local channel, if [`Self::open`] made one.
    fn local(&self, link: LinkRef<'_>) -> Option<Arc<LocalLink>> {
        match self.routes.read().get(&link.serial()) {
            Some((_, Route::Local(local))) => Some(local.clone()),
            _ => None,
        }
    }

    /// `ca_create_channel`: the channel `link` already has, else
    /// `dbChannel_create`, else the network context. `None` for a network
    /// link, or when the database is gone or captured no reactor.
    async fn open(&self, link: LinkRef<'_>) -> Option<Arc<LocalLink>> {
        if let Some((_, route)) = self.routes.read().get(&link.serial()) {
            return match route {
                Route::Local(local) => Some(local.clone()),
                Route::Network => None,
            };
        }
        let db = PvDatabase {
            inner: self.db.upgrade()?,
        };
        let channel = LocalChannel::create(&db, link.name).await;
        // The link starts as its route is published, under this lock: two
        // first opens of one link start one channel, the loser's drops
        // unstarted, and so does an open the link's removal overtook.
        let mut routes = self.routes.write();
        if link.is_removed() {
            return None;
        }
        let route = match routes.entry(link.serial()) {
            std::collections::hash_map::Entry::Occupied(won) => &won.into_mut().1,
            std::collections::hash_map::Entry::Vacant(route) => {
                let opened = match channel {
                    Some(channel) => Route::Local(Arc::new(LocalLink::open(&db, channel, link)?)),
                    None => Route::Network,
                };
                &route.insert((link.name.into(), opened)).1
            }
        };
        match route {
            Route::Local(local) => Some(local.clone()),
            Route::Network => None,
        }
    }

    /// C `dbCaSync`: return once every event queued to a local link before
    /// the call has been handed to its worker and processed. Events posted
    /// after the call are not waited for, so a CP cycle does not hold it.
    /// Only after `dbCaRun`, as C's worker is paused before it.
    pub(crate) async fn sync(&self) {
        let marks: Vec<(Arc<LocalLink>, u64)> = self
            .routes
            .read()
            .values()
            .filter_map(|(_, route)| match route {
                Route::Local(link) => Some((link.clone(), link.state.mark())),
                Route::Network => None,
            })
            .collect();
        for (link, mark) in marks {
            link.state.until_consumed(mark).await;
        }
    }
}

#[async_trait::async_trait]
impl LinkSet for CaLinks {
    fn is_connected(&self, link: LinkRef<'_>) -> bool {
        self.answer(link, LocalLink::is_connected, |n| {
            Some(n.is_connected(link))
        })
        .unwrap_or(false)
    }

    fn init_ready(&self, link: LinkRef<'_>) -> bool {
        self.answer_paused(link, LocalLink::init_ready, |n| Some(n.init_ready(link)))
            .unwrap_or(false)
    }

    async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        if !self.ca_run.has_run() {
            return None;
        }
        match self.open(link).await {
            Some(local) => local.read(),
            None => self.network()?.get_value(link).await,
        }
    }

    fn get_cached_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        self.answer(link, LocalLink::read, |n| Some(n.get_cached_value(link)))
            .flatten()
    }

    fn get_cached_enum_string(&self, link: LinkRef<'_>) -> EnumString {
        self.answer(link, LocalLink::enum_string, |n| {
            Some(n.get_cached_enum_string(link))
        })
        .unwrap_or(EnumString::Pending)
    }

    fn get_failure_disconn(&self, link: LinkRef<'_>) -> bool {
        self.answer(link, |_| false, |n| Some(n.get_failure_disconn(link)))
            .unwrap_or(false)
    }

    async fn connect_link(&self, link: LinkRef<'_>, init: Option<CaLinkInit>) {
        match self.open(link).await {
            Some(local) => {
                if let Some(init) = init {
                    local.state.process.adopt_init(init);
                }
            }
            // With no network half the link can never connect; dropping
            // `init` releases `dbCaRun` rather than holding it forever.
            None => match self.network() {
                Some(net) => net.connect_link(link, init).await,
                None => drop(init),
            },
        }
    }

    /// A link not routed yet starts after this, from the routes lock
    /// [`Self::open`] starts it under.
    fn join_cp_link(&self, link: LinkRef<'_>) {
        match self.local(link) {
            Some(local) => local.state.process.join(),
            None => {
                if let Some(net) = self.network.read().as_deref() {
                    net.join_cp_link(link);
                }
            }
        }
    }

    /// `dbCaRemoveLink` (`dbCa.c:431-446`): the link's channel is cleared,
    /// its monitor cancelled with it.
    fn remove_link(&self, holder: LinkHolder<'_>, link: LinkRef<'_>) {
        let removed = self.routes.write().remove(&link.serial());
        if !matches!(removed, Some((_, Route::Local(_))))
            && let Some(net) = self.network.read().as_deref()
        {
            net.remove_link(holder, link);
        }
    }

    /// `dbCaPutLinkCallback`'s `!pca->isConnected || !pca->hasWriteAccess`
    /// (`dbCa.c:529-532`); a local channel has every right
    /// (`cacChannel::accessRights`). An unopened link is staged, and its
    /// put performs the open.
    fn put_admission(&self, link: LinkRef<'_>) -> PutAdmission {
        if !self.ca_run.has_run() {
            return PutAdmission::Refused;
        }
        match self.routes.read().get(&link.serial()) {
            Some((_, Route::Local(local))) if local.is_connected() => {
                return PutAdmission::Connected;
            }
            Some((_, Route::Local(_))) => return PutAdmission::Refused,
            _ => {}
        }
        match self.network.read().as_deref() {
            Some(net) => net.put_admission(link),
            None => PutAdmission::Refused,
        }
    }

    fn put_native(&self, link: LinkRef<'_>) -> Option<(DbFieldType, u32)> {
        match self.routes.read().get(&link.serial()) {
            Some((_, Route::Local(local))) if local.is_connected() => {
                return Some((local.channel.native_type(), local.channel.element_count()));
            }
            Some((_, Route::Local(_))) => return None,
            _ => {}
        }
        self.network.read().as_deref()?.put_native(link)
    }

    async fn put_value(
        &self,
        link: LinkRef<'_>,
        value: EpicsValue,
        op: LinkPutOp,
    ) -> Result<(), String> {
        self.put_value_done(link, None, value, op).await.1
    }

    async fn put_value_done(
        &self,
        link: LinkRef<'_>,
        holder: Option<&[u8]>,
        value: EpicsValue,
        op: LinkPutOp,
    ) -> (LinkPutDone, Result<(), String>) {
        let name = link.name;
        let unconnected = || {
            Err(format!(
                "CA link {} not connected",
                String::from_utf8_lossy(name)
            ))
        };
        if !self.ca_run.has_run() {
            // No target processed; a local one's put would have answered here.
            let done = if self.serves(link) {
                LinkPutDone::Inline
            } else {
                LinkPutDone::Callback
            };
            return (done, unconnected());
        }
        match self.open(link).await {
            Some(local) => local.write(name, holder, value, op).await,
            None => match self.network() {
                Some(net) => net.put_value_done(link, holder, value, op).await,
                None => (LinkPutDone::Callback, unconnected()),
            },
        }
    }

    fn alarm_message(&self, link: LinkRef<'_>) -> Option<Arc<str>> {
        self.answer(link, |_| None, |n| Some(n.alarm_message(link)))
            .flatten()
    }

    fn alarm_severity(&self, link: LinkRef<'_>) -> Option<i32> {
        self.answer(
            link,
            |local| local.stamp().map(|s| s.severity as i32).filter(|&s| s > 0),
            |n| Some(n.alarm_severity(link)),
        )
        .flatten()
    }

    fn alarm_status(&self, link: LinkRef<'_>) -> Option<i32> {
        self.answer(
            link,
            |local| local.stamp().map(|s| s.status as i32),
            |n| Some(n.alarm_status(link)),
        )
        .flatten()
    }

    fn remote_alarm(&self, link: LinkRef<'_>) -> Option<super::RemoteAlarm> {
        self.answer(link, |_| None, |n| Some(n.remote_alarm(link)))
            .flatten()
    }

    /// CA's `DBR_TIME` carries no user tag.
    fn time_stamp(&self, link: LinkRef<'_>) -> Option<(i64, i32, u64)> {
        self.answer(
            link,
            |local| {
                local.stamp().map(|s| {
                    let t = s.time.since_unix_epoch();
                    (t.as_secs() as i64, t.subsec_nanos() as i32, 0)
                })
            },
            |n| Some(n.time_stamp(link)),
        )
        .flatten()
    }

    fn link_metadata(&self, link: LinkRef<'_>) -> Option<LinkMetadata> {
        self.answer(
            link,
            |local| local.state.meta.get().cloned(),
            |n| Some(n.link_metadata(link)),
        )
        .flatten()
    }

    fn link_shape(&self, link: LinkRef<'_>) -> Option<LinkShape> {
        self.answer(
            link,
            |local| local.state.meta.get().map(LinkMetadata::shape),
            |n| Some(n.link_shape(link)),
        )
        .flatten()
    }

    fn link_names(&self) -> Vec<RecordName> {
        let net = self.network();
        let mut names: Vec<RecordName> = self
            .routes
            .read()
            .values()
            .filter(|(_, route)| net.is_none() || matches!(route, Route::Local(_)))
            .map(|(name, _)| name.clone())
            .collect();
        if let Some(net) = net {
            names.extend(net.link_names());
        }
        // One route per link: links sharing a name list it once.
        names.sort();
        names.dedup();
        names
    }

    /// A local link is up from its creation (`dbChannelIO::initiateConnect`
    /// calls `connectNotify` at once) and never disconnects. A network link
    /// with no network half reports `None`: never connected.
    async fn link_diagnostics(&self, link: LinkRef<'_>) -> Option<LinkDiagnostics> {
        match self.local(link) {
            Some(local) => Some(local.diagnostics()),
            None => self.network()?.link_diagnostics(link).await,
        }
    }
}

/// The alarm and time of one event.
#[derive(Clone, Copy)]
struct LinkStamp {
    status: u16,
    severity: u16,
    time: WallTime,
}

impl LinkStamp {
    fn of(alarm: &AlarmInfo, time: WallTime) -> Self {
        Self {
            status: alarm.status,
            severity: alarm.severity,
            time,
        }
    }
}

/// What a local link's monitor task shares with the link: C `caLink`'s
/// cached value and its `CA_DBPROCESS` request.
struct LocalState {
    cache: ArcSwapOption<Snapshot>,
    /// C `pca->stat`, `sevr` and `timeStamp`: every event of either monitor
    /// overwrites them (`dbCa.c:1001-1003`), and `getAlarm` /
    /// `getTimeStamp` serve the last (`dbCa.c:672-693`).
    stamp: ArcSwapOption<LinkStamp>,
    process: ExternalCpProcess,
    /// Held while an event is taken off the queue, handed to `process` and
    /// counted, so [`Self::mark`] never sees one taken but not counted.
    delivery: parking_lot::Mutex<()>,
    /// Events taken off the queue.
    taken: AtomicU64,
    /// The monitor has ended: no queued event will be taken.
    ended: AtomicBool,
    /// The native monitor's queue slot, read for its `npend`; set when the
    /// link subscribes.
    queue: std::sync::OnceLock<(std::pin::Pin<Arc<EvQue>>, AttachId)>,
    /// The `DBR_STRING` monitor's, set when [`LocalLink::monitor_string`]
    /// subscribes it.
    string_queue: std::sync::OnceLock<(std::pin::Pin<Arc<EvQue>>, AttachId)>,
    /// C `pgetString`: the `DBR_STRING` monitor's last value.
    string: ArcSwapOption<PvString>,
    /// C `pvlOptInpString`: a read asked for the `DBR_STRING` monitor.
    input_string: AtomicBool,
    /// `CA_GET_ATTRIBUTES`, answered once at the connect: a local channel
    /// connects once.
    meta: std::sync::OnceLock<LinkMetadata>,
    /// The link is a `DBF_INLINK`, monitored from its connect.
    inlink: bool,
    /// C `pvlOptInpNative`: set at the open for a `DBF_INLINK`, else by the
    /// first read, whose `CA_MONITOR_NATIVE` [`Self::input_wanted`] carries.
    input_native: AtomicBool,
    input_wanted: crate::runtime::sync::Notify,
}

impl LocalState {
    /// `dbCaGetLink` marking the link a native input (`dbCa.c:455-457`).
    fn note_read(&self) {
        if !self.input_native.swap(true, Ordering::Relaxed) {
            self.input_wanted.notify_one();
        }
    }

    async fn until_input_native(&self) {
        if !self.input_native.load(Ordering::Relaxed) {
            self.input_wanted.notified().await;
        }
    }

    /// The `dbCaSync` mark: the count `taken` reaches once every event
    /// queued now has been taken.
    fn mark(&self) -> u64 {
        let _taking = self.delivery.lock();
        let pending: u64 = [&self.queue, &self.string_queue]
            .into_iter()
            .filter_map(|slot| slot.get())
            .map(|(queue, id)| queue.npend(*id) as u64)
            .sum();
        self.taken.load(Ordering::SeqCst) + pending
    }

    /// Wait until the events up to `mark` are taken, then until the worker
    /// has served every request made by then.
    async fn until_consumed(&self, mark: u64) {
        self.process
            .until_settled(|| {
                self.taken.load(Ordering::SeqCst) >= mark || self.ended.load(Ordering::SeqCst)
            })
            .await;
        let requested = self.process.request_mark();
        self.process
            .until_settled(|| self.process.served_through(requested))
            .await;
    }
}

/// One CA link served from this database: C `caLink` over a `dbChannelIO`.
struct LocalLink {
    state: Arc<LocalState>,
    channel: Arc<LocalChannel>,
    /// Where [`Self::monitor_string`] spawns, from the reading thread.
    bridge: BlockingBridge,
    _monitor: AbortOnDrop,
    /// C `evidString`, started by [`Self::monitor_string`].
    string_monitor: std::sync::OnceLock<AbortOnDrop>,
    _process: super::CpWorker,
}

/// Abort the wrapped task when dropped; a bare handle detaches.
struct AbortOnDrop(TaskHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl LocalLink {
    /// A link to `channel`, connected ([`subscribe`]) once `dbCaRun` has
    /// released it. `None` when the database captured no reactor to run the
    /// link on.
    fn open(db: &PvDatabase, channel: LocalChannel, link: LinkRef<'_>) -> Option<Self> {
        let bridge = db.inner.link_puts.network()?;
        let state = Arc::new(LocalState {
            cache: ArcSwapOption::empty(),
            stamp: ArcSwapOption::empty(),
            process: ExternalCpProcess::awaiting_first_event(link),
            delivery: parking_lot::Mutex::new(()),
            taken: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            queue: std::sync::OnceLock::new(),
            string_queue: std::sync::OnceLock::new(),
            string: ArcSwapOption::empty(),
            input_string: AtomicBool::new(false),
            meta: std::sync::OnceLock::new(),
            inlink: link.is_input(),
            input_native: AtomicBool::new(link.is_input()),
            input_wanted: crate::runtime::sync::Notify::new(),
        });
        let channel = Arc::new(channel);
        let weak = Arc::downgrade(&db.inner);
        let worker = state.clone();
        let process = db.spawn_cp_worker(async move {
            worker
                .process
                .run_worker(|| weak.upgrade().map(|inner| PvDatabase { inner }))
                .await
        });
        let monitor = bridge.spawn(subscribe(
            db.inner.ca_run.clone(),
            channel.clone(),
            state.clone(),
        ));
        Some(Self {
            state,
            channel,
            bridge: bridge.clone(),
            _monitor: AbortOnDrop(monitor),
            string_monitor: std::sync::OnceLock::new(),
            _process: process,
        })
    }

    /// C `isConnected`: a local channel connects at once
    /// (`dbChannelIO::initiateConnect`), here once its type and attributes
    /// are read.
    fn is_connected(&self) -> bool {
        self.state.meta.get().is_some()
    }

    /// `testInitReady`: connected, and a `DBF_INLINK` has its first event.
    fn init_ready(&self) -> bool {
        self.is_connected() && (!self.state.inlink || self.state.cache.load().is_some())
    }

    fn with_value<R>(&self, f: impl FnOnce(&Snapshot) -> R) -> Option<R> {
        self.state.cache.load().as_deref().map(f)
    }

    fn stamp(&self) -> Option<LinkStamp> {
        self.state.stamp.load().as_deref().copied()
    }

    /// `dbCaGetLink` (`dbCa.c:419-506`), which marks a connected link a
    /// native input before the empty-cache bail. The cache is read first:
    /// C queues `CA_MONITOR_NATIVE` only after this read's unlock, so the
    /// read that asks for the monitor never sees its first event.
    fn read(&self) -> Option<EpicsValue> {
        if !self.is_connected() {
            return None;
        }
        let value = self.with_value(|s| s.value.clone());
        self.state.note_read();
        value
    }

    /// `dbCaGetLink`'s `DBR_ENUM` → `DBR_STRING` branch (`dbCa.c:436-454`):
    /// the `DBR_STRING` monitor's last value, the monitor added by the first
    /// read that asks.
    fn enum_string(&self) -> EnumString {
        if self.channel.native_type() != DbFieldType::Enum {
            return EnumString::NotServed;
        }
        if !self.is_connected() {
            return EnumString::Pending;
        }
        let got = self.state.string.load_full();
        self.monitor_string();
        got.map_or(EnumString::Pending, |s| EnumString::Got((*s).clone()))
    }

    /// C `pvlOptInpString` with its `CA_MONITOR_STRING`
    /// (`dbCa.c:1236-1248`): subscribe once.
    fn monitor_string(&self) {
        self.string_monitor.get_or_init(|| {
            self.state.input_string.store(true, Ordering::Relaxed);
            AbortOnDrop(
                self.bridge
                    .spawn(subscribe_string(self.channel.clone(), self.state.clone())),
            )
        });
    }

    /// `dbCaPutLinkCallback` converts to the channel's native type and
    /// clamps to its element count (`dbCa.c:575-577`); a string staged
    /// through [`LinkSet::put_native`] is already converted.
    async fn write(
        &self,
        name: &[u8],
        holder: Option<&[u8]>,
        value: EpicsValue,
        op: LinkPutOp,
    ) -> (LinkPutDone, Result<(), String>) {
        let native = (self.channel.native_type(), self.channel.element_count());
        let value = match super::link_put_convert(name, value, native.0, native.1) {
            Ok(value) => value,
            Err(e) => return (LinkPutDone::Inline, Err(e)),
        };
        let written = self.channel.write(value, op).await;
        // `dbChannelIO::write` throws on a refused `dbChannel_put`, which
        // `ca_array_put` turns into ECA_PUTFAIL (`dbChannelIO.cpp:92-95`,
        // `oldChannelNotify.cpp:536`); a put callback's refusal is its
        // completion status.
        if op == LinkPutOp::Plain && written.1.is_err() {
            super::report_put_not_issued(crate::error::ECA_PUTFAIL, holder, name);
        }
        written
    }

    /// `dbcar`'s columns for this link (`dbCaTest.c:95-123`); the host is
    /// `cacChannel::getHostName`'s default, the local name
    /// (`cacChannel.cpp:120-129`).
    fn diagnostics(&self) -> LinkDiagnostics {
        LinkDiagnostics {
            connected: true,
            host: crate::runtime::env::ca_local_host_name(),
            read_access: true,
            write_access: true,
            n_disconnect: 0,
            input_native: self.state.input_native.load(Ordering::Relaxed),
            input_string: self.state.input_string.load(Ordering::Relaxed),
            output_native: false,
            output_string: false,
        }
    }
}

/// `dbCaTask`'s `CA_GET_ATTRIBUTES` and `CA_MONITOR_NATIVE` for a local
/// link (`dbCa.c:1209-1236`), serviced only once `dbCaRun` has run: the
/// attribute gets `connectionCallback` schedules, then the native
/// `DBR_TIME` monitor (`dbCa.c:1225-1229`), whose first event is the
/// channel's value now (`dbSubscriptionIO`). An I/O Intr source is read as
/// a turn on its own pump, behind every interrupt queued to it before; a
/// `DBF_INLINK` CP/CPP link's, first behind the edge sweeps its device
/// still owes it, while its holder's CP processes wait for that event.
/// Only a native input is monitored (`dbCa.c:865-867`): a `DBF_INLINK`
/// from here, any other link from its first read.
async fn subscribe(
    ca_run: Arc<super::processing::CaRunGate>,
    channel: Arc<LocalChannel>,
    state: Arc<LocalState>,
) {
    ca_run.reached().await;
    let mut first = state.inlink.then_some(FirstEvent::InitReady);
    if let ChannelTarget::RecordField { record, .. } = &channel.chan.target
        && state.inlink
        && state.process.processes_holder()
        && let Some(sweeps) = super::io_intr_turn::owed_sweeps(record)
    {
        // `dbCaRun` waits for this first event, and every CP process for
        // all of them; C reads the source at once, so a silent device must
        // hold neither. Only its holder's CP processes wait for it, while
        // they are still held.
        if let Some(late) = ca_run.late_link(&state.process) {
            first = Some(FirstEvent::Late(late));
        }
        sweeps.cleared().await;
    }
    let Some(db) = channel.db() else { return };
    if let ChannelTarget::RecordField { record, .. } = &channel.chan.target {
        db.io_intr_turn(record).await;
    }
    let native = channel.native_type();
    let attrs = (native != DbFieldType::String)
        .then(|| channel.read(DBR_CTRL_DOUBLE, 1))
        .flatten();
    let _ = state.meta.set(LinkMetadata::from_ca_attributes(
        Some(native),
        Some(channel.element_count()),
        attrs.as_ref(),
    ));
    let db = if state.inlink {
        db
    } else {
        // `connectionCallback`'s `testInitReady(pca,
        // DBCA_CALLBACK_INIT_WAIT_NATIVE)` (`dbCa.c` at `ef4829829`).
        drop(db);
        state.process.request_init_ready();
        state.until_input_native().await;
        let Some(db) = channel.db() else { return };
        db
    };
    match channel.subscribe(&db, native.time_dbr_type(), CA_LINK_EVENT_MASK) {
        Ok(monitor) => {
            drop(db);
            let _ = state
                .queue
                .set((monitor.reader.queue(), monitor.reader.id()));
            run_monitor(monitor, state, Feed::Native(first)).await;
        }
        // `dbCaTask`'s `ca_add_array_event` failure (`dbCa.c:1230-1233`):
        // the link stays without a value. Its init wait is released, where
        // C's `dbCaRun` would wait for it forever.
        Err(e) => {
            tracing::debug!("local CA link subscribe refused: {e:?}");
            if let Some(FirstEvent::InitReady) = first {
                state.process.request_init_ready();
            }
            state.ended.store(true, Ordering::SeqCst);
            state.process.notify_settled();
        }
    }
}

/// `dbCaTask`'s `CA_MONITOR_STRING` for a local link (`dbCa.c:1236-1248`).
async fn subscribe_string(channel: Arc<LocalChannel>, state: Arc<LocalState>) {
    let Some(db) = channel.db() else { return };
    match channel.subscribe(&db, DBR_TIME_STRING, CA_LINK_EVENT_MASK) {
        Ok(monitor) => {
            drop(db);
            let _ = state
                .string_queue
                .set((monitor.reader.queue(), monitor.reader.id()));
            run_monitor(monitor, state, Feed::String).await;
        }
        // `dbCaTask`'s `ca_add_array_event` failure: the string read keeps
        // failing.
        Err(e) => tracing::debug!("local CA link string subscribe refused: {e:?}"),
    }
}

/// Which of dbCa's two monitors a local link's events come from.
enum Feed {
    /// `evidNative`, with what its first event owes besides its process.
    Native(Option<FirstEvent>),
    /// `evidString`.
    String,
}

/// What a local link's first event owes besides its process.
enum FirstEvent {
    /// `CA_INIT_READY`, which `dbCaRun` waits for.
    InitReady,
    /// The end of a late link's hold on its holder's CP processes.
    Late(super::processing::LateLink),
}

/// C `dbContext`'s event task handing each `dbSubscriptionIO` event to
/// dbCa's `eventCallback`: refresh the cache, then add `CA_DBPROCESS`
/// (`dbCa.c:891`, `:958-962`); after a native monitor's first, what it owes.
async fn run_monitor(mut monitor: LocalMonitor, state: Arc<LocalState>, mut feed: Feed) {
    // Each event is taken and handed on under `delivery`, never across a
    // suspension.
    while std::future::poll_fn(|cx| {
        let _taking = state.delivery.lock();
        let event = match monitor.reader.poll_recv(cx) {
            std::task::Poll::Pending => return std::task::Poll::Pending,
            std::task::Poll::Ready(None) => return std::task::Poll::Ready(false),
            std::task::Poll::Ready(Some(event)) => event,
        };
        match &mut feed {
            Feed::Native(first) => {
                // dbCa's native monitor is of count 0 (`dbCa.c:1290-1293`).
                match monitor.state_notify(event, 0) {
                    Some(snapshot) => {
                        // A native ENUM event scans only while no string
                        // monitor exists (`dbCa.c:932-933`).
                        let scan = !(matches!(snapshot.value, EpicsValue::Enum(_))
                            && state.input_string.load(Ordering::Relaxed));
                        // The stamp first, so a read that sees this value
                        // sees its stamp.
                        state.stamp.store(Some(Arc::new(LinkStamp::of(
                            &snapshot.alarm,
                            snapshot.timestamp,
                        ))));
                        state.cache.store(Some(Arc::new(snapshot)));
                        if scan {
                            state.process.request();
                        }
                    }
                    // `callStateNotify`'s `ECA_GETFAIL`: dbCa keeps the last value.
                    None => {
                        tracing::debug!("local CA link event not deliverable, keeping last value")
                    }
                }
                match first.take() {
                    Some(FirstEvent::InitReady) => state.process.request_init_ready(),
                    Some(FirstEvent::Late(late)) => drop(late),
                    None => {}
                }
            }
            // `CA_MONITOR_STRING` asks for one element (`dbCa.c:1239`).
            Feed::String => match monitor.state_notify(event, 1) {
                Some(Snapshot {
                    value: EpicsValue::String(s),
                    alarm,
                    timestamp,
                    ..
                }) => {
                    state
                        .stamp
                        .store(Some(Arc::new(LinkStamp::of(&alarm, timestamp))));
                    state.string.store(Some(Arc::new(s)));
                    state.process.request();
                }
                _ => tracing::debug!("local CA link string event not deliverable, keeping last"),
            },
        }
        state.taken.fetch_add(1, Ordering::SeqCst);
        state.process.notify_settled();
        std::task::Poll::Ready(true)
    })
    .await
    {}
    if let Feed::Native(Some(FirstEvent::InitReady)) = feed {
        state.process.monitor_ended();
    }
    state.ended.store(true, Ordering::SeqCst);
    state.process.notify_settled();
}

/// C `dbChannelIO`: an open database channel plus the put-notify blocker
/// `dbContext` keeps for it. Holds the database weakly: the database owns
/// its links.
pub(super) struct LocalChannel {
    db: Weak<PvDatabaseInner>,
    chan: DbAccessChannel,
    /// C `dbPutNotifyBlocker`: one put callback at a time per channel,
    /// held until the put completes. Taken only by a channel a record opens
    /// itself ([`Self::issue_put`]); see [`Self::write`] for links.
    blocker: Arc<tokio::sync::Mutex<()>>,
}

impl LocalChannel {
    /// `dbContext::createChannel`'s `dbChannel_create`: `None` when the
    /// database does not serve `name`.
    pub(super) async fn create(db: &PvDatabase, name: &[u8]) -> Option<Self> {
        let chan = channel_io::channel_create(db, name, None).await.ok()?;
        Some(Self {
            db: Arc::downgrade(&db.inner),
            chan,
            blocker: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub(super) fn db(&self) -> Option<PvDatabase> {
        self.db.upgrade().map(|inner| PvDatabase { inner })
    }

    /// `dbChannelIO::nativeType` — `dbChannelExportCAType`.
    pub(super) fn native_type(&self) -> DbFieldType {
        DbFieldType::from_u16(self.chan.native_type.ca_wire_type())
            .expect("ca_wire_type is a CA DBF code")
    }

    /// `dbChannelIO::nativeElementCount` — `dbChannelElements`.
    pub(super) fn element_count(&self) -> u32 {
        self.chan.final_element_count
    }

    /// `dbContextReadNotifyCache::callReadNotify`
    /// (`dbContextReadNotifyCache.cpp:60-110`): `dbChannel_get` of
    /// `dbr_type`, `count` elements, completed synchronously.
    pub(super) fn read(&self, dbr_type: u16, count: u32) -> Option<Snapshot> {
        let snapshot = channel_io::get_full_snapshot(&self.db()?, &self.chan.target)?;
        let snapshot = prepare_read(
            &self.chan.target,
            self.chan.filter_chain(),
            self.chan.long_string_mode,
            self.chan.field_element_count,
            dbr_type,
            count,
            snapshot,
        )
        .ok()?;
        let mut buf = Vec::new();
        encode_dbr_into(&mut buf, dbr_type, &snapshot).ok()?;
        decode_counted(dbr_type, buf, snapshot.value.count(), count)
    }

    /// `dbSubscriptionIO` (`dbSubscriptionIO.cpp:38-56`): `db_add_event`
    /// with `mask`, then `db_post_single_event`. Each link owns its event
    /// context, where C's `dbContext` shares one.
    pub(super) fn subscribe(
        &self,
        db: &PvDatabase,
        dbr_type: u16,
        mask: u16,
    ) -> Result<LocalMonitor, SubscribeRefused> {
        let (pre_chain, notify) = self.state_notify(dbr_type);
        let user = EventUser::new();
        let reader = self.add_event(db, &user, mask, no_context(), pre_chain)?;
        Ok(LocalMonitor {
            _user: user,
            reader,
            notify,
        })
    }

    /// A `dbr_type` subscription's `callStateNotify`, and the pre chain its
    /// `db_add_event` runs.
    pub(super) fn state_notify(&self, dbr_type: u16) -> (FilterChain, StateNotify) {
        let (pre_chain, post_chain) = self.chan.filter_chain().split();
        let notify = StateNotify {
            stage: DeliveryStage::new(
                &self.chan.target,
                post_chain,
                self.chan.long_string_mode,
                self.chan.field_element_count,
                dbr_type,
            ),
            dbr_type,
            db: self.db.clone(),
            target: self.chan.target.clone(),
        };
        (pre_chain, notify)
    }

    /// `db_add_event` on `user`'s queue with `mask`, its entries carrying
    /// `context`, then `db_post_single_event`.
    pub(super) fn add_event(
        &self,
        db: &PvDatabase,
        user: &EventUser,
        mask: u16,
        context: &DeliveryContext,
        pre_chain: FilterChain,
    ) -> Result<EventReader, SubscribeRefused> {
        channel_io::add_event(
            db,
            &self.chan.target,
            user,
            0,
            self.chan.native_type,
            mask,
            context,
            pre_chain,
        )
    }

    /// `dbChannelIO::write` for a link: a plain put is `dbChannel_put`
    /// (`dbChannelIO.cpp:78-91`); a put callback completes when the record's
    /// processing does (`dbPutNotifyBlocker.cpp:132-176`). The channel is
    /// the link's own, as C's; its `dbPutNotifyBlocker` is not taken here
    /// because the link put queue already keeps one put in flight per link.
    async fn write(&self, value: EpicsValue, op: LinkPutOp) -> (LinkPutDone, Result<(), String>) {
        match op {
            LinkPutOp::Plain => (
                LinkPutDone::Callback,
                self.put(value, false)
                    .await
                    .map(|_| ())
                    .map_err(|r| format!("db_put_field() completed unsuccessfully: {}", r.error)),
            ),
            LinkPutOp::Async => {
                let (issued, status) = Issued::outcome(self.put(value, true).await);
                let status = status.map_err(|e| format!("put notify unsuccessful: {e}"));
                let done = match issued {
                    Issued::Record(ProcessCompletion::Unprocessed) => LinkPutDone::Inline,
                    _ => LinkPutDone::Callback,
                };
                let settled = issued.settle().await;
                (done, status.and(settled))
            }
        }
    }

    /// `dbChannelIO::write` issued: a plain put is done when this returns
    /// (`None`); a put callback returns its completion, which holds the
    /// channel's `dbPutNotifyBlocker` until the record's processing is
    /// done.
    pub(super) async fn issue_put(
        &self,
        value: EpicsValue,
        callback: bool,
    ) -> Result<Option<super::ca_client::PutCompletion>, String> {
        if !callback {
            return self
                .put(value, false)
                .await
                .map(|_| None)
                .map_err(|r| format!("db_put_field() completed unsuccessfully: {}", r.error));
        }
        self.put_callback(value)
            .await
            .map(Some)
            .map_err(|_| "put callback request timed out".to_string())
    }

    /// `dbChannelIO::write` with a notify: the completion, which holds the
    /// channel's `dbPutNotifyBlocker` until the record's processing is done.
    /// `Err` is the blocker's wait for the put in flight running out
    /// (`requestTimedOut`).
    pub(super) async fn put_callback(
        &self,
        value: EpicsValue,
    ) -> Result<super::ca_client::PutCompletion, crate::error::CaError> {
        let (completion, status, blocker) = self.put_notify(value).await?;
        Ok(Box::pin(async move {
            let _blocker = blocker;
            let settled = completion.settle().await;
            status.and(settled)
        }))
    }

    /// A put callback issued under the channel's `dbPutNotifyBlocker`, which
    /// the caller holds until the completion settles: the completion and the
    /// status it reports, a refusal's included (`putNotifyCompletion` reads
    /// `notifyError` once the process ends). `Err` is a put never issued.
    async fn put_notify(
        &self,
        value: EpicsValue,
    ) -> Result<(Issued, Result<(), String>, tokio::sync::OwnedMutexGuard<()>), crate::error::CaError>
    {
        let blocker = crate::runtime::task::timeout(
            PUT_NOTIFY_BLOCK_TIMEOUT,
            self.blocker.clone().lock_owned(),
        )
        .await
        .map_err(|_| crate::error::CaError::Timeout)?;
        let (issued, status) = Issued::outcome(self.put(value, true).await);
        let status = status.map_err(|e| format!("put notify unsuccessful: {e}"));
        Ok((issued, status, blocker))
    }

    async fn put(
        &self,
        value: EpicsValue,
        notify: bool,
    ) -> Result<Issued, crate::server::record::PutRefused> {
        let db = self
            .db()
            .ok_or_else(|| crate::error::CaError::ChannelNotFound("database gone".into()))?;
        loop {
            // A postponed put is sent again once the PV signals, as libca's
            // request waits behind the server's postponed circuit.
            let mut blocked = match channel_io::put(
                &db,
                &self.chan.target,
                self.chan.long_string_mode,
                value.clone(),
                notify,
                WriteContext::default,
            )
            .await?
            {
                PutStarted::Started(completion) => return Ok(Issued::Record(completion)),
                PutStarted::Posted(end) => return Ok(Issued::Pv(end)),
                // The ECA_PUTFAIL a pcas server answers it with.
                PutStarted::Failed(_) => {
                    return Err(
                        crate::error::CaError::WriteFailed(crate::error::ECA_PUTFAIL).into(),
                    );
                }
                PutStarted::Postponed(blocked) => blocked,
            };
            blocked.signalled().await;
        }
    }
}

/// A put the link issued: a record's processing, or a simple PV's write IO,
/// which posts its status.
enum Issued {
    Record(ProcessCompletion),
    Pv(WriteEnd),
}

impl Issued {
    /// A put callback's two answers: when it completes, and its status.
    fn outcome(
        put: Result<Self, crate::server::record::PutRefused>,
    ) -> (Self, crate::error::CaResult<()>) {
        match put {
            Ok(issued) => (issued, Ok(())),
            Err(refused) => (Self::Record(refused.completion), Err(refused.error)),
        }
    }

    /// Wait out a put callback's completion.
    async fn settle(self) -> Result<(), String> {
        match self {
            Self::Record(completion) => match completion.into_handle() {
                Some(done) => done
                    .await
                    .map_err(|_| "put notify unsuccessful: canceled".to_string()),
                None => Ok(()),
            },
            Self::Pv(end) => end
                .await
                .map_err(|status| format!("put notify unsuccessful: {status}")),
        }
    }
}

/// C `dbSubscriptionIO`: one installed event plus what `callStateNotify`
/// needs to hand each one over as a DBR value. Dropping it cancels the
/// event (`db_cancel_event`).
pub(super) struct LocalMonitor {
    /// The event context the reader's queue belongs to.
    _user: EventUser,
    pub(super) reader: EventReader,
    notify: StateNotify,
}

impl LocalMonitor {
    /// [`StateNotify::state_notify`].
    pub(super) fn state_notify(&self, event: MonitorEvent, count: u32) -> Option<Snapshot> {
        self.notify.state_notify(event, count)
    }
}

/// What `callStateNotify` needs to hand a subscription's event over as a
/// DBR value.
pub(super) struct StateNotify {
    stage: DeliveryStage,
    dbr_type: u16,
    db: Weak<PvDatabaseInner>,
    target: ChannelTarget,
}

impl StateNotify {
    /// `dbContext::callStateNotify` (`dbContext.cpp:141-189`): the event as
    /// a `dbr_type` value of `count` elements, 0 for its current count.
    pub(super) fn state_notify(&self, event: MonitorEvent, count: u32) -> Option<Snapshot> {
        let mut read = None;
        let update = with_live_options(self.dbr_type, Cow::Owned(event), &mut read, || {
            let db = PvDatabase {
                inner: self.db.upgrade()?,
            };
            channel_io::get_full_snapshot(&db, &self.target)
        })?;
        let mut log = None;
        let delivery = update.prepare(&self.stage, count, &mut log)?;
        let mut buf = Vec::new();
        delivery.encode_dbr_into(&mut buf, self.dbr_type).ok()?;
        decode_counted(self.dbr_type, buf, delivery.count(), count)
    }
}

/// `buf`, an encode of `carried` elements, as the `dbr_type` buffer
/// `dbChannel_get` fills for `count` (`db_access.c:121-138`): cut to
/// `count`, or zero padded to it; 0 keeps `carried`, `dbChannel_get_count`.
fn decode_counted(dbr_type: u16, mut buf: Vec<u8>, carried: u32, count: u32) -> Option<Snapshot> {
    let n = if count == 0 { carried } else { count } as usize;
    let size = dbr_buffer_size(dbr_type, native_type_for_dbr(dbr_type).ok()?, n);
    if buf.len() < size {
        buf.resize(size, 0);
    }
    decode_dbr(dbr_type, &buf, n).ok()
}

impl From<DbFieldType> for LinkDbfType {
    /// C `getDBFtype`'s `dbDBRoldToDBFnew[pca->dbrType]` (`dbCa.c:672`).
    fn from(t: DbFieldType) -> Self {
        match t {
            DbFieldType::String => Self::String,
            DbFieldType::Short => Self::Short,
            DbFieldType::Float => Self::Float,
            DbFieldType::Enum => Self::Enum,
            DbFieldType::Char => Self::Char,
            DbFieldType::Long => Self::Long,
            DbFieldType::Double => Self::Double,
            DbFieldType::Int64 => Self::Int64,
            DbFieldType::UInt64 => Self::UInt64,
            DbFieldType::UShort => Self::UShort,
            DbFieldType::ULong => Self::ULong,
            DbFieldType::UChar => Self::UChar,
        }
    }
}

impl LinkMetadata {
    /// dbCa's attribute cache from a channel's native type and count plus
    /// `attrs`, the fixed `DBR_CTRL_DOUBLE` get `getAttribEventCallback`
    /// caches (`dbCa.c:1042-1091`, issued for any non-string channel,
    /// `:878-880`), the only source of limits, precision and units. Alarm
    /// limits are `(lolo, lo, hi, hihi)`, C `getAlarmLimits` (`dbCa.c:729`).
    pub fn from_ca_attributes(
        dbf: Option<DbFieldType>,
        element_count: Option<u32>,
        attrs: Option<&Snapshot>,
    ) -> Self {
        let mut md = Self {
            dbf_type: dbf.map(LinkDbfType::from),
            element_count: element_count.map(|n| n as i64),
            ..Self::default()
        };
        if let Some(snap) = attrs {
            if let Some(d) = snap.display.as_ref() {
                md.graphic_limits = Some((d.lower_disp_limit, d.upper_disp_limit));
                md.alarm_limits = Some((
                    d.lower_alarm_limit,
                    d.lower_warning_limit,
                    d.upper_warning_limit,
                    d.upper_alarm_limit,
                ));
                md.precision = Some(d.precision);
                if !d.units.is_empty() {
                    md.units = Some(d.units.as_str_lossy().into_owned());
                }
                if !d.description.is_empty() {
                    md.description = Some(d.description.as_str_lossy().into_owned());
                }
            }
            if let Some(c) = snap.control.as_ref() {
                md.control_limits = Some((c.lower_ctrl_limit, c.upper_ctrl_limit));
            }
        }
        md
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a CTRL [`Snapshot`] carrying display + control metadata,
    /// the shape `get_with_metadata(DbrClass::Ctrl)` returns for a
    /// numeric PV.
    fn ctrl_snapshot() -> Snapshot {
        use crate::server::snapshot::{ControlInfo, DisplayInfo};
        let mut snap = Snapshot::new(
            EpicsValue::Double(0.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        snap.display = Some(DisplayInfo {
            units: "degC".into(),
            precision: 3,
            upper_disp_limit: 100.0,
            lower_disp_limit: -50.0,
            upper_alarm_limit: 90.0,    // hihi
            upper_warning_limit: 80.0,  // hi
            lower_warning_limit: -20.0, // lo
            lower_alarm_limit: -40.0,   // lolo
            ..Default::default()
        });
        snap.control = Some(ControlInfo {
            upper_ctrl_limit: 95.0,
            lower_ctrl_limit: -45.0,
        });
        snap
    }

    /// a numeric PV's CTRL attributes map into every
    /// `LinkMetadata` field. Pins the alarm-limit order to
    /// `(lolo, lo, hi, hihi)` (C `getAlarmLimits`), graphic/control to
    /// `(lower, upper)`, and the channel info to dbf type + element
    /// count.
    #[test]
    fn ca_attributes_numeric_maps_all_fields() {
        let snap = ctrl_snapshot();
        let md = LinkMetadata::from_ca_attributes(Some(DbFieldType::Double), Some(1), Some(&snap));
        assert_eq!(md.dbf_type, Some(LinkDbfType::Double));
        assert_eq!(md.element_count, Some(1));
        assert_eq!(md.graphic_limits, Some((-50.0, 100.0)));
        assert_eq!(md.control_limits, Some((-45.0, 95.0)));
        assert_eq!(md.alarm_limits, Some((-40.0, -20.0, 80.0, 90.0)));
        assert_eq!(md.precision, Some(3));
        assert_eq!(md.units.as_deref(), Some("degC"));
    }

    /// a String channel gets no attribute request at all — C
    /// `dbCa.c:878-880` sets `CA_GET_ATTRIBUTES` only when
    /// `pca->dbrType != DBR_STRING`, so `gotAttributes` stays FALSE and
    /// every getter returns -1 with the caller's buffer untouched. Here
    /// that is `attrs = None`, and every limit stays `None` so the owning
    /// record keeps its local default.
    #[test]
    fn ca_attributes_string_pv_has_no_limits() {
        let md = LinkMetadata::from_ca_attributes(Some(DbFieldType::String), Some(1), None);
        assert_eq!(md.dbf_type, Some(LinkDbfType::String));
        assert_eq!(md.element_count, Some(1));
        assert_eq!(md.graphic_limits, None);
        assert_eq!(md.control_limits, None);
        assert_eq!(md.alarm_limits, None);
        assert_eq!(md.precision, None);
        assert_eq!(md.units, None);
    }

    /// an enum target's fixed `DBR_CTRL_DOUBLE` reply carries its
    /// precision and limits, zero as C's are.
    #[test]
    fn ca_attributes_enum_target_takes_the_double_attributes() {
        use crate::server::snapshot::DisplayInfo;
        let mut attrs = Snapshot::new(
            EpicsValue::Double(1.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        attrs.display = Some(DisplayInfo::default());
        let md = LinkMetadata::from_ca_attributes(Some(DbFieldType::Enum), Some(1), Some(&attrs));
        assert_eq!(md.dbf_type, Some(LinkDbfType::Enum));
        assert_eq!(md.precision, Some(0));
        assert_eq!(md.graphic_limits, Some((0.0, 0.0)));
    }

    /// when the channel info fetch failed (`None` dbf /
    /// count) and there is no CTRL reply, every field is `None` — the
    /// link reports "no metadata yet" rather than fabricating zeros.
    #[test]
    fn ca_attributes_no_info_no_ctrl_is_all_none() {
        let md = LinkMetadata::from_ca_attributes(None, None, None);
        assert_eq!(md, LinkMetadata::default());
    }

    /// an empty `units` string is dropped to `None` (the
    /// remote carried no engineering units), not surfaced as `Some("")`.
    #[test]
    fn ca_attributes_empty_units_omitted() {
        use crate::server::snapshot::DisplayInfo;
        let mut snap = Snapshot::new(
            EpicsValue::Double(0.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        snap.display = Some(DisplayInfo {
            units: "".into(),
            precision: 0,
            ..Default::default()
        });
        let md = LinkMetadata::from_ca_attributes(Some(DbFieldType::Double), Some(1), Some(&snap));
        assert_eq!(md.units, None);
        assert_eq!(md.precision, Some(0));
    }

    /// C adds `CA_DBPROCESS` from a link's events only for a CP/CPP link
    /// (`dbCa.c:1008-1010`): an OUT link's and a plain input link's events
    /// request nothing of the `dbCaLink` worker.
    #[epics_macros_rs::epics_test]
    async fn only_a_cp_links_events_request_a_process() {
        let db = crate::server::ioc_builder::IocBuilder::new()
            .db_string(
                r#"
record(ao, "SRC") {}
record(calc, "CP") { field(INPA, "SRC CP") field(CALC, "A") }
record(calc, "IN") { field(INPA, "SRC CA") field(CALC, "A") }
record(ao, "OUT") { field(OUT, "SRC CA") }
"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap()
            .0;
        db.initialize_link_locality().await;
        db.setup_cp_links().await;
        db.setup_external_link_opens().await;
        db.process_cp_links_on_connect().await;
        let requests = |holder: &str, field: &str| {
            let (_, _, link) = db
                .record_link_fields(holder)
                .into_iter()
                .find(|(f, _, _)| f == field)
                .unwrap();
            let link = link.external().unwrap();
            let local = db.inner.ca_links.local(LinkRef::new(link.name, link.open));
            local.map(|l| l.state.process.request_mark())
        };
        db.put_pv_and_post("SRC", EpicsValue::Double(2.0))
            .await
            .unwrap();
        db.inner.ca_links.sync().await;
        assert_eq!(
            requests("CP", "INPA"),
            Some(2),
            "the first event and the put"
        );
        assert_eq!(requests("IN", "INPA"), Some(0));
        assert_eq!(requests("OUT", "OUT"), Some(0));
    }

    /// C monitors a CA link only as a native input: a `DBF_INLINK` from its
    /// connect (`dbCa.c:865-867`), any other link from its first read
    /// (`dbCa.c:455-457`). An OUT link adds no subscription to its target,
    /// and its put is admitted once it connects.
    #[epics_macros_rs::epics_test]
    async fn only_a_native_input_monitors_its_target() {
        let db = crate::server::ioc_builder::IocBuilder::new()
            .db_string(
                r#"
record(ao, "SRC") {}
record(calc, "IN") { field(INPA, "SRC CA") field(CALC, "A") }
record(ao, "OUT") { field(OUT, "SRC CA") }
"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap()
            .0;
        db.initialize_link_locality().await;
        db.setup_cp_links().await;
        db.setup_external_link_opens().await;
        db.process_cp_links_on_connect().await;
        let (_, _, out) = db
            .record_link_fields("OUT")
            .into_iter()
            .find(|(f, _, _)| f == "OUT")
            .unwrap();
        let out = out.external().unwrap();
        let out = LinkRef::new(out.name, out.open);
        let monitors = || {
            db.get_record("SRC")
                .unwrap()
                .read()
                .field_subscribers()
                .len()
        };
        assert_eq!(monitors(), 1, "the IN link's only");
        assert_eq!(
            db.inner.ca_links.put_admission(out),
            PutAdmission::Connected
        );

        assert_eq!(db.inner.ca_links.get_cached_value(out), None);
        let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
        while db.inner.ca_links.get_cached_value(out).is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the first read monitors the link"
            );
            crate::runtime::task::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(monitors(), 2);
    }

    /// A link's monitor and worker start as its route is published, under
    /// the routes lock: an open that waits on the lock, or loses the race
    /// for its link, has subscribed to nothing.
    #[epics_macros_rs::epics_test]
    async fn a_links_monitor_starts_only_with_its_route() {
        let db = crate::server::ioc_builder::IocBuilder::new()
            .db_string(
                r#"record(calc, "H") { field(INPA, "LATE CP") field(CALC, "A") }"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap()
            .0;
        db.initialize_link_locality().await;
        db.process_cp_links_on_connect().await;
        let monitors = || {
            db.get_record("LATE")
                .map_or(0, |rec| rec.read().field_subscribers().len())
        };
        // The open's channel lookup waits in the search for LATE, past the
        // open's route check.
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let go = Arc::new(crate::runtime::sync::Notify::new());
        let resolver: super::super::SearchResolver = Arc::new({
            let go = go.clone();
            move |_, _| {
                let (entered_tx, go) = (entered_tx.clone(), go.clone());
                Box::pin(async move {
                    let _ = entered_tx.send(());
                    go.notified().await;
                    true
                })
            }
        });
        db.set_search_resolver(resolver).await;
        let opener = std::thread::spawn({
            let db = db.clone();
            move || {
                let (_, _, inp) = db
                    .record_link_fields("H")
                    .into_iter()
                    .find(|(f, _, _)| f == "INPA")
                    .unwrap();
                let inp = inp.external().unwrap();
                let open = db
                    .inner
                    .ca_links
                    .connect_link(LinkRef::new(inp.name, inp.open), None);
                crate::runtime::task::block_on_sync(open).unwrap();
            }
        });
        entered.recv_timeout(fact_budget::FACT_BUDGET).unwrap();
        let (held_tx, held) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn({
            let db = db.clone();
            move || {
                let _routes = db.inner.ca_links.routes.write();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv();
            }
        });
        held.recv().unwrap();
        db.add_record(
            "LATE",
            Box::new(crate::server::records::ai::AiRecord::new(0.0)),
        )
        .await
        .unwrap();
        go.notify_one();
        for _ in 0..100 {
            crate::runtime::task::sleep(Duration::from_millis(5)).await;
            assert_eq!(monitors(), 0, "subscribed before its route is published");
        }
        release.send(()).unwrap();
        holder.join().unwrap();
        let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
        while monitors() == 0 {
            assert!(std::time::Instant::now() < deadline, "the open subscribes");
            crate::runtime::task::sleep(Duration::from_millis(5)).await;
        }
        opener.join().unwrap();
        assert_eq!(monitors(), 1);
    }

    async fn until(done: impl Fn() -> bool, what: &str) {
        let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out until {what}"
            );
            crate::runtime::task::sleep(Duration::from_millis(5)).await;
        }
    }

    /// `H` (FLNK `CNT`, which counts its processes) after iocInit, with
    /// its INP replaced by `text` as a put installs it, before the put's
    /// relink.
    async fn holder_with_new_inp(text: &str) -> Arc<PvDatabase> {
        let db = crate::server::ioc_builder::IocBuilder::new()
            .db_string(
                r#"
record(ao, "SRC") { field(VAL, "2") }
record(ai, "H") { field(FLNK, "CNT") }
record(calc, "CNT") { field(CALC, "VAL+1") }
"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap()
            .0;
        db.initialize_link_locality().await;
        db.setup_cp_links().await;
        db.setup_external_link_opens().await;
        db.process_cp_links_on_connect().await;
        let rec = db.get_record("H").unwrap();
        let mut inst = rec.write();
        inst.common.text.inp = text.into();
        inst.install_link_cache("INP", Arc::new(crate::server::record::parse_link_v2(text)))
            .expect("a common link");
        drop(inst);
        db
    }

    fn inp_of_h(db: &PvDatabase) -> crate::server::record::ParsedLink {
        db.record_link_fields("H")
            .into_iter()
            .find(|(f, _, _)| f == "INP")
            .unwrap()
            .2
    }

    fn processes_of_h(db: &PvDatabase) -> f64 {
        match db.get_pv("CNT") {
            Ok(EpicsValue::Double(n)) => n,
            other => panic!("CNT reads {other:?}"),
        }
    }

    /// C's link has its holder from `dbCaAddLink` on (`dbCa.c:958-962`): a
    /// first event whose pass ran before the link's CP edge was published
    /// still processes the holder, once the edge joins.
    #[epics_macros_rs::epics_test]
    async fn a_first_event_before_its_edge_processes_the_holder() {
        let db = holder_with_new_inp("SRC CP").await;
        let inp = inp_of_h(&db);
        let inp = inp.external().unwrap();
        assert!(db.stage_external_link_open(inp));
        let first_pass = || {
            db.inner
                .ca_links
                .local(LinkRef::new(inp.name, inp.open))
                .is_some_and(|local| local.state.process.served_through(1))
        };
        until(first_pass, "the first event's pass ran").await;
        assert_eq!(processes_of_h(&db), 0.0, "a pass with no edge");
        db.relink_cp_link(b"H", "INP");
        until(|| processes_of_h(&db) == 1.0, "the join processes H").await;
        db.ca_sync().await;
        assert_eq!(processes_of_h(&db), 1.0);
        assert_eq!(db.get_pv("H").ok(), Some(EpicsValue::Double(2.0)));
        db.inner
            .ca_links
            .join_cp_link(LinkRef::new(inp.name, inp.open));
        db.ca_sync().await;
        assert_eq!(processes_of_h(&db), 1.0, "the miss is owed once");
    }

    /// A join owes only a request a pass found no edge for: a link opened
    /// after its edge, and joined before it opened and again after, has its
    /// first event processed once.
    #[epics_macros_rs::epics_test]
    async fn a_join_owes_only_a_missed_request() {
        let db = holder_with_new_inp("LATE CP").await;
        // LATE does not exist yet: the relink publishes the edge and joins
        // a link it could not open.
        db.relink_cp_link(b"H", "INP");
        db.add_record(
            "LATE",
            Box::new(crate::server::records::ai::AiRecord::new(2.0)),
        )
        .await
        .unwrap();
        let inp = inp_of_h(&db);
        let inp = inp.external().unwrap();
        assert!(db.stage_external_link_open(inp));
        until(|| processes_of_h(&db) == 1.0, "the first event processes H").await;
        db.inner
            .ca_links
            .join_cp_link(LinkRef::new(inp.name, inp.open));
        db.ca_sync().await;
        assert_eq!(processes_of_h(&db), 1.0, "a pass that found its edge");
        assert_eq!(db.get_pv("H").ok(), Some(EpicsValue::Double(2.0)));
    }

    /// The relink's join of a link to a name this database does not serve
    /// reaches the network half, whose link runs its own processor.
    #[epics_macros_rs::epics_test]
    async fn a_join_of_a_link_not_served_here_reaches_the_network_half() {
        #[derive(Default)]
        struct Joins(parking_lot::Mutex<Vec<LinkSerial>>);
        #[async_trait::async_trait]
        impl LinkSet for Joins {
            fn is_connected(&self, _: LinkRef<'_>) -> bool {
                false
            }
            fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
                None
            }
            async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
                None
            }
            fn join_cp_link(&self, link: LinkRef<'_>) {
                self.0.lock().push(link.serial());
            }
        }
        let db = holder_with_new_inp("FAR CP").await;
        let joins = Arc::new(Joins::default());
        db.register_link_set("ca", joins.clone()).await;
        db.relink_cp_link(b"H", "INP");
        let inp = inp_of_h(&db);
        assert_eq!(*joins.0.lock(), [inp.external().unwrap().open.serial()]);
    }

    /// every CA native field type maps to its
    /// `LinkDbfType`.
    #[test]
    fn link_dbf_type_covers_every_variant() {
        assert_eq!(LinkDbfType::from(DbFieldType::String), LinkDbfType::String);
        assert_eq!(LinkDbfType::from(DbFieldType::Short), LinkDbfType::Short);
        assert_eq!(LinkDbfType::from(DbFieldType::Float), LinkDbfType::Float);
        assert_eq!(LinkDbfType::from(DbFieldType::Enum), LinkDbfType::Enum);
        assert_eq!(LinkDbfType::from(DbFieldType::Char), LinkDbfType::Char);
        assert_eq!(LinkDbfType::from(DbFieldType::Long), LinkDbfType::Long);
        assert_eq!(LinkDbfType::from(DbFieldType::Double), LinkDbfType::Double);
        assert_eq!(LinkDbfType::from(DbFieldType::Int64), LinkDbfType::Int64);
        assert_eq!(LinkDbfType::from(DbFieldType::UInt64), LinkDbfType::UInt64);
    }
}
