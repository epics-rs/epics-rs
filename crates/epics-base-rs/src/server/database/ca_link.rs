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
use super::link_set::{
    DynLinkSet, LinkDbfType, LinkDiagnostics, LinkMetadata, LinkPutOp, LinkSet, PutAdmission,
};
use super::{CaLinkInit, CpTarget, ExternalCpProcess, PvDatabase, PvDatabaseInner};
use crate::runtime::task::TaskHandle;
use crate::server::event_queue::{AttachId, EvQue, EventReader, EventUser, no_context};
use crate::server::pv::{MonitorEvent, SubscribeRefused, WriteContext};
use crate::server::snapshot::Snapshot;
use crate::types::{
    DBR_CTRL_DOUBLE, DbFieldType, EpicsValue, RecordName, decode_dbr, encode_dbr_into,
};

/// dbCa's monitor mask, `ca_add_array_event`'s `DBE_VALUE | DBE_ALARM`
/// (`dbCa.c:1225-1229`, `cadef.h:2004-2012`): never `DBE_LOG`.
pub const CA_LINK_EVENT_MASK: u16 =
    crate::server::recgbl::EventMask::VALUE.bits() | crate::server::recgbl::EventMask::ALARM.bits();

/// How long a put callback waits for the channel's previous one before it
/// gives up — `dbPutNotifyBlocker::initiatePutNotify`'s 30 s
/// (`dbPutNotifyBlocker.cpp:146-149`).
const PUT_NOTIFY_BLOCK_TIMEOUT: Duration = Duration::from_secs(30);

fn strip_ca_scheme(name: &[u8]) -> &[u8] {
    name.strip_prefix(b"ca://").unwrap_or(name)
}

/// Where `ca_create_channel` sent a name.
enum Route {
    Local(Arc<LocalLink>),
    Network,
}

