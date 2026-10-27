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

use super::ca_link::{LocalChannel, LocalMonitor, StateNotify};
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

    /// `ca_add_masked_array_event` of `count` elements of `dbr_type`.
    async fn subscribe(
        &self,
        dbr_type: u16,
        count: u32,
        mask: u16,
    ) -> Result<Box<dyn ClientMonitor>, String>;

    /// `ca_array_put` (`callback == false`, completion `None`) or
    /// `ca_array_put_callback`: the put is issued when this returns, and its
    /// completion is the returned future.
    async fn put(&self, value: EpicsValue, callback: bool)
    -> Result<Option<PutCompletion>, String>;
}

/// One `evid`: each value the subscription delivers, `None` once it ended.
#[async_trait::async_trait]
pub trait ClientMonitor: Send {
    async fn next(&mut self) -> Option<Snapshot>;
}

/// The network context's half of [`CaClientContext::create_channel`].
#[async_trait::async_trait]
pub trait CaClientService: Send + Sync {
    /// `ca_create_channel` of a name this database does not serve; `None`
    /// when the client cannot start.
    async fn create_channel(&self, name: &str) -> Option<Arc<dyn ClientChannel>>;
}

/// libca's context with `dbContext` installed as its service.
#[derive(Clone)]
pub struct CaClientContext {
    db: Weak<PvDatabaseInner>,
}

impl PvDatabase {
    /// The CA client context records of this database open channels on.
    pub fn ca_client(&self) -> CaClientContext {
        CaClientContext::from_weak(Arc::downgrade(&self.inner))
    }
}

impl CaClientContext {
    pub(super) fn from_weak(db: Weak<PvDatabaseInner>) -> Self {
        Self { db }
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
        if let Some(channel) = LocalChannel::create(&db, name.as_bytes()).await {
            return Some(Arc::new(LocalClientChannel {
                channel,
                announced: std::sync::atomic::AtomicBool::new(false),
            }));
        }
        let service = db.inner.ca_links.client_service()?;
        service.create_channel(name).await
    }

    /// `dbContext::createChannel`'s `dbChannel_create`, on the calling
    /// thread: `None` when the database does not serve `name`, is gone, or
    /// captured no reactor.
    pub fn create_in_process(&self, name: &str) -> Option<InProcessChannel> {
        let db = self.db()?;
        let bridge = db.inner.link_puts.network()?.clone();
        let channel = in_caller(&bridge, LocalChannel::create(&db, name.as_bytes())).ok()??;
        Some(InProcessChannel { channel, bridge })
    }

    /// `dbContext::createChannel`'s network context alone, for a name
    /// [`Self::create_in_process`] did not find.
    pub async fn create_network(&self, name: &str) -> Option<Arc<dyn ClientChannel>> {
        let service = self.db()?.inner.ca_links.client_service()?;
        service.create_channel(name).await
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
    channel: LocalChannel,
    bridge: BlockingBridge,
}

impl InProcessChannel {
    /// `dbChannelIO::nativeType`.
    pub fn native_type(&self) -> DbFieldType {
        self.channel.native_type()
    }

    /// `dbChannelIO::nativeElementCount`.
    pub fn element_count(&self) -> u32 {
        self.channel.element_count()
    }

    /// `dbChannelIO::read`: `count` elements of `dbr_type`.
    pub fn get(&self, dbr_type: u16, count: u32) -> Result<Snapshot, String> {
        read(&self.channel, dbr_type, count)
    }

