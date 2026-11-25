//! A libca client running inside the IOC, as a record that holds its own
//! CA channels (sscan's `recDynLink`) uses libca: `ca_create_channel` on a
//! context `dbServiceIOInit` gave `dbContext` (`dbContext.cpp:59-66`), so a
//! name this database serves opens an in-process channel and any other one
//! goes to the network context.
//!
//! [`CaClientContext::create_channel`] is that first try. The network half
//! is the [`CaClientService`] the lset registered as `ca` offers
//! ([`LinkSet::ca_client_service`](super::LinkSet::ca_client_service)).
//! [`CaClientContext::create_in_process`] is the first try alone, run as
//! `dbContext` runs it: on the calling thread.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::task::{Context, Poll, Wake, Waker};

use super::ca_link::StateNotify;
use super::{PvDatabase, PvDatabaseInner};
use crate::error::CaError;
use crate::runtime::task::{
    BlockingBridge, NotBlockable, StackSizeClass, TaskHandle, ThreadPriority, block_on_sync,
    current_thread_id, spawn_dedicated_thread, thread_by_id,
};
use crate::server::event_queue::{CircuitEvent, DeliveryContext, EventReader, EventUser};
use crate::server::pv::MonitorEvent;
use crate::server::snapshot::Snapshot;
use crate::types::{DbFieldType, EpicsValue};
use context_channel::ContextChannel;

/// A boxed future the client hands back, run wherever the caller spawns it.
pub type ClientFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// The completion of a `ca_array_put_callback`: resolves when the server
/// reports the put done, with libca's error text when it did not
/// complete. No timeout: a positioner move completes when it completes.
pub type PutCompletion = ClientFuture<Result<(), String>>;

/// A channel's connection state, as libca's connection handler reports it
/// (`cs_conn` / `cs_prev_conn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    Connected,
    Disconnected,
}

/// One `chid`.
#[async_trait::async_trait]
pub trait ClientChannel: Send + Sync {
    /// `ca_field_type`; `None` while the channel has never connected.
    fn native_type(&self) -> Option<DbFieldType>;

    /// `ca_element_count`; `None` while the channel has never connected.
    fn element_count(&self) -> Option<u32>;

    /// `ca_read_access` / `ca_write_access`.
    fn access(&self) -> (bool, bool);

    /// The next connection-handler call. The first is the state the channel
    /// is in when it is first asked; `None` once the channel is gone.
    async fn next_connection(&self) -> Option<Connection>;

    /// `ca_array_get_callback` of `count` elements of `dbr_type`.
    async fn get(&self, dbr_type: u16, count: u32) -> Result<Snapshot, String>;

    /// `ca_add_masked_array_event` of `count` elements of `dbr_type`: each
    /// value reaches `callback` until the subscription drops, one callback
    /// at a time with the context's others of the channel's kind, in-process
    /// or network; neither kind waits on the other.
    async fn subscribe(
        &self,
        dbr_type: u16,
        count: u32,
        mask: u16,
        callback: Box<dyn Fn(Snapshot) + Send + Sync>,
    ) -> Result<ClientSubscription, String>;

    /// `ca_array_put` (`callback == false`, completion `None`) or
    /// `ca_array_put_callback`: the put is issued when this returns, and its
    /// completion is the returned future.
    async fn put(&self, value: EpicsValue, callback: bool)
    -> Result<Option<PutCompletion>, String>;
}

/// One `evid`: dropping it is `ca_clear_subscription`. The drop waits for
/// the subscription's callback if one is running, unless it drops inside
/// that callback, and no callback runs after it. On a context's
/// `CAC-callback` thread it waits for none: a callback whose delivery had
/// begun may still run after it, and no later delivery reaches one.
pub struct ClientSubscription {
    _held: Box<dyn Send>,
}

impl ClientSubscription {
    /// A subscription that ends when `held` drops, which keeps that rule.
    pub fn new(held: impl Send + 'static) -> Self {
        Self {
            _held: Box::new(held),
        }
    }
}