/// The `ca` scheme's link set: every `ca` lset call goes through here, and
/// the network lset registered under `ca` is reached only for a name the
/// database does not serve.
pub(crate) struct CaLinks {
    db: Weak<PvDatabaseInner>,
    routes: RwLock<HashMap<RecordName, Route>>,
    network: RwLock<Option<DynLinkSet>>,
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
            ca_run,
        }
    }

    /// Install the half that serves names this database does not.
    pub(crate) fn set_network(&self, lset: DynLinkSet) {
        *self.network.write() = Some(lset);
    }

    /// Whether a `ca` link to `name` has anything to connect to: a name
    /// this database serves, or any name once a network half is registered.
    pub(crate) fn addresses(&self, name: &[u8]) -> bool {
        self.network.read().is_some() || self.serves(strip_ca_scheme(name))
    }

    /// `name` is, or on open would be, a local link: `dbChannelTest`
    /// (`dbLink.c:125`) until the open has decided it. A name the database
    /// does not serve is decided here, once, as C decides it once per link.
    fn serves(&self, name: &[u8]) -> bool {
        match self.routes.read().get(name) {
            Some(Route::Local(_)) => return true,
            Some(Route::Network) => return false,
            None => {}
        }
        let Some(inner) = self.db.upgrade() else {
            return false;
        };
        if (PvDatabase { inner }).has_name_no_resolve(name) {
            return true;
        }
        self.routes
            .write()
            .entry(name.into())
            .or_insert(Route::Network);
        false
    }

    fn network(&self) -> Option<DynLinkSet> {
        self.network.read().clone()
    }

    /// The network half's CA client, for a channel a record opens itself.
    pub(super) fn client_service(&self) -> Option<Arc<dyn super::ca_client::CaClientService>> {
        self.network.read().as_ref()?.ca_client_service()
    }

    /// `local` over the name's local link, else `net` over the network half
    /// (`None` with none registered, or before `dbCaRun`).
    fn answer<R>(
        &self,
        name: &[u8],
        local: impl FnOnce(&LocalLink) -> R,
        net: impl FnOnce(&dyn LinkSet) -> Option<R>,
    ) -> Option<R> {
        if !self.ca_run.has_run() {
            return None;
        }
        self.answer_paused(name, local, net)
    }

    /// [`Self::answer`] before `dbCaRun` too: for the wait `dbCaRun` itself
    /// makes on the links' first events.
    fn answer_paused<R>(
        &self,
        name: &[u8],
        local: impl FnOnce(&LocalLink) -> R,
        net: impl FnOnce(&dyn LinkSet) -> Option<R>,
    ) -> Option<R> {
        let name = strip_ca_scheme(name);
        if let Some(Route::Local(link)) = self.routes.read().get(name) {
            return Some(local(link));
        }
        net(self.network.read().as_deref()?)
    }

    /// `ca_create_channel`: the link `name` already has, else
    /// `dbChannel_create`, else the network context. `None` for a network
    /// name, or when the database is gone or captured no reactor.
    async fn open(&self, name: &[u8]) -> Option<Arc<LocalLink>> {
        if let Some(route) = self.routes.read().get(name) {
            return match route {
                Route::Local(link) => Some(link.clone()),
                Route::Network => None,
            };
        }
        let db = PvDatabase {
            inner: self.db.upgrade()?,
        };
        let route = match LocalChannel::create(&db, name).await {
            Some(channel) => Route::Local(Arc::new(LocalLink::open(&db, channel, name)?)),
            None => Route::Network,
        };
        // Two first opens converge on one link; the loser's drops.
        match self.routes.write().entry(name.into()).or_insert(route) {
            Route::Local(link) => Some(link.clone()),
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
            .filter_map(|route| match route {
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
    fn is_connected(&self, name: &[u8]) -> bool {
        self.answer(name, LocalLink::is_connected, |n| {
            Some(n.is_connected(name))
        })
        .unwrap_or(false)
    }

    fn init_ready(&self, name: &[u8]) -> bool {
        self.answer_paused(name, LocalLink::is_connected, |n| Some(n.init_ready(name)))
            .unwrap_or(false)
    }

    async fn get_value(&self, name: &[u8]) -> Option<EpicsValue> {
        if !self.ca_run.has_run() {
            return None;
        }
        let name = strip_ca_scheme(name);
        match self.open(name).await {
            Some(link) => link.read(),
            None => self.network()?.get_value(name).await,
        }
    }

    fn get_cached_value(&self, name: &[u8]) -> Option<EpicsValue> {
        self.answer(name, LocalLink::read, |n| Some(n.get_cached_value(name)))
            .flatten()
    }

    fn get_failure_disconn(&self, name: &[u8]) -> bool {
        self.answer(name, |_| false, |n| Some(n.get_failure_disconn(name)))
            .unwrap_or(false)
    }

    async fn connect_link(&self, name: &[u8], init: Option<CaLinkInit>) {
        let name = strip_ca_scheme(name);
        match self.open(name).await {
            Some(link) => {
                if let Some(init) = init {
                    link.state.process.adopt_init(init);
                }
            }
            // With no network half the link can never connect; dropping
            // `init` releases `dbCaRun` rather than holding it forever.
            None => match self.network() {
                Some(net) => net.connect_link(name, init).await,
                None => drop(init),
            },
        }
    }

    /// A link joining a local channel that already delivered its first
    /// event gets none of its own (`dbCa.c:958-962`), so it asks here.
    fn cp_link_added(&self, name: &[u8], holder: CpTarget) {
        let name = strip_ca_scheme(name);
        if let Some(Route::Local(link)) = self.routes.read().get(name) {
            if link.is_connected() {
                link.state.process.request_holder(holder);
            }
            return;
        }
        if let Some(net) = self.network.read().as_deref() {
            net.cp_link_added(name, holder);
        }
    }

    /// `dbCaPutLinkCallback`'s `!pca->isConnected || !pca->hasWriteAccess`
    /// (`dbCa.c:529-532`); a local channel has every right
    /// (`cacChannel::accessRights`). An unopened name is staged, and its
    /// put performs the open.
    fn put_admission(&self, name: &[u8]) -> PutAdmission {
        if !self.ca_run.has_run() {
            return PutAdmission::Refused;
        }
        let name = strip_ca_scheme(name);
        match self.routes.read().get(name) {
            Some(Route::Local(link)) if link.is_connected() => return PutAdmission::Connected,
            Some(Route::Local(_)) => return PutAdmission::Refused,
            _ => {}
        }
        match self.network.read().as_deref() {
            Some(net) => net.put_admission(name),
            None if self.serves(name) => PutAdmission::Unopened,
            None => PutAdmission::Refused,
        }
    }

    async fn put_value(&self, name: &[u8], value: EpicsValue, op: LinkPutOp) -> Result<(), String> {
        let name = strip_ca_scheme(name);
        if !self.ca_run.has_run() {
            return Err(format!(
                "CA link {} not connected",
                String::from_utf8_lossy(name)
            ));
        }
        match self.open(name).await {
            Some(link) => link.write(value, op).await,
            None => match self.network() {
                Some(net) => net.put_value(name, value, op).await,
                None => Err(format!(
                    "CA link {} not connected",
                    String::from_utf8_lossy(name)
                )),
            },
        }
    }

    async fn flush_puts(&self) {
        if let Some(net) = self.network() {
            net.flush_puts().await;
        }
    }

    fn alarm_message(&self, name: &[u8]) -> Option<Arc<str>> {
        self.answer(name, |_| None, |n| Some(n.alarm_message(name)))
            .flatten()
    }

    fn alarm_severity(&self, name: &[u8]) -> Option<i32> {
        self.answer(
            name,
            |link| {
                link.with_value(|s| s.alarm.severity as i32)
                    .filter(|&s| s > 0)
            },
            |n| Some(n.alarm_severity(name)),
        )
        .flatten()
    }

    fn alarm_status(&self, name: &[u8]) -> Option<i32> {
        self.answer(
            name,
            |link| link.with_value(|s| s.alarm.status as i32),
            |n| Some(n.alarm_status(name)),
        )
        .flatten()
    }

    fn remote_alarm(&self, name: &[u8]) -> Option<super::RemoteAlarm> {
        self.answer(name, |_| None, |n| Some(n.remote_alarm(name)))
            .flatten()
    }

    /// CA's `DBR_TIME` carries no user tag.
    fn time_stamp(&self, name: &[u8]) -> Option<(i64, i32, u64)> {
        self.answer(
            name,
            |link| {
                link.with_value(|s| {
                    let t = s.timestamp.since_unix_epoch();
                    (t.as_secs() as i64, t.subsec_nanos() as i32, 0)
                })
            },
            |n| Some(n.time_stamp(name)),
        )
        .flatten()
    }

    fn link_metadata(&self, name: &[u8]) -> Option<LinkMetadata> {
        self.answer(
            name,
            |link| Some(link.meta.clone()),
            |n| Some(n.link_metadata(name)),
        )
        .flatten()
    }

    fn link_names(&self) -> Vec<RecordName> {
        let net = self.network();
        let mut names: Vec<RecordName> = self
            .routes
            .read()
            .iter()
            .filter(|(_, route)| net.is_none() || matches!(route, Route::Local(_)))
            .map(|(name, _)| name.clone())
            .collect();
        if let Some(net) = net {
            names.extend(net.link_names());
        }
        names
    }

    /// A local link is up from its creation (`dbChannelIO::initiateConnect`
    /// calls `connectNotify` at once) and never disconnects. A network name
    /// with no network half reports `None`: never connected.
    async fn link_diagnostics(&self, name: &[u8]) -> Option<LinkDiagnostics> {
        let name = strip_ca_scheme(name);
        let local = match self.routes.read().get(name) {
            Some(Route::Local(link)) => Some(link.clone()),
            _ => None,
        };
        match local {
            Some(link) => Some(link.diagnostics()),
            None => self.network()?.link_diagnostics(name).await,
        }
    }
}

/// What a local link's monitor task shares with the link: C `caLink`'s
/// cached value and its `CA_DBPROCESS` request.
struct LocalState {
    cache: ArcSwapOption<Snapshot>,
    process: ExternalCpProcess,
    /// Held while an event is taken off the queue, handed to `process` and
    /// counted, so [`Self::mark`] never sees one taken but not counted.
    delivery: parking_lot::Mutex<()>,
    /// Events taken off the queue.
    taken: AtomicU64,
    /// The monitor has ended: no queued event will be taken.
    ended: AtomicBool,
    /// The monitor's queue slot, read for its `npend`.
    queue: (std::pin::Pin<Arc<EvQue>>, AttachId),
}

impl LocalState {
    /// The `dbCaSync` mark: the count `taken` reaches once every event
    /// queued now has been taken.
    fn mark(&self) -> u64 {
        let _taking = self.delivery.lock();
        self.taken.load(Ordering::SeqCst) + self.queue.0.npend(self.queue.1) as u64
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
    /// `CA_GET_ATTRIBUTES`, answered once: a local channel connects once.
    meta: LinkMetadata,
    /// C `pvlOptInpNative` (`dbCa.c:456`).
    input_native: AtomicBool,
    _monitor: AbortOnDrop,
    _process: AbortOnDrop,
}

/// Abort the wrapped task when dropped; a bare handle detaches.
struct AbortOnDrop(TaskHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl LocalLink {
    /// The `DBR_TIME` native monitor dbCa asks for (`dbCa.c:1225-1229`),
    /// whose first event is the current value (`dbSubscriptionIO`), and the
    /// attribute gets `connectionCallback` schedules on the connect.
    /// `None` when the database captured no reactor to run the link on.
    fn open(db: &PvDatabase, channel: LocalChannel, name: &[u8]) -> Option<Self> {
        let bridge = db.inner.link_puts.network()?;
        let native = channel.native_type();
        let monitor = match channel.subscribe(db, native.time_dbr_type(), CA_LINK_EVENT_MASK) {
            Ok(monitor) => monitor,
            Err(e) => {
                tracing::debug!(pv = %String::from_utf8_lossy(name), "local CA link subscribe refused: {e:?}");
                return None;
            }
        };
        let attrs = (native != DbFieldType::String)
            .then(|| channel.read(DBR_CTRL_DOUBLE, 1))
            .flatten();
        let labels = (native == DbFieldType::Enum)
            .then(|| channel.read(native.ctrl_dbr_type(), 1))
            .flatten();
        let meta = LinkMetadata::from_ca_attributes(
            Some(native),
            Some(channel.element_count()),
            attrs.as_ref(),
            labels.as_ref(),
        );
        let state = Arc::new(LocalState {
            cache: ArcSwapOption::empty(),
            process: ExternalCpProcess::awaiting_first_event(name),
            delivery: parking_lot::Mutex::new(()),
            taken: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            queue: (monitor.reader.queue(), monitor.reader.id()),
        });
        let weak = Arc::downgrade(&db.inner);
        let worker = state.clone();
        let process = bridge.spawn(async move {
            worker
                .process
                .run_worker(|| weak.upgrade().map(|inner| PvDatabase { inner }))
                .await
        });
        let monitor = bridge.spawn(run_monitor(monitor, state.clone()));
        Some(Self {
            state,
            channel: Arc::new(channel),
            meta,
            input_native: AtomicBool::new(false),
            _monitor: AbortOnDrop(monitor),
            _process: AbortOnDrop(process),
        })
    }

    /// Connected with its first event cached: `dbCaGetLink`'s readable gate.
    fn is_connected(&self) -> bool {
        self.state.cache.load().is_some()
    }

    fn with_value<R>(&self, f: impl FnOnce(&Snapshot) -> R) -> Option<R> {
        self.state.cache.load().as_deref().map(f)
    }

    /// `dbCaGetLink` (`dbCa.c:419-506`), which marks the link a native input
    /// before the empty-cache bail.
    fn read(&self) -> Option<EpicsValue> {
        self.input_native.store(true, Ordering::Relaxed);
        self.with_value(|s| s.value.clone())
    }

    /// `dbCaPutLinkCallback` converts to the channel's native type and
    /// clamps to its element count (`dbCa.c:575-577`).
    async fn write(&self, value: EpicsValue, op: LinkPutOp) -> Result<(), String> {
        let mut value = value.into_type(self.channel.native_type());
        value.truncate(self.channel.element_count() as usize);
        self.channel.write(value, op).await
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
            input_native: self.input_native.load(Ordering::Relaxed),
            input_string: false,
            output_native: false,
            output_string: false,
        }
    }
}

/// C `dbContext`'s event task handing each `dbSubscriptionIO` event to
/// dbCa's `eventCallback`: refresh the cache, then add `CA_DBPROCESS`
/// (`dbCa.c:891`, `:958-962`); after the first, `CA_INIT_READY`.
async fn run_monitor(mut monitor: LocalMonitor, state: Arc<LocalState>) {
    let mut first = true;
    // Each event is taken and handed on under `delivery`, never across a
    // suspension.
    while std::future::poll_fn(|cx| {
        let _taking = state.delivery.lock();
        let event = match monitor.reader.poll_recv(cx) {
            std::task::Poll::Pending => return std::task::Poll::Pending,
            std::task::Poll::Ready(None) => return std::task::Poll::Ready(false),
            std::task::Poll::Ready(Some(event)) => event,
        };
        match monitor.state_notify(event) {
            Some(snapshot) => {
                state.cache.store(Some(Arc::new(snapshot)));
                state.process.request();
            }
            // `callStateNotify`'s `ECA_GETFAIL`: dbCa keeps the last value.
            None => tracing::debug!("local CA link event not deliverable, keeping last value"),
        }
        if std::mem::take(&mut first) {
            state.process.request_init_ready();
        }
        state.taken.fetch_add(1, Ordering::SeqCst);
        state.process.notify_settled();
        std::task::Poll::Ready(true)
    })
    .await
    {}
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
    /// held until the put completes.
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
        );
        let mut buf = Vec::new();
        encode_dbr_into(&mut buf, dbr_type, &snapshot).ok()?;
        let n = if count == 0 {
            snapshot.value.count()
        } else {
            count
        };
        decode_dbr(dbr_type, &buf, n as usize).ok()
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
        let (pre_chain, post_chain) = self.chan.filter_chain().split();
        let user = EventUser::new();
        let reader = channel_io::add_event(
            db,
            &self.chan.target,
            &user,
            0,
            self.chan.native_type,
            mask,
            no_context(),
            pre_chain,
        )?;
        Ok(LocalMonitor {
            _user: user,
            reader,
            stage: DeliveryStage::new(
                post_chain,
                self.chan.long_string_mode,
                self.chan.field_element_count,
            ),
            dbr_type,
            db: self.db.clone(),
            target: self.chan.target.clone(),
        })
    }

    /// `dbChannelIO::write`: a plain put is `dbChannel_put`
    /// (`dbChannelIO.cpp:78-91`); a put callback goes through the
    /// `dbPutNotifyBlocker` and completes when the record's processing does
    /// (`dbPutNotifyBlocker.cpp:132-176`).
    async fn write(&self, value: EpicsValue, op: LinkPutOp) -> Result<(), String> {
        match op {
            LinkPutOp::Plain => self
                .put(value, false)
                .await
                .map(|_| ())
                .map_err(|e| format!("db_put_field() completed unsuccessfully: {e}")),
            LinkPutOp::Async => match self.issue_put(value, true).await? {
                Some(done) => done.await,
                None => Ok(()),
            },
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
                .map_err(|e| format!("db_put_field() completed unsuccessfully: {e}"));
        }
        let blocker = crate::runtime::task::timeout(
            PUT_NOTIFY_BLOCK_TIMEOUT,
            self.blocker.clone().lock_owned(),
        )
        .await
        .map_err(|_| "put callback request timed out".to_string())?;
        let completion = self
            .put(value, true)
            .await
            .map_err(|e| format!("put notify unsuccessful: {e}"))?;
        Ok(Some(Box::pin(async move {
            let _blocker = blocker;
            if let Some(done) = completion.into_handle() {
                done.await
                    .map_err(|_| "put notify unsuccessful: canceled".to_string())?;
            }
            Ok(())
        })))
    }

    async fn put(
        &self,
        value: EpicsValue,
        notify: bool,
    ) -> crate::error::CaResult<crate::server::record::ProcessCompletion> {
        let db = self
            .db()
            .ok_or_else(|| crate::error::CaError::ChannelNotFound("database gone".into()))?;
        channel_io::put(
            &db,
            &self.chan.target,
            self.chan.long_string_mode,
            value,
            notify,
            WriteContext::default,
        )
        .await
    }
}

/// C `dbSubscriptionIO`: one installed event plus what `callStateNotify`
/// needs to hand each one over as a DBR value. Dropping it cancels the
/// event (`db_cancel_event`).
pub(super) struct LocalMonitor {
    /// The event context the reader's queue belongs to.
    _user: EventUser,
    pub(super) reader: EventReader,
    stage: DeliveryStage,
    dbr_type: u16,
    db: Weak<PvDatabaseInner>,
    target: ChannelTarget,
}

impl LocalMonitor {
    /// `dbContext::callStateNotify` (`dbContext.cpp:141-189`): the event as
    /// a `dbr_type` value of its current element count.
    pub(super) fn state_notify(&self, event: MonitorEvent) -> Option<Snapshot> {
        let mut read = None;
        let update = with_live_options(self.dbr_type, Cow::Owned(event), &mut read, || {
            let db = PvDatabase {
                inner: self.db.upgrade()?,
            };
            channel_io::get_full_snapshot(&db, &self.target)
        })?;
        let mut log = None;
        let delivery = update.prepare(&self.stage, 0, &mut log)?;
        let mut buf = Vec::new();
        delivery.encode_dbr_into(&mut buf, self.dbr_type).ok()?;
        decode_dbr(self.dbr_type, &buf, delivery.count() as usize).ok()
    }
}

impl From<DbFieldType> for LinkDbfType {
    /// C `getDBFtype`'s `dbDBRoldToDBFnew[pca->dbrType]` (`dbCa.c:666`).
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
    /// two gets: `attrs`, the fixed `DBR_CTRL_DOUBLE` get
    /// `getAttribEventCallback` caches (`dbCa.c:1042-1091`, issued for any
    /// non-string channel, `:878-880`), the only source of limits,
    /// precision and units; and `labels`, an enum channel's native
    /// `DBR_CTRL_ENUM`, the only source of the choice table (C renders a
    /// remote enum through a second `DBR_STRING` monitor instead). Alarm
    /// limits are `(lolo, lo, hi, hihi)`, C `getAlarmLimits` (`dbCa.c:729`).
    pub fn from_ca_attributes(
        dbf: Option<DbFieldType>,
        element_count: Option<u32>,
        attrs: Option<&Snapshot>,
        labels: Option<&Snapshot>,
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
        if let Some(e) = labels.and_then(|s| s.enums.as_ref())
            && !e.strings.is_empty()
        {
            md.enum_choices = Some(
                e.strings
                    .iter()
                    .map(|s| s.as_str_lossy().into_owned())
                    .collect(),
            );
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
        let md =
            LinkMetadata::from_ca_attributes(Some(DbFieldType::Double), Some(1), Some(&snap), None);
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
        let md = LinkMetadata::from_ca_attributes(Some(DbFieldType::String), Some(1), None, None);
        assert_eq!(md.dbf_type, Some(LinkDbfType::String));
        assert_eq!(md.element_count, Some(1));
        assert_eq!(md.graphic_limits, None);
        assert_eq!(md.control_limits, None);
        assert_eq!(md.alarm_limits, None);
        assert_eq!(md.precision, None);
        assert_eq!(md.units, None);
    }

    /// an enum channel's `DBR_CTRL_ENUM` reply carries the state-label
    /// table; it lands in `enum_choices` so a `DBR_STRING`-requesting
    /// reader (stringin/lsi INP, printf `%s`) can render the remote index
    /// as its label — C `dbCa`'s `pgetString` monitor equivalent. An empty
    /// table stays `None` (nothing to render through).
    #[test]
    fn ca_attributes_enum_ctrl_carries_the_choices() {
        use crate::server::snapshot::EnumInfo;
        let mut snap = Snapshot::new(EpicsValue::Enum(1), 0, 0, std::time::SystemTime::UNIX_EPOCH);
        snap.enums = Some(EnumInfo::new(vec!["off".into(), "on".into()]));
        let md =
            LinkMetadata::from_ca_attributes(Some(DbFieldType::Enum), Some(1), None, Some(&snap));
        assert_eq!(md.dbf_type, Some(LinkDbfType::Enum));
        assert_eq!(
            md.enum_choices,
            Some(vec!["off".to_string(), "on".to_string()])
        );

        let empty = Snapshot::new(EpicsValue::Enum(0), 0, 0, std::time::SystemTime::UNIX_EPOCH);
        let md =
            LinkMetadata::from_ca_attributes(Some(DbFieldType::Enum), Some(1), None, Some(&empty));
        assert_eq!(md.enum_choices, None);

        // The two replies compose: the fixed DBR_CTRL_DOUBLE attribute get
        // carries an enum target's precision/units/limits (all zero, as C's
        // does) while the native DBR_CTRL_ENUM get carries the labels.
        use crate::server::snapshot::DisplayInfo;
        let mut attrs = Snapshot::new(
            EpicsValue::Double(1.0),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        attrs.display = Some(DisplayInfo::default());
        let md = LinkMetadata::from_ca_attributes(
            Some(DbFieldType::Enum),
            Some(1),
            Some(&attrs),
            Some(&snap),
        );
        assert_eq!(md.precision, Some(0));
        assert_eq!(md.graphic_limits, Some((0.0, 0.0)));
        assert_eq!(
            md.enum_choices,
            Some(vec!["off".to_string(), "on".to_string()])
        );
    }

    /// when the channel info fetch failed (`None` dbf /
    /// count) and there is no CTRL reply, every field is `None` — the
    /// link reports "no metadata yet" rather than fabricating zeros.
    #[test]
    fn ca_attributes_no_info_no_ctrl_is_all_none() {
        let md = LinkMetadata::from_ca_attributes(None, None, None, None);
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
        let md =
            LinkMetadata::from_ca_attributes(Some(DbFieldType::Double), Some(1), Some(&snap), None);
        assert_eq!(md.units, None);
        assert_eq!(md.precision, Some(0));
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