    /// `dbChannelIO::subscribe` on `events`: the first event is queued, not
    /// delivered, and each reaches `callback` as `count` elements of
    /// `dbr_type`.
    pub fn subscribe(
        &self,
        events: &InProcessEvents,
        dbr_type: u16,
        count: u32,
        mask: u16,
        callback: impl Fn(Snapshot) + Send + Sync + 'static,
    ) -> Result<InProcessSubscription, String> {
        let events = &events.0.0;
        events.start()?;
        let db = self.channel.db().ok_or("database gone")?;
        let (pre_chain, notify) = self.channel.state_notify(dbr_type);
        let route = Arc::new(Route {
            notify,
            count,
            callback: Box::new(callback),
            canceled: AtomicBool::new(false),
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

    /// `dbChannelIO::write`: `dbChannel_put`, done when this returns, run
    /// as a put of `events`' context.
    pub fn put(&self, events: &InProcessEvents, value: EpicsValue) -> Result<(), String> {
        let value = put_value(&self.channel, value);
        let put = events.0.0.running.run(self.channel.issue_put(value, false));
        in_caller(&self.bridge, put)
            .map_err(|e| e.to_string())?
            .map(drop)
    }

    /// `dbChannelIO::write` with a notify, run as a put of `events`'
    /// context: issued when this returns, and `issued` takes its completion
    /// while the put still runs, as C's `callDone` runs inside it for a
    /// record done at once. [`CaError::Timeout`] is `dbPutNotifyBlocker`'s
    /// wait for the put in flight running out.
    pub fn put_callback<R>(
        &self,
        events: &InProcessEvents,
        value: EpicsValue,
        issued: impl FnOnce(PutCompletion) -> R,
    ) -> Result<R, CaError> {
        let value = put_value(&self.channel, value);
        let put = async { self.channel.put_callback(value).await.map(issued) };
        in_caller(&self.bridge, events.0.0.running.run(put))
            .map_err(|e| CaError::Protocol(e.to_string()))?
    }
}

/// C `dbContext`'s event context (`dbContext.cpp:213-241`): the one queue
/// every in-process subscription of a client context attaches to, and
/// `CAC-event`, its consumer, which the first subscription starts at the
/// lowest priority above its own thread's. Entries reach their callbacks
/// one at a time in post order, on that thread or in [`Self::deliver`],
/// each after the runs of the context's puts going when it is taken
/// (`Running`). The thread returns when the last clone
/// drops (`db_close_events`).
#[derive(Clone, Default)]
pub struct InProcessEvents(Arc<EventsOwner>);

impl InProcessEvents {
    /// What is queued now, delivered on the calling thread in its turn
    /// with what `CAC-event` delivers. The caller holds no lock a callback
    /// takes. Inside one of this context's callbacks it does nothing, as
    /// C's callbacks run on the one thread that is already delivering.
    pub fn deliver(&self) {
        self.0.0.deliver();
    }
}

#[derive(Default)]
struct EventsOwner(Arc<Events>);

impl Drop for EventsOwner {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Release);
        if let Some(started) = self.0.started.get() {
            started.thread.unpark();
        }
    }
}

#[derive(Default)]
struct Events {
    user: EventUser,
    /// Held across an entry's take and its callback, so entries reach their
    /// callbacks in post order whichever thread takes them.
    consumer: parking_lot::Mutex<()>,
    running: Running,
    starting: parking_lot::Mutex<()>,
    started: OnceLock<Started>,
    stop: AtomicBool,
}

/// `CAC-event`, and the waker every take parks the chain on: the chain has
/// one `ppendsem`, so a take with any other waker would leave the thread
/// asleep on the next post.
struct Started {
    thread: std::thread::Thread,
    waker: Waker,
}

struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

thread_local! {
    /// The event context whose entry this thread is delivering.
    static DELIVERING: Cell<*const Events> = const { Cell::new(std::ptr::null()) };
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
        let _starting = self.starting.lock();
        if self.started.get().is_some() {
            return Ok(());
        }
        let own = thread_by_id(current_thread_id())
            .map_or(ThreadPriority::Medium.value(), |t| t.epics_priority());
        // `epicsThreadLowestPriorityLevelAbove`, or the caller's own at 99.
        let above = if own < 99 { own + 1 } else { own };
        let events = self.clone();
        let handle = spawn_dedicated_thread(
            "CAC-event".into(),
            ThreadPriority::Custom(above),
            StackSizeClass::Medium,
            move || {
                while !events.stop.load(Ordering::Acquire) {
                    events.deliver();
                    std::thread::park();
                }
            },
        )
        .map_err(|e| format!("CAC-event thread could not be created: {e}"))?;
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

    fn deliver(&self) {
        let Some(started) = self.started.get() else {
            return;
        };
        if std::ptr::eq(DELIVERING.get(), self) {
            return;
        }
        let _consumer = self.consumer.lock();
        let _delivering = Delivering::enter(self);
        let mut cx = Context::from_waker(&started.waker);
        while let Poll::Ready(entry) = self.user.poll_next(&mut cx) {
            // An ended producer's subscription ends when the client drops it.
            if let CircuitEvent::Event { context, event, .. } = entry
                && let Some(route) = context.downcast_ref::<Route>()
            {
                self.running.wait();
                route.deliver(event);
            }
        }
    }
}

/// The puts of a client context while they run on their callers, each poll
/// of one a run; what a run posts reaches its callback after the run. C's
/// `CAC-event` must be woken and scheduled before it delivers, so
/// `dbChannelIO::write` is ordinarily back in the poster first
/// (MM4005_trajectoryScan's `pvPut(motorMDVS[j])` then
/// `efClear(motorMDVSMon)` sees the event after the clear on 287 of 300 C
/// boots). A put parked between polls (`dbPutNotifyBlocker`, a postponed
/// write) runs nothing and holds up no one, as C's blocked caller does
/// not, and no put waits for another.
#[derive(Default)]
struct Running {
    /// The next run's number, and the numbers of the runs not ended.
    runs: parking_lot::Mutex<(u64, Vec<u64>)>,
    ended: parking_lot::Condvar,
}

impl Running {
    /// `put`, each poll of it a run.
    async fn run<F: Future>(&self, put: F) -> F::Output {
        let mut put = std::pin::pin!(put);
        std::future::poll_fn(|cx| {
            let _run = Run::start(self);
            put.as_mut().poll(cx)
        })
        .await
    }