/// The network context's half of [`CaClientContext::create_channel`].
#[async_trait::async_trait]
pub trait CaClientService: Send + Sync {
    /// `ca_create_channel` of a name this database does not serve, for the
    /// context whose event context is `events`; `None` when the client
    /// cannot start.
    async fn create_channel(
        &self,
        name: &str,
        events: &InProcessEvents,
    ) -> Option<Arc<dyn ClientChannel>>;
}

/// libca's context with `dbContext` installed as its service. Its clones
/// are the same context.
#[derive(Clone)]
pub struct CaClientContext {
    db: Weak<PvDatabaseInner>,
    /// `dbContext`'s event context: every in-process subscription of this
    /// context queues here.
    events: InProcessEvents,
}

impl PvDatabase {
    /// A new CA client context (`ca_context_create`) records of this
    /// database open channels on.
    pub fn ca_client(&self) -> CaClientContext {
        CaClientContext::from_weak(Arc::downgrade(&self.inner))
    }
}

impl CaClientContext {
    pub(super) fn from_weak(db: Weak<PvDatabaseInner>) -> Self {
        Self {
            db,
            events: InProcessEvents::default(),
        }
    }

    /// The event context this context's in-process subscriptions deliver
    /// on, and its puts run as.
    pub fn events(&self) -> &InProcessEvents {
        &self.events
    }

    fn db(&self) -> Option<PvDatabase> {
        self.db.upgrade().map(|inner| PvDatabase { inner })
    }

    /// `ca_create_channel`: `dbChannel_create` first, then the network
    /// context. `None` when neither exists — the database is gone, or the
    /// name is not served here and no network half is registered, a channel
    /// whose search nobody answers.
    pub async fn create_channel(&self, name: &str) -> Option<Arc<dyn ClientChannel>> {
        let db = self.db()?;
        if let Some(chan) = ContextChannel::create(&db, name, &self.events).await {
            return Some(Arc::new(LocalClientChannel {
                chan,
                announced: std::sync::atomic::AtomicBool::new(false),
            }));
        }
        let service = db.inner.ca_links.client_service()?;
        service.create_channel(name, &self.events).await
    }

    /// `dbContext::createChannel`'s `dbChannel_create`, on the calling
    /// thread: `None` when the database does not serve `name`, is gone, or
    /// captured no reactor.
    pub fn create_in_process(&self, name: &str) -> Option<InProcessChannel> {
        let db = self.db()?;
        let bridge = db.inner.link_puts.network()?.clone();
        let chan = in_caller(&bridge, ContextChannel::create(&db, name, &self.events)).ok()??;
        Some(InProcessChannel { chan, bridge })
    }

    /// `dbContext::createChannel`'s network context alone, for a name
    /// [`Self::create_in_process`] did not find.
    pub async fn create_network(&self, name: &str) -> Option<Arc<dyn ClientChannel>> {
        let service = self.db()?.inner.ca_links.client_service()?;
        service.create_channel(name, &self.events).await
    }

    /// Run `future` on the reactor the database's CA links run on. `None`
    /// when the database is gone or captured no reactor.
    pub fn spawn<F>(&self, future: F) -> Option<TaskHandle<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let db = self.db()?;
        let bridge = db.inner.link_puts.network()?;
        Some(bridge.spawn(future))
    }
}

/// Run `fut` to its end on the calling thread, as `dbContext` runs a request
/// on its caller's. A thread no runtime runs on waits with the database's
/// reactor entered; a runtime's own thread polls it once and waits only
/// where [`block_on_sync`] may.
fn in_caller<F: Future>(bridge: &BlockingBridge, fut: F) -> Result<F::Output, NotBlockable> {
    #[cfg(tokio_backend)]
    if tokio::runtime::Handle::try_current().is_err() {
        return Ok(bridge.block_on(fut));
    }
    #[cfg(exec_backend)]
    let _ = bridge;
    let mut fut = std::pin::pin!(fut);
    if let Poll::Ready(out) = fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        return Ok(out);
    }
    block_on_sync(fut)
}

