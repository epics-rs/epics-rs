//! Actor-based port driver executor.
//!
//! Each port driver is owned exclusively by a `PortActor` task. Requests arrive
//! via an mpsc channel, are prioritized in a heap, and dispatched to the
//! driver's `io_*` methods. Replies go back through oneshot channels.
//!
//! For `can_block=true` ports, the actor runs on `tokio::task::spawn_blocking`.
//! For `can_block=false` ports, it runs on a normal `tokio::spawn` task.

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Instant, SystemTime};

use tokio::sync::{mpsc, oneshot};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interfaces::InterfaceType;
use crate::interpose::EomReason;
use crate::interrupt::InterruptValue;
use crate::param::ParamValue;
use crate::port::{PortDriver, QueuePriority};
use crate::request::{CancelToken, RequestOp, RequestResult};
use crate::user::{AsynUser, ConnectCheck};

static ACTOR_SEQ: AtomicU64 = AtomicU64::new(0);
static ACTOR_IDS: AtomicU64 = AtomicU64::new(0);

/// Identity of a port actor.
///
/// Minted once per [`PortActor`] and carried by every
/// [`PortHandle`](crate::port_handle::PortHandle) that targets it, so that a
/// caller about to block can answer the one question that decides whether
/// blocking is safe: *am I the thread that would have to service the request I
/// am about to wait on?*
///
/// The actor publishes its own id on its thread for as long as it runs (see
/// [`ActorScope`]); [`current_actor`] reads it back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ActorId(u64);

impl ActorId {
    /// Mint a fresh identity, distinct from every other actor's.
    ///
    /// A handle whose actor is never spawned (some unit-test fixtures hold the
    /// receiving end and run no actor loop) gets an id that is never published
    /// on any thread — which is the truth: no thread is that port's actor, so
    /// no caller can be it.
    pub(crate) fn new() -> Self {
        Self(ACTOR_IDS.fetch_add(1, AtomicOrdering::Relaxed))
    }
}

thread_local! {
    /// The port actor running on this thread, if this thread is an actor thread.
    static CURRENT_ACTOR: Cell<Option<ActorId>> = const { Cell::new(None) };
}

/// Publishes an actor's id on its own thread, and unpublishes it on every exit
/// path (return, panic).
struct ActorScope(Option<ActorId>);

impl ActorScope {
    fn enter(id: ActorId) -> Self {
        Self(CURRENT_ACTOR.with(|cur| cur.replace(Some(id))))
    }
}

impl Drop for ActorScope {
    fn drop(&mut self) {
        CURRENT_ACTOR.with(|cur| cur.set(self.0));
    }
}

/// The port actor owning the calling thread, or `None` if the caller is not an
/// actor thread (a device-support scan thread, an `ad-core` driver thread, a
/// plain `std::thread`, a tokio worker).
pub(crate) fn current_actor() -> Option<ActorId> {
    CURRENT_ACTOR.with(|cur| cur.get())
}

/// Message sent from [`super::port_handle::PortHandle`] to the actor.
pub(crate) struct ActorMessage {
    pub op: RequestOp,
    pub user: AsynUser,
    pub cancel: CancelToken,
    pub reply: oneshot::Sender<AsynResult<RequestResult>>,
    pub seq: u64,
    pub priority: QueuePriority,
    pub block_token: Option<u64>,
    /// When this request stops being allowed to wait — C
    /// `epicsTimerStartDelay(puserPvt->timer, timeout)` at enqueue
    /// (asynManager.c:1617-1623). `None` is C's `queueRequest(..., 0.0)`: no
    /// timer, the request waits as long as the queue makes it.
    ///
    /// Stamped **here**, from [`AsynUser::queue_timeout`], so the clock starts
    /// when the request is queued rather than when someone gets around to
    /// looking at it — every submit path funnels through this constructor, so no
    /// caller can enqueue a request whose deadline is measured from the wrong
    /// instant.
    pub queue_deadline: Option<Instant>,
}

impl ActorMessage {
    pub fn new(
        op: RequestOp,
        user: AsynUser,
        cancel: CancelToken,
        reply: oneshot::Sender<AsynResult<RequestResult>>,
    ) -> Self {
        // C derives the queue band from the KIND of request, never from a
        // field a caller has to remember to set: `asynRecord` picks
        // `asynQueuePriorityConnect` from `pmsg->callbackType ==
        // callbackConnect` (asynRecord.c:562-566), and `asynCommonSyncIO`'s
        // connect/disconnect pass the constant literally
        // (asynCommonSyncIO.c:142, :154). `CDispatch::ConnectQueue` is that
        // kind, and it already fixes the connected waiver
        // ([`PortActor::queue_gate`]) — the heap key is taken from it here for
        // the same reason, so a connect cannot sort below a sustained High
        // scan stream because its caller built a `#[default]` Medium user.
        //
        // Every other class keeps the user's own band: C's HOSTINFO put is a
        // `callbackSetOption` that asynRecord *escalates* to Connect by hand
        // (:566-569), and that escalation still arrives through
        // [`AsynUser::queue_even_if_not_connected`].
        let priority = match PortActor::c_dispatch(&op) {
            CDispatch::ConnectQueue => QueuePriority::Connect,
            CDispatch::Direct | CDispatch::Queued => user.priority,
        };
        let block_token = user.block_token;
        let queue_deadline = user.queue_timeout.map(|d| Instant::now() + d);
        Self {
            op,
            user,
            cancel,
            reply,
            seq: ACTOR_SEQ.fetch_add(1, AtomicOrdering::Relaxed),
            priority,
            block_token,
            queue_deadline,
        }
    }
}

/// Sleep until `deadline`, or never if there is none.
///
/// A disarmed connect timer must not make the `select!` arm ready — otherwise
/// the actor would spin. `pending()` is the "this arm never completes" form,
/// leaving the other arms (request / shutdown) to drive the loop exactly as
/// they did before the timer existed.
/// The sooner of two optional deadlines — `None` meaning "never".
fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending().await,
    }
}

/// Marks a claimed request's cancel token `Done` when execution leaves the
/// dispatch scope, on every exit path (reply, early return, panic). This is the
/// C `callbackActive -> idle` transition: once the port thread finishes the
/// callback, a pending `cancelRequest` reports `wasQueued==0` (asynManager.c:1645-1659).
struct FinishGuard(CancelToken);

impl Drop for FinishGuard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

// Heap ordering: higher priority first, then lower seq (strict FIFO
// within a priority). C asynManager queues each priority as a FIFO list
// (queueRequest ellAdd to the queueList[priority] tail; portThread walks
// ellFirst->ellNext, asynManager.c:1613/869-898) — a request's
// timeout never reorders it relative to same-priority peers. An earlier
// deadline-based tiebreaker let a later-submitted request with a shorter
// timeout jump ahead of an earlier one, violating that FIFO.
impl Eq for ActorMessage {}
impl PartialEq for ActorMessage {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}
impl Ord for ActorMessage {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}
impl PartialOrd for ActorMessage {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// How C performs an operation: the one classification every gate in the actor
/// derives from ([`PortActor::queue_gate`], [`PortActor::is_lifecycle_op`]).
///
/// The match that produces it ([`PortActor::c_dispatch`]) is **exhaustive by
/// construction — no wildcard arm**. "Does C queue this?" has a different answer
/// per operation and it must be *answered*, not defaulted: a wildcard silently
/// put every unlisted op in the queued class, and C performs several of them as
/// direct calls (R13-46). Adding a `RequestOp` now fails to compile until its C
/// dispatch is stated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CDispatch {
    /// C performs it as a direct call — no `queueRequest`, so no queue gate, and
    /// no queue for the block holder to divert it into. These are `pasynManager`'s
    /// own entry points, which take `asynManagerLock`, act and return: `enable`
    /// (asynManager.c:2222-2249), `autoConnectAsyn` (:2310-2324),
    /// `isConnected`/`isEnabled`/`isAutoConnect` (:2326-2354),
    /// `blockProcessCallback` (:1692-1723), `shutdownPort`, `interposeInterface`.
    ///
    /// The gate must not run on them: a port disabled with `asynEnable(port,0)`
    /// could never be re-enabled if the enable itself had to pass the enabled
    /// check.
    Direct,
    /// C queues it at `asynQueuePriorityConnect`, and the port thread drains that
    /// queue *before* it consults any connected flag (`portThread`,
    /// asynManager.c:811-855) — so the *connected* refusal is waived. `enabled`
    /// still binds: the same thread refuses to run anything at all on a disabled
    /// port (:806-809), which is why a `CNCT=1` put cannot open the device
    /// connection of a port disabled precisely to keep the IOC off the hardware
    /// (R12-48).
    ///
    /// The block holder cannot divert these either: the Connect queue is drained
    /// ahead of the lower-priority loop it gates, with no block check.
    ConnectQueue,
    /// C hands it to `queueRequest` (asynManager.c:1517-1552), so C's gate runs.
    /// Its two refusals are **independent**: `!pport->dpc.enabled → asynDisabled`
    /// (:1541-1546) is unconditional, and only `checkPortConnect` is conditioned
    /// on the priority and reason (:1536-1538). The request's own user
    /// ([`AsynUser::connect_check`]) selects the connected waiver;
    /// [`crate::port::PortDriverBase::check_queue`] enforces the split, so nothing can waive
    /// the enabled half.
    ///
    /// This is also the only class the block holder can stall: C's
    /// `pblockProcessHolder` gates exactly the `processUser` dispatch in the port
    /// thread's lower-priority loop (:887-895).
    Queued,
}

/// Which `dpCommon` a request resolves to — C `findDpCommon`
/// (asynManager.c:536-543). `locateDevice` (:574) hands back NULL unless the
/// port carries `ASYN_MULTIDEVICE`, so an addr names a device only there;
/// everywhere else the port's own `dpCommon` is the one and only slot.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum DpKey {
    Port,
    Device(i32),
}

impl DpKey {
    fn of(multi_device: bool, addr: i32) -> Self {
        if multi_device && addr >= 0 {
            DpKey::Device(addr)
        } else {
            DpKey::Port
        }
    }
}

/// C's two `pblockProcessHolder` fields, and the one gate that reads them.
///
/// `port` is `pport->pblockProcessHolder`; `dp_commons` holds
/// `pdpCommon->pblockProcessHolder` per `dpCommon`, including the port's own —
/// C really does keep a *third* distinct field there, `pport->dpc`, so a
/// device-scoped block taken on a single-device port is not the same thing as
/// a port-wide one even though both end up gating every request.
///
/// Each entry is `(token, nesting_count)`, C's `blockPortCount` /
/// `blockDeviceCount` per holder.
#[derive(Default)]
struct BlockHolders {
    port: Option<(u64, u32)>,
    dp_commons: HashMap<DpKey, (u64, u32)>,
}

impl BlockHolders {
    fn any_held(&self) -> bool {
        self.port.is_some() || !self.dp_commons.is_empty()
    }

    /// C's eligibility test, verbatim: **both** holders must be NULL or this
    /// user (asynManager.c:889-892). One holder alone is never enough to admit
    /// a request, and never enough to refuse one.
    fn admits(&self, token: Option<u64>, dp: DpKey) -> bool {
        let free = |h: Option<&(u64, u32)>| match h {
            None => true,
            Some((owner, _)) => token == Some(*owner),
        };
        free(self.port.as_ref()) && free(self.dp_commons.get(&dp))
    }

    /// The holder a block/unblock at this scope owns. `all_devices` is C's
    /// argument to `blockProcessCallback` (asynManager.c:1692); the `dpCommon`
    /// it names when the answer is "not all" comes from the user's own device,
    /// exactly as `findDpCommon(puserPvt)` reads it (:1765).
    fn holder(&self, all_devices: bool, dp: DpKey) -> Option<(u64, u32)> {
        if all_devices {
            self.port
        } else {
            self.dp_commons.get(&dp).copied()
        }
    }

    /// The only mutator. Every transition of either holder goes through here,
    /// so "held ⟺ some caller's block is outstanding" cannot drift between the
    /// two scopes.
    fn set_holder(&mut self, all_devices: bool, dp: DpKey, held: Option<(u64, u32)>) {
        if all_devices {
            self.port = held;
        } else {
            match held {
                Some(h) => {
                    self.dp_commons.insert(dp, h);
                }
                None => {
                    self.dp_commons.remove(&dp);
                }
            }
        }
    }
}

/// The actor that exclusively owns a port driver instance.
pub(crate) struct PortActor {
    id: ActorId,
    driver: Box<dyn PortDriver>,
    rx: mpsc::Receiver<ActorMessage>,
    heap: BinaryHeap<ActorMessage>,
    /// C keeps **two** block holders, and the drain gate requires both to be
    /// NULL-or-me before a request is eligible (asynManager.c:889-892):
    /// `pport->pblockProcessHolder` (set when `blockPortCount > 0`, :929-930)
    /// and `pdpCommon->pblockProcessHolder` (`blockDeviceCount > 0`, :931-932).
    /// devGpib takes the device form for an SRQ transaction
    /// (devSupportGpib.c:1216), so a port-wide-only model stalls every other
    /// GPIB address on the port for the duration.
    block_holders: BlockHolders,
    pending_while_blocked: Vec<ActorMessage>,
    /// Requests whose *device* is disabled. C's `portThread` leaves them in the
    /// queue and skips them on every pass (`if(!pdpCommon->enabled) continue;`,
    /// asynManager.c:875) — they are still queued, so they still run when the
    /// device is re-enabled, and still time out on their own queue timer. This
    /// is that "still queued, not yet eligible" half of C's queue list; the heap
    /// is the eligible half (R15-47).
    parked: Vec<ActorMessage>,
    /// C `pport->queueStateChange` (asynManager.c:868, 1615): something happened
    /// that could make a parked request eligible, so the queue must be rescanned.
    /// Set by an arriving request and by a request that ran (either can enable a
    /// device); *not* by a re-park, which is what keeps the loop from spinning on
    /// a request whose device is still disabled.
    rescan_parked: bool,
    /// C `pport->notifyPortThread` (asynManager.c:217) — the binary event C's
    /// `portThread` waits on. It is signalled from exactly five places
    /// (`queueRequest` :1626, `announceExceptionOccurred` :636,
    /// `queueTimeoutCallback` :700, `cancelRequest` :1688,
    /// `unblockProcessCallback` :1772) and everything the woken thread does —
    /// draining the queues, and the port-level auto-connect at :856-861 — is
    /// downstream of one of them.
    ///
    /// The actor's loop is woken by *any* message, status queries included, so
    /// "a pass ran" is not "C's thread was signalled": a `GetConnected` or an
    /// `asynReport` dialled the hardware, and a poller pulled a dead port's
    /// reconnect cadence from 20 s down to its own period (R16-46). This latch is
    /// the missing half — the pass runs, but only a pass C would have signalled
    /// reaches the auto-connect tail.
    ///
    /// The exception source is *not* here: it is counted on the port itself
    /// (`PortDriverBase::exceptions_announced`), because an exception can be
    /// announced from inside a driver call, where the actor has no hook.
    /// [`Self::take_port_thread_wake`] is the one reader of both halves.
    ///
    /// Four of C's five signal sites set this or the exception count; the fifth,
    /// `cancelRequest` (:1688), has no actor-side site to set it from — a cancel
    /// is a [`CancelToken`] flip on the *caller's* thread, and the actor learns
    /// of it when it pops the request. It sends no message, so it wakes nothing,
    /// which is the same end state C's woken thread reaches: it rescans a queue
    /// that lost one entry.
    woken: bool,
    /// The port's announced-exception count as of the last
    /// [`Self::take_port_thread_wake`] — the memory that turns a monotonic
    /// counter back into C's edge-triggered signal.
    seen_exceptions: u64,
}

impl PortActor {
    pub fn new(driver: Box<dyn PortDriver>, rx: mpsc::Receiver<ActorMessage>) -> Self {
        // Whatever the driver announced before the actor existed (registration's
        // own connect edge, say) is not a signal *this* thread ever waited on —
        // C's port thread is started with the event unsignalled.
        let seen_exceptions = driver.base().exceptions_announced();
        Self {
            id: ActorId::new(),
            driver,
            rx,
            heap: BinaryHeap::new(),
            block_holders: BlockHolders::default(),
            pending_while_blocked: Vec::new(),
            parked: Vec::new(),
            rescan_parked: false,
            woken: false,
            seen_exceptions,
        }
    }

    /// This actor's identity. Hand it to every [`PortHandle`] built against
    /// this actor's channel so blocking callers can detect a re-entrant call
    /// from the actor's own thread.
    ///
    /// [`PortHandle`]: crate::port_handle::PortHandle
    pub fn id(&self) -> ActorId {
        self.id
    }

    /// Run the actor loop. Returns when the channel is closed (all senders dropped).
    /// Calls `shutdown()` on the driver before returning.
    #[cfg(test)]
    pub fn run(mut self) {
        let _scope = ActorScope::enter(self.id);
        loop {
            // Drain all pending messages into the heap
            self.drain_channel();
            self.rescan_parked();
            self.expire_parked();

            if self.heap.is_empty() {
                // No work — block on the channel
                match self.rx.blocking_recv() {
                    Some(msg) => self.enqueue_message(msg),
                    None => break,
                }
                // Drain any more that arrived
                self.drain_channel();
                self.rescan_parked();
                self.expire_parked();
            }

            // Process one eligible request from the heap
            self.process_one();
            self.port_thread_auto_connect();
        }
        let _ = self.driver.shutdown();
    }