    /// Until each run going now has ended; one started later is not waited
    /// for.
    fn wait(&self) {
        let mut runs = self.runs.lock();
        let now = runs.0;
        while runs.1.iter().any(|&run| run < now) {
            self.ended.wait(&mut runs);
        }
    }
}

/// One poll of a put, ended on every exit.
struct Run<'a> {
    running: &'a Running,
    number: u64,
}

impl<'a> Run<'a> {
    fn start(running: &'a Running) -> Self {
        let mut runs = running.runs.lock();
        let number = runs.0;
        runs.0 += 1;
        runs.1.push(number);
        Run { running, number }
    }
}

impl Drop for Run<'_> {
    fn drop(&mut self) {
        let mut runs = self.running.runs.lock();
        if let Some(at) = runs.1.iter().position(|&run| run == self.number) {
            runs.1.swap_remove(at);
        }
        self.running.ended.notify_all();
    }
}

/// One subscription's way from an entry to its callback.
struct Route {
    notify: StateNotify,
    count: u32,
    callback: Box<dyn Fn(Snapshot) + Send + Sync>,
    canceled: AtomicBool,
}

impl Route {
    fn deliver(&self, event: MonitorEvent) {
        if self.canceled.load(Ordering::Acquire) {
            return;
        }
        // `callStateNotify`'s `ECA_GETFAIL` reaches no callback here.
        if let Some(snapshot) = self.notify.state_notify(event, self.count) {
            (self.callback)(snapshot);
        }
    }
}

/// A subscription on an [`InProcessEvents`]. Dropping it cancels the event
/// (`db_cancel_event`), and no entry taken after reaches the callback. It
/// does not wait for a callback already running, as C's cancel does: a drop
/// can come under a lock that callback takes, so the client waits itself.
pub struct InProcessSubscription {
    _reader: EventReader,
    route: Arc<Route>,
}

impl Drop for InProcessSubscription {
    fn drop(&mut self) {
        self.route.canceled.store(true, Ordering::Release);
    }
}

/// `dbChannelIO::read`.
fn read(channel: &LocalChannel, dbr_type: u16, count: u32) -> Result<Snapshot, String> {
    channel
        .read(dbr_type, count)
        .ok_or_else(|| "dbChannel_get failed".to_string())
}

/// `dbChannelIO::subscribe`.
fn subscribe(
    channel: &LocalChannel,
    dbr_type: u16,
    count: u32,
    mask: u16,
) -> Result<Box<dyn ClientMonitor>, String> {
    let db = channel.db().ok_or("database gone")?;
    let monitor = channel
        .subscribe(&db, dbr_type, mask)
        .map_err(|e| format!("{e:?}"))?;
    Ok(Box::new(LocalClientMonitor { monitor, count }))
}

/// `value` as the channel's native type and element count.
fn put_value(channel: &LocalChannel, value: EpicsValue) -> EpicsValue {
    let mut value = value.into_type(channel.native_type());
    value.truncate(channel.element_count() as usize);
    value
}

/// An in-process channel: connected from creation, never disconnected,
/// full access (`dbChannelIO`).
struct LocalClientChannel {
    channel: LocalChannel,
    announced: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ClientChannel for LocalClientChannel {
    fn native_type(&self) -> Option<DbFieldType> {
        Some(self.channel.native_type())
    }

    fn element_count(&self) -> Option<u32> {
        Some(self.channel.element_count())
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
        read(&self.channel, dbr_type, count)
    }

    async fn subscribe(
        &self,
        dbr_type: u16,
        count: u32,
        mask: u16,
    ) -> Result<Box<dyn ClientMonitor>, String> {
        subscribe(&self.channel, dbr_type, count, mask)
    }

    async fn put(
        &self,
        value: EpicsValue,
        callback: bool,
    ) -> Result<Option<PutCompletion>, String> {
        let value = put_value(&self.channel, value);
        self.channel.issue_put(value, callback).await
    }
}

struct LocalClientMonitor {
    monitor: LocalMonitor,
    count: u32,
}

#[async_trait::async_trait]
impl ClientMonitor for LocalClientMonitor {
    async fn next(&mut self) -> Option<Snapshot> {
        loop {
            let event = self.monitor.reader.recv().await?;
            if let Some(snapshot) = self.monitor.state_notify(event, self.count) {
                return Some(snapshot);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// [`Running::wait`] on another thread; the receiver hears when it ends.
    fn waiting(running: &Arc<Running>) -> mpsc::Receiver<()> {
        let (ended, waited) = mpsc::channel();
        let running = Arc::clone(running);
        std::thread::spawn(move || {
            running.wait();
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
        let going = Run::start(&running);
        let waited = waiting(&running);
        std::thread::sleep(SETTLE);
        let later = Run::start(&running);
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
        let mut put = std::pin::pin!(running.run(std::future::pending::<()>()));
        let polled = put.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(polled.is_pending());
        waiting(&running)
            .recv_timeout(fact_budget::FACT_BUDGET)
            .unwrap();
    }
}