/// `dbChannelIO` as `dbContext` runs it: connected from creation, never
/// disconnected, full access, and each request made on the calling thread,
/// which waits there as C's caller does.
pub struct InProcessChannel {
    chan: ContextChannel,
    bridge: BlockingBridge,
}

impl InProcessChannel {
    /// `dbChannelIO::nativeType`.
    pub fn native_type(&self) -> DbFieldType {
        self.chan.native_type()
    }

    /// `dbChannelIO::nativeElementCount`.
    pub fn element_count(&self) -> u32 {
        self.chan.element_count()
    }

    /// `dbChannelIO::read`: `count` elements of `dbr_type`.
    pub fn get(&self, dbr_type: u16, count: u32) -> Result<Snapshot, String> {
        self.chan.get(dbr_type, count)
    }

    /// `dbChannelIO::subscribe` on its context's event context: the first
    /// event is queued, not delivered, and each reaches `callback` as
    /// `count` elements of `dbr_type`.
    pub fn subscribe(
        &self,
        dbr_type: u16,
        count: u32,
        mask: u16,
        callback: impl Fn(Snapshot) + Send + Sync + 'static,
    ) -> Result<InProcessSubscription, String> {
        self.chan
            .subscribe(dbr_type, count, mask, Box::new(callback))
    }

    /// `dbChannelIO::write`: `dbChannel_put`, done when this returns, run
    /// as a put of its context.
    pub fn put(&self, value: EpicsValue) -> Result<(), String> {
        in_caller(&self.bridge, self.chan.put(value, false))
            .map_err(|e| e.to_string())?
            .map(drop)
    }

    /// `dbChannelIO::write` with a notify, run as a put of its context:
    /// issued when this returns, and `issued` takes its completion
    /// while the put still runs, as C's `callDone` runs inside it for a
    /// record done at once. [`CaError::Timeout`] is `dbPutNotifyBlocker`'s
    /// wait for the put in flight running out.
    pub fn put_callback<R>(
        &self,
        value: EpicsValue,
        issued: impl FnOnce(PutCompletion) -> R,
    ) -> Result<R, CaError> {
        in_caller(&self.bridge, self.chan.put_callback(value, issued))
            .map_err(|e| CaError::Protocol(e.to_string()))?
    }
}

/// C `dbContext`'s event context (`dbContext.cpp:213-241`): the one queue
/// every in-process subscription of a client context attaches to, and
/// `CAC-event`, its consumer, which the first subscription starts at the
/// lowest priority above its own thread's. Entries reach their callbacks
/// one at a time in post order, on that thread or in [`Self::deliver`],
/// each after the runs of the context's puts on its record going when it
/// is taken (`Running`). The context's network subscriptions
/// ([`Self::subscribe_network`]) are delivered apart, on their own thread.
/// The threads return when the last clone drops (`db_close_events`).
#[derive(Clone, Default)]
pub struct InProcessEvents(Arc<EventsOwner>);

impl InProcessEvents {
    /// The in-process entries queued now, delivered on the calling thread
    /// in their turn with what `CAC-event` delivers. The caller holds no
    /// lock a callback takes. Inside one of this context's callbacks it does
    /// nothing: an in-process callback's thread is already delivering, as
    /// C's `CAC-event` is, and a network callback's never takes an entry.
    /// Inside a put of any client context, in its record's processing or
    /// [`InProcessChannel::put_callback`]'s `issued`, it does nothing as
    /// well: `CAC-event` may be waiting on that put, and delivers after it.
    pub fn deliver(&self) {
        self.0.0.deliver();
    }