    /// Run the actor loop with a dedicated shutdown channel.
    /// Calls `shutdown()` on the driver before returning.
    ///
    /// A port stops for exactly two reasons, and dropping a
    /// [`PortRuntimeHandle`](crate::runtime::port::PortRuntimeHandle) is
    /// neither of them:
    ///
    /// - **Someone asked it to.** An explicit shutdown *sends* `()` on the
    ///   shutdown channel (`PortRuntimeHandle::shutdown`). It outranks every
    ///   outstanding handle: the port stops even while other `PortHandle`s
    ///   could still reach it.
    /// - **Nobody can reach it any more.** The request channel closed, i.e.
    ///   the last `PortHandle` was dropped. No request can ever arrive again,
    ///   so stopping is unobservable.
    ///
    /// The shutdown channel *closing* (every `PortRuntimeHandle` dropped) is
    /// deliberately NOT a stop condition. It used to be — closing was the only
    /// shutdown signal — which conflated "the creator dropped its handle" with
    /// "shut this port down" and killed ports that the registry still held a
    /// live `PortHandle` for.
    pub fn run_with_shutdown(mut self, mut shutdown_rx: mpsc::Receiver<()>) {
        // Publish our identity for the whole life of the actor thread: every
        // `PortDriver` method we dispatch below runs on this thread, so a
        // driver that calls back into its own port through a `PortHandle` can
        // be detected and refused instead of deadlocking on itself.
        let _scope = ActorScope::enter(self.id);
        // A *current-thread* runtime is deliberate: the actor owns its driver
        // exclusively and dispatches serially (C asyn's per-port `portThread`).
        // Nothing here may assume a multi-threaded runtime — see
        // `PortHandle::block_on_reply`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            // Once every PortRuntimeHandle is gone no explicit shutdown can
            // ever arrive, so the (now permanently ready) shutdown branch is
            // disabled rather than spun on.
            let mut can_be_shut_down = true;
            loop {
                // Drain all pending messages into the heap
                self.drain_channel();
                self.rescan_parked();
                self.expire_parked();

                if self.heap.is_empty() {
                    // Wait for a message, an explicit shutdown, or the
                    // connect-retry deadline. The timer arm is what makes the
                    // autonomous reconnect work with NO queued traffic (C's
                    // connect timer fires on its own timer queue and posts a
                    // Connect-priority request to the port thread,
                    // asynManager.c:3252-3266); here the actor IS the port
                    // thread, so it wakes itself.
                    // ...and at the earliest parked request's queue deadline: C
                    // arms an `epicsTimer` per request at `queueRequest`
                    // (asynManager.c:1617-1623) that unlinks it wherever it is
                    // sitting, and a request parked behind a disabled device has
                    // no other clock — a blocking caller with no Tokio reactor
                    // arms no timer of its own (`PortHandle::await_reply`).
                    let retry_at = earliest(
                        self.driver.base().connect_retry_at,
                        self.earliest_parked_deadline(),
                    );
                    tokio::select! {
                        msg = self.rx.recv() => {
                            match msg {
                                Some(m) => self.enqueue_message(m),
                                // Unreachable: the last PortHandle is gone.
                                None => break,
                            }
                        }
                        signal = shutdown_rx.recv(), if can_be_shut_down => {
                            match signal {
                                // Explicit shutdown.
                                Some(()) => break,
                                // Every PortRuntimeHandle was dropped. Not a
                                // shutdown — keep serving whoever still holds
                                // a PortHandle.
                                None => can_be_shut_down = false,
                            }
                        }
                        _ = sleep_until_opt(retry_at) => {}
                    }
                    // Drain any more that arrived
                    self.drain_channel();
                    self.rescan_parked();
                    self.expire_parked();
                }

                // Run any due connect retry before servicing the queue: a
                // request dequeued now would otherwise see a port that the
                // timer was about to bring back up.
                self.service_connect_timer();

                // Process one eligible request from the heap
                self.process_one();

                // C `portThread`'s post-drain auto-connect, run on every wake.
                self.port_thread_auto_connect();
            }
        });
        let _ = self.driver.shutdown();
    }

    /// C `portConnectTimerCallback` + `portConnectProcessCallback`
    /// (asynManager.c:3252-3283), collapsed into one step because the actor
    /// is both the timer's consumer and the port thread the callback would
    /// have queued onto.
    ///
    /// C's timer callback re-checks `!connected && autoConnect` before
    /// queueing (:3258) — the port may have come back since the timer was
    /// armed — and the process callback re-checks `isConnected` again (:3278)
    /// before calling `pasynCommon->connect`. On failure it re-arms at
    /// `secondsBetweenPortConnect` (:3281), which is the 20 s retry loop that
    /// keeps a dead link trying forever.
    ///
    /// The re-arm decision keys on the port still being disconnected rather
    /// than on the connect call's return: that is what C's own guards test
    /// (`!pport->dpc.connected`), it is identical for any driver that reports
    /// failure honestly, and it cannot wedge a port whose `connect()` returns
    /// success without actually bringing the link up.
    fn service_connect_timer(&mut self) {
        match self.driver.base().connect_retry_at {
            Some(at) if at <= Instant::now() => {}
            _ => return,
        }
        // C's timer callback reaches the port thread through `queueRequest`
        // (:3259), so the retry is one of the five wakes — the pass it starts is
        // allowed to end in the port-level auto-connect
        // ([`Self::port_thread_auto_connect`]) even with no traffic on the port.
        self.woken = true;

        // Disarm first: `connect()` re-arms via `set_connected` on either
        // edge, and a stale deadline left behind here would spin the loop.
        self.driver.base_mut().connect_retry_at = None;

        // C :3258 — the port reconnected (or auto-connect was turned off)
        // while the timer was pending: nothing to do.
        if self.driver.base().is_connected() || !self.driver.base().auto_connect {
            return;
        }

        // C :3259 — the callback does not call `pasynCommon->connect`, it posts
        // a `queueRequest`, so the gate runs. A refusal (`asynDisabled` from
        // :1541-1546) means `portConnectProcessCallback` never runs, and it is
        // the *only* thing that re-arms the timer (:3281): `asynEnable(port,0)`
        // stops the retry loop dead, and a `shutdownPort`ed port never touches
        // the hardware again. So: no attempt, and no re-arm.
        if self.connect_gate().is_err() {
            return;
        }

        // Addr -1: this is the *port's* attempt. C derives it as
        // `addr = (pdevice ? pdevice->addr : -1)` in `connectAttempt`
        // (asynManager.c:752), and the port-level caller passes
        // `&pport->dpc`, i.e. no device (:716). A driver that reads the
        // address — prologix branches on it exactly as C's `prologixConnect`
        // does — must not see this as a device 0 connect.
        let _ = self.driver.connect(&AsynUser::default().with_addr(-1));
        // Anchor the auto-connect throttle window on this attempt, so the
        // port-thread wake below ([`Self::port_thread_auto_connect`]) does not
        // fire a second attempt at the same hardware microseconds later. In C
        // the two paths interleave the same way — the timer's queued callback
        // runs, and `portThread` then reaches `autoConnectDevice(pport,0)`
        // (asynManager.c:856-861), whose `connectAttempt` stamps
        // `lastConnectDisconnect` (:718) — so the window ends up anchored at the
        // attempt either way.
        self.driver
            .base_mut()
            .stamp_auto_connect_attempt(-1, Instant::now());

        if !self.driver.base().is_connected() {
            // Still down — back off and try again (C :3281).
            let retry = self.driver.base().seconds_between_port_connect;
            self.driver.base_mut().connect_retry_at = Some(Instant::now() + retry);
        }
    }

    /// C `portThread`'s post-drain auto-connect (asynManager.c:856-861): the
    /// woken port thread finds `!pport->dpc.connected` and runs one
    /// 2 s-throttled `autoConnectDevice(pport,0)`.
    ///
    /// It runs on **every** wake, and an exception announcement is such a wake:
    /// `announceExceptionOccurred` signals `notifyPortThread` for every
    /// exception on a CANBLOCK port (:635-636), and `enable` ends in
    /// `exceptionOccurred` (:2247). That — not any re-arm inside the enable
    /// itself — is what makes `asynEnable(port,1)` bring a down port back up in
    /// C. The Rust actor previously had only the disconnect-EDGE retry timer as
    /// an autonomous connect path, so a port disabled while down stayed down
    /// after re-enable until some I/O request forced a connect (R15-46).
    ///
    /// What it does *not* run on is a pass C's thread would never have taken. C
    /// waits on `notifyPortThread` and only the five signal sites listed on
    /// [`Self::woken`] reach it; `isConnected` (:2326-2337), `isEnabled` (:2340),
    /// `isAutoConnect` (:2348) and `report` (:1136) are plain reads under
    /// `asynManagerLock` and signal nothing. Here every one of them is a message,
    /// and a message wakes the actor — so the wake latch, not the message, is
    /// what gates this tail (R16-46). Without it a status poller dials the
    /// hardware on its own period and `asynReport` becomes an action.
    ///
    /// Beyond the latch the rule is uniform, because every decision it could
    /// special-case already lives inside [`Self::auto_connect_port`]:
    /// [`Self::connect_gate`] refuses on a disabled or defunct port, and the 2 s
    /// throttle bounds an exception storm to one attempt per window.
    fn port_thread_auto_connect(&mut self) {
        if !self.take_port_thread_wake() {
            return;
        }
        self.auto_connect_port();
    }

    /// C `epicsEventMustWait(pport->notifyPortThread)` (asynManager.c:805) —
    /// consume the wake and say whether there was one. The two halves are the
    /// latch the actor sets at C's signal sites ([`Self::woken`]) and the port's
    /// announced-exception count, which catches the announcements C signals for
    /// but the actor cannot see from the outside (a driver's own `set_connected`,
    /// an interpose's, an IP-server listener's).
    ///
    /// Like C's binary event, a burst collapses: N requests queued between two
    /// passes are one wake, and one port-level auto-connect attempt.
    fn take_port_thread_wake(&mut self) -> bool {
        let announced = self.driver.base().exceptions_announced();
        let exception = announced != self.seen_exceptions;
        self.seen_exceptions = announced;
        std::mem::take(&mut self.woken) || exception
    }

    fn drain_channel(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.enqueue_message(msg);
        }
    }

    /// Return the parked requests to the eligible queue, if anything has
    /// happened that could make one of them eligible — C's `queueStateChange`
    /// rescan (asynManager.c:868-898, where the *whole* queue is walked again on
    /// every pass that ran a callback or took a new request).
    ///
    /// A re-park does not set the flag, so a request whose device is still
    /// disabled is not rescanned in a loop: the actor goes back to waiting, and
    /// the request stays queued exactly as C leaves it.
    fn rescan_parked(&mut self) {
        if !self.rescan_parked {
            return;
        }
        self.rescan_parked = false;
        for msg in self.parked.drain(..) {
            self.heap.push(msg);
        }
    }

    /// The queue timer of a parked request, fired where it sits — C
    /// `queueTimeoutCallback` unlinks the request from `queueList` while
    /// `isQueued` (asynManager.c:649-701) and runs the user's timeout callback,
    /// wherever in the queue the port thread had left it.
    fn expire_parked(&mut self) {
        if self.parked.is_empty() {
            return;
        }
        let now = Instant::now();
        let port = self.driver.base().port_name.clone();
        let (expired, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.parked)
            .into_iter()
            .partition(|msg| msg.queue_deadline.is_some_and(|at| now >= at));
        self.parked = kept;
        // C `queueTimeoutCallback` ends by signalling the port thread (:700): the
        // request left the queue, so the thread must look at what is left.
        if !expired.is_empty() {
            self.woken = true;
        }
        for msg in expired {
            // The token settles which outcome wins, exactly as at the dequeue
            // gate: if the caller's own timer got there first it already
            // reported, and this reply lands on a dropped receiver.
            msg.cancel.time_out_if_queued();
            let _ = msg
                .reply
                .send(Err(AsynError::QueueTimeout { port: port.clone() }));
        }
    }

    /// When the earliest-deadlined parked request must be woken.
    fn earliest_parked_deadline(&self) -> Option<Instant> {
        self.parked.iter().filter_map(|m| m.queue_deadline).min()
    }

    /// The gate every connect attempt the actor makes *on its own initiative*
    /// passes — the retry timer and both auto-connect levels. It is the same
    /// gate a request passes, asked on behalf of the connect user C builds for
    /// the port (`pport->pconnectUser`, asynManager.c:3231): `addr == -1` at
    /// Connect priority, so `checkPortConnect` is `FALSE` (:1536-1538) and only
    /// the unconditional enabled/defunct half binds (:1541-1546).
    ///
    /// Nothing in C reaches `pasynCommon->connect` without it — the timer
    /// callback goes through `queueRequest` (:3259), and `portThread` refuses to
    /// run anything at all on a disabled port, its own `autoConnectDevice`
    /// included (:806-809). Routing every attempt through the one gate is what
    /// makes `asynEnable(port,0)` stop the hardware retries, rather than merely
    /// stop the reads that ride on them.
    fn connect_gate(&self) -> AsynResult<()> {
        self.driver.base().check_queue(-1, ConnectCheck::Waived)
    }

    /// C `reportPrintPort` (asynManager.c:1018-1122): the *manager's* report of a
    /// port, printed before the driver's own `pasynCommon->report` (:1121-1123).
    ///
    /// The manager block is the manager's to print, not the driver's — in C no
    /// driver prints its port's queue depth, enable state or trace masks, because
    /// no driver owns them. Here the actor owns them, so the actor prints them and
    /// then calls the driver (R14-51). The port used to print only the driver's own
    /// line, so `asynReport` answered none of the questions it exists for: is the
    /// port enabled, is anything queued behind a stuck request, is it flapping
    /// (`numberConnects`), which interposes are stacked on it.
    ///
    /// C's shape, kept: a negative `details` suppresses the per-address block
    /// (:1032-1035); `details >= 1` adds the state / queue / lock / exception /
    /// trace lines; `details >= 2` adds the interface lists; a destroyed port
    /// prints one line and nothing else, driver included (:1038-1042) — its
    /// interfaces are gone. Disabled and disconnected are *not* such states: they
    /// report, as `enabled:No` / `connected:No`.
    fn report_port(&self, details: i32) {
        use std::io::Write as _;
        let mut out = self.manager_report(details);
        // C ends by calling the port's `asynCommon->report` (:1113-1122) — the
        // driver's own detail, and only that. A destroyed port's interfaces are
        // gone, so C never reaches it (:1038-1042).
        if !self.driver.base().is_defunct() {
            self.driver.report(&mut out, details.abs());
        }
        // The one place the report picks a stream, and it picks the one C picks:
        // `asynReport` passes `stdout` to `pasynManager->report`
        // (asynShellCommands.c:589). The report is an answer to an operator's
        // iocsh question, so it belongs on the shell's output stream, where a
        // `>` redirect captures it — not on stderr with the diagnostics.
        print!("{out}");
        let _ = std::io::stdout().flush();
    }

    /// The text of C's manager block — [`Self::report_port`] is what prints it.
    /// Split out so the block can be asserted on: every line here is a question an
    /// operator ran `asynReport` to answer.
    fn manager_report(&self, details: i32) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let base = self.driver.base();
        if base.is_defunct() {
            let _ = writeln!(out, "{} destroyed", base.port_name);
            return out;
        }

        let show_devices = details >= 0;
        let details = details.abs();
        let yn = |b: bool| if b { "Yes" } else { "No" };

        let _ = writeln!(
            out,
            "{} multiDevice:{} canBlock:{} autoConnect:{}",
            base.port_name,
            yn(base.flags.multi_device),
            yn(base.flags.can_block),
            yn(base.auto_connect)
        );
        if details >= 1 {
            let _ = writeln!(
                out,
                "    enabled:{} connected:{} numberConnects {}",
                yn(base.enabled),
                yn(base.is_connected()),
                base.number_connects
            );
            // C counts every priority's queue (:1036-1037) and asks whether a
            // `blockProcessCallback` holder owns the port (:1063). Both are the
            // actor's state: the heap is the queue, `block_holders` the holders, and
            // the requests held back behind the block or a disabled device are
            // queued as much as the ones in the heap — in C they are all one
            // `queueList`.
            let _ = writeln!(
                out,
                "    nDevices {} nQueued {} blocked:{}",
                base.device_states.len(),
                self.heap.len() + self.pending_while_blocked.len() + self.parked.len(),
                yn(self.block_holders.any_held())
            );
            // C try-locks the two port mutexes from a *separate* report thread, so
            // a port thread stuck inside a driver call shows `Yes` (:1049-1070).
            // The actor model has neither mutex: the driver is reachable only from
            // the actor thread, and this report *is* the actor's current request —
            // so if it prints at all, the actor is not inside a driver call. A
            // wedged driver is visible instead as a report that never prints.
            let _ = writeln!(out, "    asynManagerLock:No synchronousLock:No");
            // C's `exceptionActive` is set while `exceptionOccurred` is fanning
            // out (:2050-2070) and its notify list holds the users waiting on that
            // fan-out. Rust announces synchronously from the transition owner with
            // no deferred list, so on the actor thread neither can be pending; the
            // user count is the real number of registered callbacks.
            let _ = writeln!(
                out,
                "    exceptionActive:No exceptionUsers {} exceptionNotifys 0",
                base.exception_callback_count()
            );
            let (mask, io_mask, info_mask) = self.trace_masks(None);
            let _ = writeln!(
                out,
                "    traceMask:0x{mask:x} traceIOMask:0x{io_mask:x} traceInfoMask:0x{info_mask:x}"
            );
        }
        if details >= 2 {
            // C prints the two interface lists by `interfaceType`
            // (`reportPrintInterfaceList`, :993-1005). The pointers it prints with
            // them have no Rust analogue; the type names do. Every octet interpose
            // registers `asynOctet` — that is what makes it an interpose of the
            // octet interface.
            if !base.interpose_octet.is_empty() {
                let _ = writeln!(out, "    interposeInterfaceList");
                for _ in 0..base.interpose_octet.len() {
                    let _ = writeln!(out, "        asynOctet");
                }
            }
            let mut ifaces: Vec<&'static str> = self
                .driver
                .capabilities()
                .iter()
                .map(|c| c.interface_type().asyn_name())
                .collect();
            ifaces.sort_unstable();
            ifaces.dedup();
            if !ifaces.is_empty() {
                let _ = writeln!(out, "    interfaceList");
                for name in ifaces {
                    let _ = writeln!(out, "        {name}");
                }
            }
        }
        if show_devices {
            // C walks the devices it has created and prints the disconnected ones
            // even at `details == 0` — a down device is what the operator is
            // looking for (:1081-1094).
            let mut addrs: Vec<i32> = base.device_states.keys().copied().collect();
            addrs.sort_unstable();
            for addr in addrs {
                let ds = &base.device_states[&addr];
                if !ds.connected || details >= 1 {
                    let _ = writeln!(
                        out,
                        "    addr {} autoConnect {} enabled {} connected {} exceptionActive {}",
                        addr,
                        // The *device's* autoConnect, which is what C prints here
                        // (`pdpc = &pdevice->dpc`, asynManager.c:1084-1089) — the
                        // port's own is on the header line. `asynAutoConnect(port,
                        // addr, 0)` turns off one device's auto-connect, and the
                        // report has to be able to say so.
                        yn(ds.auto_connect),
                        yn(ds.enabled),
                        yn(ds.connected),
                        yn(false)
                    );
                }
                if details >= 1 {
                    let _ = writeln!(
                        out,
                        "        exceptionActive No exceptionUsers 0 exceptionNotifys 0"
                    );
                    let _ = writeln!(out, "        blocked No");
                    let (mask, io_mask, info_mask) = self.trace_masks(Some(addr));
                    let _ = writeln!(
                        out,
                        "        traceMask:0x{mask:x} traceIOMask:0x{io_mask:x} \
                         traceInfoMask:0x{info_mask:x}"
                    );
                }
            }
        }

        out
    }

    /// The port's (or one device's) trace masks, as `reportPrintPort` prints them
    /// (asynManager.c:1072-1074, :1099-1101). A port registered without a trace
    /// manager has no masks to print, which is C's zero-initialised `dpCommon.trace`.
    fn trace_masks(&self, addr: Option<i32>) -> (u32, u32, u32) {
        let base = self.driver.base();
        let Some(trace) = base.trace.as_ref() else {
            return (0, 0, 0);
        };
        // `snapshot` is the single owner of C's `findTracePvt` fallback chain —
        // device, else port, else global (asynManager.c:546-551) — so the report
        // prints the masks the port actually traces with.
        let snap = trace.snapshot(&base.port_name, addr);
        (
            snap.trace_mask.bits(),
            snap.io_mask.bits(),
            snap.info_mask.bits(),
        )
    }

    /// The single owner of "how does C perform this operation" — see [`CDispatch`].
    /// Exhaustive on purpose; do not add a wildcard arm.
    fn c_dispatch(op: &RequestOp) -> CDispatch {
        match op {
            // Direct `pasynManager` calls under `asynManagerLock`.
            RequestOp::EnableAddr
            | RequestOp::DisableAddr
            | RequestOp::SetEnable { .. }
            | RequestOp::SetAutoConnect { .. }
            | RequestOp::SetAutoConnectAddr { .. }
            | RequestOp::GetEnable
            | RequestOp::GetAutoConnect
            | RequestOp::GetConnected
            | RequestOp::PushEchoInterpose
            | RequestOp::PushDelayInterpose { .. }
            | RequestOp::PushEosInterpose { .. }
            | RequestOp::PushFlushInterpose { .. }
            | RequestOp::SetTimeStampSource { .. }
            | RequestOp::BlockProcess { .. }
            | RequestOp::UnblockProcess { .. }
            | RequestOp::ShutdownPort
            // `asynPortDriver::callParamCallbacks` (asynPortDriver.cpp:1785-1794)
            // is a plain method — `getParamList` then `pList->callCallbacks(addr)`.
            // Driver code calls it directly; it never goes near `queueRequest`, so
            // no enabled or connected check runs. A driver that models its device
            // link sets `connected = false` when the hardware drops and *then*
            // publishes the status parameter announcing it — gating this op
            // discarded exactly the updates that matter most (R13-47).
            | RequestOp::CallParamCallbacks { .. }
            // `asynDrvUser->create` is called straight off the interface pointer,
            // at record init and at connect time: asynRecord.c:1243-1257 and
            // devAsynInt32.c:264-277. No `queueRequest` is anywhere in either
            // path. It is a string → parameter-index lookup in the driver's own
            // table and cannot fail for connectivity reasons; gating it made a
            // record bound to a down driver resolve its DRVINFO to reason 0
            // (R13-48).
            | RequestOp::DrvUserCreate { .. }
            // `report` (asynManager.c:1136-1171) spawns a `reportPort` thread and
            // calls `pasynCommon->report(drvPvt, fp, details)` directly (:1121-1123).
            // No queue, no gate: a disabled port reports `enabled:No`, a
            // disconnected one `connected:No` (:1112-1115). The diagnostic you
            // reach for *because* the port is down must not be the one the down
            // port refuses (W10-D6). The one state C will not report on is a
            // destroyed port, handled in the dispatch arm (:1038-1042).
            | RequestOp::Report { .. } => CDispatch::Direct,

            // C reaches `asynCommon->connect`/`disconnect` only from a callback
            // queued at Connect priority (asynRecord's CNCT/PCNCT put,
            // asynRecord.c:503-505,562-571) or from the port thread itself.
            RequestOp::Connect
            | RequestOp::Disconnect
            | RequestOp::ConnectAddr
            | RequestOp::DisconnectAddr => CDispatch::ConnectQueue,

            // Everything device support reaches through `queueRequest`: the
            // interface I/O (asynOctet / asynInt32 / asynInt64 / asynFloat64 /
            // asynUInt32Digital / the array interfaces / asynEnum), the option and
            // EOS accessors asynRecord queues (asynRecord.c:1787-1826, 1987-2025),
            // the GPIB commands (:1638-1756), `flush`, and `getBounds`.
            RequestOp::OctetWrite { .. }
            | RequestOp::OctetRead { .. }
            | RequestOp::OctetWriteRead { .. }
            | RequestOp::OctetWriteBinary { .. }
            | RequestOp::OctetReadBinary { .. }
            | RequestOp::Int32Write { .. }
            | RequestOp::Int32Read
            | RequestOp::Int64Write { .. }
            | RequestOp::Int64Read
            | RequestOp::Float64Write { .. }
            | RequestOp::Float64Read
            | RequestOp::UInt32DigitalWrite { .. }
            | RequestOp::UInt32DigitalRead { .. }
            | RequestOp::Flush
            | RequestOp::NoIo
            | RequestOp::GetBoundsInt32
            | RequestOp::GetBoundsInt64
            | RequestOp::EnumRead
            | RequestOp::EnumWrite { .. }
            | RequestOp::Int32ArrayRead { .. }
            | RequestOp::Int32ArrayWrite { .. }
            | RequestOp::Float64ArrayRead { .. }
            | RequestOp::Float64ArrayWrite { .. }
            | RequestOp::Int8ArrayRead { .. }
            | RequestOp::Int8ArrayWrite { .. }
            | RequestOp::Int16ArrayRead { .. }
            | RequestOp::Int16ArrayWrite { .. }
            | RequestOp::Int64ArrayRead { .. }
            | RequestOp::Int64ArrayWrite { .. }
            | RequestOp::Float32ArrayRead { .. }
            | RequestOp::Float32ArrayWrite { .. }
            | RequestOp::GetOption { .. }
            | RequestOp::SetOption { .. }
            | RequestOp::SetInputEos { .. }
            | RequestOp::SetOutputEos { .. }
            | RequestOp::GetInputEos
            | RequestOp::GetOutputEos
            | RequestOp::GpibUniversalCmd { .. }
            | RequestOp::GpibAddressedCmd { .. }
            | RequestOp::GpibIfc
            | RequestOp::GpibRen { .. } => CDispatch::Queued,
        }
    }

    /// The `dpCommon` an `addr` names on THIS port — C `findDpCommon`
    /// through `locateDevice` (asynManager.c:536-543, :574).
    fn dp_key(&self, addr: i32) -> DpKey {
        DpKey::of(self.driver.base().flags.multi_device, addr)
    }

    /// Whether both block holders admit this request — C's drain-loop test
    /// (asynManager.c:889-892). The one place that reads the holders, so a
    /// device block can never be mistaken for a port-wide one.
    fn admits(&self, msg: &ActorMessage) -> bool {
        self.block_holders
            .admits(msg.block_token, self.dp_key(msg.user.addr))
    }

    /// The ops the block holder (`pblockProcessHolder`) cannot stall — everything
    /// C does not put through `queueRequest`'s lower-priority loop.
    fn is_lifecycle_op(op: &RequestOp) -> bool {
        Self::c_dispatch(op) != CDispatch::Queued
    }

    /// Which refusals C's `queueRequest` gate (asynManager.c:1541-1552) applies
    /// to this request. `None` — C never queues it, so no gate runs.
    fn queue_gate(op: &RequestOp, user: &AsynUser) -> Option<ConnectCheck> {
        match Self::c_dispatch(op) {
            CDispatch::Direct => None,
            // The op fixes the priority at Connect (every C call site passes
            // `asynQueuePriorityConnect`), but the WAIVER is still the user's:
            // C waives `checkPortConnect` only for a port-level user
            // (`addr == -1`) or the explicit sentinel (asynManager.c:1536-1538).
            // A device-addressed CNCT put without either is refused on a
            // disconnected port — the C wart W10-D1, reproduced by decision.
            CDispatch::ConnectQueue => Some(user.connect_check_at_connect_priority()),
            CDispatch::Queued => Some(user.connect_check()),
        }
    }

    fn enqueue_message(&mut self, msg: ActorMessage) {
        // C `queueRequest` sets `pport->queueStateChange` and signals the port
        // thread (asynManager.c:1615, 1626) — the queue changed, so anything the
        // thread had skipped is looked at again.
        self.rescan_parked = true;
        // …and the signal is `queueRequest`'s, so only what C *queues* raises it.
        // A `pasynManager` direct call — `isConnected`, `isEnabled`,
        // `isAutoConnect`, `report`, `enable`, `blockProcessCallback` — takes
        // `asynManagerLock`, acts and returns without touching the event
        // ([`CDispatch::Direct`]). Those that nonetheless end in an exception
        // (`enable` → `exceptionOccurred`, :2247) wake the thread through the
        // exception count, not through here.
        if Self::c_dispatch(&msg.op) != CDispatch::Direct {
            self.woken = true;
        }
        // C parity: lifecycle/state ops are not queued, so neither block
        // holder ever stalls them — only non-owner I/O is diverted.
        // Previously only UnblockProcess was exempt, so a non-owner's
        // enable/auto-connect/connect/disconnect/get stalled until
        // UnblockProcess.
        if !Self::is_lifecycle_op(&msg.op) && !self.admits(&msg) {
            self.pending_while_blocked.push(msg);
            return;
        }
        self.heap.push(msg);
    }

    fn process_one(&mut self) {
        let msg = match self.heap.pop() {
            Some(m) => m,
            None => return,
        };

        // The queue-wait deadline, enforced where C enforces it: at the boundary
        // between "queued" and "running". C arms a timer at `queueRequest`
        // (asynManager.c:1617-1623) and its callback unlinks the request only
        // while `isQueued` (:655-661); the port thread cancels the timer as it
        // dequeues (:827,:906). Both halves reduce to one rule — *a request that
        // waited past its deadline never runs* — and the token is where that
        // rule is settled, so a deadline, an `AQR` cancel and this dequeue race
        // through one CAS and exactly one of them wins.
        if msg.queue_deadline.is_some_and(|at| Instant::now() >= at)
            && msg.cancel.time_out_if_queued()
        {
            let _ = msg.reply.send(Err(AsynError::QueueTimeout {
                port: self.driver.base().port_name.clone(),
            }));
            return;
        }

        // Everything down to `begin_running` is C's *queued* phase — the request
        // is still on the queue there, which is what lets a disabled device's
        // request stay on it (:875).

        // The queue gate — C `queueRequest` (asynManager.c:1541-1552).
        // [`Self::queue_gate`] says whether it runs at all (C queues this op or
        // performs it as a direct manager call) and, when it does, which of its
        // two independent refusals the request may waive. The *connected* waiver
        // follows the request's own user (`AsynUser::connect_check`, C's
        // `checkPortConnect` at :1536-1538); the *enabled* refusal is waivable by
        // nobody, and `check_queue` is where that holds.
        //
        // The waiver is the only route C gives an operator to reconfigure a line
        // that is down — repoint a dead IP port through HOSTINFO
        // (asynRecord.c:566-569), read a down serial port's options back into the
        // record (:1277-1280), run `asynSetOption`/`asynSetEos` from `st.cmd`
        // before the crate is powered on (asynShellCommands.c:121,169,240,291).
        // Bare Connect *priority* is not enough: C keeps the EOS readback at Low
        // priority with no waiver (:1296), so IEOS/OEOS still stay blank on a
        // disconnected port, and keying the waiver on the reason preserves that.
        // A port whose link is owned by another object — an IP-server child port,
        // whose socket the parent's accept loop assigns and clears — can change
        // connection state with no request of its own having run. Its `connected`
        // is the owner's cell, so the gate below already reads the truth; what is
        // still owed is the edge C's listener publishes explicitly when it hands
        // the child a socket (`connectDevice`, drvAsynIPServerPort.c:358-360):
        // the `exceptionConnect` fan-out and the interpose stack's reset, which
        // drops the previous client's EOS read-ahead. Every other port is already
        // in sync here (its own `set_connected` published the edge), so this is a
        // no-op for them.
        self.driver.base_mut().sync_connection_edge();

        if let Some(connect) = Self::queue_gate(&msg.op, &msg.user) {
            // C drains the Connect queue *before* `autoConnectDevice`
            // (asynManager.c:811-855): a waived request runs on the dead line
            // with no connect attempt in front of it. The attempt C makes
            // *after* that drain (:856-861) is the actor loop's.
            if connect == ConnectCheck::Required {
                self.auto_connect_device(msg.user.addr, msg.user.reason);
            }
            if let Err(e) = self.driver.base().check_queue(msg.user.addr, connect) {
                let _ = msg.reply.send(Err(e));
                return;
            }
            // C `portThread`'s per-request DEVICE checks (asynManager.c:875-886):
            // the half of the device state `check_queue` deliberately leaves to
            // the thread on a CANBLOCK port, because C answers them here in a way
            // `queueRequest` cannot — by *not answering*.
            //
            // The scope is the requests C leaves in the lower-priority loop: the
            // Connect-priority queue is drained above it with no device checks at
            // all (:811-855), and every waived request C queues rides that queue
            // (asynShellCommands.c:127,174,245,296; asynRecord.c:562-571). Hence
            // `Queued` dispatch and a `Required` connect check — the same
            // requests, asked the same way.
            if connect == ConnectCheck::Required
                && self.driver.base().flags.can_block
                && Self::c_dispatch(&msg.op) == CDispatch::Queued
            {
                // :875 `if(!pdpCommon->enabled) continue;` — the request keeps
                // its place in the queue. It runs when the device is re-enabled,
                // and its queue timer still expires under it if it never is.
                if self
                    .driver
                    .base()
                    .check_device_enabled(msg.user.addr)
                    .is_err()
                {
                    self.parked.push(msg);
                    return;
                }
                // :877-886 — auto-connect has already run above (C calls it at
                // :877); a device still down diverts the request to its
                // *timeout* callback rather than `processUser`, but only when
                // one was registered (`:884` guards on `timeoutUser != 0`;
                // `createAsynUser`'s second argument is optional —
                // asynRecord.c:307-308 passes one, devAsynInt32.c:212 passes
                // 0). A request that asked for a timeout always has one:
                // `queueRequest` refuses `timeout > 0.0` without it
                // (:1590-1595). Here the reply channel *is* that callback, so
                // the request reports the same queue timeout its own timer
                // would.
                if self
                    .driver
                    .base()
                    .check_device_connected(msg.user.addr)
                    .is_err()
                {
                    msg.cancel.time_out_if_queued();
                    let _ = msg.reply.send(Err(AsynError::QueueTimeout {
                        port: self.driver.base().port_name.clone(),
                    }));
                    return;
                }
            }
        }

        let ActorMessage {
            op,
            mut user,
            cancel,
            reply,
            ..
        } = msg;

        // Claim the request for execution. This is the C dequeue under
        // `asynManagerLock` (:889-891): `begin_running` transitions the cancel
        // token `Queued -> Running`, which closes the window in which an `AQR`
        // `cancelRequest` could report `wasQueued==1` (asynManager.c:1661-1666).
        // It fails only if the request left the queue without running — an `AQR`
        // cancel, or a queue-wait deadline the *waiter* resolved first (the
        // async path arms its own timer, `PortHandle::submit_cancellable`) — in
        // which case it must be dropped, reporting the outcome that won.
        if !cancel.begin_running() {
            if cancel.is_timed_out() {
                let _ = reply.send(Err(AsynError::QueueTimeout {
                    port: self.driver.base().port_name.clone(),
                }));
                return;
            }
            let _ = reply.send(Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "request cancelled".into(),
            }));
            return;
        }
        // From here the request is running (C `callbackActive`): a concurrent
        // cancel can no longer win, and `finish` marks it `Done` on every exit
        // path so a late cancel stays a no-op and a multi-phase plan can
        // re-claim the token for its next phase.
        let _finish = FinishGuard(cancel);

        // A request that begins running is never aborted by a queue deadline (the
        // gate above is the only place one is read); from here `user.timeout` is
        // the *I/O* timeout handed to the driver's read/write, and nothing else.
        // The two clocks are separate in C too (`pasynUser->timeout` vs
        // `queueRequest`'s `timeout` argument, `AsynUser::queue_timeout` here).

        // A request ran, so the queue state may have moved (it could have enabled
        // a device): C's `queueStateChange` rescan (:868). A *parked* request
        // does not set this — that is what keeps the loop from spinning on a
        // device that is still disabled.
        self.rescan_parked = true;

        // Connect the user to the port it is about to run on, so every layer the
        // request passes through can trace through it — C's `pasynUser →
        // pport/pdevice` linkage, which `asynPrint`/`asynPrintIO` resolve the
        // trace config from (`findTracePvt`, asynManager.c:546-551). The actor
        // is the single owner of the linkage: an interpose has no other handle on
        // the port, and a user that never reached a port traces nothing.
        user.trace = self.user_trace();

        // Dispatch
        let result = self.dispatch_io(&mut user, &op);
        let _ = reply.send(result);

        // The port-level auto-connect C's `portThread` runs after this
        // (asynManager.c:856-861) is not this function's — it is the actor
        // loop's, once per wake ([`Self::port_thread_auto_connect`]), because in
        // C it is the *thread*, not the request, that performs it. That is what
        // brings a dead line up on the new configuration a waived
        // `asynSetOption`/`asynSetEos`/HOSTINFO repoint just wrote, and equally
        // what brings a re-enabled port back up with no request at all (R15-46).
    }

    /// The port context [`AsynUser::trace`] carries — the driver's trace manager
    /// and the port's name, both `Arc`s, so stamping a request is two refcount
    /// bumps.
    fn user_trace(&self) -> Option<crate::user::UserTrace> {
        let base = self.driver.base();
        base.trace.as_ref().map(|manager| crate::user::UserTrace {
            manager: manager.clone(),
            port: base.port_name.as_str().into(),
        })
    }

    /// The **port** half of C `autoConnectDevice` (asynManager.c:706-721) —
    /// the single owner of every port-level auto-connect attempt, so the two
    /// callers cannot drift apart.
    ///
    /// C reaches it two ways, and both are here: through `autoConnectDevice`
    /// when a queued request needs the line up, and straight from `portThread`
    /// with `pdevice = 0` once the Connect-priority queue has drained
    /// (:856-861). One attempt per 2 s window either way
    /// (`auto_connect_throttle_ok`, C :712-713), every attempt restarting the
    /// window whether it succeeded or failed (`stamp_auto_connect_attempt`,
    /// C :718).
    ///
    /// Returns whether the port is connected afterwards (C's `:721` bail-out),
    /// or `false` outright when [`Self::connect_gate`] refuses — the whole
    /// device level below hangs off that `false` (:755), so one gate check
    /// covers both levels.
    fn auto_connect_port(&mut self) -> bool {
        // C `portThread` re-applies the queue gate's unconditional refusal
        // before it reaches *either* `autoConnectDevice` call site:
        // `if(!pport->dpc.enabled) continue;` (:806-809) sits above the
        // Connect-queue drain, above `autoConnectDevice(pport,0)` (:856-861)
        // and above the per-request one (:876-883).
        if self.connect_gate().is_err() {
            return false;
        }
        if !self.driver.base().is_connected() && self.driver.base().auto_connect {
            if !self
                .driver
                .base()
                .auto_connect_throttle_ok(-1, Instant::now())
            {
                return false;
            }
            // Addr -1: this is the *port's* attempt. C derives it as
            // `addr = (pdevice ? pdevice->addr : -1)` in `connectAttempt`
            // (asynManager.c:752), and the port-level caller passes
            // `&pport->dpc`, i.e. no device (:716). A driver that reads the
            // address — prologix branches on it exactly as C's `prologixConnect`
            // does — must not see this as a device 0 connect.
            let _ = self.driver.connect(&AsynUser::default().with_addr(-1));
            self.driver
                .base_mut()
                .stamp_auto_connect_attempt(-1, Instant::now());
        }
        self.driver.base().is_connected()
    }

    /// C `autoConnectDevice` (asynManager.c:704-739) — the single owner of
    /// every auto-connect attempt, port-level and device-level.
    ///
    /// The order is the whole point: C reconnects the **port** first
    /// (`connectAttempt(&pport->dpc)` at :716) and bails out if the port is
    /// still down (:721), only then reconnecting the device (:723-737). A
    /// multi-device port that skipped straight to the device would never
    /// reopen its transport — `PortDriver::connect_addr`'s default just
    /// flips the per-address `connected` flag (port.rs) and opens no socket,
    /// so the port-level `check_ready` that follows rejects the request
    /// forever.
    ///
    /// Both levels are throttled to one attempt per 2 s window
    /// (`auto_connect_throttle_ok`, C :712-713 / :729-730), and every attempt
    /// restarts the window whether it succeeded or failed
    /// (`stamp_auto_connect_attempt`, C :718 / :735) — so a burst of N queued
    /// requests to an offline port fires one attempt, not N. A port whose
    /// throttle window has not elapsed gives up for this request exactly as C
    /// does (`return FALSE` at :713), without falling through to the device.
    ///
    /// Returns whether the addressed device is connected afterwards. The
    /// single-threaded actor owns the driver throughout, so C's
    /// `autoConnectActive` re-entry guard has no observable analogue here.
    fn auto_connect_device(&mut self, addr: i32, reason: usize) -> bool {
        // --- Port level (C :705-721). Runs for single- and multi-device
        // ports alike: in C the port `dpCommon` is reconnected before any
        // device `dpCommon` is even looked at.
        if !self.auto_connect_port() {
            // C :721 — the port is still down, so the device cannot come up.
            return false;
        }
        if !self.driver.base().flags.multi_device {
            // C :722 `if(!pdevice) return TRUE`.
            return true;
        }

        // --- Device level (C :723-737).
        let ds = self.driver.base().device_states.get(&addr);
        let dev_disconnected = !ds.is_none_or(|d| d.connected);
        let dev_auto = ds.map_or(self.driver.base().auto_connect, |d| d.auto_connect);
        if dev_disconnected && dev_auto {
            if !self
                .driver
                .base()
                .auto_connect_throttle_ok(addr, Instant::now())
            {
                return false;
            }
            let connect_user = AsynUser::new(reason).with_addr(addr);
            let _ = self.driver.connect_addr(&connect_user);
            self.driver
                .base_mut()
                .stamp_auto_connect_attempt(addr, Instant::now());
        }
        self.driver.base().is_device_connected(addr)
    }

    /// The octet read every request op goes through: the port's interpose chain,
    /// ending at the driver (C `asynOctet::read` on the interface `findInterface`
    /// resolves — [`crate::port::octet_read_chain`]).
    ///
    /// The octet interrupt fan-out is NOT here. In C it lives in
    /// `asynOctetBase::readIt` (asynOctetBase.c:224-238), which is interposed
    /// directly on the driver and therefore sits *below* the EOS layer — so it
    /// runs per lower-level read, on the raw driver chunk. That position is
    /// `DriverOctetLink::read`; firing it here instead handed
    /// interrupt users the post-EOS message and the EOS eomReason (R19-112).
    fn octet_read(
        &mut self,
        user: &AsynUser,
        buf_size: usize,
    ) -> AsynResult<(Vec<u8>, usize, EomReason)> {
        let mut buf = vec![0u8; buf_size];
        let (n, eom) = crate::port::octet_read_chain(&mut *self.driver, user, &mut buf)?;
        buf.truncate(n);
        Ok((buf, n, eom))
    }

    /// The port's octet write: through the interpose chain, ending at the driver
    /// (C `asynOctet::write` on the interface `findInterface` resolves —
    /// [`crate::port::octet_write_chain`]).
    fn octet_write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        crate::port::octet_write_chain(&mut *self.driver, user, data)
    }

    /// The port's octet flush, through the same chain (C `asynOctet::flush`).
    fn octet_flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
        crate::port::octet_flush_chain(&mut *self.driver, user)
    }

    fn dispatch_io(&mut self, user: &mut AsynUser, op: &RequestOp) -> AsynResult<RequestResult> {
        let is_read = matches!(
            op,
            RequestOp::Int32Read
                | RequestOp::Int64Read
                | RequestOp::Float64Read
                | RequestOp::OctetRead { .. }
                | RequestOp::OctetReadBinary { .. }
                | RequestOp::OctetWriteRead { .. }
                | RequestOp::UInt32DigitalRead { .. }
                | RequestOp::EnumRead
                | RequestOp::Int32ArrayRead { .. }
                | RequestOp::Float64ArrayRead { .. }
                | RequestOp::Int8ArrayRead { .. }
                | RequestOp::Int16ArrayRead { .. }
                | RequestOp::Int64ArrayRead { .. }
                | RequestOp::Float32ArrayRead { .. }
        );

        let result = match op {
            RequestOp::OctetWrite { data } => {
                let n = self.octet_write(user, data)?;
                Ok(RequestResult::write_n(n))
            }
            RequestOp::OctetRead { buf_size } => {
                let (buf, n, eom) = self.octet_read(user, *buf_size)?;
                Ok(RequestResult::octet_read_eom(buf, n, eom.bits()))
            }
            RequestOp::OctetWriteBinary { data } => {
                // C parity: asynRecord binary output (asynRecord.c:1528-1541).
                // Save the driver's output EOS, clear it for the raw write,
                // and restore it on every exit path so a configured OEOS does
                // not append terminator bytes to a binary payload. The actor
                // owns the bracket atomically under its serial dispatch.
                //
                // The bracket runs only when there IS a saved terminator, as
                // C's `if (saveEosLen)` guard does (:1535, :1540) — a driver
                // with no EOS methods answers `asynError` to both accessors
                // (asynOctetBase.c:401-411) and C explicitly tolerates that
                // (:1531 "getOutputEos can return an error if the driver does
                // not implement it"). Clearing unconditionally turned every
                // binary transfer on such a port into that refusal.
                let saved = self.driver.get_output_eos(user);
                let bracket = !saved.is_empty();
                if bracket {
                    self.driver.set_output_eos(user, &[])?;
                }
                let res = self.octet_write(user, data);
                if bracket {
                    let _ = self.driver.set_output_eos(user, &saved);
                }
                let n = res?;
                Ok(RequestResult::write_n(n))
            }
            RequestOp::OctetReadBinary { buf_size } => {
                // C parity: asynRecord binary input (asynRecord.c:1564-1577).
                // Save the driver's input EOS, clear it for the read, and
                // restore it on every exit path so a configured IEOS does not
                // stop the read early or strip bytes belonging to the binary
                // payload. Guarded by `saveEosLen` exactly as C guards it
                // (:1571, :1576) — see `OctetWriteBinary` above.
                let saved = self.driver.get_input_eos(user);
                let bracket = !saved.is_empty();
                if bracket {
                    self.driver.set_input_eos(user, &[])?;
                }
                let res = self.octet_read(user, *buf_size);
                if bracket {
                    let _ = self.driver.set_input_eos(user, &saved);
                }
                let (buf, n, eom) = res?;
                Ok(RequestResult::octet_read_eom(buf, n, eom.bits()))
            }
            RequestOp::OctetWriteRead {
                data,
                buf_size,
                flush,
            } => {
                // Two C patterns share this op, distinguished by `flush`:
                //   flush=true  — asynOctetSyncIO::writeRead (asynOctetSyncIO.c:250)
                //     does flush() → write() → read() under a single
                //     queueLockPort. The flush drains stale bytes left in the
                //     driver's input buffer so the post-write read returns only
                //     the response to *this* command (the StreamDevice /
                //     asynRecord writeRead pattern).
                //   flush=false — devAsynOctet command-response
                //     (callbackSiCmdResponse, devAsynOctet.c:853-855) does plain
                //     writeIt → readIt on the raw asynOctet interface with NO
                //     flush, so the read returns whatever the device sends,
                //     including bytes already on the warm line.
                // Both run atomically here: the actor owns the port for the whole
                // op, the queueLockPort equivalent.
                if *flush {
                    self.octet_flush(user)?;
                }
                self.octet_write(user, data)?;
                let (buf, n, eom) = self.octet_read(user, *buf_size)?;
                Ok(RequestResult::octet_read_eom(buf, n, eom.bits()))
            }
            RequestOp::Int32Write { value } => {
                self.driver.io_write_int32(user, *value)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int32Read => {
                let v = self.driver.io_read_int32(user)?;
                Ok(RequestResult::int32_read(v))
            }
            RequestOp::Int64Write { value } => {
                self.driver.io_write_int64(user, *value)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int64Read => {
                let v = self.driver.io_read_int64(user)?;
                Ok(RequestResult::int64_read(v))
            }
            RequestOp::Float64Write { value } => {
                self.driver.io_write_float64(user, *value)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Float64Read => {
                let v = self.driver.io_read_float64(user)?;
                Ok(RequestResult::float64_read(v))
            }
            RequestOp::UInt32DigitalWrite { value, mask } => {
                self.driver.io_write_uint32_digital(user, *value, *mask)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::UInt32DigitalRead { mask } => {
                let v = self.driver.io_read_uint32_digital(user, *mask)?;
                Ok(RequestResult::uint32_read(v))
            }
            RequestOp::Flush => {
                self.octet_flush(user)?;
                Ok(RequestResult::write_ok())
            }
            // C `asynCallbackProcess` under `tmod == asynTMOD_NoIO`: the queue
            // entry is the whole request, and the callback body calls nothing
            // (asynRecord.c:827). Everything this op exists for — the port-thread
            // wake that runs auto-connect, and the queue gate's refusal on a
            // down port — has already happened by the time the dispatch gets
            // here.
            RequestOp::NoIo => Ok(RequestResult::write_ok()),
            RequestOp::Connect => {
                self.driver.connect(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Disconnect => {
                self.driver.disconnect(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::ShutdownPort => {
                // C `shutdownPort` lifecycle — opt-in via destructible
                // flag. Calls the driver's own shutdown() hook *after*
                // marking the lifecycle complete so the announcer sees
                // the port already-defunct.
                self.driver.base_mut().shutdown_lifecycle()?;
                // Driver's own shutdown plumbing (release hardware
                // handles, etc.). Errors are tolerated — the port is
                // already defunct and there is no recovery path.
                let _ = self.driver.shutdown();
                Ok(RequestResult::write_ok())
            }
            RequestOp::ConnectAddr => {
                self.driver.connect_addr(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::DisconnectAddr => {
                self.driver.disconnect_addr(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::EnableAddr => {
                self.driver.enable_addr(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::DisableAddr => {
                self.driver.disable_addr(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetEnable { yes } => {
                // C parity: pasynManager->enable(pasynUser, enable) at
                // asynManager.c — toggles per-port `enabled` state and
                // emits `asynExceptionEnable`. Routed through the
                // driver trait so subclasses can override.
                if *yes {
                    self.driver.enable(user)?;
                } else {
                    self.driver.disable(user)?;
                }
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetAutoConnect { yes } => {
                // C parity: pasynManager->autoConnect(pasynUser, value)
                // at asynManager.c:2310-2324 — fires
                // `asynExceptionAutoConnect` unconditionally.
                self.driver.base_mut().set_auto_connect(*yes);
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetAutoConnectAddr { yes } => {
                // The device half of the same C call: `findDpCommon` hands
                // autoConnectAsyn the DEVICE's dpCommon when the user names one
                // on a multi-device port (asynManager.c:536-544, 2314).
                self.driver
                    .base_mut()
                    .set_auto_connect_addr(user.addr, *yes);
                Ok(RequestResult::write_ok())
            }
            RequestOp::GetEnable => {
                let enabled = self.driver.base().enabled;
                Ok(RequestResult::int32_read(i32::from(enabled)))
            }
            RequestOp::GetAutoConnect => {
                let auto = self.driver.base().auto_connect;
                Ok(RequestResult::int32_read(i32::from(auto)))
            }
            RequestOp::PushEchoInterpose => {
                // C `asynInterposeEcho` (asynInterposeEcho.c:165-186) pushes the
                // layer onto the octet interface of the device its `addr`
                // argument names (:176), at any time after configure. The actor
                // owns the driver, so the install lands between transfers instead
                // of racing one; the addr rides the request's own user, which is
                // C's `pasynUser`-selects-the-device linkage.
                self.driver.base_mut().install_octet_interpose_addr(
                    user.addr,
                    Box::new(crate::interpose::echo::EchoInterpose::new()),
                );
                Ok(RequestResult::write_ok())
            }
            RequestOp::PushDelayInterpose { delay } => {
                // C `asynInterposeDelay` (asynInterposeDelay.c:176-215) pushes
                // the per-character write delay onto the addressed device's octet
                // interface from the iocsh command (:187,200,221-234). Same
                // actor-owned install point, same per-device addr, as the echo
                // layer above: `asynInterposeDelay("gpib",4,0.01)` slows device 4
                // and leaves the rest of the bus at full speed.
                self.driver.base_mut().install_octet_interpose_addr(
                    user.addr,
                    Box::new(crate::interpose::delay::DelayInterpose::new(*delay)),
                );
                Ok(RequestResult::write_ok())
            }
            RequestOp::PushEosInterpose {
                process_in,
                process_out,
            } => {
                // C `asynInterposeEosConfig` (asynInterposeEos.c:84-140) pushes
                // the EOS layer onto the addressed device's octet interface at
                // any time after configure — the same actor-owned install point
                // as the echo and delay layers. The terminators themselves
                // arrive later, through `asynOctetSetInputEos`.
                self.driver.base_mut().install_octet_interpose_addr(
                    user.addr,
                    Box::new(crate::interpose::eos::EosInterpose::with_processing(
                        crate::interpose::eos::EosConfig::default(),
                        *process_in,
                        *process_out,
                    )),
                );
                Ok(RequestResult::write_ok())
            }
            RequestOp::PushFlushInterpose { flush_timeout } => {
                // C `asynInterposeFlushConfig` (asynInterposeFlush.c:66-91).
                self.driver.base_mut().install_octet_interpose_addr(
                    user.addr,
                    Box::new(crate::interpose::flush::FlushTimeoutInterpose::new(
                        *flush_timeout,
                    )),
                );
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetTimeStampSource { name } => {
                // C `registerTimeStampSource` / `unregisterTimeStampSource`
                // (asynManager.c:333-334). The actor owns the driver, so the
                // swap lands between transfers — a value being stamped right
                // now finishes on the source it started with.
                match name {
                    Some(n) => match crate::timestamp::find_time_stamp_source(n) {
                        Some(source) => {
                            self.driver
                                .base_mut()
                                .register_timestamp_source(move || source());
                            Ok(RequestResult::write_ok())
                        }
                        // C prints "cannot find function %s" and refuses
                        // (asynShellCommands.c:1198-1201).
                        None => Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!("cannot find time-stamp source '{n}'"),
                        }),
                    },
                    None => {
                        self.driver.base_mut().unregister_timestamp_source();
                        Ok(RequestResult::write_ok())
                    }
                }
            }
            RequestOp::GetConnected => {
                // C `pasynManager->isConnected`: the transport state the driver
                // publishes, which `asynRecord` reads back into CNCT and gates
                // its connect/disconnect request on (asynRecord.c:1089-1093,
                // 858-888).
                let connected = self.driver.base().is_connected();
                Ok(RequestResult::int32_read(i32::from(connected)))
            }
            RequestOp::GetBoundsInt32 => {
                let (low, high) = self.driver.get_bounds_int32(user)?;
                Ok(RequestResult::bounds_read(low as i64, high as i64))
            }
            RequestOp::GetBoundsInt64 => {
                let (low, high) = self.driver.get_bounds_int64(user)?;
                Ok(RequestResult::bounds_read(low, high))
            }
            RequestOp::BlockProcess { all_devices } => {
                // Block-token contract: the block identity is
                // `user.block_token` when set, else it falls back to
                // `user.reason`. CALLER CONTRACT: any caller that
                // relies on block/unblock exclusivity MUST set a
                // distinct `block_token` — two callers that share a
                // `reason` and both omit `block_token` would collide
                // on the same fallback token, so one could
                // unblock/nest the other's lock. `block` /
                // `unblock` / and any owner-gated op must use the
                // SAME token; mismatched tokens are rejected below.
                let token = user.block_token.unwrap_or(user.reason as u64);
                let dp = self.dp_key(user.addr);
                match self.block_holders.holder(*all_devices, dp) {
                    Some((existing, count)) => {
                        if existing != token {
                            return Err(AsynError::Status {
                                status: AsynStatus::Error,
                                message: "port already blocked by another user".into(),
                            });
                        }
                        // C parity: nested lock — increment the count
                        // (`blockPortCount++` / `blockDeviceCount++`,
                        // asynManager.c:1717-1720).
                        self.block_holders
                            .set_holder(*all_devices, dp, Some((token, count + 1)));
                    }
                    None => {
                        self.block_holders
                            .set_holder(*all_devices, dp, Some((token, 1)));
                        // Messages already drained into the heap before this
                        // BlockProcess executed would otherwise still be
                        // dispatched to the driver, breaking block exclusivity.
                        // enqueue_message only diverts messages that arrive
                        // *after* the block, so re-test the heap against the
                        // holders now. Re-testing rather than comparing tokens
                        // is what keeps this in step with the gate: a device
                        // block must divert only the requests resolving to
                        // *that* `dpCommon`, and the gate is the one place that
                        // knows which those are.
                        let drained: Vec<ActorMessage> = self.heap.drain().collect();
                        for msg in drained {
                            if Self::is_lifecycle_op(&msg.op) || self.admits(&msg) {
                                self.heap.push(msg);
                            } else {
                                self.pending_while_blocked.push(msg);
                            }
                        }
                    }
                }
                Ok(RequestResult::write_ok())
            }
            RequestOp::UnblockProcess { all_devices } => {
                let token = user.block_token.unwrap_or(user.reason as u64);
                let dp = self.dp_key(user.addr);
                if let Some((owner, count)) = self.block_holders.holder(*all_devices, dp) {
                    if owner != token {
                        // C parity: only the block holder can unblock
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: "unblock rejected: not the block holder".into(),
                        });
                    }
                    if count > 1 {
                        self.block_holders
                            .set_holder(*all_devices, dp, Some((owner, count - 1)));
                    } else {
                        self.block_holders.set_holder(*all_devices, dp, None);
                        // C `unblockProcessCallback` signals the port thread only
                        // when the block was actually released (`wasOwner`,
                        // asynManager.c:1772) — the nested decrement above leaves
                        // the port blocked and signals nothing.
                        self.woken = true;
                        // Only what the *remaining* holders now admit goes back
                        // to the heap: releasing the device holder must not free
                        // requests the port-wide holder still gates, and vice
                        // versa. C gets this for free because it never moved
                        // them off the queue in the first place (:889-892).
                        let pending = std::mem::take(&mut self.pending_while_blocked);
                        for msg in pending {
                            if self.admits(&msg) {
                                self.heap.push(msg);
                            } else {
                                self.pending_while_blocked.push(msg);
                            }
                        }
                    }
                }
                Ok(RequestResult::write_ok())
            }
            RequestOp::DrvUserCreate(req) => {
                let info = self.driver.drv_user_create(req)?;
                Ok(RequestResult::drv_user_create(
                    info.reason,
                    info.max_octet_len,
                ))
            }
            RequestOp::EnumRead => {
                // Carry the driver's enum table (strings/values/severities)
                // alongside the current index so device-support init can push
                // it onto the record's state fields — C devAsynInt32.c::initCommon
                // reads asynEnum and calls setEnums (298-324, 415-435). Dropping
                // the table left mbbi/mbbo/bi/bo with their .db state strings.
                let (idx, entries) = self.driver.read_enum(user)?;
                Ok(RequestResult::enum_read_with_entries(idx, entries))
            }
            RequestOp::EnumWrite { index } => {
                self.driver.write_enum(user, *index)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int32ArrayRead { max_elements } => {
                let mut buf = vec![0i32; *max_elements];
                let n = self.driver.read_int32_array(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::int32_array_read(buf))
            }
            RequestOp::Int32ArrayWrite { data } => {
                self.driver.write_int32_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Float64ArrayRead { max_elements } => {
                let mut buf = vec![0f64; *max_elements];
                let n = self.driver.read_float64_array(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::float64_array_read(buf))
            }
            RequestOp::Float64ArrayWrite { data } => {
                self.driver.write_float64_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int8ArrayRead { max_elements } => {
                let mut buf = vec![0i8; *max_elements];
                let n = self.driver.read_int8_array(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::int8_array_read(buf))
            }
            RequestOp::Int8ArrayWrite { data } => {
                self.driver.write_int8_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int16ArrayRead { max_elements } => {
                let mut buf = vec![0i16; *max_elements];
                let n = self.driver.read_int16_array(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::int16_array_read(buf))
            }
            RequestOp::Int16ArrayWrite { data } => {
                self.driver.write_int16_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int64ArrayRead { max_elements } => {
                let mut buf = vec![0i64; *max_elements];
                let n = self.driver.read_int64_array(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::int64_array_read(buf))
            }
            RequestOp::Int64ArrayWrite { data } => {
                self.driver.write_int64_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Float32ArrayRead { max_elements } => {
                let mut buf = vec![0f32; *max_elements];
                let n = self.driver.read_float32_array(user, &mut buf)?;
                buf.truncate(n);
                Ok(RequestResult::float32_array_read(buf))
            }
            RequestOp::Float32ArrayWrite { data } => {
                self.driver.write_float32_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::CallParamCallbacks { addr, updates } => {
                let base = self.driver.base_mut();
                // One dispatch for every parameter type (`ParamList::set_value`),
                // so this carrier and the store cannot support different type sets.
                let mut failed: Option<AsynError> = None;
                for u in updates {
                    let outcome = match u {
                        crate::request::ParamSetValue::Value {
                            reason,
                            addr,
                            value,
                        } => base.params.set_value(*reason, *addr, value.clone()),
                        crate::request::ParamSetValue::UInt32Digital {
                            reason,
                            addr,
                            value,
                            mask,
                            interrupt_mask,
                        } => base.set_uint32_param(*reason, *addr, *value, *mask, *interrupt_mask),
                        crate::request::ParamSetValue::Status {
                            reason,
                            addr,
                            status,
                            alarm_status,
                            alarm_severity,
                        } => base.set_param_status(
                            *reason,
                            *addr,
                            *status,
                            *alarm_status,
                            *alarm_severity,
                        ),
                    };
                    // A set that fails (a value the parameter cannot hold) is
                    // reported, not swallowed: C `asynPortDriver::setIntegerParam`
                    // asynPrints the paramList error at ASYN_TRACE_ERROR and
                    // returns the status. The remaining updates still land and the
                    // callbacks still fire — one bad update must not strand the
                    // good ones — and the first error is returned to whoever waits
                    // on the reply.
                    if let Err(e) = outcome {
                        base.trace_print(
                            crate::trace::TraceMask::ERROR,
                            &format!("callParamCallbacks: param set failed: {e}\n"),
                        );
                        failed.get_or_insert(e);
                    }
                }
                // Flush every address list this batch wrote, not just the
                // request's. C has no bundled set-then-flush: a driver calls
                // `setXxxParam` per list and then `callParamCallbacks(addr)`
                // once for each list it touched (asynPortDriver.cpp:820). This
                // carrier bundles the two, so it owes a flush for every list it
                // just wrote — `call_param_callbacks` consumes the changed flags
                // of one list only, so flushing the request address alone stores
                // the other lists' values while firing no interrupt for them,
                // and a multi-device driver's per-address records never update
                // at all (six joint angles written at addresses 0..6, one
                // I/O Intr). Request address first, then each further address in
                // the order the batch first wrote it.
                let mut lists = vec![*addr];
                for u in updates {
                    if !lists.contains(&u.addr()) {
                        lists.push(u.addr());
                    }
                }
                // One list that cannot be flushed must not strand the rest, for
                // the same reason one bad update does not strand the good ones
                // above; the first error still reaches whoever waits on the
                // reply.
                for list in lists {
                    if let Err(e) = base.call_param_callbacks(list) {
                        failed.get_or_insert(e);
                    }
                }
                match failed {
                    Some(e) => Err(e),
                    None => Ok(RequestResult::write_ok()),
                }
            }
            RequestOp::GetOption { key } => {
                let val = self.driver.get_option(key)?;
                Ok(RequestResult::option_read(val))
            }
            RequestOp::SetOption { key, value } => {
                // The request's own AsynUser, as C hands `setOption` the caller's
                // pasynUser (asynRecord.c:1787-1826 passes the record's, whose
                // timeout is TMOT; asynShellCommands.c:119 passes iocsh's 2 s).
                // A COM port's negotiation is bounded by exactly that timeout.
                self.driver.set_option(user, key, value)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Report { level } => {
                // C parity: `asynManager::report` (asynManager.c:1136-1171) walks
                // every registered port and hands each to `reportPrintPort`
                // (:1018-1122), which prints the *manager's* view of the port and
                // only then calls the driver's `pasynCommon->report` (:1121-1123).
                // The iocsh wrapper (`asynReport`) does the per-port loop; here we
                // dispatch the per-port report from the actor thread so the driver
                // observes its own state under the actor's serial ownership.
                self.report_port(*level);
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetInputEos { eos } => {
                // C parity: asynRecord IEOS write at asynRecord.c:1964-1967
                // calls `pasynOctet->setInputEos(pasynUser, eos, len)`.
                // Route through the driver trait so the EOS interpose
                // layer (interpose/eos.rs) reads from a single source
                // of truth (`PortDriverBase::input_eos`) rather than
                // the orphaned options HashMap. The terminator belongs to the
                // device the request addressed, so the user goes with it.
                self.driver.set_input_eos(user, eos)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetOutputEos { eos } => {
                self.driver.set_output_eos(user, eos)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::GetInputEos => {
                let eos = self.driver.get_input_eos(user);
                let n = eos.len();
                Ok(RequestResult::octet_read(eos, n))
            }
            RequestOp::GetOutputEos => {
                let eos = self.driver.get_output_eos(user);
                let n = eos.len();
                Ok(RequestResult::octet_read(eos, n))
            }
            // The asynGpib command interface. C reaches the driver's
            // `asynGpibPort` methods through asynGpib's pass-through
            // (asynGpib.c:472-496) on the port thread, under the same
            // queue/auto-connect gating as any other transfer — which is where
            // this dispatch sits.
            RequestOp::GpibUniversalCmd { cmd } => {
                self.driver.gpib_universal_cmd(user, *cmd)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::GpibAddressedCmd { data } => {
                self.driver.gpib_addressed_cmd(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::GpibIfc => {
                self.driver.gpib_ifc(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::GpibRen { enable } => {
                self.driver.gpib_ren(user, *enable)?;
                Ok(RequestResult::write_ok())
            }
        };

        // Attach alarm/timestamp metadata on successful reads
        if is_read {
            if let Ok(r) = result {
                let (status, alarm_status, alarm_severity) = self
                    .driver
                    .base()
                    .params
                    .get_param_status(user.reason, user.addr)
                    .unwrap_or((crate::error::AsynStatus::Success, 0, 0));
                let ts = self
                    .driver
                    .base()
                    .params
                    .get_timestamp(user.reason, user.addr)
                    .unwrap_or(None);
                // C devAsynInt32.c:844-847 — a non-success read/param status
                // maps to a record alarm (asynStatusToEpicsAlarm) and is
                // recGblSetSevr'd together with the explicit setParamAlarm.
                // The old `_` discarded the status, so a setParamStatus(error
                // /timeout) read returned clean (no INVALID, UDF-clearing).
                let (alarm_status, alarm_severity) =
                    combine_read_alarm(status, alarm_status, alarm_severity);
                // Carry the device read status (C pasynUser->auxStatus) to the
                // device-support consumer so it can gate the value store the way
                // C processAi does (store iff result.status == asynSuccess,
                // devAsynInt32.c:848-855). Kept distinct from `status` (the
                // op/request outcome) so the network reply path — which keys on
                // `status` to emit an Error reply — is unaffected.
                let mut out = r.with_alarm(alarm_status, alarm_severity, ts);
                out.aux_status = status;
                return Ok(out);
            }
        }

        result
    }
}

/// Combine a param's stored asynStatus with its explicit setParamAlarm into
/// the record alarm for a READ, mirroring C `asynStatusToEpicsAlarm`
/// (asynEpicsUtils.c:234) as invoked by devAsynXxx processXxx
/// (devAsynInt32.c:844-847): the explicit alarm wins per field, and a
/// non-success status fills any field still None — status maps to a
/// condition with READ_ALARM as the asynError/default condition and INVALID
/// as the severity.
fn combine_read_alarm(status: AsynStatus, alarm_status: u16, alarm_severity: u16) -> (u16, u16) {
    use alarm as al;
    let stat_default = match status {
        AsynStatus::Success => return (alarm_status, alarm_severity),
        AsynStatus::Timeout => al::TIMEOUT_ALARM,
        AsynStatus::Overflow => al::HW_LIMIT_ALARM,
        AsynStatus::Disconnected => al::COMM_ALARM,
        AsynStatus::Disabled => al::DISABLE_ALARM,
        AsynStatus::Error => al::READ_ALARM,
    };
    let stat = if alarm_status == al::NO_ALARM {
        stat_default
    } else {
        alarm_status
    };
    let sevr = if alarm_severity == al::SEV_NO_ALARM {
        al::SEV_INVALID
    } else {
        alarm_severity
    };
    (stat, sevr)
}

/// The EPICS alarm condition and severity codes this mapping produces.
///
/// Owned here rather than imported from `epics_base_rs` because the mapping is
/// asyn's, not base's: C keeps `asynStatusToEpicsAlarm` in asyn's own
/// `asynEpicsUtils.c:234-265` and takes only the *numbers* from base's
/// `alarm.h`. Importing them instead made a core module depend on the optional
/// `epics` feature, which is why `--no-default-features` never compiled.
///
/// The values are the `menuAlarmStat.dbd` / `menuAlarmSevr.dbd` ordinals, fixed
/// by `libcom/src/misc/alarm.h:62-85` and part of the EPICS record ABI.
/// `tests::asyn_alarm_codes_match_epics_base` holds them to
/// `epics_base_rs`'s under the `epics` feature, so a drift on either side is a
/// test failure rather than a silently wrong ALARM/SEVR field.
mod alarm {
    pub const NO_ALARM: u16 = 0;
    pub const READ_ALARM: u16 = 1;
    pub const COMM_ALARM: u16 = 9;
    pub const TIMEOUT_ALARM: u16 = 10;
    pub const HW_LIMIT_ALARM: u16 = 11;
    pub const DISABLE_ALARM: u16 = 18;

    pub const SEV_NO_ALARM: u16 = 0;
    pub const SEV_INVALID: u16 = 3;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interrupt::InterruptFilter;
    use crate::param::ParamType;
    use crate::port::{PortDriverBase, PortFlags};
    use std::sync::Arc;
    use std::time::Duration;

    struct TestDriver {
        base: PortDriverBase,
    }

    impl TestDriver {
        fn new() -> Self {
            let mut base = PortDriverBase::new("actor_test", 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.create_param("F64", ParamType::Float64).unwrap();
            base.create_param("MSG", ParamType::Octet).unwrap();
            base.create_param("BIG", ParamType::Int64).unwrap();
            Self { base }
        }
    }

    impl PortDriver for TestDriver {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }

    /// Take a port through C's `shutdownPort` (asynManager.c:2251-2308) — the
    /// only way to reach the defunct state, which is what keeps the invariant
    /// `defunct ⟹ !enabled` true by construction (R15-50).
    /// A driver that is nothing but its base — for the tests that assert on what
    /// the *manager* prints or checks, where the driver's own behaviour is noise.
    struct TestDriverBase(PortDriverBase);
    impl PortDriver for TestDriverBase {
        fn base(&self) -> &PortDriverBase {
            &self.0
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.0
        }
    }

    fn shut_down(base: &mut PortDriverBase) {
        base.flags.destructible = true;
        base.shutdown_lifecycle().unwrap();
    }

    fn spawn_actor(driver: impl PortDriver) -> mpsc::Sender<ActorMessage> {
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        tx
    }

    fn send_and_wait(
        tx: &mpsc::Sender<ActorMessage>,
        op: RequestOp,
        user: AsynUser,
    ) -> AsynResult<RequestResult> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = ActorMessage::new(op, user, CancelToken::new(), reply_tx);
        tx.blocking_send(msg).expect("actor channel closed");
        reply_rx.blocking_recv().expect("actor dropped reply")
    }

    #[test]
    fn actor_int32_write_read() {
        let tx = spawn_actor(TestDriver::new());
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Int32Write { value: 42 }, user).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user).unwrap();
        assert_eq!(result.int_val, Some(42));
    }

    /// The owned codes in [`super::alarm`] are the EPICS record ABI, so they
    /// must equal `epics_base_rs`'s wherever both exist. Gated on `epics`
    /// because that is the only configuration in which base is linked at all —
    /// the point of owning them is that the mapping compiles without it.
    #[cfg(feature = "epics")]
    #[test]
    fn asyn_alarm_codes_match_epics_base() {
        use epics_base_rs::server::recgbl::alarm_status as base;
        use epics_base_rs::server::record::AlarmSeverity;
        assert_eq!(super::alarm::NO_ALARM, base::NO_ALARM);
        assert_eq!(super::alarm::READ_ALARM, base::READ_ALARM);
        assert_eq!(super::alarm::COMM_ALARM, base::COMM_ALARM);
        assert_eq!(super::alarm::TIMEOUT_ALARM, base::TIMEOUT_ALARM);
        assert_eq!(super::alarm::HW_LIMIT_ALARM, base::HW_LIMIT_ALARM);
        assert_eq!(super::alarm::DISABLE_ALARM, base::DISABLE_ALARM);
        assert_eq!(super::alarm::SEV_NO_ALARM, AlarmSeverity::NoAlarm as u16);
        assert_eq!(super::alarm::SEV_INVALID, AlarmSeverity::Invalid as u16);
    }

    #[test]
    fn combine_read_alarm_matches_c_status_to_epics_alarm() {
        use super::alarm as al;
        let none = al::SEV_NO_ALARM;
        // MAJOR_ALARM — `epicsSevMajor`, alarm.h:42.
        let major = 2u16;
        let invalid = al::SEV_INVALID;
        // Success: the explicit alarm passes through unchanged (incl. clean).
        assert_eq!(
            combine_read_alarm(AsynStatus::Success, al::NO_ALARM, none),
            (al::NO_ALARM, none)
        );
        assert_eq!(
            combine_read_alarm(AsynStatus::Success, al::HW_LIMIT_ALARM, major),
            (al::HW_LIMIT_ALARM, major)
        );
        // Non-success, no explicit alarm: status-derived condition + INVALID.
        assert_eq!(
            combine_read_alarm(AsynStatus::Error, al::NO_ALARM, none),
            (al::READ_ALARM, invalid)
        );
        assert_eq!(
            combine_read_alarm(AsynStatus::Timeout, al::NO_ALARM, none),
            (al::TIMEOUT_ALARM, invalid)
        );
        assert_eq!(
            combine_read_alarm(AsynStatus::Overflow, al::NO_ALARM, none),
            (al::HW_LIMIT_ALARM, invalid)
        );
        assert_eq!(
            combine_read_alarm(AsynStatus::Disconnected, al::NO_ALARM, none),
            (al::COMM_ALARM, invalid)
        );
        assert_eq!(
            combine_read_alarm(AsynStatus::Disabled, al::NO_ALARM, none),
            (al::DISABLE_ALARM, invalid)
        );
        // Explicit alarm present wins per field over the status-derived one.
        assert_eq!(
            combine_read_alarm(AsynStatus::Error, al::COMM_ALARM, major),
            (al::COMM_ALARM, major)
        );
    }

    #[test]
    fn actor_read_surfaces_param_status_alarm() {
        use super::alarm as al;
        // A defined param flagged setParamStatus(Error) with no explicit
        // setParamAlarm: the read must surface READ_ALARM/INVALID (C
        // devAsynInt32.c:844-847), not a clean (UDF-clearing) read.
        let mut drv = TestDriver::new();
        drv.base.params.set_int32(0, 0, 5).unwrap(); // define VAL
        drv.base
            .params
            .set_param_status(0, 0, AsynStatus::Error, 0, 0)
            .unwrap();
        let tx = spawn_actor(drv);

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user).unwrap();
        assert_eq!(result.int_val, Some(5));
        assert_eq!(result.alarm_status, al::READ_ALARM);
        assert_eq!(result.alarm_severity, al::SEV_INVALID);
        // The transport status is also carried as aux_status (distinct from the
        // op-level `status`, which stays Success) so device support can gate the
        // value store on it — C processAi (devAsynInt32.c:848-855).
        assert_eq!(result.aux_status, AsynStatus::Error);
        assert_eq!(
            result.status,
            AsynStatus::Success,
            "op-level status stays Success so the network reply path is unaffected"
        );
    }

    #[test]
    fn actor_float64_write_read() {
        let tx = spawn_actor(TestDriver::new());
        let user = AsynUser::new(1).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Float64Write { value: 3.14 }, user).unwrap();

        let user = AsynUser::new(1).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Float64Read, user).unwrap();
        assert!((result.float_val.unwrap() - 3.14).abs() < 1e-10);
    }

    #[test]
    fn actor_int64_write_read() {
        let tx = spawn_actor(TestDriver::new());
        let user = AsynUser::new(3).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Int64Write { value: i64::MAX }, user).unwrap();

        let user = AsynUser::new(3).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int64Read, user).unwrap();
        assert_eq!(result.int64_val, Some(i64::MAX));
    }

    #[test]
    fn actor_octet_write_read() {
        let tx = spawn_actor(TestDriver::new());
        let user = AsynUser::new(2).with_timeout(Duration::from_secs(1));
        send_and_wait(
            &tx,
            RequestOp::OctetWrite {
                data: b"hello".to_vec(),
            },
            user,
        )
        .unwrap();

        let user = AsynUser::new(2).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::OctetRead { buf_size: 256 }, user).unwrap();
        assert_eq!(&result.data.unwrap()[..5], b"hello");
    }

    /// C parity: asynRecord binary I/O (asynRecord.c:1528-1577) saves the
    /// driver EOS, clears it for the raw transfer, and restores it. The
    /// OctetWriteBinary/OctetReadBinary ops must observe an empty EOS during
    /// the binary transfer and leave the configured EOS intact afterward.
    #[test]
    fn actor_octet_binary_io_suppresses_and_restores_eos() {
        struct EosObserver {
            base: PortDriverBase,
            write_eos: Arc<parking_lot::Mutex<Vec<u8>>>,
            read_eos: Arc<parking_lot::Mutex<Vec<u8>>>,
        }
        impl PortDriver for EosObserver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                *self.write_eos.lock() = self.base().output_eos(_user.addr).to_vec();
                Ok(data.len())
            }
            fn io_read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
                *self.read_eos.lock() = self.base().input_eos(_user.addr).to_vec();
                let resp = [0x01u8, 0x02];
                let n = resp.len().min(buf.len());
                buf[..n].copy_from_slice(&resp[..n]);
                Ok(n)
            }
        }

        let write_eos = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let read_eos = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut eos_base = PortDriverBase::new("eos_test", 1, PortFlags::default());
        eos_base.install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
        let tx = spawn_actor(EosObserver {
            base: eos_base,
            write_eos: write_eos.clone(),
            read_eos: read_eos.clone(),
        });
        let mk = || AsynUser::new(0).with_timeout(Duration::from_secs(1));

        send_and_wait(
            &tx,
            RequestOp::SetOutputEos {
                eos: b"\r\n".to_vec(),
            },
            mk(),
        )
        .unwrap();
        send_and_wait(
            &tx,
            RequestOp::SetInputEos {
                eos: b"\n".to_vec(),
            },
            mk(),
        )
        .unwrap();

        // Binary transfers observe a suppressed EOS.
        send_and_wait(
            &tx,
            RequestOp::OctetWriteBinary {
                data: vec![0x00, 0x01],
            },
            mk(),
        )
        .unwrap();
        assert!(
            write_eos.lock().is_empty(),
            "binary write must see the output EOS suppressed"
        );
        send_and_wait(&tx, RequestOp::OctetReadBinary { buf_size: 16 }, mk()).unwrap();
        assert!(
            read_eos.lock().is_empty(),
            "binary read must see the input EOS suppressed"
        );

        // A subsequent non-binary transfer sees the restored EOS.
        send_and_wait(
            &tx,
            RequestOp::OctetWrite {
                data: b"x".to_vec(),
            },
            mk(),
        )
        .unwrap();
        assert_eq!(
            &*write_eos.lock(),
            b"\r\n",
            "output EOS must be restored after a binary write"
        );
        send_and_wait(&tx, RequestOp::OctetRead { buf_size: 16 }, mk()).unwrap();
        assert_eq!(
            &*read_eos.lock(),
            b"\n",
            "input EOS must be restored after a binary read"
        );
    }

    /// C parity: asynOctetSyncIO::writeRead (asynOctetSyncIO.c:250)
    /// calls flush() before write() so any stale input bytes (echoes,
    /// half-received responses from a previous command) are drained
    /// out of the driver's input buffer before the new write+read
    /// pair. The atomic OctetWriteRead op must do the same.
    #[test]
    fn actor_octet_write_read_calls_flush_first() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct FlushTracker {
            base: PortDriverBase,
            flush_calls: Arc<AtomicUsize>,
            write_calls: Arc<AtomicUsize>,
            sequence: Arc<parking_lot::Mutex<Vec<&'static str>>>,
        }
        impl PortDriver for FlushTracker {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                self.flush_calls.fetch_add(1, Ordering::Relaxed);
                self.sequence.lock().push("flush");
                Ok(())
            }
            fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.write_calls.fetch_add(1, Ordering::Relaxed);
                self.sequence.lock().push("write");
                Ok(data.len())
            }
            fn io_read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
                self.sequence.lock().push("read");
                let resp = b"RSP";
                let n = resp.len().min(buf.len());
                buf[..n].copy_from_slice(&resp[..n]);
                Ok(n)
            }
        }

        let flush_calls = Arc::new(AtomicUsize::new(0));
        let write_calls = Arc::new(AtomicUsize::new(0));
        let sequence = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let drv = FlushTracker {
            base: PortDriverBase::new("flush_test", 1, PortFlags::default()),
            flush_calls: flush_calls.clone(),
            write_calls: write_calls.clone(),
            sequence: sequence.clone(),
        };
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(
            &tx,
            RequestOp::OctetWriteRead {
                data: b"CMD".to_vec(),
                buf_size: 16,
                flush: true,
            },
            user,
        )
        .unwrap();
        assert_eq!(result.data.unwrap(), b"RSP".to_vec());
        assert_eq!(flush_calls.load(Ordering::Relaxed), 1, "flush called once");
        assert_eq!(write_calls.load(Ordering::Relaxed), 1);
        // Order must be: flush, then write, then read.
        let seq = sequence.lock().clone();
        assert_eq!(seq, vec!["flush", "write", "read"]);
    }

    /// C parity: `asynOctet::read` returns `nbytes` together with
    /// `int *eomReason` (`interfaces/asynOctet.h:38-40`). Drivers
    /// that override `io_read_octet_eom` must have their EOM flags
    /// propagated through `RequestResult::eom_reason` so consumers
    /// (e.g. asynRecord `EOMR`) can distinguish "byte count" /
    /// "EOS match" / "END indicator" terminations.
    #[test]
    fn actor_octet_read_eom_propagates_driver_flags() {
        use crate::interpose::EomReason;

        struct EomDriver {
            base: PortDriverBase,
        }
        impl PortDriver for EomDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_read_octet_eom(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> AsynResult<(usize, EomReason)> {
                let payload = b"ABC";
                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                // Driver explicitly reports both EOS match and END indicator
                // (e.g. a GPIB END line plus terminator detection).
                Ok((n, EomReason::EOS | EomReason::END))
            }
        }
        let drv = EomDriver {
            base: PortDriverBase::new("eom_test", 1, PortFlags::default()),
        };
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::OctetRead { buf_size: 16 }, user).unwrap();
        assert_eq!(result.data.as_deref(), Some(&b"ABC"[..]));
        let eom = EomReason::from_bits_truncate(result.eom_reason);
        assert!(eom.contains(EomReason::EOS));
        assert!(eom.contains(EomReason::END));
        assert!(!eom.contains(EomReason::CNT));
    }

    /// Default `io_read_octet_eom` (drivers that override only the
    /// non-eom `io_read_octet`) must synthesize `CNT` when the
    /// buffer filled — C `asynOctetSyncIO::read` synthesises the
    /// same flag at the syncIO level (`asynOctetSyncIO.c:213-217`).
    #[test]
    fn actor_octet_read_eom_default_synthesizes_cnt_on_buffer_full() {
        use crate::interpose::EomReason;

        struct FillDriver {
            base: PortDriverBase,
        }
        impl PortDriver for FillDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
                // Always fill the buffer to completion.
                for b in buf.iter_mut() {
                    *b = b'X';
                }
                Ok(buf.len())
            }
        }
        let drv = FillDriver {
            base: PortDriverBase::new("fill_test", 1, PortFlags::default()),
        };
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::OctetRead { buf_size: 4 }, user).unwrap();
        assert_eq!(result.nbytes, 4);
        let eom = EomReason::from_bits_truncate(result.eom_reason);
        assert!(eom.contains(EomReason::CNT));
        assert!(!eom.contains(EomReason::EOS));
        assert!(!eom.contains(EomReason::END));
    }

    #[test]
    fn heap_pops_fifo_within_priority() {
        // C asynManager services each priority as a strict FIFO (queueRequest
        // ellAdd to the tail; portThread walks ellFirst->ellNext): submission
        // order is preserved within a priority by the seq tiebreaker alone.
        let mk = |seq: u64| {
            let (reply, _rx) = oneshot::channel();
            ActorMessage {
                op: RequestOp::Int32Read,
                user: AsynUser::new(0),
                cancel: CancelToken::new(),
                reply,
                seq,
                priority: QueuePriority::Medium,
                block_token: None,
                queue_deadline: None,
            }
        };
        let mut heap = BinaryHeap::new();
        // seq 0 submitted before seq 1, same priority.
        heap.push(mk(0));
        heap.push(mk(1));
        // FIFO: seq 0 pops first.
        assert_eq!(heap.pop().unwrap().seq, 0);
        assert_eq!(heap.pop().unwrap().seq, 1);
    }

    /// R18-68: C's port thread drains `queueList[asynQueuePriorityConnect]`
    /// to empty before it looks at any other band (`portThread`,
    /// asynManager.c:811-855), and every C caller of connect/disconnect
    /// queues there — `asynCommonSyncIO` passes the constant
    /// (asynCommonSyncIO.c:142, :154), `asynRecord` derives it from the
    /// callback type (asynRecord.c:562-566). Here the band comes from
    /// [`PortActor::c_dispatch`] for the same reason, so a `CNCT` put built
    /// on a `#[default]` Medium user still overtakes a sustained High stream
    /// instead of starving behind it.
    #[test]
    fn a_connect_overtakes_a_sustained_high_stream() {
        let mk = |op: RequestOp, user: AsynUser| {
            let (reply, _rx) = oneshot::channel();
            ActorMessage::new(op, user, CancelToken::new(), reply)
        };
        let mut heap = BinaryHeap::new();

        // A High scan stream already queued...
        for _ in 0..8 {
            heap.push(mk(
                RequestOp::Int32Read,
                AsynUser::new(0).with_priority(QueuePriority::High),
            ));
        }
        // ...and then the operator's CNCT put, exactly as
        // `PortHandle::connect_blocking` builds it: no explicit priority.
        let connect_seq = {
            let m = mk(RequestOp::Connect, AsynUser::new(0).with_addr(-1));
            let seq = m.seq;
            heap.push(m);
            seq
        };
        // ...and more of the stream behind it.
        for _ in 0..8 {
            heap.push(mk(
                RequestOp::Int32Read,
                AsynUser::new(0).with_priority(QueuePriority::High),
            ));
        }

        let first = heap.pop().unwrap();
        assert_eq!(
            first.seq, connect_seq,
            "the connect must be drained before the High band"
        );
        assert_eq!(first.priority, QueuePriority::Connect);
    }

    /// The other half of the same rule: a band is a property of the op class,
    /// so an op C really does hand to `queueRequest` keeps the user's own —
    /// including asynRecord's hand-escalated HOSTINFO put, which is a
    /// `callbackSetOption` raised to Connect by the record (asynRecord.c:566-569)
    /// and reaches here through [`AsynUser::queue_even_if_not_connected`].
    #[test]
    fn a_queued_op_keeps_the_users_own_band() {
        let mk = |op: RequestOp, user: AsynUser| {
            let (reply, _rx) = oneshot::channel();
            ActorMessage::new(op, user, CancelToken::new(), reply)
        };
        assert_eq!(
            mk(
                RequestOp::Int32Read,
                AsynUser::new(0).with_priority(QueuePriority::Low)
            )
            .priority,
            QueuePriority::Low
        );
        assert_eq!(
            mk(RequestOp::Int32Read, AsynUser::new(0)).priority,
            QueuePriority::Medium
        );
        // The HOSTINFO escalation survives.
        assert_eq!(
            mk(
                RequestOp::SetOption {
                    key: "hostInfo".into(),
                    value: "host:5025".into()
                },
                AsynUser::new(0).queue_even_if_not_connected()
            )
            .priority,
            QueuePriority::Connect
        );
    }

    #[test]
    fn heap_pops_higher_priority_first() {
        // Priority dominates the seq FIFO tiebreaker.
        let mk = |seq: u64, priority: QueuePriority| {
            let (reply, _rx) = oneshot::channel();
            ActorMessage {
                op: RequestOp::Int32Read,
                user: AsynUser::new(0),
                cancel: CancelToken::new(),
                reply,
                seq,
                priority,
                block_token: None,
                queue_deadline: None,
            }
        };
        let mut heap = BinaryHeap::new();
        heap.push(mk(0, QueuePriority::Low));
        heap.push(mk(1, QueuePriority::High));
        heap.push(mk(2, QueuePriority::Medium));
        // High then Medium then Low, regardless of submission order.
        assert_eq!(heap.pop().unwrap().seq, 1);
        assert_eq!(heap.pop().unwrap().seq, 2);
        assert_eq!(heap.pop().unwrap().seq, 0);
    }

    #[test]
    fn actor_cancel() {
        let tx = spawn_actor(TestDriver::new());
        let cancel = CancelToken::new();
        cancel.cancel();
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = ActorMessage::new(RequestOp::Int32Read, user, cancel, reply_tx);
        tx.blocking_send(msg).unwrap();
        let result = reply_rx.blocking_recv().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn actor_dequeued_request_runs_despite_elapsed_timeout() {
        // C parity: a request that reaches the head of the queue always
        // executes I/O — the queue-wait timer (when armed at all) is
        // cancelled at dequeue (asynManager.c:906), and standard device
        // support passes queueRequest(..., 0.0) so no timer exists. The
        // I/O `user.timeout` must NOT abort a dequeued request just because
        // a slow predecessor delayed it past that budget.
        let mut drv = TestDriver::new();
        drv.base.params.set_int32(0, 0, 7).unwrap();
        let tx = spawn_actor(drv);
        // Tiny I/O timeout; let it lapse before the request is serviced.
        let user = AsynUser::new(0).with_timeout(Duration::from_nanos(1));
        std::thread::sleep(Duration::from_millis(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        // No queue-deadline abort: the read runs and returns the value.
        assert_eq!(result.unwrap().int_val, Some(7));
    }

    #[test]
    fn actor_disabled_port() {
        let mut drv = TestDriver::new();
        drv.base.enabled = false;
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        let err = result.unwrap_err();
        assert!(
            err.is_queue_refused(),
            "the gate's refusal is a QueueRefused, not a driver error: {err:?}"
        );
        assert_eq!(err.status(), AsynStatus::Disabled);
    }

    #[test]
    fn actor_setenable_refuses_defunct_port() {
        // SetEnable is a lifecycle op that bypasses check_ready's
        // defunct-first gate, so the refusal must come from the enable
        // owner itself (C `enable`, asynManager.c:2236-2241). A defunct
        // port must answer SetEnable with asynDisabled, not silently
        // re-enable.
        let mut drv = TestDriver::new();
        shut_down(&mut drv.base);
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::SetEnable { yes: true }, user);
        // Not a queue refusal: the request ran, and the *enable owner* turned it
        // down — so it stays an `AsynError::Status`, the way C's `enable` returns
        // `asynDisabled` from the manager call itself rather than from
        // `queueRequest` (asynManager.c:2236-2241).
        match result {
            Err(AsynError::Status { status, message }) => {
                assert_eq!(status, AsynStatus::Disabled);
                assert_eq!(message, "asynManager:enable: port has been shut down");
            }
            other => panic!("expected a Status(Disabled), got {other:?}"),
        }
    }

    #[test]
    fn actor_auto_connect() {
        let mut drv = TestDriver::new();
        drv.base.init_connected(false);
        drv.base.auto_connect = true;
        // Seed VAL so the read probe returns a value: the default read now
        // surfaces an unset param as ParamUndefined (C parity), so this
        // lifecycle test must assert the auto-connect outcome (the request
        // reaches the driver and is not rejected), not undefined-read state.
        drv.base.params.set_int32(0, 0, 0).unwrap();
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        assert!(result.is_ok());
    }

    #[test]
    fn actor_auto_connect_throttled_to_one_attempt_per_window() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A driver whose `connect()` counts the attempt but leaves the port
        // disconnected — simulating a hardware reconnect that keeps failing.
        // Without the 2s throttle (C autoConnectDevice, asynManager.c:713),
        // every queued request to an offline auto_connect port would fire a
        // fresh full connect attempt.
        struct ThrottleDriver {
            base: PortDriverBase,
            connect_calls: Arc<AtomicUsize>,
        }
        impl PortDriver for ThrottleDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                self.connect_calls.fetch_add(1, Ordering::SeqCst);
                // Stay disconnected: the throttle, not a state change, must
                // be what bounds retries.
                Ok(())
            }
        }

        let connect_calls = Arc::new(AtomicUsize::new(0));
        let mut base = PortDriverBase::new("throttle_test", 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        base.init_connected(false);
        base.auto_connect = true;
        let drv = ThrottleDriver {
            base,
            connect_calls: connect_calls.clone(),
        };
        let tx = spawn_actor(drv);

        // Three back-to-back reads to the disconnected auto_connect port.
        for _ in 0..3 {
            let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
            let result = send_and_wait(&tx, RequestOp::Int32Read, user);
            // The port never reconnects, so each request fails Disconnected
            // (C autoConnectDevice returns FALSE => queueRequest fails).
            let err = result.unwrap_err();
            assert!(err.is_queue_refused(), "got {err:?}");
            assert_eq!(err.status(), AsynStatus::Disconnected);
        }

        // Only the first request's attempt fired; the second and third fell
        // inside the 2s throttle window and were refused without a connect
        // call (C autoConnectDevice 2.0s gate, asynManager.c:712-713).
        assert_eq!(connect_calls.load(Ordering::SeqCst), 1);
    }

    /// A connection-waived request is followed by the port auto-connect C's
    /// `portThread` performs.
    ///
    /// C queues the waived requests — `asynSetOption`/`asynSetEos` from
    /// `st.cmd`, the record's HOSTINFO repoint (asynRecord.c:566-569) — at
    /// `asynQueuePriorityConnect`. `portThread` drains that queue and then, at
    /// asynManager.c:856-861, runs `autoConnectDevice(pport,0)` whenever the
    /// port is still disconnected. That is what brings a dead line up on its
    /// *new* configuration immediately, instead of leaving it down until some
    /// later I/O request happens to force a connect.
    ///
    /// Boundary 1 (here): the waived request runs on the dead line AND the port
    /// connect attempt follows it.
    /// Boundary 2 (below): an explicit Disconnect is not undone by the same
    /// pass — C's 2 s throttle (:712-713) anchors on the transition it just
    /// stamped.
    #[test]
    fn a_waived_request_is_followed_by_the_port_auto_connect() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct WaivedDriver {
            base: PortDriverBase,
            connect_calls: Arc<AtomicUsize>,
            options: Arc<parking_lot::Mutex<Vec<(String, String)>>>,
        }
        impl PortDriver for WaivedDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                self.connect_calls.fetch_add(1, Ordering::SeqCst);
                // The line comes up on the configuration the waived request
                // just wrote — a real driver reopens its transport here.
                self.base.set_connected(true);
                Ok(())
            }
            fn set_option(
                &mut self,
                _user: &mut AsynUser,
                key: &str,
                value: &str,
            ) -> AsynResult<()> {
                self.options
                    .lock()
                    .push((key.to_string(), value.to_string()));
                Ok(())
            }
        }

        let connect_calls = Arc::new(AtomicUsize::new(0));
        let options = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut base = PortDriverBase::new("waived_test", 1, PortFlags::default());
        base.init_connected(false);
        base.auto_connect = true;
        let tx = spawn_actor(WaivedDriver {
            base,
            connect_calls: connect_calls.clone(),
            options: options.clone(),
        });

        // The `asynSetOption` user: Connect priority + the reason that waives
        // the connected refusal (asynShellCommands.c:121,127).
        let user = AsynUser::default().queue_even_if_not_connected();
        send_and_wait(
            &tx,
            RequestOp::SetOption {
                key: "baud".to_string(),
                value: "9600".to_string(),
            },
            user,
        )
        .expect("a waived request runs on a disconnected port");
        assert_eq!(
            options.lock().as_slice(),
            &[("baud".to_string(), "9600".to_string())],
            "the waived request reaches the driver on the dead line"
        );

        // The follow-up read now finds a live port, which is the observable the
        // operator cares about: the line is back without further traffic. The
        // auto-connect runs on the actor's tail AFTER the waived request's
        // reply (as C portThread does after the callback), so this probe's
        // reply is also the sequencing barrier that orders the counter check
        // below — the single-threaded actor cannot dequeue the probe before
        // that tail has run.
        let probe = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let err = send_and_wait(&tx, RequestOp::Int32Read, probe);
        assert!(
            !matches!(
                err,
                Err(AsynError::Status {
                    status: AsynStatus::Disconnected,
                    ..
                })
            ),
            "the port is connected after the waived request, so nothing is \
             refused Disconnected: {err:?}"
        );

        // ...and C's portThread tried exactly one connect to get there.
        assert_eq!(
            connect_calls.load(Ordering::SeqCst),
            1,
            "C portThread runs autoConnectDevice(pport,0) once the Connect \
             queue has drained (asynManager.c:856-861)"
        );
    }

    /// Boundary 2 of the above: the auto-connect that follows a waived request
    /// must not undo an explicit Disconnect. C's throttle (asynManager.c:712-713)
    /// refuses an attempt within 2 s of the last transition, and the Disconnect
    /// callback stamps that transition as it runs — so C's very next
    /// `autoConnectDevice(pport,0)` on the same pass returns FALSE.
    #[test]
    fn the_port_auto_connect_does_not_undo_an_explicit_disconnect() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct DisconnectDriver {
            base: PortDriverBase,
            connect_calls: Arc<AtomicUsize>,
        }
        impl PortDriver for DisconnectDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                self.connect_calls.fetch_add(1, Ordering::SeqCst);
                self.base.set_connected(true);
                Ok(())
            }
            fn disconnect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                self.base.set_connected(false);
                Ok(())
            }
        }

        let connect_calls = Arc::new(AtomicUsize::new(0));
        let mut base = PortDriverBase::new("disconnect_test", 1, PortFlags::default());
        base.init_connected(true);
        base.auto_connect = true;
        let tx = spawn_actor(DisconnectDriver {
            base,
            connect_calls: connect_calls.clone(),
        });

        send_and_wait(&tx, RequestOp::Disconnect, AsynUser::default())
            .expect("an explicit disconnect succeeds");

        // The auto-connect the Disconnect must NOT trigger runs on the actor's
        // tail after the reply, so a probe is needed as a sequencing barrier
        // before the counter can be trusted: its reply proves the tail ran.
        // The probe itself is refused Disconnected through the same throttle
        // (its Required-path attempt is inside the 2 s window), so it cannot
        // perturb the counter.
        let probe = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let _ = send_and_wait(&tx, RequestOp::Int32Read, probe);

        assert_eq!(
            connect_calls.load(Ordering::SeqCst),
            0,
            "the transition the Disconnect just stamped keeps the auto-connect \
             inside its 2 s window (asynManager.c:712-713)"
        );
    }

    /// C's **port-level** auto-connect reaches the driver with `addr == -1`:
    /// `autoConnectDevice` calls `connectAttempt(&pport->dpc)`
    /// (asynManager.c:716) with `pdevice == 0`, and `connectAttempt` derives
    /// `addr = (pdevice ? pdevice->addr : -1)` (:752) before handing that user
    /// to `pasynCommon->connect` (:775). The device-level attempt is the
    /// separate `connectAttempt(&pdevice->dpc)` (:733), which carries the real
    /// address.
    ///
    /// `AsynUser::default()` carries `addr: 0` (user.rs:153). Passing it here
    /// told any driver that reads the address — `DrvAsynPrologixPort::connect`
    /// is one, branching on `user.addr < 0` exactly as C's `prologixConnect`
    /// branches on `getAddr` (drvPrologixGPIB.c:169-171) — that the port-level
    /// attempt was a *device 0* connect. Prologix then skipped its `++ver`
    /// handshake and still reported the port connected.
    #[test]
    fn the_port_level_auto_connect_reaches_the_driver_with_addr_minus_one() {
        struct AddrDriver {
            base: PortDriverBase,
            seen: Arc<parking_lot::Mutex<Vec<i32>>>,
        }
        impl PortDriver for AddrDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, user: &AsynUser) -> AsynResult<()> {
                self.seen.lock().push(user.addr);
                self.base.set_connected(true);
                Ok(())
            }
        }

        let mut base = PortDriverBase::new("addr_ac", 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        base.params.set_int32(0, 0, 7).unwrap();
        base.init_connected(false);
        base.auto_connect = true;
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let tx = spawn_actor(AddrDriver {
            base,
            seen: seen.clone(),
        });

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Int32Read, user).unwrap();

        assert_eq!(
            seen.lock().clone(),
            vec![-1],
            "the port-level connect must carry addr -1 (C connectAttempt, asynManager.c:752)"
        );
    }

    /// R6-49: C `autoConnectDevice` reconnects the PORT first
    /// (`connectAttempt(&pport->dpc)`, asynManager.c:716) and only then the
    /// device (:723-737). A multi-device port whose transport dropped used to
    /// call `connect_addr` alone — whose default merely flips the per-address
    /// flag and opens no socket — so the port-level `check_ready` rejected
    /// every subsequent request forever.
    ///
    /// Boundary 1 (here): the port's connect succeeds, so the device connect
    /// runs after it and the request goes through.
    /// Boundary 2 (below): the port's connect fails, so the device connect
    /// must NOT run (C :721 `return FALSE`).
    #[test]
    fn actor_multi_device_auto_connect_reconnects_port_first() {
        struct MultiDriver {
            base: PortDriverBase,
            calls: Arc<parking_lot::Mutex<Vec<&'static str>>>,
            port_connect_succeeds: bool,
        }
        impl PortDriver for MultiDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                self.calls.lock().push("connect");
                if self.port_connect_succeeds {
                    // A real driver reopens its transport here; the flag flip
                    // is what `check_ready` keys on.
                    self.base.set_connected(true);
                }
                Ok(())
            }
            fn connect_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
                self.calls.lock().push("connect_addr");
                self.base.connect_addr(user.addr);
                Ok(())
            }
        }

        let mk = |port_connect_succeeds: bool| {
            let mut base = PortDriverBase::new(
                "multi_ac",
                4,
                PortFlags {
                    multi_device: true,
                    ..PortFlags::default()
                },
            );
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.params.set_int32(0, 1, 7).unwrap();
            // The transport dropped: port down, and the device with it.
            base.init_connected(false);
            base.auto_connect = true;
            base.device_state(1).connected = false;
            let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
            (
                MultiDriver {
                    base,
                    calls: calls.clone(),
                    port_connect_succeeds,
                },
                calls,
            )
        };

        // Boundary 1: port comes back → port connect, then device connect.
        let (drv, calls) = mk(true);
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0)
            .with_addr(1)
            .with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        assert!(
            result.is_ok(),
            "a multi-device auto-connect port must reopen its transport, got {result:?}"
        );
        assert_eq!(
            calls.lock().clone(),
            vec!["connect", "connect_addr"],
            "the PORT must be reconnected before the device (C asynManager.c:716 then :723)"
        );

        // Boundary 2: the port stays down → C bails at :721 before touching
        // the device, and the request fails Disconnected.
        let (drv, calls) = mk(false);
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0)
            .with_addr(1)
            .with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        let err = result.unwrap_err();
        assert!(err.is_queue_refused(), "got {err:?}");
        assert_eq!(err.status(), AsynStatus::Disconnected);
        assert_eq!(
            calls.lock().clone(),
            vec!["connect"],
            "a device connect must not be attempted while the port is still down"
        );
    }

    /// R6-47: an auto-connect port that drops must reconnect on its own, with
    /// NO queued traffic. C `exceptionDisconnect` arms the port's connect
    /// timer at .01 s (asynManager.c:2181-2182); `portConnectTimerCallback`
    /// then posts a Connect-priority request (:3252-3266) and
    /// `portConnectProcessCallback` calls the driver's connect (:3268-3283).
    /// Before this, `port_actor`'s only auto-connect attempt lived inside
    /// `process_one` — it fired only when a request was dequeued, so an
    /// I/O-Intr-only or quiescent port stayed dead forever.
    ///
    /// Uses the real runtime (`create_port_runtime`), because the timer lives
    /// in the actor's idle `select!` — the whole point is that nothing else
    /// wakes it.
    #[test]
    fn idle_auto_connect_port_reconnects_without_queued_traffic() {
        use crate::runtime::{RuntimeConfig, create_port_runtime};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct FlappyDriver {
            base: PortDriverBase,
            connect_calls: Arc<AtomicUsize>,
            /// Mirrors `base.connected` outside the actor so the test can
            /// observe the reconnect without sending any request (sending one
            /// would itself trigger the `process_one` auto-connect and prove
            /// nothing about the timer).
            link_up: Arc<std::sync::atomic::AtomicBool>,
            /// Connect attempts to fail before the link comes back — proves
            /// the retry loop re-arms rather than giving up after one try.
            fail_first: usize,
        }
        impl PortDriver for FlappyDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                let n = self.connect_calls.fetch_add(1, Ordering::SeqCst);
                if n < self.fail_first {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "link still down".into(),
                    });
                }
                self.base.set_connected(true);
                self.link_up.store(true, Ordering::SeqCst);
                Ok(())
            }
            fn disconnect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                self.base.set_connected(false);
                self.link_up.store(false, Ordering::SeqCst);
                Ok(())
            }
        }

        let connect_calls = Arc::new(AtomicUsize::new(0));
        let link_up = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut base = PortDriverBase::new("retry_test", 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        base.auto_connect = true;
        // Shorten C's 20 s back-off (asynManager.c:48) so the second attempt
        // lands inside the test. The first attempt always uses C's .01 s.
        base.seconds_between_port_connect = Duration::from_millis(30);
        let drv = FlappyDriver {
            base,
            connect_calls: connect_calls.clone(),
            link_up: link_up.clone(),
            fail_first: 1,
        };
        let (handle, _jh) = create_port_runtime(drv, RuntimeConfig::default())
            .expect("the port runtime thread must start");

        // Drop the link. `Disconnect` is a lifecycle op, so this is the last
        // request the port ever sees — from here on there is NO queued
        // traffic, exactly the case the old code could not recover from.
        handle
            .port_handle()
            .submit_blocking(RequestOp::Disconnect, AsynUser::default())
            .unwrap();
        assert!(
            !link_up.load(Ordering::SeqCst),
            "precondition: the port is down"
        );

        // Wait for the autonomous retries: .01 s (fails) then 30 ms (succeeds).
        // Not one request is submitted in this loop.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline && !link_up.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(
            link_up.load(Ordering::SeqCst),
            "an idle auto-connect port must reconnect on its own timer, with no queued traffic"
        );
        assert!(
            connect_calls.load(Ordering::SeqCst) >= 2,
            "the failed first attempt must re-arm the timer (C asynManager.c:3281), got {} call(s)",
            connect_calls.load(Ordering::SeqCst)
        );
    }

    /// R14-50: no connect attempt on a port the queue gate would refuse.
    ///
    /// C's retry timer does not call `pasynCommon->connect` — it posts a
    /// `queueRequest` (asynManager.c:3259), which a disabled or defunct port
    /// refuses with `asynDisabled` (:1541-1546); the process callback that would
    /// re-arm the timer (:3281) never runs. `portThread` applies the same
    /// refusal above `autoConnectDevice` (:806-809). So `asynEnable(port,0)` and
    /// `shutdownPort` stop the hardware retries, not just the reads.
    ///
    /// One case per gate boundary: disabled, defunct, and the enabled control
    /// that must still connect.
    #[test]
    fn a_gate_refused_port_makes_no_connect_attempt() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingDriver {
            base: PortDriverBase,
            connect_calls: Arc<AtomicUsize>,
            connect_addr_calls: Arc<AtomicUsize>,
        }
        impl PortDriver for CountingDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                self.connect_calls.fetch_add(1, Ordering::SeqCst);
                self.base.set_connected(true);
                Ok(())
            }
            fn connect_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
                self.connect_addr_calls.fetch_add(1, Ordering::SeqCst);
                self.base.connect_addr(user.addr);
                Ok(())
            }
        }

        // A down, auto-connect, multi-device port with its retry timer due.
        let actor_with = |enabled: bool, defunct: bool| {
            let calls = Arc::new(AtomicUsize::new(0));
            let addr_calls = Arc::new(AtomicUsize::new(0));
            let mut base = PortDriverBase::new(
                "gate_test",
                2,
                PortFlags {
                    multi_device: true,
                    ..PortFlags::default()
                },
            );
            base.auto_connect = true;
            base.set_connected(false);
            base.enabled = enabled;
            if defunct {
                // C `shutdownPort` clears `enabled` on its way to `defunct`
                // (:2283-2284), so the gate refuses the port as a disabled one.
                shut_down(&mut base);
            }
            base.connect_retry_at = Some(Instant::now() - Duration::from_millis(1));
            let drv = CountingDriver {
                base,
                connect_calls: calls.clone(),
                connect_addr_calls: addr_calls.clone(),
            };
            let (_tx, rx) = mpsc::channel(1);
            (PortActor::new(Box::new(drv), rx), calls, addr_calls, _tx)
        };

        for (label, enabled, defunct) in [("disabled", false, false), ("defunct", true, true)] {
            let (mut actor, calls, addr_calls, _tx) = actor_with(enabled, defunct);

            actor.service_connect_timer();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "{label}: the retry timer must not reach the driver's connect"
            );
            assert!(
                actor.driver.base().connect_retry_at.is_none(),
                "{label}: a refused queueRequest never re-arms the timer (C :3281)"
            );

            assert!(!actor.auto_connect_port(), "{label}: no port auto-connect");
            assert!(
                !actor.auto_connect_device(1, 0),
                "{label}: no device auto-connect"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst) + addr_calls.load(Ordering::SeqCst),
                0,
                "{label}: auto-connect must not reach the driver either"
            );
        }

        // Control: enabled and alive — the same tick connects, as before.
        let (mut actor, calls, _addr_calls, _tx) = actor_with(true, false);
        actor.service_connect_timer();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "an enabled port's retry timer still connects"
        );
        assert!(actor.driver.base().is_connected());
    }

    /// Boundary: auto-connect OFF. C arms the timer only when
    /// `pport->dpc.autoConnect` is set (asynManager.c:2181), so a manual port
    /// that drops must stay down until something explicitly connects it.
    #[test]
    fn disconnect_does_not_arm_retry_when_auto_connect_is_off() {
        let mut base = PortDriverBase::new("no_auto", 1, PortFlags::default());
        base.auto_connect = false;
        assert!(base.set_connected(false));
        assert!(
            base.connect_retry_at.is_none(),
            "a non-auto-connect port must not schedule an autonomous reconnect"
        );

        // ...and with auto-connect on, the same edge arms it.
        let mut base = PortDriverBase::new("auto", 1, PortFlags::default());
        base.auto_connect = true;
        assert!(base.set_connected(false));
        assert!(base.connect_retry_at.is_some());
        // Coming back up disarms it.
        assert!(base.set_connected(true));
        assert!(base.connect_retry_at.is_none());
    }

    /// A multi-device port, CANBLOCK or synchronous, whose devices 0 and 1
    /// exist and whose `VAL` param is seeded at both addresses.
    fn multi_device_driver(can_block: bool) -> TestDriver {
        let mut base = PortDriverBase::new(
            "dev_gate",
            2,
            PortFlags {
                multi_device: true,
                can_block,
                ..PortFlags::default()
            },
        );
        base.create_param("VAL", ParamType::Int32).unwrap();
        base.params.set_int32(0, 0, 7).unwrap();
        base.params.set_int32(0, 1, 7).unwrap();
        // Both devices exist, both up.
        base.device_state(0);
        base.device_state(1);
        TestDriver { base }
    }

    fn send(
        tx: &mpsc::Sender<ActorMessage>,
        op: RequestOp,
        user: AsynUser,
    ) -> oneshot::Receiver<AsynResult<RequestResult>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.blocking_send(ActorMessage::new(op, user, CancelToken::new(), reply_tx))
            .expect("actor channel closed");
        reply_rx
    }

    /// R15-47, boundary 1: on a CANBLOCK port a disabled device's request
    /// **parks** — it keeps its place in the queue and runs when the device is
    /// re-enabled.
    ///
    /// C's device-level refusals live inside `if(!(pport->attributes &
    /// ASYN_CANBLOCK))` in `queueRequest` (asynManager.c:1553-1575), so a
    /// CANBLOCK port never returns them. Its `portThread` answers the same
    /// condition by skipping the request — `if(!pdpCommon->enabled) continue;`
    /// (:875) — leaving it queued. Every real transport port is CANBLOCK.
    #[test]
    fn a_disabled_device_parks_a_canblock_ports_request_until_re_enable() {
        let mut drv = multi_device_driver(true);
        drv.base.disable_addr(1);
        let tx = spawn_actor(drv);

        // Addr 1 is disabled: the read parks rather than being refused.
        let mut parked = send(&tx, RequestOp::Int32Read, AsynUser::new(0).with_addr(1));

        // Addr 0 is untouched and still runs — the park holds up only its own
        // device, exactly as C's per-request `continue` does.
        let r = send_and_wait(&tx, RequestOp::Int32Read, AsynUser::new(0).with_addr(0)).unwrap();
        assert_eq!(r.int_val, Some(7));
        assert!(
            matches!(parked.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "a disabled device's request must stay queued, not be refused"
        );

        // Re-enable it: the parked request runs, with no resubmission.
        send_and_wait(&tx, RequestOp::EnableAddr, AsynUser::new(0).with_addr(1)).unwrap();
        let r = parked
            .blocking_recv()
            .expect("actor dropped the parked reply")
            .expect("the parked request runs once its device is enabled");
        assert_eq!(r.int_val, Some(7));
    }

    /// R15-47, boundary 2: a parked request still expires on its own queue
    /// timer. C arms an `epicsTimer` at `queueRequest` (asynManager.c:1617-1623)
    /// whose callback unlinks the request wherever the port thread left it
    /// (:647-701) — so a device that is never re-enabled does not hang the
    /// caller, it times out.
    ///
    /// Uses the real runtime: the deadline of a parked request has no other
    /// clock (a blocking caller with no Tokio reactor arms no timer of its own),
    /// so the actor's own `select!` must wake for it.
    #[test]
    fn a_parked_request_still_times_out_on_its_queue_timer() {
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        let mut drv = multi_device_driver(true);
        drv.base.disable_addr(1);
        let (handle, _jh) = create_port_runtime(drv, RuntimeConfig::default())
            .expect("the port runtime thread must start");

        let started = Instant::now();
        let err = handle
            .port_handle()
            .submit_blocking(
                RequestOp::Int32Read,
                AsynUser::new(0)
                    .with_addr(1)
                    .with_queue_timeout(Duration::from_millis(50)),
            )
            .unwrap_err();
        assert!(
            err.is_queue_timeout(),
            "a request parked behind a device that is never re-enabled must time \
             out on its queue timer, got {err:?}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "it must WAIT for the deadline, not be refused up front"
        );
    }

    /// R15-47, boundary 3: a **synchronous** port keeps the device refusals —
    /// that is where C's texts live (asynManager.c:1561-1575), and the whole
    /// block is inside `if(!(pport->attributes & ASYN_CANBLOCK))`.
    #[test]
    fn a_synchronous_port_refuses_a_disabled_device_with_cs_text() {
        let mut drv = multi_device_driver(false);
        drv.base.disable_addr(1);
        let tx = spawn_actor(drv);

        let err = send_and_wait(&tx, RequestOp::Int32Read, AsynUser::new(0).with_addr(1))
            .expect_err("a synchronous port refuses a disabled device up front");
        assert!(err.is_queue_refused(), "got {err:?}");
        assert_eq!(err.status(), AsynStatus::Disabled);
        // C's double space, verbatim (asynManager.c:1564).
        assert!(
            err.to_string()
                .contains("port dev_gate  or device 1 not enabled"),
            "got {err}"
        );
    }

    /// R15-47, boundary 4: a disconnected device on a CANBLOCK port. C's
    /// `portThread` auto-connects it (:877) and, if it is still down, hands the
    /// request to its *timeout* callback (:884-885, :915-919) — it does not
    /// refuse it with `asynDisconnected`, which is what `queueRequest` would have
    /// done had this been a synchronous port.
    #[test]
    fn a_disconnected_device_on_a_canblock_port_takes_the_timeout_path() {
        let mut drv = multi_device_driver(true);
        // No auto-connect: nothing can bring the device back, so the request
        // reaches the timeout path rather than the connect that precedes it.
        drv.base.auto_connect = false;
        drv.base.device_state(1).auto_connect = false;
        drv.base.device_state(1).connected = false;
        let tx = spawn_actor(drv);

        let err = send_and_wait(&tx, RequestOp::Int32Read, AsynUser::new(0).with_addr(1))
            .expect_err("a device that will not connect cannot serve the read");
        assert!(
            err.is_queue_timeout(),
            "C portThread calls the request's timeoutUser, not queueRequest's \
             asynDisconnected refusal: {err:?}"
        );

        // The synchronous port is the one that answers with the refusal, and its
        // text is C's (:1571).
        let mut drv = multi_device_driver(false);
        drv.base.auto_connect = false;
        drv.base.device_state(1).auto_connect = false;
        drv.base.device_state(1).connected = false;
        let tx = spawn_actor(drv);
        let err = send_and_wait(&tx, RequestOp::Int32Read, AsynUser::new(0).with_addr(1))
            .expect_err("a synchronous port refuses a disconnected device");
        assert!(err.is_queue_refused(), "got {err:?}");
        assert_eq!(err.status(), AsynStatus::Disconnected);
        assert!(
            err.to_string()
                .contains("port dev_gate or device 1 not connected"),
            "got {err}"
        );
    }

    /// R15-47, boundary 5: the two **port**-level refusals are unchanged on a
    /// CANBLOCK port — they are the only ones C's `queueRequest` applies to it
    /// (asynManager.c:1541-1552).
    #[test]
    fn a_canblock_port_still_applies_the_port_level_refusals() {
        let mut drv = multi_device_driver(true);
        drv.base.enabled = false;
        let tx = spawn_actor(drv);
        let err = send_and_wait(&tx, RequestOp::Int32Read, AsynUser::new(0).with_addr(1))
            .expect_err("a disabled port refuses everything");
        assert!(err.is_queue_refused(), "got {err:?}");
        assert_eq!(err.status(), AsynStatus::Disabled);
        assert!(
            err.to_string().contains("port dev_gate disabled"),
            "got {err}"
        );

        let mut drv = multi_device_driver(true);
        drv.base.auto_connect = false;
        drv.base.init_connected(false);
        let tx = spawn_actor(drv);
        let err = send_and_wait(&tx, RequestOp::Int32Read, AsynUser::new(0).with_addr(1))
            .expect_err("a disconnected port refuses everything");
        assert!(err.is_queue_refused(), "got {err:?}");
        assert_eq!(err.status(), AsynStatus::Disconnected);
        assert!(
            err.to_string().contains("port dev_gate not connected"),
            "got {err}"
        );
    }

    /// R15-48: `asynInterposeDelay(port, addr, delay)` installs on the DEVICE the
    /// addr names, so it slows that device and leaves the rest of the bus alone.
    ///
    /// C hands the iocsh `addr` to `interposeInterface` (asynInterposeDelay.c:187),
    /// which lands the layer on the device's `dpCommon` (asynManager.c:2202-2206).
    /// The Rust command parsed the addr and dropped it, and the stack was one
    /// port-wide chain — so `asynInterposeDelay("gpib",4,0.01)` slowed every
    /// device on the bus. This drives the whole path the operator does: the actor
    /// op, the addr on its user, the stack, and a real write through it.
    #[test]
    fn a_delay_interpose_installed_on_one_device_does_not_slow_the_others() {
        use crate::interpose::{OctetNext, OctetReadResult};

        struct InterposeDriver {
            base: PortDriverBase,
        }
        struct Sink;
        impl OctetNext for Sink {
            fn read(&mut self, _u: &AsynUser, _b: &mut [u8]) -> AsynResult<OctetReadResult> {
                Ok(OctetReadResult {
                    nbytes_transferred: 0,
                    eom_reason: crate::interpose::EomReason::CNT,
                })
            }
            fn write(&mut self, _u: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                Ok(data.len())
            }
            fn flush(&mut self, _u: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }
        impl PortDriver for InterposeDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.base
                    .interpose_octet
                    .dispatch_write(user, data, &mut Sink)
            }
        }

        let base = PortDriverBase::new(
            "gpib",
            8,
            PortFlags {
                multi_device: true,
                can_block: true,
                ..PortFlags::default()
            },
        );
        let tx = spawn_actor(InterposeDriver { base });

        // asynInterposeDelay("gpib", 4, 0.02)
        send_and_wait(
            &tx,
            RequestOp::PushDelayInterpose {
                delay: Duration::from_millis(20),
            },
            AsynUser::new(0).with_addr(4),
        )
        .unwrap();

        let write_to = |addr: i32| {
            let started = Instant::now();
            send_and_wait(
                &tx,
                RequestOp::OctetWrite {
                    data: b"abcd".to_vec(),
                },
                AsynUser::new(0).with_addr(addr),
            )
            .unwrap();
            started.elapsed()
        };

        // Device 4 pays the per-character delay: 4 bytes x 20 ms.
        assert!(
            write_to(4) >= Duration::from_millis(60),
            "the addressed device must run the delay interpose"
        );
        // Device 2 — same bus, same port — does not.
        assert!(
            write_to(2) < Duration::from_millis(30),
            "asynInterposeDelay on device 4 must not slow device 2"
        );
    }

    /// R15-46: the enable exception is a port-thread wake, and a woken port
    /// thread reconnects a down port.
    ///
    /// C `enable` ends in `exceptionOccurred` (asynManager.c:2247);
    /// `announceExceptionOccurred` signals `notifyPortThread` for every exception
    /// on a CANBLOCK port (:635-636); the woken `portThread` finds
    /// `!pport->dpc.connected` and runs the 2 s-throttled
    /// `autoConnectDevice(pport,0)` (:856-861). So `asynEnable(port,1)` brings a
    /// port that went down while disabled back up, with no I/O request at all.
    ///
    /// One case per gate boundary: re-enabled (must attempt), disabled (must
    /// not), defunct (must not), and an exception storm (throttled to one).
    #[test]
    fn re_enabling_a_down_port_reconnects_it_with_no_io_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct EnableDriver {
            base: PortDriverBase,
            connect_calls: Arc<AtomicUsize>,
            /// A link that never comes up, so the counter measures attempts
            /// rather than the one transition that ends them.
            comes_up: bool,
        }
        impl PortDriver for EnableDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                self.connect_calls.fetch_add(1, Ordering::SeqCst);
                if self.comes_up {
                    self.base.set_connected(true);
                }
                Ok(())
            }
        }

        // A down, enabled, auto-connect port with no throttle window running
        // (`init_connected` does not stamp a transition).
        let spawn = |enabled: bool, defunct: bool, comes_up: bool| {
            let calls = Arc::new(AtomicUsize::new(0));
            let mut base = PortDriverBase::new("enable_test", 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.init_connected(false);
            base.auto_connect = true;
            base.enabled = enabled;
            if defunct {
                shut_down(&mut base);
            }
            let tx = spawn_actor(EnableDriver {
                base,
                connect_calls: calls.clone(),
                comes_up,
            });
            (tx, calls)
        };

        // Boundary 1 — disabled: the wake finds the queue gate refusing, so no
        // attempt reaches the hardware (C `portThread` :802-805; R14-50).
        let (tx, calls) = spawn(true, false, true);
        send_and_wait(
            &tx,
            RequestOp::SetEnable { yes: false },
            AsynUser::default(),
        )
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a disabled port makes no connect attempt"
        );

        // Boundary 2 — re-enabled: the same wake now passes the gate and the
        // port comes back up, with no I/O request having been submitted.
        send_and_wait(&tx, RequestOp::SetEnable { yes: true }, AsynUser::default()).unwrap();
        let r = send_and_wait(&tx, RequestOp::GetConnected, AsynUser::default()).unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "re-enabling a down auto-connect port must attempt exactly one connect"
        );
        assert_eq!(r.int_val, Some(1), "...and the port is back up");

        // Boundary 3 — defunct: shut down for good, so even a wake on an
        // otherwise-enabled port never touches the hardware again
        // (C `shutdownPort`, asynManager.c:2283-2284).
        let (tx, calls) = spawn(true, true, true);
        let _ = send_and_wait(&tx, RequestOp::GetEnable, AsynUser::default());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a defunct port makes no connect attempt"
        );

        // Boundary 4 — exception storm on a link that stays down: C's 2 s
        // throttle (:712-713) bounds it to one attempt per window, however many
        // exceptions are announced.
        let (tx, calls) = spawn(true, false, false);
        for _ in 0..5 {
            send_and_wait(&tx, RequestOp::SetEnable { yes: true }, AsynUser::default()).unwrap();
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "an exception storm is throttled to one connect attempt per 2 s window"
        );
    }

    /// R16-46: a status query is a plain read and must not dial the hardware.
    ///
    /// C's `portThread` runs a pass only when `notifyPortThread` is signalled, and
    /// `isConnected` (asynManager.c:2326-2337), `isEnabled` (:2340),
    /// `isAutoConnect` (:2348) and `report` (:1136) signal nothing — they take
    /// `asynManagerLock`, read a flag and return. Here every one of them is a
    /// message that wakes the actor, so before the wake latch each of them ended
    /// in `autoConnectDevice(pport,0)`: a status poller pulled a dead port's
    /// reconnect cadence from `secondsBetweenPortConnect` down to its own period,
    /// and `asynReport` became an action.
    ///
    /// One case per boundary: each of the four plain reads (no attempt), the idle
    /// pass (no attempt), and a real wake source (one attempt) — the control that
    /// keeps the latch from being a blanket "never auto-connect".
    #[test]
    fn a_status_query_makes_no_connect_attempt() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct StatusDriver {
            base: PortDriverBase,
            connect_calls: Arc<AtomicUsize>,
        }
        impl PortDriver for StatusDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                // The link never comes up, so the counter measures *attempts*:
                // one connect that succeeded would hide every later query behind
                // `is_connected()`.
                self.connect_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let mut base = PortDriverBase::new("status_test", 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        // Down, enabled, auto-connect, and with no throttle window running
        // (`init_connected` stamps no transition) — so the *first* attempt any
        // pass wanted to make would be permitted. No transition means no retry
        // timer either, so nothing but a wake can produce an attempt.
        base.init_connected(false);
        base.auto_connect = true;
        let tx = spawn_actor(StatusDriver {
            base,
            connect_calls: calls.clone(),
        });

        for op in [
            RequestOp::GetConnected,
            RequestOp::GetEnable,
            RequestOp::GetAutoConnect,
            RequestOp::Report { level: 1 },
        ] {
            let label = format!("{op:?}");
            send_and_wait(&tx, op, AsynUser::default()).unwrap();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "{label} is a plain read in C and must not reach the driver's connect"
            );
        }

        // The idle pass: no message, no timer, no attempt.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "an idle port with no armed retry timer must not dial the hardware"
        );

        // The control — `enable` ends in `exceptionOccurred` (:2247), which *is* a
        // wake (:635-636), so this pass reaches the auto-connect tail.
        //
        // The reply is sent from inside the dispatch, *before* the pass's
        // auto-connect tail runs, so a second request is what makes the tail
        // observable: its own reply cannot be sent until the actor has come back
        // round the loop. It is a plain read, so it adds no attempt of its own —
        // which is the point of the test.
        send_and_wait(&tx, RequestOp::SetEnable { yes: true }, AsynUser::default()).unwrap();
        send_and_wait(&tx, RequestOp::GetEnable, AsynUser::default()).unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "an exception is a port-thread wake and a woken pass does attempt one connect"
        );
    }

    #[test]
    fn actor_get_enable_and_auto_connect() {
        // GetEnable/GetAutoConnect report the driver's actual state and
        // answer even when the port is disabled (they are lifecycle ops
        // that bypass the enabled/connected check).
        let mut drv = TestDriver::new();
        drv.base.enabled = false;
        drv.base.auto_connect = true;
        let tx = spawn_actor(drv);

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let r = send_and_wait(&tx, RequestOp::GetEnable, user).unwrap();
        assert_eq!(r.int_val, Some(0));

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let r = send_and_wait(&tx, RequestOp::GetAutoConnect, user).unwrap();
        assert_eq!(r.int_val, Some(1));
    }

    /// C `queueRequest` (asynManager.c:1536-1552): a request queued at
    /// `asynQueuePriorityConnect` carrying `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED`
    /// skips the `!pport->dpc.connected → asynDisconnected` refusal. Bare Connect
    /// priority does not (unless the user carries no device, `addr == -1`) — that
    /// is what keeps the record's Low-priority EOS readback (asynRecord.c:1296)
    /// blank on a down port while its option readback (:1277-1280) fills in.
    #[test]
    fn queue_even_if_not_connected_reason_waives_only_the_connected_refusal() {
        let mut drv = TestDriver::new();
        drv.base.auto_connect = false; // no reconnect can rescue the request
        drv.base.set_connected(false);
        let tx = spawn_actor(drv);

        // No waiver: refused, exactly as C's :1547-1552.
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let err = send_and_wait(
            &tx,
            RequestOp::SetOption {
                key: "hostinfo".into(),
                value: "newhost:5000".into(),
            },
            user,
        )
        .unwrap_err();
        assert!(
            err.is_queue_refused(),
            "the gate's refusal is a QueueRefused, not a driver error: {err:?}"
        );
        assert_eq!(err.status(), AsynStatus::Disconnected);

        // Connect priority alone is not the waiver: the user carries addr 0, so
        // C's :1536-1538 leaves `checkPortConnect` set.
        let user = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(1))
            .with_priority(QueuePriority::Connect);
        let err = send_and_wait(
            &tx,
            RequestOp::SetOption {
                key: "hostinfo".into(),
                value: "newhost:5000".into(),
            },
            user,
        )
        .unwrap_err();
        assert!(
            err.is_queue_refused(),
            "the gate's refusal is a QueueRefused, not a driver error: {err:?}"
        );
        assert_eq!(err.status(), AsynStatus::Disconnected);

        // With the waiver the put reaches the driver on the dead line — the
        // operator's only route to repoint a wrong or moved host.
        let user = AsynUser::new(0)
            .with_timeout(Duration::from_secs(1))
            .queue_even_if_not_connected();
        send_and_wait(
            &tx,
            RequestOp::SetOption {
                key: "hostinfo".into(),
                value: "newhost:5000".into(),
            },
            user,
        )
        .expect("QUEUE_EVEN_IF_NOT_CONNECTED must reach the driver on a down port");

        // And the readback comes back off the same dead line.
        let user = AsynUser::new(0)
            .with_timeout(Duration::from_secs(1))
            .queue_even_if_not_connected();
        let r = send_and_wait(
            &tx,
            RequestOp::GetOption {
                key: "hostinfo".into(),
            },
            user,
        )
        .expect("the connect-time option readback is queued with the same waiver");
        assert_eq!(r.option_value.as_deref(), Some("newhost:5000"));
    }

    /// R13-47: `asynPortDriver::callParamCallbacks` (asynPortDriver.cpp:1785-1794)
    /// is a plain method call — `getParamList(list)` then `pList->callCallbacks(addr)`.
    /// It is invoked from driver code, never through `queueRequest`, so neither
    /// the `asynDisabled` (asynManager.c:1541-1546) nor the `asynDisconnected`
    /// (:1547-1552) refusal is reachable from it. A driver's parameter publish
    /// must land on a disconnected *and* on a disabled port — that is precisely
    /// when a driver publishes its status/health parameters.
    #[test]
    fn a_parameter_publish_lands_on_a_disconnected_or_disabled_port() {
        for (connected, enabled) in [(false, true), (true, false), (false, false)] {
            let mut drv = TestDriver::new();
            drv.base.auto_connect = false; // no reconnect can rescue the update
            drv.base.set_connected(connected);
            drv.base.enabled = enabled;
            let val = drv.base.find_param("VAL").unwrap();
            let tx = spawn_actor(drv);

            // C: the driver's own publish, on a port whose line is down / disabled.
            let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
            send_and_wait(
                &tx,
                RequestOp::CallParamCallbacks {
                    addr: 0,
                    updates: vec![crate::request::ParamSetValue::new(
                        val,
                        0,
                        crate::param::ParamValue::Int32(42),
                    )],
                },
                user,
            )
            .unwrap_or_else(|e| {
                panic!("connected={connected} enabled={enabled}: C never gates callParamCallbacks, got {e:?}")
            });

            // Bring the port up and re-enable it, then read the parameter back:
            // the publish must be *there*, not lost to a refusal the driver never
            // saw (`set_params_and_notify` drops the reply channel).
            let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
            send_and_wait(&tx, RequestOp::SetEnable { yes: true }, user).unwrap();
            // Port-level user (addr == -1): the Connect-queue waiver holds on
            // the disconnected port (a device-addressed connect would be
            // refused — W10-D1).
            let user = AsynUser::new(0)
                .with_addr(-1)
                .with_timeout(Duration::from_secs(1));
            send_and_wait(&tx, RequestOp::Connect, user).unwrap();

            let user = AsynUser::new(val).with_timeout(Duration::from_secs(1));
            let r = send_and_wait(&tx, RequestOp::Int32Read, user).unwrap();
            assert_eq!(
                r.int_val,
                Some(42),
                "connected={connected} enabled={enabled}: the parameter published \
                 while the port was down must survive (it read back ParamUndefined \
                 when the op was queue-gated)"
            );
        }
    }

    /// `callParamCallbacks(addr)` consumes the changed flags of **one** address
    /// list (`ParamList::take_changed`), and C drivers call it once per list
    /// they wrote (asynPortDriver.cpp:820). This carrier bundles the sets and
    /// the flush, so it owes a flush for every list the batch wrote: flushing
    /// only the request address stored the other lists' values and fired no
    /// interrupt for them, so a multi-device driver publishing six joint angles
    /// at addresses 0..6 in one call left five records at UDF for the life of
    /// the IOC while the array parameter carrying the same six numbers updated
    /// normally.
    #[test]
    fn a_publish_flushes_every_address_list_it_wrote() {
        let mut base = PortDriverBase::new(
            "multi_addr",
            3,
            PortFlags {
                multi_device: true,
                ..PortFlags::default()
            },
        );
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let _sub = base.interrupts.register_sync_callback(
            InterruptFilter {
                reason: Some(val),
                ..InterruptFilter::default()
            },
            move |iv| {
                recorder
                    .lock()
                    .push((iv.addr, iv.value.as_int32().unwrap()))
            },
        );
        let tx = spawn_actor(TestDriverBase(base));

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(
            &tx,
            RequestOp::CallParamCallbacks {
                addr: 0,
                updates: (0..3)
                    .map(|a| {
                        crate::request::ParamSetValue::new(
                            val,
                            a,
                            crate::param::ParamValue::Int32(10 + a),
                        )
                    })
                    .collect(),
            },
            user,
        )
        .unwrap();

        assert_eq!(
            *seen.lock(),
            vec![(0, 10), (1, 11), (2, 12)],
            "every address the batch wrote is flushed, request address first"
        );
    }

    /// `ParamSetValue::Status` is the background-thread half of C's alarm
    /// push — `lock(); setParamStatus(..); callParamCallbacks(); unlock()`.
    /// A status-only transition marks the param changed (paramVal.cpp:71-78),
    /// so the flush delivers the stored triplet on the `InterruptValue` while
    /// the value stays what it was; the recovery transition back to `Success`
    /// is delivered the same way.
    #[test]
    fn a_status_publish_delivers_the_alarm_without_touching_the_value() {
        let mut base = PortDriverBase::new("status_pub", 1, PortFlags::default());
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let _sub = base.interrupts.register_sync_callback(
            InterruptFilter {
                reason: Some(val),
                ..InterruptFilter::default()
            },
            move |iv| {
                recorder.lock().push((
                    iv.value.as_int32().unwrap(),
                    iv.aux_status,
                    iv.alarm_status,
                    iv.alarm_severity,
                ))
            },
        );
        let tx = spawn_actor(TestDriverBase(base));

        let publish = |updates| {
            let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
            send_and_wait(
                &tx,
                RequestOp::CallParamCallbacks { addr: 0, updates },
                user,
            )
            .unwrap();
        };
        publish(vec![crate::request::ParamSetValue::new(
            val,
            0,
            crate::param::ParamValue::Int32(7),
        )]);
        publish(vec![crate::request::ParamSetValue::status(
            val,
            0,
            crate::error::AsynStatus::Disconnected,
            0,
            0,
        )]);
        publish(vec![crate::request::ParamSetValue::status(
            val,
            0,
            crate::error::AsynStatus::Success,
            0,
            0,
        )]);

        assert_eq!(
            *seen.lock(),
            vec![
                (7, crate::error::AsynStatus::Success, 0, 0),
                (7, crate::error::AsynStatus::Disconnected, 0, 0),
                (7, crate::error::AsynStatus::Success, 0, 0),
            ],
            "a status transition fires with the prior value intact, and so \
             does the recovery back to Success"
        );
    }

    /// R13-48: `asynDrvUser->create` is a direct interface call — asynRecord's
    /// `connectDevice` (asynRecord.c:1243-1257) and `devAsynInt32::initCommon`
    /// (devAsynInt32.c:264-277) both call `pasynDrvUser->create(drvPvt, pasynUser,
    /// drvInfo, 0, 0)` straight off the interface pointer, with no `queueRequest`
    /// in either path. It is a string → parameter-index lookup in the driver's own
    /// table: it cannot fail for connectivity reasons. Queue-gating it made a
    /// record whose device is down resolve its DRVINFO to reason 0 and report
    /// "Error in asynDrvUser->create()" (asyn_record/mod.rs:2977-2993).
    #[test]
    fn drv_user_create_resolves_on_a_disconnected_or_disabled_port() {
        for (connected, enabled) in [(false, true), (true, false), (false, false)] {
            let mut drv = TestDriver::new();
            drv.base.auto_connect = false;
            drv.base.set_connected(connected);
            drv.base.enabled = enabled;
            let f64_reason = drv.base.find_param("F64").unwrap();
            let tx = spawn_actor(drv);

            let user = AsynUser::default().with_addr(0);
            let r = send_and_wait(
                &tx,
                RequestOp::DrvUserCreate(crate::port::DrvUserRequest::new("F64", 0)),
                user,
            )
            .unwrap_or_else(|e| {
                panic!(
                    "connected={connected} enabled={enabled}: C's drvUser->create is a \
                     direct table lookup and cannot fail for connectivity, got {e:?}"
                )
            });
            assert_eq!(
                r.reason,
                Some(f64_reason),
                "connected={connected} enabled={enabled}: DRVINFO must resolve to the \
                 driver's real reason, not fall back to 0"
            );
        }
    }

    /// R14-51: `asynReport` prints C's manager block — the port state no driver
    /// owns and none of them printed.
    ///
    /// C `reportPrintPort` (asynManager.c:1018-1122) prints, in order: the
    /// multiDevice/canBlock/autoConnect header; at `details >= 1` the
    /// enabled/connected/numberConnects line, the nDevices/nQueued/blocked line,
    /// the two lock states, the exception counts and the trace masks; at
    /// `details >= 2` the interpose and interface lists; then the per-address
    /// block (suppressed by a *negative* `details`, :1032-1035) — and only then
    /// the driver's own `pasynCommon->report`.
    ///
    /// One case per boundary of C's `details` rules: 0, 1, 2, negative, defunct.
    /// C's `asynReport` header prints `multiDevice:` straight off
    /// `pport->attributes & ASYN_MULTIDEVICE` (asynManager.c:1043-1047), and the
    /// IP server listener is registered with plain `ASYN_CANBLOCK`
    /// (drvAsynIPServerPort.c:625-626) — its asynOctet table is all NULL
    /// (:118-122), so it is an interrupt source, not an addressable port. The
    /// per-client ports are separate single-device registrations (:688-694).
    #[test]
    fn the_ip_server_listener_reports_multidevice_no_like_c() {
        let srv =
            crate::drivers::ip_server_port::DrvAsynIPServerPort::new("ipsrv_rep", "127.0.0.1:0")
                .expect("server");
        let (_tx, rx) = mpsc::channel(1);
        let actor = PortActor::new(Box::new(srv), rx);

        assert_eq!(
            actor.manager_report(0).lines().next().unwrap(),
            "ipsrv_rep multiDevice:No canBlock:Yes autoConnect:Yes",
            "C's header line for the listener (asynManager.c:1043-1047)"
        );
    }

    #[test]
    fn asyn_report_prints_the_manager_block_c_prints() {
        struct Plain(PortDriverBase);
        impl PortDriver for Plain {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
                crate::interfaces::octet_transport_capabilities()
            }
        }

        let build = || {
            let mut base = PortDriverBase::new(
                "rep_block",
                4,
                PortFlags {
                    multi_device: true,
                    can_block: true,
                    ..PortFlags::default()
                },
            );
            base.auto_connect = true;
            base.init_connected(false);
            base.set_connected(true); // one published connect → numberConnects 1
            base.device_state(0).connected = true;
            base.device_state(1).connected = false; // a down device
            base.install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
            let (_tx, rx) = mpsc::channel(1);
            (PortActor::new(Box::new(Plain(base)), rx), _tx)
        };

        // details = 0: the header, and the down device — nothing else. C prints a
        // disconnected device even at 0 (:1087), which is what the operator ran
        // the report for.
        let (actor, _tx) = build();
        let r0 = actor.manager_report(0);
        assert_eq!(
            r0.lines().next().unwrap(),
            "rep_block multiDevice:Yes canBlock:Yes autoConnect:Yes",
            "C's header line (asynManager.c:1043-1047)"
        );
        assert!(!r0.contains("enabled:"), "details 0 prints no state line");
        assert!(
            r0.contains("addr 1 autoConnect Yes enabled Yes connected No"),
            "a disconnected device prints at details 0: {r0}"
        );
        assert!(
            !r0.contains("addr 0 "),
            "…and a connected one does not: {r0}"
        );

        // details = 1: the state, queue, lock, exception and trace lines.
        let r1 = actor.manager_report(1);
        assert!(
            r1.contains("    enabled:Yes connected:Yes numberConnects 1"),
            "C :1057-1060 — including the count of connects: {r1}"
        );
        assert!(
            r1.contains("    nDevices 2 nQueued 0 blocked:No"),
            "C :1061-1064: {r1}"
        );
        assert!(
            r1.contains("    asynManagerLock:No synchronousLock:No"),
            "C :1065-1067: {r1}"
        );
        assert!(
            r1.contains("    exceptionActive:No exceptionUsers 0 exceptionNotifys 0"),
            "C :1068-1073: {r1}"
        );
        assert!(
            r1.contains("    traceMask:0x"),
            "C :1074-1075 prints the port's three trace masks: {r1}"
        );
        assert!(
            r1.contains("addr 0 autoConnect Yes enabled Yes connected Yes"),
            "at details 1 every device prints, connected or not (C :1087): {r1}"
        );

        // details = 2: the interpose and interface lists (C :1076-1080).
        let r2 = actor.manager_report(2);
        assert!(
            r2.contains("    interposeInterfaceList\n        asynOctet"),
            "the installed EOS interpose is an asynOctet interpose: {r2}"
        );
        assert!(
            r2.contains("    interfaceList\n        asynCommon"),
            "the driver's registered interfaces: {r2}"
        );
        assert!(r2.contains("        asynOctet\n"), "…asynOctet among them");

        // A negative details is C's "detail, but no devices" (:1032-1035).
        let rneg = actor.manager_report(-1);
        assert!(
            rneg.contains("    enabled:Yes connected:Yes numberConnects 1"),
            "a negative details still prints the detail lines: {rneg}"
        );
        assert!(
            !rneg.contains("addr "),
            "…and suppresses the per-address block: {rneg}"
        );

        // A destroyed port: one line, and C never reaches the driver (:1038-1042).
        let (mut actor, _tx) = build();
        actor.driver.base_mut().flags.destructible = true;
        actor.driver.base_mut().shutdown_lifecycle().unwrap();
        assert_eq!(actor.manager_report(2), "rep_block destroyed\n");
    }

    /// W10-D6: C's `report` (asynManager.c:1136-1171) spawns a `reportPort` thread
    /// and calls `pasynCommon->report(drvPvt, fp, details)` directly (:1121-1123).
    /// It never queues, so no gate runs: a disabled port reports `enabled:No` and
    /// a disconnected one `connected:No` (:1112-1115). Queue-gating it made
    /// `asynReport` on a down port print a refusal instead of the diagnostic the
    /// operator opened it for.
    ///
    /// The one port C refuses to report on is a destroyed one: `reportPrintPort`
    /// prints "<name> destroyed" and returns without touching the driver
    /// (:1038-1042).
    #[test]
    fn report_runs_on_a_disabled_or_disconnected_port_but_not_on_a_destroyed_one() {
        struct ReportSpy {
            base: PortDriverBase,
            reports: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl PortDriver for ReportSpy {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn report(&self, _out: &mut dyn std::fmt::Write, _level: i32) {
                self.reports
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        for (connected, enabled) in [(false, true), (true, false), (false, false)] {
            let reports = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut base = PortDriverBase::new("rep", 1, PortFlags::default());
            base.auto_connect = false;
            base.set_connected(connected);
            base.enabled = enabled;
            let tx = spawn_actor(ReportSpy {
                base,
                reports: reports.clone(),
            });

            let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
            send_and_wait(&tx, RequestOp::Report { level: 1 }, user).unwrap_or_else(|e| {
                panic!(
                    "connected={connected} enabled={enabled}: C's report is a direct call \
                     and cannot be refused, got {e:?}"
                )
            });
            assert_eq!(
                reports.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "connected={connected} enabled={enabled}: the driver's report must run"
            );
        }

        // A destroyed port: C prints "<name> destroyed" and never reaches the
        // driver's interfaces (asynManager.c:1038-1042).
        let reports = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut base = PortDriverBase::new("gone", 1, PortFlags::default());
        shut_down(&mut base);
        let tx = spawn_actor(ReportSpy {
            base,
            reports: reports.clone(),
        });
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Report { level: 1 }, user).unwrap();
        assert_eq!(
            reports.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a destroyed port prints \"<name> destroyed\"; its driver is not called"
        );
    }

    /// R15-51: the per-address line prints the *device's* autoConnect. C reads it
    /// from the device's own dpCommon (`pdpc = &pdevice->dpc`, asynManager.c:1084-1089);
    /// the port's is on the header line. Printing the port's on both makes
    /// `asynAutoConnect(port, addr, 0)` invisible to the report.
    ///
    /// Two boundaries: a device whose auto-connect was turned off individually, and
    /// a device created on a port that has auto-connect off — C seeds a new device
    /// from the port (`dpCommonInit(pport, pdevice, pport->dpc.autoConnect)`, :584).
    #[test]
    fn the_per_address_line_prints_the_devices_auto_connect() {
        let build = |port_auto_connect: bool| {
            let mut base = PortDriverBase::new(
                "ac_rep",
                4,
                PortFlags {
                    multi_device: true,
                    ..PortFlags::default()
                },
            );
            base.auto_connect = port_auto_connect;
            base.device_state(0);
            base.device_state(1);
            let (_tx, rx) = mpsc::channel(1);
            (PortActor::new(Box::new(TestDriverBase(base)), rx), _tx)
        };

        // Boundary 1: the port auto-connects, one device does not.
        let (mut actor, _tx) = build(true);
        actor.driver.base_mut().set_auto_connect_addr(1, false);
        let r = actor.manager_report(1);
        assert!(
            r.lines().next().unwrap().ends_with("autoConnect:Yes"),
            "the header line is the port's autoConnect: {r}"
        );
        assert!(
            r.contains("addr 0 autoConnect Yes"),
            "addr 0 still auto-connects: {r}"
        );
        assert!(
            r.contains("addr 1 autoConnect No"),
            "addr 1's own autoConnect must print, not the port's: {r}"
        );

        // Boundary 2: a manual-connect port — every device it creates inherits No.
        let (actor, _tx) = build(false);
        let r = actor.manager_report(1);
        assert!(
            r.contains("addr 0 autoConnect No") && r.contains("addr 1 autoConnect No"),
            "a device of a manual-connect port must not claim autoConnect Yes: {r}"
        );
    }

    /// R15-51: the driver block is C++ `asynPortDriver::report`
    /// (asynPortDriver.cpp:3677-3710) — a Timestamp line, the EOS pair *only* when
    /// the driver registered `asynOctet` (:3685), and at `details >= 3` the
    /// interrupt clients (:3695-3708).
    #[test]
    fn the_default_driver_block_is_asyn_port_drivers() {
        use crate::interfaces::{Capability, InterfaceType};
        use crate::interrupt::InterruptFilter;

        struct Drv(PortDriverBase, Vec<Capability>);
        impl PortDriver for Drv {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<Capability> {
                self.1.clone()
            }
        }

        let build = |caps: Vec<Capability>| {
            let mut base = PortDriverBase::new("drv_rep", 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
            let mut drv = Drv(base, caps);
            drv.set_input_eos(&AsynUser::default(), b"\r\n").unwrap();
            drv
        };

        // No octet interface → no EOS lines. C reads them off
        // `pasynStdInterfaces->octet.pinterface`, which such a port never set.
        let int32_only = build(vec![Capability::Int32Read]);
        let mut out = String::new();
        int32_only.report(&mut out, 1);
        assert!(out.starts_with("Port: drv_rep\n"), "{out}");
        assert!(
            out.contains("  Timestamp: "),
            "C prints the port's timestamp at details >= 1 (:3682-3684): {out}"
        );
        assert!(
            !out.contains("EOS["),
            "a port with no octet interface has no EOS to report: {out}"
        );
        // C `reportParams(fp, details)` with the level unshifted (:3692): at
        // details 1 the whole parameter block is already there — list 0, the
        // count, and every parameter with its value (:1804-1807,
        // paramVal.cpp:296-330).
        assert!(
            out.contains("Parameter list 0\nNumber of parameters is: 1\n")
                && out.contains("Parameter 0 type=asynInt32, name=VAL, value is undefined\n"),
            "asynReport 1 prints the parameter list, as C does: {out}"
        );

        // details 0: the port line and nothing else (C :3678-3679).
        let mut out0 = String::new();
        int32_only.report(&mut out0, 0);
        assert_eq!(out0, "Port: drv_rep\n");

        // An octet port reports the terminators it actually has.
        let octet = build(crate::interfaces::octet_transport_capabilities());
        let mut out = String::new();
        octet.report(&mut out, 1);
        assert!(
            out.contains("  Input EOS[2]: \\r\\n") && out.contains("  Output EOS[0]: "),
            "an octet port prints its EOS pair: {out}"
        );

        // details >= 3: one line per interrupt client, and none below 3.
        let with_client = build(vec![Capability::Int32Read]);
        let (_sub, _rx) = with_client
            .0
            .interrupts
            .register_interrupt_user(InterruptFilter {
                reason: Some(0),
                addr: Some(0),
                iface: Some(InterfaceType::Int32),
                ..InterruptFilter::default()
            });
        let mut out2 = String::new();
        with_client.report(&mut out2, 2);
        assert!(
            !out2.contains("callback client"),
            "C gates the interrupt block on details >= 3: {out2}"
        );
        let mut out3 = String::new();
        with_client.report(&mut out3, 3);
        assert!(
            out3.contains("    int32 callback client addr=0, reason=0"),
            "C's reportInterrupt line for the registered client: {out3}"
        );
    }

    /// R15-51: `asynReport` writes to stdout. C's `asynReport` passes `stdout` to
    /// `pasynManager->report` (asynShellCommands.c:589) — it is an answer to an
    /// iocsh question, so `asynReport 1 port > file` captures it. The Rust actor
    /// printed it to stderr, where a shell redirect misses it.
    ///
    /// A `dup2` on fd 1 around the call cannot observe this: libtest's default
    /// capture rewrites `print!`/`println!` to an in-process buffer *before* the
    /// bytes ever reach a file descriptor, and it does so regardless of which
    /// thread makes the call — spawning a helper thread does not escape it
    /// either. The intercept is only off for a process actually started with
    /// `--nocapture`, so this re-execs the test binary as a child, selects just
    /// this test by exact name, forces `--nocapture` on that child, and reads
    /// the child's real stdout back through the pipe `Command::output` sets up.
    /// That holds under plain `cargo test`, `cargo nextest run`, and
    /// `--nocapture`, since the child's capture state is forced explicitly
    /// rather than inherited from however the outer run was invoked.
    #[cfg(unix)]
    #[test]
    fn asyn_report_writes_to_stdout() {
        const CHILD_ENV: &str = "ASYN_REPORT_WRITES_TO_STDOUT_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            let base = PortDriverBase::new("stdout_rep", 1, PortFlags::default());
            let (_tx, rx) = mpsc::channel(1);
            let actor = PortActor::new(Box::new(TestDriverBase(base)), rx);
            actor.report_port(0);
            return;
        }

        let exe = std::env::current_exe().unwrap();
        let output = std::process::Command::new(exe)
            .args([
                "port_actor::tests::asyn_report_writes_to_stdout",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        let captured = String::from_utf8_lossy(&output.stdout);
        assert!(
            captured.contains("stdout_rep multiDevice:No") && captured.contains("Port: stdout_rep"),
            "the manager block and the driver block both belong on stdout: {captured}"
        );
    }

    /// R12-48: C conditions **only** `checkPortConnect` on the priority
    /// (asynManager.c:1536-1538). `if(!pport->dpc.enabled) return asynDisabled`
    /// (:1541-1546) sits above it and every queued request takes it — and the
    /// port thread will not drain even the Connect queue on a disabled port
    /// (:806-809). So a `CNCT=1` put on a port disabled with `asynEnable(port,0)`
    /// must not open the device connection: the port was disabled precisely to
    /// keep the IOC off the hardware.
    ///
    /// The enable/query ops are the other half: C runs them as direct
    /// `pasynManager` calls that never reach the gate, so they must keep working
    /// on a disabled port — otherwise a disabled port could never be re-enabled.
    #[test]
    fn a_disabled_port_refuses_a_connect_request_but_still_takes_the_enable() {
        struct ConnectSpy {
            base: PortDriverBase,
            connects: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl PortDriver for ConnectSpy {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                self.connects
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.base.set_connected(true);
                Ok(())
            }
        }

        let connects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut base = PortDriverBase::new("disabled_port", 1, PortFlags::default());
        base.auto_connect = false;
        base.set_connected(false);
        base.enabled = false; // C `asynEnable(port, 0)`
        let tx = spawn_actor(ConnectSpy {
            base,
            connects: connects.clone(),
        });

        // CNCT=1 on a disabled port: asynDisabled, and the driver's connect() is
        // never called — the hardware is not touched.
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let err = send_and_wait(&tx, RequestOp::Connect, user).unwrap_err();
        assert!(
            err.is_queue_refused(),
            "the gate's refusal is a QueueRefused, not a driver error: {err:?}"
        );
        assert_eq!(err.status(), AsynStatus::Disabled);
        assert_eq!(
            connects.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a disabled port must not open the device connection"
        );

        // Not even the option waiver buys the enabled refusal off (C's :1541-1546
        // is above `checkPortConnect`, not beside it).
        let user = AsynUser::new(0)
            .with_timeout(Duration::from_secs(1))
            .queue_even_if_not_connected();
        let err = send_and_wait(
            &tx,
            RequestOp::SetOption {
                key: "hostinfo".into(),
                value: "h:1".into(),
            },
            user,
        )
        .unwrap_err();
        assert!(
            err.is_queue_refused(),
            "the gate's refusal is a QueueRefused, not a driver error: {err:?}"
        );
        assert_eq!(err.status(), AsynStatus::Disabled);

        // The enable itself is a direct manager call in C and takes no gate…
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::SetEnable { yes: true }, user).unwrap();

        // …but a device-addressed CNCT=1 put is still refused: the port is
        // enabled yet disconnected, and C's `checkPortConnect` waiver covers
        // only `addr == -1` or the sentinel (asynManager.c:1536-1538) — the
        // W10-D1 wart, reproduced.
        let user = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(1));
        let err = send_and_wait(&tx, RequestOp::Connect, user).unwrap_err();
        assert!(
            err.is_queue_refused(),
            "the gate's refusal is a QueueRefused, not a driver error: {err:?}"
        );
        assert_eq!(err.status(), AsynStatus::Disconnected);
        assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 0);

        // The route that DOES bring the enabled port up is the port-level
        // user — C `connectDevice(port, -1)`, whose waiver holds.
        let user = AsynUser::new(0)
            .with_addr(-1)
            .with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Connect, user).unwrap();
        assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn actor_connect_disconnect() {
        let mut drv = TestDriver::new();
        // Seed VAL so the post-reconnect read probe returns a value rather
        // than ParamUndefined; this test asserts the reconnect lifecycle.
        drv.base.params.set_int32(0, 0, 0).unwrap();
        let tx = spawn_actor(drv);

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Disconnect, user).unwrap();

        // Port is now disconnected. A device-addressed reconnect is refused
        // (C's checkPortConnect, W10-D1) — the port-level user (addr == -1,
        // C `connectDevice(port, -1)`) is the route back up.
        let user = AsynUser::new(0)
            .with_addr(-1)
            .with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Connect, user).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        assert!(result.is_ok());
    }

    /// W10-D1 boundaries of C's Connect-queue waiver (asynManager.c:1536-1538):
    /// on a disconnected, non-autoconnect port a Connect-priority request runs
    /// iff the user is port-level (`addr == -1`) or carries the sentinel. The
    /// device-addressed CNCT put (asynRecord's special user at the record's
    /// ADDR, no sentinel) is refused `asynDisconnected` — the C wart, kept by
    /// decision.
    #[test]
    fn device_addressed_connect_is_refused_on_a_disconnected_port() {
        let mut drv = TestDriver::new();
        drv.base.auto_connect = false;
        drv.base.set_connected(false);
        let tx = spawn_actor(drv);

        // Boundary 1: addr >= 0, no sentinel → refused (C checkPortConnect).
        let user = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(1));
        let err = send_and_wait(&tx, RequestOp::Connect, user).unwrap_err();
        assert!(
            err.is_queue_refused(),
            "the gate's refusal is a QueueRefused, not a driver error: {err:?}"
        );
        assert_eq!(err.status(), AsynStatus::Disconnected);

        // Boundary 2: addr >= 0 WITH the sentinel → waived, runs.
        let user = AsynUser::new(0)
            .with_timeout(Duration::from_secs(1))
            .queue_even_if_not_connected();
        send_and_wait(&tx, RequestOp::Connect, user)
            .expect("the sentinel waives the connected refusal at any addr");

        // Reset for boundary 3.
        let user = AsynUser::new(0)
            .with_addr(-1)
            .with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Disconnect, user).unwrap();

        // Boundary 3: addr == -1, no sentinel → waived, runs.
        let user = AsynUser::new(0)
            .with_addr(-1)
            .with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Connect, user)
            .expect("a port-level user is waived on the Connect queue");
    }

    /// Boundary 4, and the reason the other three can be written at all: a user
    /// that never chose a device is *port-level*, so it clears C's
    /// `checkPortConnect` waiver (`addr == -1`, asynManager.c:1536-1538)
    /// without the caller spelling `-1` anywhere.
    ///
    /// C carries the distinction as a pointer, not a number:
    /// `connectDevice` allocates `puserPvt->pdevice` only for `addr >= 0`
    /// (asynManager.c:1348-1351), `findDpCommon` returns `&pport->dpc` whenever
    /// that pointer is NULL (:536-544), and `getAddr` renders a NULL `pdevice`
    /// as -1 (:2004-2008) — the one place the number appears, at the driver
    /// boundary. So "no device chosen" and "device 0" are different states in
    /// C, and `AsynUser::default()` is the Rust spelling of the first. A
    /// default that said 0 would make every framework-constructed user silently
    /// device-addressed, and each new reader of `user.addr` would need its own
    /// `-1` patch.
    #[test]
    fn a_user_that_never_chose_a_device_is_port_level() {
        let mut drv = TestDriver::new();
        drv.base.auto_connect = false;
        drv.base.set_connected(false);
        let tx = spawn_actor(drv);

        let user = AsynUser::default().with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Connect, user)
            .expect("a defaulted user is port-level, so the Connect gate waives it");

        assert_eq!(AsynUser::default().addr, -1);
    }

    #[test]
    fn actor_block_unblock_process() {
        let tx = spawn_actor(TestDriver::new());

        // Write initial value
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Int32Write { value: 10 }, user).unwrap();

        // Block with token 42
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(42);
        send_and_wait(&tx, RequestOp::BlockProcess { all_devices: true }, user).unwrap();

        // Owner request should succeed
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(42);
        send_and_wait(&tx, RequestOp::Int32Write { value: 99 }, user).unwrap();

        // Unblock (must use same token as the block holder)
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(42);
        send_and_wait(&tx, RequestOp::UnblockProcess { all_devices: true }, user).unwrap();

        // Non-owner should now work
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user).unwrap();
        assert_eq!(result.int_val, Some(99));
    }

    /// R18-70: C keeps `pport->pblockProcessHolder` **and**
    /// `pdpCommon->pblockProcessHolder`, and admits a request only when both
    /// are NULL-or-it (asynManager.c:889-892, :929-932). devGpib takes the
    /// device form for an SRQ transaction (devSupportGpib.c:1216), so one GPIB
    /// address holding a block must leave every other address on the port
    /// running.
    #[test]
    fn a_device_block_leaves_the_ports_other_addresses_running() {
        let tx = spawn_actor(multi_device_driver(true));

        // Address 0 takes a DEVICE block.
        let mut owner = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(1));
        owner.block_token = Some(42);
        send_and_wait(&tx, RequestOp::BlockProcess { all_devices: false }, owner).unwrap();

        // Address 1 — a different dpCommon — is not gated by it. Probed
        // without blocking on the reply: under a port-wide holder this read
        // is diverted and never answers, and a test that waited on it would
        // hang rather than fail. `GetEnable` behind it is the barrier — it is
        // enqueued after this read and can only answer once the actor has
        // dealt with it, so the receiver's state is settled by then.
        let (other_tx, mut other_rx) = oneshot::channel();
        tx.blocking_send(ActorMessage::new(
            RequestOp::Int32Read,
            AsynUser::new(0)
                .with_addr(1)
                .with_timeout(Duration::from_secs(1)),
            CancelToken::new(),
            other_tx,
        ))
        .unwrap();
        send_and_wait(
            &tx,
            RequestOp::GetEnable,
            AsynUser::new(0).with_timeout(Duration::from_secs(1)),
        )
        .unwrap();
        assert_eq!(
            other_rx
                .try_recv()
                .expect("address 1 must run while address 0 holds a device block")
                .unwrap()
                .int_val,
            Some(7)
        );

        // The holder itself still runs on its own address...
        let mut owner = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(1));
        owner.block_token = Some(42);
        send_and_wait(&tx, RequestOp::Int32Write { value: 99 }, owner).unwrap();

        // ...and a non-owner on address 0 does not: it sits in
        // pending_while_blocked until the release. Its reply arrives only
        // after the unblock below, so hold the receiver rather than waiting.
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let stalled = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(1));
        tx.blocking_send(ActorMessage::new(
            RequestOp::Int32Read,
            stalled,
            CancelToken::new(),
            reply_tx,
        ))
        .unwrap();
        assert!(
            matches!(
                reply_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "a non-owner on the blocked address must be held"
        );

        let mut owner = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(1));
        owner.block_token = Some(42);
        send_and_wait(&tx, RequestOp::UnblockProcess { all_devices: false }, owner).unwrap();

        let held = reply_rx.blocking_recv().unwrap().unwrap();
        assert_eq!(held.int_val, Some(99), "released by the device unblock");
    }

    /// The port-wide holder still gates every address — releasing a device
    /// holder must not free what the port holder is still holding, which is
    /// the half C gets for free by never moving a request off its queue.
    #[test]
    fn releasing_a_device_block_does_not_free_what_the_port_block_holds() {
        let tx = spawn_actor(multi_device_driver(true));

        let u = |token: u64, addr: i32| {
            let mut user = AsynUser::new(0)
                .with_addr(addr)
                .with_timeout(Duration::from_secs(1));
            user.block_token = Some(token);
            user
        };

        send_and_wait(&tx, RequestOp::BlockProcess { all_devices: true }, u(7, -1)).unwrap();
        send_and_wait(&tx, RequestOp::BlockProcess { all_devices: false }, u(7, 0)).unwrap();

        // A non-owner on address 1: only the PORT holder gates it.
        let (reply_tx, mut reply_rx) = oneshot::channel();
        tx.blocking_send(ActorMessage::new(
            RequestOp::Int32Read,
            AsynUser::new(0)
                .with_addr(1)
                .with_timeout(Duration::from_secs(1)),
            CancelToken::new(),
            reply_tx,
        ))
        .unwrap();

        // Releasing the DEVICE holder leaves it held.
        send_and_wait(
            &tx,
            RequestOp::UnblockProcess { all_devices: false },
            u(7, 0),
        )
        .unwrap();
        assert!(
            matches!(
                reply_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "the port-wide holder still gates address 1"
        );

        // Releasing the PORT holder frees it.
        send_and_wait(
            &tx,
            RequestOp::UnblockProcess { all_devices: true },
            u(7, -1),
        )
        .unwrap();
        assert_eq!(
            reply_rx.blocking_recv().unwrap().unwrap().int_val,
            Some(7),
            "released by the port unblock"
        );
    }

    #[test]
    fn actor_unblock_with_expired_deadline_still_runs() {
        // An UnblockProcess that waited out its nominal I/O timeout must
        // still execute once dequeued — otherwise the port stays wedged for
        // every non-owner caller forever. No dequeued request (lifecycle or
        // I/O) is aborted by a queue deadline (C parity, asynManager.c:906).
        let mut drv = TestDriver::new();
        // Seed VAL so the post-unblock read probe returns a value rather than
        // the C-parity ParamUndefined for an unset param; this test asserts
        // the unblock lifecycle, not undefined-read state.
        drv.base.params.set_int32(0, 0, 0).unwrap();
        let tx = spawn_actor(drv);

        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(7);
        send_and_wait(&tx, RequestOp::BlockProcess { all_devices: true }, user).unwrap();

        // Submit the unblock with an already-expired deadline.
        let mut user = AsynUser::new(0).with_timeout(Duration::from_nanos(1));
        user.block_token = Some(7);
        std::thread::sleep(Duration::from_millis(1));
        send_and_wait(&tx, RequestOp::UnblockProcess { all_devices: true }, user)
            .expect("expired-deadline UnblockProcess must still run");

        // Port must be unblocked: a non-owner request now succeeds.
        // (VAL was written above, so the read returns a defined value rather
        // than the C-parity ParamUndefined for an unset param.)
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Int32Read, user)
            .expect("port should be unblocked after UnblockProcess");
    }

    /// C runs enable/auto-connect/get/connect/disconnect directly under
    /// asynManagerLock — never via queueRequest — so the block holder (which
    /// only gates the portThread's I/O `processUser` dispatch) cannot stall
    /// them. A non-owner's lifecycle op must complete while the port is
    /// blocked by another owner. Before the fix only UnblockProcess was exempt
    /// from the block divert, so a non-owner SetEnable/GetEnable/Connect/
    /// Disconnect sat in `pending_while_blocked` until UnblockProcess (the
    /// `send_and_wait` here would hang).
    #[test]
    fn actor_nonowner_lifecycle_runs_while_blocked() {
        let tx = spawn_actor(TestDriver::new());

        // Block the port with owner token 42.
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(42);
        send_and_wait(&tx, RequestOp::BlockProcess { all_devices: true }, user).unwrap();

        // A non-owner (no block_token) SetEnable(false) must run immediately,
        // not divert — otherwise this call never returns.
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::SetEnable { yes: false }, user)
            .expect("non-owner SetEnable must run while the port is blocked");

        // And it took effect while still blocked: a non-owner GetEnable
        // reflects the disable.
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let r = send_and_wait(&tx, RequestOp::GetEnable, user)
            .expect("non-owner GetEnable must run while the port is blocked");
        assert_eq!(r.int_val, Some(0), "SetEnable(false) applied while blocked");

        // The owner can still unblock cleanly (token must match).
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(42);
        send_and_wait(&tx, RequestOp::UnblockProcess { all_devices: true }, user).unwrap();
    }

    #[test]
    fn actor_serialization() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingDriver {
            base: PortDriverBase,
            concurrent: Arc<AtomicUsize>,
            max_concurrent: Arc<AtomicUsize>,
        }

        impl PortDriver for CountingDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_write_int32(&mut self, _user: &mut AsynUser, value: i32) -> AsynResult<()> {
                let c = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = self.max_concurrent.fetch_max(c, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(1));
                self.concurrent.fetch_sub(1, Ordering::SeqCst);
                self.base_mut().params.set_int32(0, 0, value)?;
                Ok(())
            }
        }

        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let mut base = PortDriverBase::new("serial_actor", 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        let driver = CountingDriver {
            base,
            concurrent: concurrent.clone(),
            max_concurrent: max_concurrent.clone(),
        };

        let tx = spawn_actor(driver);

        let handles: Vec<_> = (0..20)
            .map(|i| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let user = AsynUser::new(0).with_timeout(Duration::from_secs(5));
                    send_and_wait(&tx, RequestOp::Int32Write { value: i }, user).unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn actor_flush() {
        let tx = spawn_actor(TestDriver::new());
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Flush, user);
        assert!(result.is_ok());
    }

    #[test]
    fn actor_uint32_digital() {
        let mut drv = TestDriver::new();
        drv.base
            .create_param("BITS", ParamType::UInt32Digital)
            .unwrap();
        let tx = spawn_actor(drv);

        let user = AsynUser::new(4).with_timeout(Duration::from_secs(1));
        send_and_wait(
            &tx,
            RequestOp::UInt32DigitalWrite {
                value: 0xFF,
                mask: 0x0F,
            },
            user,
        )
        .unwrap();

        let user = AsynUser::new(4).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::UInt32DigitalRead { mask: 0xFF }, user).unwrap();
        assert_eq!(result.uint_val, Some(0x0F));
    }

    #[test]
    fn actor_clean_shutdown() {
        let tx = spawn_actor(TestDriver::new());
        drop(tx); // Dropping all senders causes the actor to return
        std::thread::sleep(Duration::from_millis(10));
        // No hang, no panic
    }

    /// R9-57. Every layer a request passes through must be able to trace through
    /// its `asynUser`, as in C, where `pasynUser` reaches the port's trace config
    /// through its `pport`/`pdevice` linkage (`findTracePvt`,
    /// asynManager.c:546-551) — an interpose has no other handle on the port
    /// (`asynInterposeCom.c:237-239`). The actor is the single owner of that
    /// linkage: it stamps the request's user with the port it is about to run on.
    #[test]
    fn the_actor_connects_every_request_s_user_to_the_port() {
        use crate::manager::PortManager;
        use crate::trace::TraceMask;
        use std::sync::Mutex as StdMutex;

        static SEEN: StdMutex<Option<(String, bool)>> = StdMutex::new(None);

        struct TraceSpy(PortDriverBase);
        impl PortDriver for TraceSpy {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn io_write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                let t = user.trace.as_ref();
                *SEEN.lock().unwrap() = Some((
                    t.map(|t| t.port.to_string()).unwrap_or_default(),
                    t.is_some(),
                ));
                // And the trace actually resolves through it.
                user.print_io(TraceMask::IO_DRIVER, data, "write:", file!(), line!());
                Ok(data.len())
            }
        }

        let mgr = PortManager::new();
        mgr.register_port(TraceSpy(PortDriverBase::new(
            "trace_link",
            1,
            PortFlags::default(),
        )))
        .unwrap();
        let handle = mgr.find_port_handle("trace_link").unwrap();
        handle
            .submit_blocking(
                RequestOp::OctetWrite {
                    data: b"hi".to_vec(),
                },
                AsynUser::default(),
            )
            .unwrap();

        assert_eq!(
            SEEN.lock().unwrap().take(),
            Some(("trace_link".to_string(), true)),
            "the driver's user carries the port it is running on"
        );
    }
}