    /// A network subscription of this context: each value `values` yields
    /// (`None` once it ends) reaches `callback` on the context's
    /// `CAC-callback` thread, one at a time with its other network
    /// callbacks, as libca's receive thread calls them under the context's
    /// `cbMutex` (`tcpiiu.cpp:498-519`). The thread starts at the highest
    /// priority below the first network subscriber's, as the receive thread
    /// is below the context creator's (`tcpiiu.cpp:676-678`). A callback
    /// that waits holds up the context's network delivery only.
    pub fn subscribe_network(
        &self,
        values: Box<dyn FnMut(&mut Context<'_>) -> Poll<Option<Snapshot>> + Send>,
        callback: Box<dyn Fn(Snapshot) + Send + Sync>,
    ) -> Result<ClientSubscription, String> {
        let events = &self.0.0;
        events.start_network()?;
        let route = Arc::new(NetworkRoute {
            values: parking_lot::Mutex::new(Some(values)),
            callback: Callback::new(callback),
        });
        events.network.routes.lock().push(Arc::downgrade(&route));
        // Its first poll is what registers the waker its values wake.
        events.network.thread.wake();
        Ok(ClientSubscription::new(NetworkSubscription(route)))
    }
}

#[derive(Default)]
struct EventsOwner(Arc<Events>);

impl Drop for EventsOwner {
    fn drop(&mut self) {
        self.0.cac_event.stop();
        self.0.network.thread.stop();
    }
}

#[derive(Default)]
struct Events {
    user: EventUser,
    /// Held across an entry's take and its callback, so entries reach their
    /// callbacks in post order whichever thread takes them.
    consumer: parking_lot::Mutex<()>,
    running: Running,
    cac_event: Deliverer,
    network: Network,
}

/// The context's network subscriptions, which only their own thread
/// delivers.
#[derive(Default)]
struct Network {
    routes: parking_lot::Mutex<Vec<Weak<NetworkRoute>>>,
    thread: Deliverer,
}

/// One of a context's delivery threads, started by the first subscription
/// it delivers and parked between passes.
#[derive(Default)]
struct Deliverer {
    starting: parking_lot::Mutex<()>,
    started: OnceLock<Started>,
    stop: Arc<AtomicBool>,
}

/// The thread, and the waker every take it delivers parks on: the event
/// chain has one `ppendsem`, so a take with any other waker would leave the
/// thread asleep on the next post.
struct Started {
    thread: std::thread::Thread,
    waker: Waker,
}

impl Deliverer {
    /// `name` at `priority`, each pass `pass`, unless it is started.
    fn start(
        &self,
        name: &str,
        priority: u32,
        stack: StackSizeClass,
        pass: impl Fn() + Send + 'static,
    ) -> Result<(), String> {
        let _starting = self.starting.lock();
        if self.started.get().is_some() {
            return Ok(());
        }
        let stop = self.stop.clone();
        let handle = spawn_dedicated_thread(
            name.into(),
            ThreadPriority::Custom(priority),
            stack,
            move || {
                while !stop.load(Ordering::Acquire) {
                    pass();
                    std::thread::park();
                }
            },
        )
        .map_err(|e| format!("{name} thread could not be created: {e}"))?;
        let thread = handle.thread().clone();
        let waker = Waker::from(Arc::new(Unpark(thread.clone())));
        let _ = self.started.set(Started {
            thread: thread.clone(),
            waker,
        });
        // Its first pass may have run before it had the waker to park on.
        thread.unpark();
        Ok(())
    }

    fn waker(&self) -> Option<&Waker> {
        self.started.get().map(|started| &started.waker)
    }

    fn wake(&self) {
        if let Some(started) = self.started.get() {
            started.thread.unpark();
        }
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.wake();
    }
}

/// The calling thread's EPICS priority.
fn own_priority() -> u32 {
    thread_by_id(current_thread_id()).map_or(ThreadPriority::Medium.value(), |t| t.epics_priority())
}

struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

thread_local! {
    /// The event context whose callbacks, of either kind, this thread is
    /// delivering.
    static DELIVERING: Cell<*const Events> = const { Cell::new(std::ptr::null()) };

    /// How many runs of client contexts' puts this thread is in.
    static IN_RUNS: Cell<usize> = const { Cell::new(0) };

    /// Whether this thread is a context's `CAC-callback`, which no drop of
    /// a subscription on it waits on; only that thread's body sets it.
    static CAC_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

/// [`DELIVERING`] for one delivery pass, restored on every exit.
struct Delivering(*const Events);

impl Delivering {
    fn enter(events: &Events) -> Self {
        Self(DELIVERING.replace(events))
    }
}

impl Drop for Delivering {
    fn drop(&mut self) {
        DELIVERING.set(self.0);
    }
}

impl Events {
    /// `db_start_events` for `dbContext::subscribe`'s first call.
    fn start(self: &Arc<Self>) -> Result<(), String> {
        let own = own_priority();
        // `epicsThreadLowestPriorityLevelAbove`, or the caller's own at 99.
        let above = if own < 99 { own + 1 } else { own };
        let events = self.clone();
        self.cac_event
            .start("CAC-event", above, StackSizeClass::Medium, move || {
                events.deliver()
            })
    }

    /// The network thread, for the first network subscription.
    fn start_network(self: &Arc<Self>) -> Result<(), String> {
        // `cac::highestPriorityLevelBelow`, or the caller's own at 0.
        let below = own_priority().saturating_sub(1);
        let events = self.clone();
        self.network
            .thread
            .start("CAC-callback", below, StackSizeClass::Big, move || {
                CAC_CALLBACK.set(true);
                let _delivering = Delivering::enter(&events);
                events.network.deliver();
            })
    }

    fn deliver(&self) {
        let Some(waker) = self.cac_event.waker() else {
            return;
        };
        // A thread in a run never waits for the turn: its holder may be
        // waiting on that run.
        if std::ptr::eq(DELIVERING.get(), self) || IN_RUNS.get() > 0 {
            return;
        }
        let _consumer = self.consumer.lock();
        let _delivering = Delivering::enter(self);
        let mut cx = Context::from_waker(waker);
        while let Poll::Ready(entry) = self.user.poll_next(&mut cx) {
            // An ended producer's subscription ends when the client drops it.
            if let CircuitEvent::Event { context, event, .. } = entry
                && let Some(route) = context.downcast_ref::<Route>()
            {
                self.running.wait(route.record);
                route.deliver(event);
            }
        }
    }
}

impl Network {
    /// Each value ready now to its callback, the subscriptions in turn.
    fn deliver(&self) {
        let Some(waker) = self.thread.waker() else {
            return;
        };
        let mut cx = Context::from_waker(waker);
        let routes = self.routes();
        loop {
            let mut taken = false;
            for route in &routes {
                taken |= route.deliver(&mut cx);
            }
            if !taken {
                break;
            }
        }
    }

    /// The routes whose subscriptions live.
    fn routes(&self) -> Vec<Arc<NetworkRoute>> {
        let mut routes = self.routes.lock();
        routes.retain(|route| route.strong_count() > 0);
        routes.iter().filter_map(Weak::upgrade).collect()
    }
}

/// The puts of a client context while they run on their callers, each poll
/// of one a run; what a run posts on its own record reaches its callback
/// after the run. C's `CAC-event` must be woken and scheduled before it
/// delivers, so `dbChannelIO::write` is ordinarily back in the poster first
/// (MM4005_trajectoryScan's `pvPut(motorMDVS[j])` then
/// `efClear(motorMDVSMon)` sees the event after the clear on 287 of 300 C
/// boots). An entry waits only for runs on its own record, whose value is
/// the one still changing, and a network value, on its own thread, for
/// none; C's `CAC-event` waits on the record's whole lockset only because
/// it reads under `dbScanLock` (`db_access.c:158`). A put parked between
/// polls (`dbPutNotifyBlocker`, a postponed write) runs nothing and holds up
/// no one, as C's blocked caller does not, and no put waits for another.
#[derive(Default)]
struct Running {
    /// The next run's number, and the number and record of each run not
    /// ended.
    runs: parking_lot::Mutex<(u64, Vec<(u64, RecordKey)>)>,
    ended: parking_lot::Condvar,
}

impl Running {
    /// `put` to `record`, each poll of it a run.
    async fn run<F: Future>(&self, record: RecordKey, put: F) -> F::Output {
        let mut put = std::pin::pin!(put);
        std::future::poll_fn(|cx| {
            let _run = Run::start(self, record);
            put.as_mut().poll(cx)
        })
        .await
    }

    /// Until each run on `record` going now has ended; one started later is
    /// not waited for.
    fn wait(&self, record: RecordKey) {
        let mut runs = self.runs.lock();
        let now = runs.0;
        while runs.1.iter().any(|&(run, on)| run < now && on == record) {
            self.ended.wait(&mut runs);
        }
    }
}

/// The record a channel addresses. The run's channel and the entry's
/// subscription each hold it, so while both live, equal keys are one record.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RecordKey(usize);

/// One poll of a put, ended on every exit.
struct Run<'a> {
    running: &'a Running,
    number: u64,
}

impl<'a> Run<'a> {
    fn start(running: &'a Running, record: RecordKey) -> Self {
        let mut runs = running.runs.lock();
        let number = runs.0;
        runs.0 += 1;
        runs.1.push((number, record));
        IN_RUNS.set(IN_RUNS.get() + 1);
        Run { running, number }
    }
}

impl Drop for Run<'_> {
    fn drop(&mut self) {
        IN_RUNS.set(IN_RUNS.get() - 1);
        let mut runs = self.running.runs.lock();
        if let Some(at) = runs.1.iter().position(|&(run, _)| run == self.number) {
            runs.1.swap_remove(at);
        }
        self.running.ended.notify_all();
    }
}

/// A subscription's callback, which its drop cancels: `db_cancel_event`
/// waits for one running except on its own thread, and none runs after.
/// On `CAC-callback` the cancel waits for none, so a network callback's
/// drop never waits on another thread's callback.
struct Callback {
    /// Held across each call, and taken by the cancel.
    running: parking_lot::ReentrantMutex<()>,
    canceled: AtomicBool,
    call: Box<dyn Fn(Snapshot) + Send + Sync>,
}

impl Callback {
    fn new(call: Box<dyn Fn(Snapshot) + Send + Sync>) -> Self {
        Self {
            running: parking_lot::ReentrantMutex::new(()),
            canceled: AtomicBool::new(false),
            call,
        }
    }

    fn canceled(&self) -> bool {
        self.canceled.load(Ordering::Acquire)
    }

    fn call(&self, snapshot: Snapshot) {
        let _running = self.running.lock();
        if !self.canceled() {
            (self.call)(snapshot);
        }
    }

    /// Returns only once no call can pass its check.
    fn cancel(&self) {
        let _running = (!CAC_CALLBACK.get()).then(|| self.running.lock());
        self.canceled.store(true, Ordering::Release);
    }
}

/// One subscription's way from an entry to its callback.
struct Route {
    record: RecordKey,
    notify: StateNotify,
    count: u32,
    callback: Callback,
}

impl Route {
    fn deliver(&self, event: MonitorEvent) {
        if self.callback.canceled() {
            return;
        }
        // `callStateNotify`'s `ECA_GETFAIL` reaches no callback here.
        if let Some(snapshot) = self.notify.state_notify(event, self.count) {
            self.callback.call(snapshot);
        }
    }
}

/// A network subscription's values, as a stream's `poll_next`.
type NetworkValues = Box<dyn FnMut(&mut Context<'_>) -> Poll<Option<Snapshot>> + Send>;

/// One network subscription's way from its values to its callback.
struct NetworkRoute {
    /// `None` once they end or the subscription drops.
    values: parking_lot::Mutex<Option<NetworkValues>>,
    callback: Callback,
}

impl NetworkRoute {
    /// Its next value to its callback, if one is there; whether one was.
    fn deliver(&self, cx: &mut Context<'_>) -> bool {
        let next = {
            let mut values = self.values.lock();
            let Some(poll_next) = values.as_mut() else {
                return false;
            };
            let next = poll_next(cx);
            if let Poll::Ready(None) = next {
                *values = None;
            }
            next
        };
        let Poll::Ready(Some(snapshot)) = next else {
            return false;
        };
        self.callback.call(snapshot);
        true
    }
}

/// Dropping it cancels its callback and ends its values.
struct NetworkSubscription(Arc<NetworkRoute>);

impl Drop for NetworkSubscription {
    fn drop(&mut self) {
        self.0.callback.cancel();
        let values = self.0.values.lock().take();
        drop(values);
    }
}

/// A subscription on a context's [`InProcessEvents`]. Dropping it cancels
/// the event (`db_cancel_event`) as dropping a [`ClientSubscription`] does.
pub struct InProcessSubscription {
    _reader: EventReader,
    route: Arc<Route>,
}

impl Drop for InProcessSubscription {
    fn drop(&mut self) {
        self.route.callback.cancel();
    }
}

/// The context's way to its database's channels: no channel type reaches
/// their put but through [`ContextChannel::run`].
mod context_channel {
    use super::*;
    use crate::server::database::ca_link::LocalChannel;
    use crate::server::database::channel_io::ChannelTarget;

    /// `dbChannelIO` on its context's event context.
    pub(super) struct ContextChannel {
        channel: LocalChannel,
        /// Its context's.
        events: InProcessEvents,
    }

    impl ContextChannel {
        /// `dbContext::createChannel`'s `dbChannel_create`: `None` when the
        /// database does not serve `name`.
        pub(super) async fn create(
            db: &PvDatabase,
            name: &str,
            events: &InProcessEvents,
        ) -> Option<Self> {
            let channel = LocalChannel::create(db, name.as_bytes()).await?;
            Some(Self {
                channel,
                events: events.clone(),
            })
        }

        /// `dbChannelIO::nativeType`.
        pub(super) fn native_type(&self) -> DbFieldType {
            self.channel.native_type()
        }

        /// `dbChannelIO::nativeElementCount`.
        pub(super) fn element_count(&self) -> u32 {
            self.channel.element_count()
        }

        /// `dbChannelIO::read`.
        pub(super) fn get(&self, dbr_type: u16, count: u32) -> Result<Snapshot, String> {
            self.channel
                .read(dbr_type, count)
                .ok_or_else(|| "dbChannel_get failed".to_string())
        }

        /// `dbChannelIO::subscribe` on its context's event context: the
        /// first event is queued, not delivered, and each reaches `callback`
        /// as `count` elements of `dbr_type`.
        pub(super) fn subscribe(
            &self,
            dbr_type: u16,
            count: u32,
            mask: u16,
            callback: Box<dyn Fn(Snapshot) + Send + Sync>,
        ) -> Result<InProcessSubscription, String> {
            let events = &self.events.0.0;
            events.start()?;
            let db = self.channel.db().ok_or("database gone")?;
            let (pre_chain, notify) = self.channel.state_notify(dbr_type);
            let route = Arc::new(Route {
                record: RecordKey::of(&self.channel),
                notify,
                count,
                callback: Callback::new(callback),
            });
            let context: DeliveryContext = route.clone();
            let reader = self
                .channel
                .add_event(&db, &events.user, mask, &context, pre_chain)
                .map_err(|e| format!("{e:?}"))?;
            Ok(InProcessSubscription {
                _reader: reader,
                route,
            })
        }

        /// `dbChannelIO::write`, run as a put of its context: a plain put is
        /// done when this returns (`None`); a put callback returns its
        /// completion.
        pub(super) async fn put(
            &self,
            value: EpicsValue,
            callback: bool,
        ) -> Result<Option<PutCompletion>, String> {
            let value = self.put_value(value);
            self.run(self.channel.issue_put(value, callback)).await
        }

        /// `dbChannelIO::write` with a notify, run as a put of its context,
        /// `issued` taking its completion while the put still runs. `Err` is
        /// `dbPutNotifyBlocker`'s wait for the put in flight running out.
        pub(super) async fn put_callback<R>(
            &self,
            value: EpicsValue,
            issued: impl FnOnce(PutCompletion) -> R,
        ) -> Result<R, CaError> {
            let value = self.put_value(value);
            self.run(async { self.channel.put_callback(value).await.map(issued) })
                .await
        }

        /// `put` as a put of the context, each poll of it a run.
        async fn run<F: Future>(&self, put: F) -> F::Output {
            let record = RecordKey::of(&self.channel);
            self.events.0.0.running.run(record, put).await
        }

        /// `value` as the channel's native type and element count.
        fn put_value(&self, value: EpicsValue) -> EpicsValue {
            let mut value = value.into_type(self.channel.native_type());
            value.truncate(self.channel.element_count() as usize);
            value
        }
    }

    impl RecordKey {
        fn of(channel: &LocalChannel) -> Self {
            let record = match channel.target() {
                ChannelTarget::SimplePv(pv) => Arc::as_ptr(pv).cast::<()>(),
                ChannelTarget::RecordField { record, .. } => Arc::as_ptr(record).cast::<()>(),
            };
            Self(record as usize)
        }
    }
}

/// An in-process channel: connected from creation, never disconnected,
/// full access (`dbChannelIO`).
struct LocalClientChannel {
    chan: ContextChannel,
    announced: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ClientChannel for LocalClientChannel {
    fn native_type(&self) -> Option<DbFieldType> {
        Some(self.chan.native_type())
    }

    fn element_count(&self) -> Option<u32> {
        Some(self.chan.element_count())
    }

    fn access(&self) -> (bool, bool) {
        (true, true)
    }

    async fn next_connection(&self) -> Option<Connection> {
        if !self
            .announced
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Some(Connection::Connected);
        }
        std::future::pending().await
    }

    async fn get(&self, dbr_type: u16, count: u32) -> Result<Snapshot, String> {
        self.chan.get(dbr_type, count)
    }

    async fn subscribe(
        &self,
        dbr_type: u16,
        count: u32,
        mask: u16,
        callback: Box<dyn Fn(Snapshot) + Send + Sync>,
    ) -> Result<ClientSubscription, String> {
        self.chan
            .subscribe(dbr_type, count, mask, callback)
            .map(ClientSubscription::new)
    }

    async fn put(
        &self,
        value: EpicsValue,
        callback: bool,
    ) -> Result<Option<PutCompletion>, String> {
        self.chan.put(value, callback).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    const RECORD: RecordKey = RecordKey(1);

    /// [`Running::wait`] for [`RECORD`] on another thread; the receiver
    /// hears when it ends.
    fn waiting(running: &Arc<Running>) -> mpsc::Receiver<()> {
        let (ended, waited) = mpsc::channel();
        let running = Arc::clone(running);
        std::thread::spawn(move || {
            running.wait(RECORD);
            ended.send(()).unwrap();
        });
        waited
    }

    const SETTLE: Duration = Duration::from_millis(50);

    /// An entry waits for the runs going when it is taken, not for one
    /// started after.
    #[test]
    fn a_wait_is_for_the_runs_going_when_it_starts() {
        let running = Arc::new(Running::default());
        let going = Run::start(&running, RECORD);
        let waited = waiting(&running);
        std::thread::sleep(SETTLE);
        let later = Run::start(&running, RECORD);
        assert!(waited.recv_timeout(SETTLE).is_err(), "before the run ends");
        drop(going);
        waited.recv_timeout(fact_budget::FACT_BUDGET).unwrap();
        drop(later);
    }

    /// A put parked between polls, as one in `dbPutNotifyBlocker` is, holds
    /// up no entry.
    #[test]
    fn a_put_parked_between_polls_is_no_run() {
        let running = Arc::new(Running::default());
        let mut put = std::pin::pin!(running.run(RECORD, std::future::pending::<()>()));
        let polled = put.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(polled.is_pending());
        waiting(&running)
            .recv_timeout(fact_budget::FACT_BUDGET)
            .unwrap();
    }
}
