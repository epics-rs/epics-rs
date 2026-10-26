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
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering as AtomicOrdering};
use std::time::{Instant, SystemTime};

use epics_libcom_rs::SharedArray;
use tokio::sync::{mpsc, oneshot};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interfaces::InterfaceType;
use crate::interpose::EomReason;
use crate::interrupt::InterruptValue;
use crate::param::ParamValue;
use crate::port::{PortDriver, QueueGate, QueuePriority};
use crate::request::{
    CancelToken, CellQueue, Claim, OctetRead, PortCalls, RequestCell, RequestOp, RequestResult,
};
use crate::user::{AsynUser, ConnectCheck};

static ACTOR_SEQ: AtomicU64 = AtomicU64::new(0);

/// The next request's place in its priority band's FIFO — stamped where the
/// request is queued, whichever way it reaches the actor.
pub(crate) fn next_seq() -> u64 {
    ACTOR_SEQ.fetch_add(1, AtomicOrdering::Relaxed)
}
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

/// Which refusals C's `queueRequest` gate (asynManager.c:1543-1554) applies
/// to this request on a port that is (`multi_device`) or is not
/// `ASYN_MULTIDEVICE`. `None` — C never queues it, so no gate runs.
pub(crate) fn queue_gate(
    op: &RequestOp,
    user: &AsynUser,
    multi_device: bool,
) -> Option<ConnectCheck> {
    match PortActor::c_dispatch(op) {
        CDispatch::Direct => None,
        // The op fixes the priority at Connect (every C call site passes
        // `asynQueuePriorityConnect`), but the waiver is still the user's
        // (asynManager.c:1538-1540).
        CDispatch::ConnectQueue => Some(user.connect_check_at_connect_priority(multi_device)),
        CDispatch::Queued => Some(user.connect_check(multi_device)),
    }
}

/// C `queueRequest`'s refusals (asynManager.c:1538-1554), made before `op` is
/// queued. Its dequeue counterpart does not refuse: the port thread leaves a
/// request queued on a port that went down after it (:808-810, :858-863).
pub(crate) fn queue_request_gate(
    gate: &QueueGate,
    op: &RequestOp,
    user: &AsynUser,
    multi_device: bool,
) -> AsynResult<()> {
    match queue_gate(op, user, multi_device) {
        Some(connect) => gate.check(connect).map_err(AsynError::from),
        None => Ok(()),
    }
}

/// C's `synchronousLock` as `asynReport` sees it: held while the port thread
/// runs a callback, a `lockPort` span or a connect attempt (asynManager.c:776,
/// :830, :909, :1800), which the report's own thread try-locks (:1053). The
/// actor is the only holder; a [`crate::port_handle::PortHandle`] only looks.
#[derive(Clone, Default)]
pub(crate) struct SyncLock(Arc<AtomicU32>);

impl SyncLock {
    /// Whether C's `epicsMutexTryLock` would fail now.
    pub(crate) fn is_held(&self) -> bool {
        self.0.load(AtomicOrdering::Acquire) != 0
    }

    fn hold(&self) -> SyncLockHeld {
        self.0.fetch_add(1, AtomicOrdering::Release);
        SyncLockHeld(Arc::clone(&self.0))
    }
}

/// One hold of [`SyncLock`], released on every exit from the span.
struct SyncLockHeld(Arc<AtomicU32>);

impl Drop for SyncLockHeld {
    fn drop(&mut self) {
        self.0.fetch_sub(1, AtomicOrdering::Release);
    }
}

/// The driver's input EOS for [`crate::port_handle::PortHandle::get_input_eos`],
/// which a reader on the port thread (an interrupt callback inside the
/// driver's read) needs while the driver is borrowed: what `get_input_eos`
/// answers for each device, refusal included, since C's callers test it
/// (AsynDriverInterface.cc:450-457, :1166-1187). The actor republishes it
/// after every driver `set_input_eos` it makes ([`PortActor::set_input_eos`])
/// and every layer it pushes ([`PortActor::install_octet_interpose`]), and
/// nothing else writes it.
#[derive(Clone, Default)]
pub(crate) struct InputEos {
    multi_device: bool,
    by_key: Arc<parking_lot::RwLock<HashMap<i32, EosAnswer>>>,
}

/// One device's `getInputEos` answer and who gave it: C's EOS layer prints a
/// FLOW line on the asking user (asynInterposeEos.c:340-341), a driver's own
/// answer prints none (asynPortDriver.cpp:2782-2788).
#[derive(Clone)]
enum EosAnswer {
    Layer(Vec<u8>),
    Driver(Vec<u8>),
    Refused(AsynStatus, String),
}

impl InputEos {
    fn of(driver: &dyn PortDriver) -> Self {
        let eos = Self {
            multi_device: driver.base().flags.multi_device,
            by_key: Arc::default(),
        };
        eos.publish(driver, -1);
        eos
    }

    /// What `get_input_eos` answers `user`, a user linked to the port. An addr
    /// no device was published for resolves to the port, as C `findInterface`
    /// falls back to the port's interface (asynManager.c:1494-1502).
    pub(crate) fn get(&self, user: &AsynUser) -> AsynResult<Vec<u8>> {
        let key = crate::port::eos_device_key(self.multi_device, user.addr);
        let answer = {
            let by_key = self.by_key.read();
            by_key.get(&key).or_else(|| by_key.get(&-1)).cloned()
        };
        match answer {
            Some(EosAnswer::Layer(eos)) => {
                crate::interpose::eos::trace_get_eos(user, &eos);
                Ok(eos)
            }
            Some(EosAnswer::Driver(eos)) => Ok(eos),
            Some(EosAnswer::Refused(status, message)) => Err(AsynError::Status { status, message }),
            None => Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "getInputEos: no driver behind this port".into(),
            }),
        }
    }

    /// Every device's readback, not only `addr`'s: a driver may keep one
    /// terminator for all its addresses (Prologix's single `++eos`).
    fn publish(&self, driver: &dyn PortDriver, addr: i32) {
        let devices = if self.multi_device {
            0..driver.base().max_addr as i32
        } else {
            0..0
        };
        let known: Vec<i32> = self.by_key.read().keys().copied().collect();
        let readback: HashMap<_, _> = [-1, addr]
            .into_iter()
            .chain(devices)
            .chain(known)
            .map(|addr| {
                let user = AsynUser::default().with_addr(addr);
                let answer = match driver.get_input_eos(&user) {
                    Ok(eos) if driver.base().interpose_octet.input_eos(&user).is_some() => {
                        EosAnswer::Layer(eos)
                    }
                    Ok(eos) => EosAnswer::Driver(eos),
                    Err(e) => EosAnswer::Refused(e.status(), e.message()),
                };
                (crate::port::eos_device_key(self.multi_device, addr), answer)
            })
            .collect();
        *self.by_key.write() = readback;
    }
}

/// What a [`crate::port_handle::PortHandle`] takes from the actor behind its
/// channel: the actor's identity, for the re-entrancy guard, and the port's
/// [`QueueGate`], for C `queueRequest`'s refusals on the queueing thread.
#[derive(Clone)]
pub(crate) struct ActorLink {
    pub(crate) id: ActorId,
    /// The queue the actor drains [`RequestCell`]s from.
    pub(crate) cells: Arc<CellQueue>,
    pub(crate) gate: QueueGate,
    pub(crate) devices: crate::port::DeviceTable,
    pub(crate) sync_lock: SyncLock,
    pub(crate) input_eos: InputEos,
    pub(crate) lend: Arc<Lend>,
}

impl ActorLink {
    /// A link for a handle whose channel has no actor behind it (a test
    /// fixture): a fresh id no thread publishes, and an open gate.
    #[cfg(test)]
    pub(crate) fn unowned() -> Self {
        let cells = Arc::new(CellQueue::default());
        cells.close();
        Self {
            id: ActorId::new(),
            cells,
            gate: QueueGate::open(),
            devices: crate::port::DeviceTable::detached(),
            sync_lock: SyncLock::default(),
            input_eos: InputEos::default(),
            lend: Lend::new("unowned".into()),
        }
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

/// The port taken by a thread that is not its own, as C's callers take it:
/// `queueRequest` on a synchronous port runs `processUser` on the caller
/// under `synchronousLock` (asynManager.c:1579-1581), `lockPort` takes that
/// lock on the caller (:1800), a CANBLOCK port's `queueLockPort` leaves its
/// port thread waiting in the callback while the caller calls the driver
/// (:1858-1914), and the `pasynManager` direct calls and a driver's `lock()`
/// (asynPortDriver.cpp:3858) run on their caller. The actor reaches such a
/// request in its queue like any other, parks itself here and sends the
/// claimed request to its waiter, which holds it until it gives it back.
pub(crate) struct Lend {
    port: Arc<str>,
    slot: std::sync::Mutex<LendSlot>,
    returned: tokio::sync::Notify,
}

enum LendSlot {
    /// On the actor's own thread.
    Home,
    /// Parked for the thread the actor sent the claimed request to.
    Offered(Box<PortActor>),
    /// That thread holds it; `None` while one of its steps is inside.
    Held(Option<Box<PortActor>>),
    /// Given back, for the actor to resume with.
    Back(Box<PortActor>),
}

impl Lend {
    pub(crate) fn new(port: Arc<str>) -> Arc<Self> {
        Arc::new(Self {
            port,
            slot: std::sync::Mutex::new(LendSlot::Home),
            returned: tokio::sync::Notify::new(),
        })
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, LendSlot> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The actor's side: park for `handoff`'s thread and wait to be given back.
    async fn hand_off(&self, actor: Box<PortActor>, handoff: Handoff) -> Box<PortActor> {
        *self.slot() = LendSlot::Offered(actor);
        if handoff.reply.send(Ok(handoff.claimed)).is_err()
            && let LendSlot::Offered(actor) = std::mem::replace(&mut *self.slot(), LendSlot::Home)
        {
            return actor;
        }
        loop {
            let returned = self.returned.notified();
            let mut returned = std::pin::pin!(returned);
            returned.as_mut().enable();
            {
                let mut slot = self.slot();
                if matches!(*slot, LendSlot::Back(_)) {
                    let LendSlot::Back(actor) = std::mem::replace(&mut *slot, LendSlot::Home)
                    else {
                        unreachable!()
                    };
                    return actor;
                }
            }
            returned.await;
        }
    }

    /// The side of the thread the claimed request went to.
    pub(crate) fn take(&self) {
        let mut slot = self.slot();
        match std::mem::replace(&mut *slot, LendSlot::Home) {
            LendSlot::Offered(actor) => *slot = LendSlot::Held(Some(actor)),
            _ => unreachable!("the actor sends a lend its request only once it is parked"),
        }
    }

    pub(crate) fn give_back(&self) {
        {
            let mut slot = self.slot();
            match std::mem::replace(&mut *slot, LendSlot::Home) {
                LendSlot::Held(Some(mut actor)) => {
                    actor.dequeue_at_release();
                    *slot = LendSlot::Back(actor);
                }
                _ => unreachable!("only the holder gives the actor back, between its steps"),
            }
        }
        self.returned.notify_one();
    }

    /// One step of the holder on the actor. A step made from inside another
    /// — a driver calling back into its own port — finds it taken, and is
    /// refused as it is on the port's own thread.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut PortActor) -> R) -> AsynResult<R> {
        let actor = match &mut *self.slot() {
            LendSlot::Held(actor) => actor.take(),
            _ => None,
        }
        .ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!(
                "a request on port {} from inside that port's own driver call: a driver \
                 cannot make a blocking request to itself. Call the driver method directly \
                 instead of going back through the port.",
                self.port
            ),
        })?;
        /// Puts the actor back on every exit from the step, unwinding too.
        struct PutBack<'a>(&'a Lend, Option<Box<PortActor>>);
        impl Drop for PutBack<'_> {
            fn drop(&mut self) {
                if let LendSlot::Held(slot) = &mut *self.0.slot() {
                    *slot = self.1.take();
                }
            }
        }
        let mut back = PutBack(self, Some(actor));
        let actor = back.1.as_deref_mut().expect("set above");
        Ok(f(actor))
    }

    /// The driver as the holder of the port reaches it.
    pub(crate) fn with_port_driver<R>(
        &self,
        f: impl FnOnce(&mut dyn PortDriver) -> R,
    ) -> AsynResult<R> {
        self.with(|actor| f(actor.driver.as_mut()))
    }

    /// The driver as the holder of a `lock()` reaches it.
    pub(crate) fn with_driver<D: PortDriver, R>(
        &self,
        f: impl FnOnce(&mut D) -> R,
    ) -> AsynResult<Option<R>> {
        self.with(|actor| {
            let driver: &mut dyn std::any::Any = actor.driver.as_mut();
            driver.downcast_mut::<D>().map(f)
        })
    }
}

/// A request the actor claimed for the thread it lends itself to, or one a
/// thread already holding the port runs in place — C's recursive
/// `synchronousLock` (asynManager.c:1579-1581) and `queueLockPort`'s count
/// (:1872-1876).
pub(crate) struct Claimed {
    op: RequestOp,
    user: AsynUser,
    priority: QueuePriority,
}

impl Claimed {
    pub(crate) fn in_place(op: RequestOp, user: AsynUser) -> Self {
        Self {
            priority: request_priority(&op, &user),
            op,
            user,
        }
    }
}

/// What [`PortActor::process_one`] leaves the loop to do with a lend request.
struct Handoff {
    claimed: Claimed,
    reply: oneshot::Sender<AsynResult<Claimed>>,
}

/// The queue band of a request: C derives it from the KIND of request, never
/// from a field a caller has to remember to set. `asynRecord` picks
/// `asynQueuePriorityConnect` from `pmsg->callbackType == callbackConnect`
/// (asynRecord.c:561-565), and `asynCommonSyncIO`'s connect/disconnect pass
/// the constant literally (asynCommonSyncIO.c:142, :154).
/// `CDispatch::ConnectQueue` is that kind, and it already fixes the connected
/// waiver ([`queue_gate`]) — the band is taken from it for the same
/// reason, so a connect cannot sort below a sustained High scan stream because
/// its caller built a `#[default]` Medium user.
///
/// Every other class keeps the user's own band: C's HOSTINFO put is a
/// `callbackSetOption` that asynRecord *escalates* to Connect by hand
/// (:566-569), and that escalation still arrives through
/// [`AsynUser::queue_even_if_not_connected`].
fn request_priority(op: &RequestOp, user: &AsynUser) -> QueuePriority {
    match PortActor::c_dispatch(op) {
        CDispatch::ConnectQueue => QueuePriority::Connect,
        CDispatch::Direct | CDispatch::Queued => user.priority,
    }
}

/// Whether C runs `op` on its caller's thread rather than queueing it for
/// the port thread: one of `pasynManager`'s direct calls, `lockPort`, or a
/// driver's `lock()` ([`CDispatch::Direct`]).
pub(crate) fn runs_on_caller(op: &RequestOp) -> bool {
    PortActor::c_dispatch(op) == CDispatch::Direct
}

/// The actor as the code running a request reaches it: owned by its own
/// thread, or lent ([`Lend`]) and borrowed for each step, so that a request
/// the steps make back into their own port finds it free.
enum Access<'a> {
    Owned(&'a mut PortActor),
    Lent(&'a Lend),
}

impl Access<'_> {
    fn with<R>(&mut self, f: impl FnOnce(&mut PortActor) -> R) -> AsynResult<R> {
        match self {
            Access::Owned(actor) => Ok(f(actor)),
            Access::Lent(lend) => lend.with(f),
        }
    }
}

/// A request run by the thread the actor is lent to, or in place by one
/// already holding it: on a synchronous port, C `queueRequest`'s gate
/// (asynManager.c:1555-1577) first, on the caller, as C runs it.
pub(crate) fn run_lent(lend: &Lend, claimed: Claimed) -> AsynResult<RequestResult> {
    let Claimed { op, user, priority } = claimed;
    let mut access = Access::Lent(lend);
    access.with(|actor| actor.lent_gate(&op, &user, priority))??;
    run_claimed(&mut access, op, user)
}

/// A claimed request from the user's linkage to its block settlement — the
/// part of [`PortActor::process_one`] that runs on whichever thread holds the
/// actor.
fn run_claimed(
    access: &mut Access<'_>,
    op: RequestOp,
    mut user: AsynUser,
) -> AsynResult<RequestResult> {
    let callback = PortActor::c_dispatch(&op) != CDispatch::Direct;
    // C runs a callback and a `lockPort` span under `synchronousLock`
    // (asynManager.c:830, :909, :1581, :1800); a direct manager call
    // takes no port lock.
    let held = access.with(|actor| {
        // A request ran, so the queue state may have moved (it could have
        // enabled a device): C's `queueStateChange` rescan (:870). A *parked*
        // request does not set this — that is what keeps the loop from
        // spinning on a device that is still disabled.
        actor.rescan_parked = true;
        // Connect the user to the port it is about to run on, so every layer
        // the request passes through can trace through it — C's `pasynUser →
        // pport/pdevice` linkage, which `asynPrint`/`asynPrintIO` resolve the
        // trace config from (`findTracePvt`, asynManager.c:548-553). The actor
        // is the single owner of the linkage: an interpose has no other handle
        // on the port, and a user that never reached a port traces nothing.
        actor.link_user(&mut user);
        (callback || matches!(op, RequestOp::LockPort(_))).then(|| actor.sync_lock.hold())
    })?;
    let result = match op {
        RequestOp::CallParamCallbacks {
            addr,
            updates,
            time_stamp,
            gate: Some(gate),
        } => access.with(|actor| actor.post_section(addr, updates, time_stamp, gate))?,
        op => dispatch(access, &mut user, &op),
    };
    drop(held);
    if callback {
        access.with(|actor| actor.settle_block(&user))?;
    }
    result
}

/// One request on the port: the ops that make calls of their own (a cell's
/// body, a `lockPort` span) make each as a step, every other op is one step.
fn dispatch(
    access: &mut Access<'_>,
    user: &mut AsynUser,
    op: &RequestOp,
) -> AsynResult<RequestResult> {
    let token = user.block_token;
    match op {
        RequestOp::Cell(cell) => {
            cell.run(&mut ActorCalls(access, token));
            Ok(RequestResult::write_ok())
        }
        RequestOp::CellInline(cell) => {
            cell.run_inline(&mut ActorCalls(access, token));
            Ok(RequestResult::write_ok())
        }
        RequestOp::LockPort(call) => lock_port(access, user, call),
        RequestOp::WithDriverAfterQueue(_) => access.with(|actor| actor.dispatch_io(user, op))?,
        _ => access.with(|actor| actor.dispatch_io(user, op))?,
    }
}

/// C `lockPort` … `unlockPort` around the caller's calls (asynManager.c:1778-1830).
fn lock_port(
    access: &mut Access<'_>,
    user: &mut AsynUser,
    call: &crate::request::PortLockCall,
) -> AsynResult<RequestResult> {
    access.with(|actor| {
        let port = &actor.driver.base().port_name;
        // C `lockPort` traces as its caller before it looks (:1789), and
        // refuses only a destroyed port (:1790-1795).
        user.print(
            crate::trace::TraceMask::FLOW,
            file!(),
            line!(),
            format_args!("{port} lockPort\n"),
        );
        if actor.driver.base().is_defunct() {
            return Err(AsynError::Status {
                status: AsynStatus::Disabled,
                message: "asynManager:lockPort: port has been shut down".into(),
            });
        }
        Ok(())
    })??;
    let result = match call.take() {
        Some(f) => {
            f(&mut ActorCalls(access, user.block_token));
            Ok(RequestResult::write_ok())
        }
        None => Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "port lock call carries no closure".into(),
        }),
    };
    access.with(|actor| {
        user.print(
            crate::trace::TraceMask::FLOW,
            file!(),
            line!(),
            format_args!("{} unlockPort\n", actor.driver.base().port_name),
        )
    })?;
    result
}

/// Message sent from [`super::port_handle::PortHandle`] to the actor.
pub(crate) struct ActorMessage {
    pub op: RequestOp,
    pub user: AsynUser,
    /// How a waiter takes the request off the queue; a request nobody waits
    /// on ([`Reply::Nobody`]) has none and always runs.
    pub cancel: Option<CancelToken>,
    pub reply: Reply,
    pub seq: u64,
    pub priority: QueuePriority,
    /// Where the message sorts ahead of `seq`. Set only by
    /// [`PortActor::enqueue_message`], which knows the port.
    rank: Rank,
}

/// Whom a request answers.
pub(crate) enum Reply {
    /// A [`RequestOp::Cell`], whose body and `unrun` are its answer.
    Cell,
    /// A waiter for the result.
    Result(oneshot::Sender<AsynResult<RequestResult>>),
    /// A waiter that runs the claimed request itself, with the actor lent
    /// to it ([`Lend`]).
    Lend(oneshot::Sender<AsynResult<Claimed>>),
    /// Nobody: an enqueue-only request.
    Nobody,
}

impl ActorMessage {
    pub fn new(
        op: RequestOp,
        user: AsynUser,
        cancel: CancelToken,
        reply: oneshot::Sender<AsynResult<RequestResult>>,
    ) -> Self {
        Self::answering(op, user, Some(cancel), Reply::Result(reply))
    }

    /// A request whose waiter runs it with the actor lent ([`Lend`]).
    pub(crate) fn lending(
        op: RequestOp,
        user: AsynUser,
        cancel: CancelToken,
        reply: oneshot::Sender<AsynResult<Claimed>>,
    ) -> Self {
        Self::answering(op, user, Some(cancel), Reply::Lend(reply))
    }

    /// A request nobody waits on: nothing can cancel it or time it out, so
    /// it carries neither a reply channel nor a token, and allocates nothing.
    pub(crate) fn unanswered(op: RequestOp, user: AsynUser) -> Self {
        Self::answering(op, user, None, Reply::Nobody)
    }

    fn answering(op: RequestOp, user: AsynUser, cancel: Option<CancelToken>, reply: Reply) -> Self {
        let priority = request_priority(&op, &user);
        Self {
            op,
            user,
            cancel,
            reply,
            seq: next_seq(),
            priority,
            rank: Rank::Arrival,
        }
    }

    /// A queued cell as the actor's queue holds it: its order and band were
    /// fixed by [`RequestCell::queue`], and `seq` names the request this entry
    /// is for.
    fn from_cell(cell: Arc<RequestCell>, seq: u64) -> Self {
        let request = cell.request();
        let mut user = AsynUser::new(request.reason)
            .with_addr(request.addr)
            .with_priority(request.priority);
        user.block_token = Some(cell.block_token());
        Self {
            user,
            cancel: Some(cell.token().clone()),
            reply: Reply::Cell,
            seq,
            priority: request.priority,
            rank: Rank::Arrival,
            op: RequestOp::Cell(cell),
        }
    }
}

impl ActorMessage {
    /// The request leaves the queue without running: its waiter gets `e`, or
    /// a [`RequestOp::Cell`] runs its `unrun` with it — C's `timeoutUser` —
    /// since a cell has no waiter. The one exit every never-ran path takes.
    fn refuse(self, e: AsynError) {
        match (self.op, self.reply) {
            (RequestOp::Cell(cell), _) => cell.leave(self.seq, e),
            (_, Reply::Result(reply)) => {
                let _ = reply.send(Err(e));
            }
            (_, Reply::Lend(reply)) => {
                let _ = reply.send(Err(e));
            }
            (_, Reply::Cell | Reply::Nobody) => {}
        }
    }

    /// The device-not-connected exit, which C sends to the request's
    /// `timeoutUser` (asynManager.c:877-888).
    fn time_out(self, port: String) {
        if !matches!(self.op, RequestOp::Cell(_))
            && let Some(cancel) = &self.cancel
        {
            cancel.time_out_if_queued();
        }
        self.refuse(AsynError::QueueTimeout { port });
    }

    /// C `timeoutUser != 0` (asynManager.c:886): a waiter's reply channel
    /// always is one, and a request nobody waits on has none.
    fn timeout_user(&self) -> bool {
        match &self.op {
            RequestOp::Cell(cell) => cell.timeout_user(),
            _ if matches!(self.reply, Reply::Nobody) => false,
            RequestOp::WithDriverAfterQueue(_) => true,
            _ => true,
        }
    }

    /// The dequeue (asynManager.c:894-895).
    fn claim(&self) -> Claim {
        match &self.op {
            RequestOp::Cell(cell) => cell.claim(self.seq),
            RequestOp::WithDriverAfterQueue(_) if self.begin_running() => Claim::Run,
            RequestOp::WithDriverAfterQueue(_) => Claim::Left,
            _ if self.begin_running() => Claim::Run,
            _ => Claim::Left,
        }
    }

    /// It left the queue on another clock and was reported there — C's timer
    /// unlinked it (asynManager.c:664-674), so the port thread never sees it.
    fn is_gone(&self) -> bool {
        match &self.op {
            RequestOp::Cell(cell) => cell.is_gone(self.seq),
            RequestOp::WithDriverAfterQueue(_) => self.is_timed_out(),
            _ => self.is_timed_out(),
        }
    }

    /// `Queued -> Running` on the token; without one nothing could have
    /// taken the request off the queue.
    fn begin_running(&self) -> bool {
        self.cancel.as_ref().is_none_or(CancelToken::begin_running)
    }

    fn is_timed_out(&self) -> bool {
        self.cancel.as_ref().is_some_and(CancelToken::is_timed_out)
    }
}

/// Sleep until `deadline`, or never if there is none.
///
/// A disarmed connect timer must not make the `select!` arm ready — otherwise
/// the actor would spin. `pending()` is the "this arm never completes" form,
/// leaving the other arms (request / shutdown) to drive the loop exactly as
/// they did before the timer existed.
async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending().await,
    }
}

/// The runtime's shutdown channel as the actor reads it. Only a `()` sent on
/// it stops the port; its closing (every `PortRuntimeHandle` dropped) only
/// means no stop can come.
enum Stop {
    Listening(mpsc::Receiver<()>),
    Sent,
    Unreachable,
}

impl Stop {
    /// Whether a stop was sent, so [`PortActor::run_with_shutdown`] can leave.
    fn sent(&mut self) -> bool {
        if let Stop::Listening(rx) = self {
            match rx.try_recv() {
                Ok(()) => *self = Stop::Sent,
                Err(mpsc::error::TryRecvError::Disconnected) => *self = Stop::Unreachable,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
        }
        matches!(self, Stop::Sent)
    }

    /// Ready once a stop is sent; never, once none can be.
    async fn arrived(&mut self) {
        if let Stop::Listening(rx) = self {
            let next = match rx.recv().await {
                Some(()) => Stop::Sent,
                None => Stop::Unreachable,
            };
            *self = next;
        }
        if !matches!(self, Stop::Sent) {
            std::future::pending::<()>().await;
        }
    }
}

/// Marks a claimed request's cancel token `Done` when execution leaves the
/// dispatch scope, on every exit path (reply, early return, panic). This is the
/// C `callbackActive -> idle` transition: once the port thread finishes the
/// callback, a pending `cancelRequest` reports `wasQueued==0` (asynManager.c:1647-1661).
struct FinishGuard(Option<CancelToken>);

impl Drop for FinishGuard {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            cancel.finish();
        }
    }
}

// Heap ordering: higher priority first, then lower seq (strict FIFO
// within a priority). C asynManager queues each priority as a FIFO list
// (queueRequest ellAdd to the queueList[priority] tail; portThread walks
// ellFirst->ellNext, asynManager.c:1615/871-900) — a request's
// timeout never reorders it relative to same-priority peers. An earlier
// deadline-based tiebreaker let a later-submitted request with a shorter
// timeout jump ahead of an earlier one, violating that FIFO.
/// The heap key in front of `seq`.
///
/// A port without `ASYN_CANBLOCK` has no queue in C: `queueRequest` runs each
/// request at once in the caller's thread under `synchronousLock`
/// (asynManager.c:1555-1584), so arrival order is the only order — every
/// message ranks [`Rank::Arrival`]. A CANBLOCK port drains its bands
/// Connect first, and a [`CDispatch::Direct`] call (`lockPort` and the
/// `pasynManager` getters) waits only for the request in progress
/// (:1778-1808), so it ranks above every band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Arrival,
    Band(QueuePriority),
    Direct,
}

impl Eq for ActorMessage {}
impl PartialEq for ActorMessage {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}
impl Ord for ActorMessage {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank
            .cmp(&other.rank)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}
impl PartialOrd for ActorMessage {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// How C performs an operation: the one classification every gate in the actor
/// derives from ([`queue_gate`], [`PortActor::is_lifecycle_op`]).
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
    /// (asynManager.c:2224-2251), `autoConnectAsyn` (:2312-2329),
    /// `isConnected`/`isEnabled`/`isAutoConnect` (:2331-2373),
    /// `blockProcessCallback` (:1694-1725), `shutdownPort`, `interposeInterface`.
    ///
    /// The gate must not run on them: a port disabled with `asynEnable(port,0)`
    /// could never be re-enabled if the enable itself had to pass the enabled
    /// check.
    Direct,
    /// C queues it at `asynQueuePriorityConnect`, and the port thread drains that
    /// queue *before* it consults any connected flag (`portThread`,
    /// asynManager.c:813-857) — so the *connected* refusal is waived. `enabled`
    /// still binds: the same thread refuses to run anything at all on a disabled
    /// port (:808-811), which is why a `CNCT=1` put cannot open the device
    /// connection of a port disabled precisely to keep the IOC off the hardware
    /// (R12-48).
    ///
    /// The block holder cannot divert these either: the Connect queue is drained
    /// ahead of the lower-priority loop it gates, with no block check.
    ConnectQueue,
    /// C hands it to `queueRequest` (asynManager.c:1517-1554), so C's gate runs.
    /// Its two refusals are **independent**: `!pport->dpc.enabled → asynDisabled`
    /// (:1543-1548) is unconditional, and only `checkPortConnect` is conditioned
    /// on the priority and reason (:1538-1540). The request's own user
    /// ([`AsynUser::connect_check`]) selects the connected waiver;
    /// [`crate::port::PortDriverBase::check_queue`] enforces the split, so nothing can waive
    /// the enabled half.
    ///
    /// This is also the only class the block holder can stall: C's
    /// `pblockProcessHolder` gates exactly the `processUser` dispatch in the port
    /// thread's lower-priority loop (:889-897).
    Queued,
}

/// Which `dpCommon` a request resolves to — C `findDpCommon`
/// (asynManager.c:538-545). `locateDevice` (:576) hands back NULL unless the
/// port carries `ASYN_MULTIDEVICE`, so an addr names a device only there;
/// everywhere else the port's own `dpCommon` is the one and only slot.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum DpKey {
    Port,
    Device(i32),
}

impl DpKey {
    pub(crate) fn of(multi_device: bool, addr: i32) -> Self {
        match crate::port::dp_common_key(multi_device, addr) {
            Some(addr) => DpKey::Device(addr),
            None => DpKey::Port,
        }
    }
}

/// C's block state, all of it under `asynManagerLock`: each user's
/// `blockPortCount` / `blockDeviceCount` and the two `pblockProcessHolder`
/// fields the drain gate reads (asynManager.c:889-892).
///
/// `port` is `pport->pblockProcessHolder`; `dp_commons` holds
/// `pdpCommon->pblockProcessHolder` per `dpCommon`, including the port's own —
/// C really does keep a *third* distinct field there, `pport->dpc`, so a
/// device-scoped block taken on a single-device port is not the same thing as
/// a port-wide one even though both end up gating every request.
///
/// A count changes on the caller's thread ([`RequestCell::block_process_callback`],
/// or the actor for a [`RequestOp::BlockProcess`]); a holder is taken only by the
/// actor, after the counting user's callback returns ([`Self::settle`], :931-934),
/// and released by whichever thread drops the count to zero ([`Self::unblock`],
/// :1773-1786). The maps keep their capacity, so a block taken again allocates
/// nothing.
#[derive(Default)]
pub(crate) struct BlockBook {
    /// `(blockPortCount, blockDeviceCount)` per block identity.
    counts: HashMap<u64, (u32, u32)>,
    port: Option<u64>,
    dp_commons: HashMap<DpKey, u64>,
    /// A holder was released since the actor last looked: C's
    /// `epicsEventSignal(pport->notifyPortThread)` (:1786).
    released: bool,
}

impl BlockBook {
    fn any_held(&self) -> bool {
        self.port.is_some() || !self.dp_commons.is_empty()
    }

    /// C's eligibility test, verbatim: **both** holders must be NULL or this
    /// user (asynManager.c:889-892). One holder alone is never enough to admit
    /// a request, and never enough to refuse one.
    fn admits(&self, token: Option<u64>, dp: DpKey) -> bool {
        let free = |h: Option<&u64>| h.is_none_or(|owner| token == Some(*owner));
        free(self.port.as_ref()) && free(self.dp_commons.get(&dp))
    }

    /// The count `all_devices` names for `token`.
    pub(crate) fn count(&self, token: u64, all_devices: bool) -> u32 {
        let (port, device) = self.counts.get(&token).copied().unwrap_or_default();
        if all_devices { port } else { device }
    }

    /// C `blockProcessCallback`'s `blockPortCount++` / `blockDeviceCount++`
    /// (:1719-1722).
    pub(crate) fn block(&mut self, token: u64, all_devices: bool) {
        let (port, device) = self.counts.entry(token).or_default();
        if all_devices {
            *port += 1;
        } else {
            *device += 1;
        }
    }

    /// C `unblockProcessCallback` from its count check on (:1741-1786): the
    /// count drops, and at zero the holder that is this user is released.
    /// `Ok(true)` is C's `wasOwner`, the actor's wake.
    pub(crate) fn unblock(
        &mut self,
        token: u64,
        dp: DpKey,
        all_devices: bool,
    ) -> Result<bool, &'static str> {
        let Some(counts) = self.counts.get_mut(&token) else {
            return Err("asynManager::unblockProcessCallback but not locked");
        };
        let count = if all_devices {
            &mut counts.0
        } else {
            &mut counts.1
        };
        if *count == 0 {
            return Err("asynManager::unblockProcessCallback but not locked");
        }
        *count -= 1;
        let released = *count == 0;
        if *counts == (0, 0) {
            self.counts.remove(&token);
        }
        let was_owner = released
            && if all_devices {
                self.port.take_if(|owner| *owner == token).is_some()
            } else if self.dp_commons.get(&dp) == Some(&token) {
                self.dp_commons.remove(&dp);
                true
            } else {
                false
            };
        self.released |= was_owner;
        Ok(was_owner)
    }

    /// C `portThread` after a callback (:931-934): a user still counting a
    /// block becomes its holder. True when a holder was newly taken.
    fn settle(&mut self, token: u64, dp: DpKey) -> bool {
        let Some(&(port, device)) = self.counts.get(&token) else {
            return false;
        };
        let mut taken = false;
        if port > 0 && self.port != Some(token) {
            self.port = Some(token);
            taken = true;
        }
        if device > 0 && self.dp_commons.get(&dp) != Some(&token) {
            self.dp_commons.insert(dp, token);
            taken = true;
        }
        taken
    }

    /// Everything `token` counts or holds, at once: the end of a block
    /// identity whose owner is gone. True when it held a holder.
    pub(crate) fn release(&mut self, token: u64) -> bool {
        self.counts.remove(&token);
        let mut was_owner = self.port.take_if(|owner| *owner == token).is_some();
        let held = self.dp_commons.len();
        self.dp_commons.retain(|_, owner| *owner != token);
        was_owner |= self.dp_commons.len() != held;
        self.released |= was_owner;
        was_owner
    }

    fn take_released(&mut self) -> bool {
        std::mem::take(&mut self.released)
    }

    fn summary(&self) -> u8 {
        let bit = |set: bool, bit: u8| if set { bit } else { 0 };
        bit(!self.counts.is_empty(), Blocks::COUNTED)
            | bit(self.any_held(), Blocks::HELD)
            | bit(self.released, Blocks::RELEASED)
    }
}

/// The [`BlockBook`] under its lock, with a summary of it the actor reads
/// without the lock on every request. Only [`BlocksGuard`]'s drop writes the
/// summary, under the lock, so it is the book's state as of the last release.
/// A stale `HELD` or `COUNTED` costs the actor one lock; a stale clear one
/// cannot hide a holder, because only the actor takes holders.
#[derive(Default)]
pub(crate) struct Blocks {
    book: parking_lot::Mutex<BlockBook>,
    summary: std::sync::atomic::AtomicU8,
}

pub(crate) struct BlocksGuard<'a> {
    book: parking_lot::MutexGuard<'a, BlockBook>,
    summary: &'a std::sync::atomic::AtomicU8,
}

impl std::ops::Deref for BlocksGuard<'_> {
    type Target = BlockBook;
    fn deref(&self) -> &BlockBook {
        &self.book
    }
}

impl std::ops::DerefMut for BlocksGuard<'_> {
    fn deref_mut(&mut self) -> &mut BlockBook {
        &mut self.book
    }
}

impl Drop for BlocksGuard<'_> {
    fn drop(&mut self) {
        self.summary
            .store(self.book.summary(), std::sync::atomic::Ordering::Release);
    }
}

impl Blocks {
    const COUNTED: u8 = 1;
    const HELD: u8 = 2;
    const RELEASED: u8 = 4;

    pub(crate) fn lock(&self) -> BlocksGuard<'_> {
        BlocksGuard {
            book: self.book.lock(),
            summary: &self.summary,
        }
    }

    #[inline]
    fn any(&self, bit: u8) -> bool {
        self.summary.load(std::sync::atomic::Ordering::Acquire) & bit != 0
    }

    /// Some holder may be set: [`BlockBook::admits`] must be asked.
    #[inline]
    fn held(&self) -> bool {
        self.any(Self::HELD)
    }

    /// Some block may be counted: [`BlockBook::settle`] must be asked.
    #[inline]
    fn counted(&self) -> bool {
        self.any(Self::COUNTED)
    }

    /// A holder may have been released: [`BlockBook::take_released`] must
    /// be asked.
    #[inline]
    fn released(&self) -> bool {
        self.any(Self::RELEASED)
    }
}

/// The actor that exclusively owns a port driver instance.
pub(crate) struct PortActor {
    id: ActorId,
    driver: Box<dyn PortDriver>,
    /// C's port queue, as unbounded as its `ellAdd` list (asynManager.c:1615):
    /// the only refusal a sender can meet is the actor being gone.
    rx: mpsc::UnboundedReceiver<ActorMessage>,
    /// The other way a request reaches this actor: a [`RequestCell`] queued
    /// with no message, so queueing one allocates nothing.
    cells: Arc<CellQueue>,
    heap: BinaryHeap<ActorMessage>,
    /// The request taken off the queue at a lend's give back, run next
    /// ([`Self::dequeue_at_release`]).
    dequeued: Option<ActorMessage>,
    /// The requests a block holder ([`BlockBook`], in the cell queue) holds
    /// back. devGpib takes the device form of a block for an SRQ transaction
    /// (devSupportGpib.c:1216), so a port-wide-only model stalls every other
    /// GPIB address on the port for the duration.
    pending_while_blocked: Vec<ActorMessage>,
    /// Requests whose *device* is disabled. C's `portThread` leaves them in the
    /// queue and skips them on every pass (`if(!pdpCommon->enabled) continue;`,
    /// asynManager.c:877) — they are still queued, so they still run when the
    /// device is re-enabled, and still time out on their own queue timer. This
    /// is that "still queued, not yet eligible" half of C's queue list; the heap
    /// is the eligible half (R15-47).
    parked: Vec<ActorMessage>,
    /// C `pport->queueStateChange` (asynManager.c:870, 1617): something happened
    /// that could make a parked request eligible, so the queue must be rescanned.
    /// Set by an arriving request and by a request that ran (either can enable a
    /// device); *not* by a re-park, which is what keeps the loop from spinning on
    /// a request whose device is still disabled.
    rescan_parked: bool,
    /// C `pport->previousConnectStatus` (asynManager.c:210), reduced to the
    /// two outcomes an attempt here can have: whether the last
    /// [`Self::report_connect_attempt`] reported a failure. Starts as a
    /// success (:2070).
    connect_failure_reported: bool,
    /// C `pport->notifyPortThread` (asynManager.c:217) — the binary event C's
    /// `portThread` waits on. It is signalled from exactly five places
    /// (`queueRequest` :1628, `announceExceptionOccurred` :638,
    /// `queueTimeoutCallback` :702, `cancelRequest` :1690,
    /// `unblockProcessCallback` :1774) and everything the woken thread does —
    /// draining the queues, and the port-level auto-connect at :858-863 — is
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
    /// `cancelRequest` (:1690), is on the *caller's* thread and pokes the queue
    /// (`CellQueue::poke`), and [`Self::drop_left`], dropping the cancelled
    /// entry the actor still held, sets it.
    woken: bool,
    /// The port's announced-exception count as of the last
    /// [`Self::take_port_thread_wake`] — the memory that turns a monotonic
    /// counter back into C's edge-triggered signal.
    seen_exceptions: u64,
    /// The announced-exception count as of the last parked rescan: a port that
    /// came back or was re-enabled is an exception (asynManager.c:2249), and C's
    /// port thread rescans its whole queue on every wake.
    rescanned_exceptions: u64,
    sync_lock: SyncLock,
    input_eos: InputEos,
    lend: Arc<Lend>,
}

/// A cell still queued when the actor goes runs its body as refused: a cell
/// outlives the queue, so the actor that held it settles it.
impl Drop for PortActor {
    fn drop(&mut self) {
        self.cells.close();
        let queued = std::mem::take(&mut self.heap)
            .into_iter()
            .chain(self.dequeued.take())
            .chain(std::mem::take(&mut self.parked))
            .chain(std::mem::take(&mut self.pending_while_blocked));
        for msg in queued {
            if let RequestOp::Cell(_) = msg.op {
                msg.refuse(AsynError::QueueRefused {
                    status: AsynStatus::Error,
                    message: "port shut down before the request ran".into(),
                });
            }
        }
    }
}

impl PortActor {
    pub fn new(mut driver: Box<dyn PortDriver>, rx: mpsc::UnboundedReceiver<ActorMessage>) -> Self {
        // C `registerInterface(asynCommonType)` calls `portConnectTimerCallback`
        // (asynManager.c:2133-2135), queueing the port's connect at Connect
        // priority before any request can exist (:3283-3297). Every port starts
        // down (:523), so this is how one comes up; the actor runs it before
        // its first message.
        if driver.base().is_auto_connect() && !driver.base().is_connected() {
            driver.base_mut().connect_retry_at = Some(Instant::now());
        }
        // Whatever the driver announced before the actor existed (registration's
        // own connect edge, say) is not a signal *this* thread ever waited on —
        // C's port thread is started with the event unsignalled.
        let seen_exceptions = driver.base().exceptions_announced();
        let flags = driver.base().flags;
        let cells = Arc::new(CellQueue::for_port(flags.can_block, flags.multi_device));
        let input_eos = InputEos::of(&*driver);
        let lend = Lend::new(Arc::clone(&driver.base().port_name));
        Self {
            id: ActorId::new(),
            driver,
            rx,
            cells,
            heap: BinaryHeap::new(),
            dequeued: None,
            pending_while_blocked: Vec::new(),
            parked: Vec::new(),
            rescan_parked: false,
            connect_failure_reported: false,
            woken: false,
            seen_exceptions,
            rescanned_exceptions: seen_exceptions,
            sync_lock: SyncLock::default(),
            input_eos,
            lend,
        }
    }

    /// This actor's [`ActorLink`]. Hand it to every [`PortHandle`] built
    /// against this actor's channel.
    ///
    /// [`PortHandle`]: crate::port_handle::PortHandle
    pub(crate) fn link(&self) -> ActorLink {
        ActorLink {
            id: self.id,
            cells: Arc::clone(&self.cells),
            gate: self.driver.base().queue_gate(),
            devices: self.driver.base().device_table(),
            sync_lock: self.sync_lock.clone(),
            input_eos: self.input_eos.clone(),
            lend: Arc::clone(&self.lend),
        }
    }

    /// The driver's `setInputEos`, and with [`Self::install_octet_interpose`]
    /// the one writer of [`InputEos`]: its readers see the driver's terminator
    /// as of the last set, refused or not.
    fn set_input_eos(&mut self, user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
        let set = self.driver.set_input_eos(user, eos);
        self.input_eos.publish(&*self.driver, user.addr);
        set
    }

    /// A layer pushed onto `addr`'s octet chain, which can change what that
    /// device's `getInputEos` answers: C's EOS layer answers it itself
    /// (asynInterposeEos.c:314-344) where the driver below may refuse.
    fn install_octet_interpose(
        &mut self,
        addr: i32,
        layer: Box<dyn crate::interpose::OctetInterpose>,
    ) {
        self.driver
            .base_mut()
            .install_octet_interpose_addr(addr, layer);
        self.input_eos.publish(&*self.driver, addr);
    }

    /// A fixture's registration: the connect `new` armed runs now, as C's
    /// `registerPort` returns only after `waitConnect` (asynManager.c:2137), so
    /// no request is submitted ahead of it.
    #[cfg(test)]
    pub(crate) fn register(&mut self) {
        self.registration_pass();
    }

    /// The port thread's first pass: the connect registration queued (`new`),
    /// then the post-drain `autoConnectDevice(pport,0)` the same wake reaches
    /// (asynManager.c:858-863).
    fn registration_pass(&mut self) {
        self.service_connect_timer();
        self.port_thread_auto_connect();
    }

    /// [`Self::register`], then the test loop to spawn.
    #[cfg(test)]
    pub fn registered(mut self) -> impl FnOnce() + Send {
        self.register();
        move || self.run()
    }

    /// Run the actor loop. Returns when the channel is closed (all senders dropped).
    /// Calls `shutdown()` on the driver before returning.
    #[cfg(test)]
    fn run(self) {
        let _scope = ActorScope::enter(self.id);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let lend = Arc::clone(&self.lend);
        let mut actor = Box::new(self);
        let mut actor = rt.block_on(async move {
            loop {
                // Drain all pending messages into the heap
                actor.drain_channel();
                actor.rescan_parked();
                actor.drop_left();

                if actor.idle() {
                    // No work — wait for a message or a queued cell.
                    let cells = Arc::clone(&actor.cells);
                    tokio::select! {
                        msg = actor.rx.recv() => match msg {
                            Some(msg) => actor.enqueue_message(msg),
                            None => break,
                        },
                        _ = cells.pushed() => {}
                    }
                    // Drain any more that arrived
                    actor.drain_channel();
                    actor.rescan_parked();
                    actor.drop_left();
                }

                // Process one eligible request from the heap
                if let Some(handoff) = actor.process_one() {
                    actor = lend.hand_off(actor, handoff).await;
                }
                actor.port_thread_auto_connect();
            }
            actor
        });
        let _ = actor.driver.shutdown();
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
    ///
    /// The port is defunct before the stop is sent (see
    /// `runtime::port::StopRequest`), so no request or connect the actor
    /// reaches after that gets past its gate; the `stop.sent()` checks below
    /// only make the actor leave promptly, whatever the traffic.
    pub fn run_with_shutdown(self, shutdown_rx: mpsc::Receiver<()>) {
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
        let lend = Arc::clone(&self.lend);
        let mut actor = Box::new(self);
        let mut actor = rt.block_on(async move {
            // The registration connect armed in `new`, ahead of every request.
            actor.registration_pass();
            let mut stop = Stop::Listening(shutdown_rx);
            loop {
                // Drain all pending messages into the heap
                actor.drain_channel();
                actor.rescan_parked();
                actor.drop_left();

                if actor.idle() {
                    // Wait for a message, an explicit shutdown, or the
                    // connect-retry deadline. The timer arm is what makes the
                    // autonomous reconnect work with NO queued traffic (C's
                    // connect timer fires on its own timer queue and posts a
                    // Connect-priority request to the port thread,
                    // asynManager.c:3283-3297); here the actor IS the port
                    // thread, so it wakes itself.
                    let retry_at = actor.driver.base().connect_retry_at;
                    let cells = Arc::clone(&actor.cells);
                    tokio::select! {
                        msg = actor.rx.recv() => {
                            match msg {
                                Some(m) => actor.enqueue_message(m),
                                // Unreachable: the last PortHandle is gone.
                                None => break,
                            }
                        }
                        // C `queueRequest`'s `epicsEventSignal`
                        // (asynManager.c:1628) for a queued cell.
                        _ = cells.pushed() => {}
                        () = stop.arrived() => {}
                        _ = sleep_until_opt(retry_at) => {}
                    }
                    // Drain any more that arrived
                    actor.drain_channel();
                    actor.rescan_parked();
                    actor.drop_left();
                }

                // Asked here, not only when idle: a client that borrows the
                // port back-to-back has queued its next request before the
                // actor is back, so the queue never empties.
                if stop.sent() {
                    break;
                }

                // Run any due connect retry before servicing the queue: a
                // request dequeued now would otherwise see a port that the
                // timer was about to bring back up.
                actor.service_connect_timer();

                // Process one eligible request from the heap
                if let Some(handoff) = actor.process_one() {
                    actor = lend.hand_off(actor, handoff).await;
                }

                if stop.sent() {
                    break;
                }

                // C `portThread`'s post-drain auto-connect, run on every wake.
                actor.port_thread_auto_connect();
            }
            actor
        });
        let _ = actor.driver.shutdown();
    }

    /// C `portConnectTimerCallback` + `portConnectProcessCallback`
    /// (asynManager.c:3283-3314), collapsed into one step because the actor
    /// is both the timer's consumer and the port thread the callback would
    /// have queued onto.
    ///
    /// C's timer callback re-checks `!connected && autoConnect` before
    /// queueing (:3289) — the port may have come back since the timer was
    /// armed — and the process callback re-checks `isConnected` again (:3309)
    /// before calling `pasynCommon->connect`. On failure it re-arms at
    /// `secondsBetweenPortConnect` (:3312), which is the 20 s retry loop that
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
        // (:3290), so the retry is one of the five wakes — the pass it starts is
        // allowed to end in the port-level auto-connect
        // ([`Self::port_thread_auto_connect`]) even with no traffic on the port.
        self.woken = true;

        // Disarm first: `connect()` re-arms via `set_connected` on either
        // edge, and a stale deadline left behind here would spin the loop.
        self.driver.base_mut().connect_retry_at = None;

        // C :3289 — the port reconnected (or auto-connect was turned off)
        // while the timer was pending: nothing to do.
        if self.driver.base().is_connected() || !self.driver.base().is_auto_connect() {
            return;
        }

        // C :3290 — the callback does not call `pasynCommon->connect`, it posts
        // a `queueRequest`, so the gate runs. A refusal (`asynDisabled` from
        // :1543-1548) means `portConnectProcessCallback` never runs, and it is
        // the *only* thing that re-arms the timer (:3312): `asynEnable(port,0)`
        // stops the retry loop dead, and a `shutdownPort`ed port never touches
        // the hardware again. So: no attempt, and no re-arm.
        if self.connect_gate().is_err() {
            return;
        }

        // Addr -1: this is the *port's* attempt. C derives it as
        // `addr = (pdevice ? pdevice->addr : -1)` in `connectAttempt`
        // (asynManager.c:754), and the port-level caller passes
        // `&pport->dpc`, i.e. no device (:718). A driver that reads the
        // address — prologix branches on it exactly as C's `prologixConnect`
        // does — must not see this as a device 0 connect.
        // Unstamped and unreported: `portConnectProcessCallback` touches
        // neither `lastConnectDisconnect` nor `previousConnectStatus`, so the
        // `autoConnectDevice(pport,0)` the same wake reaches (:858-863) makes a
        // second attempt, and that one reports (`connectAttempt`, :778-786).
        let held = self.sync_lock.hold();
        let _ = self.driver.connect(&AsynUser::default().with_addr(-1));
        drop(held);

        if !self.driver.base().is_connected() {
            // Still down — back off and try again (C :3312).
            let retry = self.driver.base().seconds_between_port_connect;
            self.driver.base_mut().connect_retry_at = Some(Instant::now() + retry);
        }
    }

    /// C `portThread`'s post-drain auto-connect (asynManager.c:858-863): the
    /// woken port thread finds `!pport->dpc.connected` and runs one
    /// 2 s-throttled `autoConnectDevice(pport,0)`.
    ///
    /// It runs on **every** wake, and an exception announcement is such a wake:
    /// `announceExceptionOccurred` signals `notifyPortThread` for every
    /// exception on a CANBLOCK port (:637-638), and `enable` ends in
    /// `exceptionOccurred` (:2249). That — not any re-arm inside the enable
    /// itself — is what makes `asynEnable(port,1)` bring a down port back up in
    /// C. The Rust actor previously had only the disconnect-EDGE retry timer as
    /// an autonomous connect path, so a port disabled while down stayed down
    /// after re-enable until some I/O request forced a connect (R15-46).
    ///
    /// What it does *not* run on is a pass C's thread would never have taken. C
    /// waits on `notifyPortThread` and only the five signal sites listed on
    /// [`Self::woken`] reach it; `isConnected` (:2331-2342), `isEnabled` (:2345),
    /// `isAutoConnect` (:2361) and `report` (:1138) are plain reads under
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

    /// C `epicsEventMustWait(pport->notifyPortThread)` (asynManager.c:806) —
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
        self.driver.base_mut().start_owed_connect_timer();
        self.readmit_released();
        while let Ok(msg) = self.rx.try_recv() {
            self.enqueue_message(msg);
        }
        while let Some((cell, seq)) = self.cells.pop() {
            self.enqueue_message(ActorMessage::from_cell(cell, seq));
        }
    }

    /// Return the parked requests to the eligible queue, if anything has
    /// happened that could make one of them eligible — C's `queueStateChange`
    /// rescan (asynManager.c:870-900, where the *whole* queue is walked again on
    /// every pass that ran a callback or took a new request).
    ///
    /// A re-park does not set the flag, so a request whose device is still
    /// disabled is not rescanned in a loop: the actor goes back to waiting, and
    /// the request stays queued exactly as C leaves it.
    fn rescan_parked(&mut self) {
        let announced = self.driver.base().exceptions_announced();
        let edge = announced != self.rescanned_exceptions;
        self.rescanned_exceptions = announced;
        if !self.rescan_parked && !edge {
            return;
        }
        self.rescan_parked = false;
        for msg in self.parked.drain(..) {
            self.heap.push(msg);
        }
    }

    /// Drop the entries of requests that left the queue on another clock — the
    /// queue timer's (`RequestCell::expire`) or a waiter's own — which C's
    /// `queueTimeoutCallback` unlinks from wherever they sit: behind other
    /// requests, parked behind a disabled device, or held by a block
    /// (asynManager.c:664-674). Its signal (:702) is what wakes this pass.
    fn drop_left(&mut self) {
        if !self.cells.take_left() {
            return;
        }
        self.parked.retain(|msg| !msg.is_gone());
        self.pending_while_blocked.retain(|msg| !msg.is_gone());
        self.heap.retain(|msg| !msg.is_gone());
        self.woken = true;
    }

    /// The gate every connect attempt the actor makes *on its own initiative*
    /// passes — the retry timer and both auto-connect levels. It is the same
    /// gate a request passes, asked on behalf of the connect user C builds for
    /// the port (`pport->pconnectUser`, asynManager.c:3257-3262): `addr == -1` at
    /// Connect priority, so `checkPortConnect` is `FALSE` (:1538-1540) and only
    /// the unconditional enabled/defunct half binds (:1543-1548).
    ///
    /// Nothing in C reaches `pasynCommon->connect` without it — the timer
    /// callback goes through `queueRequest` (:3290), and `portThread` refuses to
    /// run anything at all on a disabled port, its own `autoConnectDevice`
    /// included (:808-811). Routing every attempt through the one gate is what
    /// makes `asynEnable(port,0)` stop the hardware retries, rather than merely
    /// stop the reads that ride on them.
    fn connect_gate(&self) -> AsynResult<()> {
        self.driver.base().check_queue(-1, ConnectCheck::Waived)
    }

    /// C `reportPrintPort` (asynManager.c:1020-1124): the *manager's* report of a
    /// port, printed before the driver's own `pasynCommon->report` (:1123-1125).
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
    /// (:1034-1037); `details >= 1` adds the state / queue / lock / exception /
    /// trace lines; `details >= 2` adds the interface lists; a destroyed port
    /// prints one line and nothing else, driver included (:1040-1044) — its
    /// interfaces are gone. Disabled and disconnected are *not* such states: they
    /// report, as `enabled:No` / `connected:No`.
    fn report_port(&mut self, details: i32, sync_locked: bool) -> String {
        let mut out = self.manager_report(details, sync_locked);
        // C ends by calling the port's `asynCommon->report` (:1115-1124) — the
        // driver's own detail, and only that. A destroyed port's interfaces are
        // gone, so C never reaches it (:1040-1044).
        if !self.driver.base().is_defunct() {
            self.driver.report(&mut out, details.abs());
        }
        out
    }

    /// The text of C's manager block, which [`Self::report_port`] starts with.
    /// Split out so the block can be asserted on: every line here is a question an
    /// operator ran `asynReport` to answer.
    fn manager_report(&self, details: i32, sync_locked: bool) -> String {
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
            yn(base.is_auto_connect())
        );
        if details >= 1 {
            let _ = writeln!(
                out,
                "    enabled:{} connected:{} numberConnects {}",
                yn(base.is_enabled()),
                yn(base.is_connected()),
                base.number_connects
            );
            // C counts every priority's queue (:1038-1039) and asks whether a
            // `blockProcessCallback` holder owns the port (:1063). Both are the
            // actor's state: the heap is the queue, [`BlockBook`] the holders, and
            // the requests held back behind the block or a disabled device are
            // queued as much as the ones in the heap — in C they are all one
            // `queueList`.
            let _ = writeln!(
                out,
                "    nDevices {} nQueued {} blocked:{}",
                base.devices().len(),
                self.heap.len() + self.pending_while_blocked.len() + self.parked.len(),
                yn(self.cells.blocks().lock().any_held())
            );
            // C try-locks the two port mutexes from a *separate* report thread
            // (:1051-1072). `synchronousLock` was sampled on the caller's thread
            // as the report was submitted; the actor holds no counterpart of
            // `asynManagerLock` across a call, so that one is always free.
            let _ = writeln!(
                out,
                "    asynManagerLock:No synchronousLock:{}",
                yn(sync_locked)
            );
            // C's `exceptionActive` is set while `exceptionOccurred` is fanning
            // out (:621-634) and its notify list holds the users waiting on that
            // fan-out. Rust announces synchronously from the transition owner with
            // no deferred list, so on the actor thread neither can be pending; the
            // users are the registered callbacks and the port's interposes C
            // registers one for.
            let _ = writeln!(
                out,
                "    exceptionActive:No exceptionUsers {} exceptionNotifys 0",
                base.exception_user_count(-1)
            );
            let (mask, io_mask, info_mask) = self.trace_masks(None);
            let _ = writeln!(
                out,
                "    traceMask:0x{mask:x} traceIOMask:0x{io_mask:x} traceInfoMask:0x{info_mask:x}"
            );
        }
        if details >= 2 {
            // C prints the two interface lists by `interfaceType`
            // (`reportPrintInterfaceList`, :995-1007). The pointers it prints with
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
            // looking for (:1083-1096).
            for (addr, ds) in base.devices() {
                if !ds.connected() || details >= 1 {
                    let _ = writeln!(
                        out,
                        "    addr {} autoConnect {} enabled {} connected {} exceptionActive {}",
                        addr,
                        // The *device's* autoConnect, which is what C prints here
                        // (`pdpc = &pdevice->dpc`, asynManager.c:1084-1089) — the
                        // port's own is on the header line. `asynAutoConnect(port,
                        // addr, 0)` turns off one device's auto-connect, and the
                        // report has to be able to say so.
                        yn(ds.auto_connect()),
                        yn(ds.enabled()),
                        yn(ds.connected()),
                        yn(false)
                    );
                }
                if details >= 1 {
                    let _ = writeln!(
                        out,
                        "        exceptionActive No exceptionUsers {} exceptionNotifys 0",
                        base.exception_user_count(addr)
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
    /// (asynManager.c:1072-1074, :1101-1103).
    fn trace_masks(&self, addr: Option<i32>) -> (u32, u32, u32) {
        let base = self.driver.base();
        let trace = &base.trace;
        // `snapshot` is the single owner of C's `findTracePvt` fallback chain —
        // device, else port, else global (asynManager.c:548-553) — so the report
        // prints the masks the port actually traces with.
        let snap = trace.snapshot(Some(&base.port_name), addr);
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
            RequestOp::SetEnable { .. }
            | RequestOp::ExceptionDisconnect
            | RequestOp::ExceptionConnect
            | RequestOp::SetAutoConnect { .. }
            | RequestOp::GetEnable
            | RequestOp::GetAutoConnect
            | RequestOp::GetConnected
            // `connectDevice` (asynManager.c:1324-1357) takes `asynManagerLock`
            // and calls `locateDevice(pport, addr, TRUE)`: no queue, no gate.
            | RequestOp::ConnectUser
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
            // `setXxxParam` is as plain a method as `callParamCallbacks`.
            | RequestOp::SetParams { .. }
            // `asynDrvUser->create` is called straight off the interface pointer,
            // at record init and at connect time: asynRecord.c:1243-1257 and
            // devAsynInt32.c:264-277. No `queueRequest` is anywhere in either
            // path. It is a string → parameter-index lookup in the driver's own
            // table and cannot fail for connectivity reasons; gating it made a
            // record bound to a down driver resolve its DRVINFO to reason 0
            // (R13-48).
            | RequestOp::DrvUserCreate { .. }
            // `report` (asynManager.c:1138-1173) spawns a `reportPort` thread and
            // calls `pasynCommon->report(drvPvt, fp, details)` directly (:1123-1125).
            // No queue, no gate: a disabled port reports `enabled:No`, a
            // disconnected one `connected:No` (:1059-1062). The diagnostic you
            // reach for *because* the port is down must not be the one the down
            // port refuses (W10-D6). The one state C will not report on is a
            // destroyed port, handled in the dispatch arm (:1040-1044).
            | RequestOp::Report { .. }
            // `asynPortDriver::lock()` (asynPortDriver.cpp:3858) is a mutex take
            // on the driver's own thread: no `queueRequest`, no connected or
            // enabled check. The actor's serial ownership is that mutex.
            | RequestOp::WithDriver(_)
            // The same lock, taken behind the queue; see `enqueue_message`.
            | RequestOp::WithDriverAfterQueue(_)
            // `lockPort` (asynManager.c:1778-1808) takes `synchronousLock` and
            // nothing else; the defunct refusal is the dispatch arm's.
            | RequestOp::LockPort(_) => CDispatch::Direct,

            // The caller's own callback, queued at the band it names.
            RequestOp::Cell(cell) | RequestOp::CellInline(cell) if cell.connect_queue() => {
                CDispatch::ConnectQueue
            }
            RequestOp::Cell(_) | RequestOp::CellInline(_) => CDispatch::Queued,

            // C reaches `asynCommon->connect`/`disconnect` only from a callback
            // queued at Connect priority (asynRecord's CNCT put,
            // asynRecord.c:537-538,561-571) or from the port thread itself.
            RequestOp::Connect
            | RequestOp::Disconnect
            | RequestOp::ConnectAddr
            | RequestOp::DisconnectAddr => CDispatch::ConnectQueue,

            // Everything device support reaches through `queueRequest`: the
            // interface I/O (asynOctet / asynInt32 / asynInt64 / asynFloat64 /
            // asynUInt32Digital / the array interfaces / asynEnum), the option and
            // EOS accessors asynRecord queues (asynRecord.c:1758-1944, 1947-2025),
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
    /// through `locateDevice` (asynManager.c:538-545, :576).
    fn dp_key(&self, addr: i32) -> DpKey {
        DpKey::of(self.driver.base().flags.multi_device, addr)
    }

    /// Whether both block holders admit this request — C's drain-loop test
    /// (asynManager.c:889-892). The one place that reads the holders, so a
    /// device block can never be mistaken for a port-wide one.
    #[inline]
    fn admits(&self, msg: &ActorMessage) -> bool {
        !self.cells.blocks().held() || self.admits_held(msg)
    }

    #[cold]
    #[inline(never)]
    fn admits_held(&self, msg: &ActorMessage) -> bool {
        self.cells
            .blocks()
            .lock()
            .admits(msg.user.block_token, self.dp_key(msg.user.addr))
    }

    /// A [`RequestOp::CallParamCallbacks`] of a caller's section, as
    /// [`Self::dispatch_io`] runs one, but its `updates` go back to the
    /// caller once stored, before any callback fires: a caller those
    /// callbacks release finds them there for its next section.
    fn post_section(
        &mut self,
        addr: i32,
        updates: Vec<crate::request::ParamSetValue>,
        time_stamp: Option<SystemTime>,
        gate: crate::request::SectionCallbacks,
    ) -> AsynResult<RequestResult> {
        let base = self.driver.base_mut();
        if let Some(ts) = time_stamp {
            base.set_time_stamp(ts);
        }
        let mut failed = set_params(base, &updates);
        let fire = gate.owe(base, addr, &updates);
        gate.give_back(updates);
        if fire && let Some(e) = gate.fire(base) {
            failed.get_or_insert(e);
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(RequestResult::write_ok()),
        }
    }

    /// C `portThread` after a callback (:931-934): the user's counted blocks
    /// become holders, and what the heap already took that they now hold back
    /// leaves it — re-tested through the gate, so a device block diverts only
    /// the requests resolving to *that* `dpCommon`.
    #[inline]
    fn settle_block(&mut self, user: &AsynUser) {
        if let Some(token) = user.block_token
            && self.cells.blocks().counted()
        {
            self.settle_counted(token, user.addr);
        }
    }

    #[cold]
    #[inline(never)]
    fn settle_counted(&mut self, token: u64, addr: i32) {
        let dp = self.dp_key(addr);
        if !self.cells.blocks().lock().settle(token, dp) {
            return;
        }
        let drained: Vec<ActorMessage> = self.heap.drain().collect();
        for msg in drained {
            if Self::is_lifecycle_op(&msg.op) || self.admits(&msg) {
                self.heap.push(msg);
            } else {
                self.pending_while_blocked.push(msg);
            }
        }
    }

    /// The port-thread side of C `unblockProcessCallback`'s signal (:1786):
    /// once a holder was released, from any thread, what the remaining holders
    /// admit goes back to the heap. Releasing the device holder must not free
    /// requests the port-wide holder still gates, and vice versa; C gets this
    /// for free because it never moved them off the queue (:889-892).
    #[inline]
    fn readmit_released(&mut self) {
        if self.cells.blocks().released() {
            self.readmit();
        }
    }

    #[cold]
    #[inline(never)]
    fn readmit(&mut self) {
        if !self.cells.blocks().lock().take_released() {
            return;
        }
        self.woken = true;
        let pending = std::mem::take(&mut self.pending_while_blocked);
        for msg in pending {
            if self.admits(&msg) {
                self.heap.push(msg);
            } else {
                self.pending_while_blocked.push(msg);
            }
        }
    }

    /// C `puserPvt->isQueued` for a block identity: a request of its own still
    /// waits in the queue.
    fn is_queued(&self, token: u64) -> bool {
        self.heap
            .iter()
            .chain(&self.pending_while_blocked)
            .chain(&self.parked)
            .any(|msg| msg.user.block_token == Some(token))
    }

    /// The block identity a BlockProcess/UnblockProcess holds or releases.
    fn block_identity(user: &AsynUser) -> AsynResult<u64> {
        user.block_token.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "blockProcessCallback: the user has no block identity".into(),
        })
    }

    /// The ops the block holder (`pblockProcessHolder`) cannot stall — everything
    /// C does not put through `queueRequest`'s lower-priority loop.
    fn is_lifecycle_op(op: &RequestOp) -> bool {
        Self::c_dispatch(op) != CDispatch::Queued
    }

    fn enqueue_message(&mut self, mut msg: ActorMessage) {
        // C `queueRequest` sets `pport->queueStateChange` and signals the port
        // thread (asynManager.c:1615, 1628) — the queue changed, so anything the
        // thread had skipped is looked at again.
        self.rescan_parked = true;
        // …and the signal is `queueRequest`'s, so only what C *queues* raises it.
        // A `pasynManager` direct call — `isConnected`, `isEnabled`,
        // `isAutoConnect`, `report`, `enable`, `blockProcessCallback` — takes
        // `asynManagerLock`, acts and returns without touching the event
        // ([`CDispatch::Direct`]). Those that nonetheless end in an exception
        // (`enable` → `exceptionOccurred`, :2249) wake the thread through the
        // exception count, not through here.
        if Self::c_dispatch(&msg.op) != CDispatch::Direct {
            self.woken = true;
        }
        // Ranked before a block can divert it: the release re-pushes it as is.
        msg.rank = if !self.driver.base().flags.can_block {
            Rank::Arrival
        } else if matches!(msg.op, RequestOp::WithDriverAfterQueue(_)) {
            // Behind everything queued before it, at any band.
            Rank::Band(QueuePriority::Low)
        } else if Self::c_dispatch(&msg.op) == CDispatch::Direct {
            Rank::Direct
        } else {
            Rank::Band(msg.priority)
        };
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

    /// C's port thread at a holder's release ([`Lend::give_back`]): it took
    /// the queue's head while the port was held and waits at the lock with it
    /// (asynManager.c:894-910), so that one request gets the port ahead of
    /// anything queued after, the holder's next lock included
    /// (specsAnalyser.cpp:617-619). It holds one at a time.
    fn dequeue_at_release(&mut self) {
        self.drain_channel();
        self.dequeued = self.dequeued.take().or_else(|| self.heap.pop());
    }

    fn pop_next(&mut self) -> Option<ActorMessage> {
        self.dequeued.take().or_else(|| self.heap.pop())
    }

    /// Nothing for [`Self::process_one`] to take.
    fn idle(&self) -> bool {
        self.dequeued.is_none() && self.heap.is_empty()
    }

    fn process_one(&mut self) -> Option<Handoff> {
        // A port whose link is owned by another object — an IP-server child port,
        // whose socket the parent's accept loop assigns and clears — can change
        // connection state with no request of its own having run. The owner wakes
        // this actor when it does, and every wake ends here, request or not: the
        // `exceptionConnect` fan-out and the interpose stack's reset (which drops
        // the previous client's EOS read-ahead) are published before anything is
        // dispatched. C's listener publishes the same edge from its own thread
        // (`connectDevice`, drvAsynIPServerPort.c:372-375). Every other port is
        // already in sync (its own `set_connected` published the edge), so this
        // is a no-op for them.
        self.driver.base_mut().sync_connection_edge();
        let msg = self.pop_next()?;

        // The queue-wait deadline is not the port thread's: C's timer thread
        // unlinks the request (asynManager.c:649-703) while this thread may be
        // inside the driver, and so does `RequestCell::expire` or the waiter's
        // own timer here. A deadline, an `AQR` cancel and the dequeue below race
        // through one CAS on the token, and exactly one of them wins.
        if msg.is_gone() {
            return None;
        }

        // Everything down to `begin_running` is C's *queued* phase — the request
        // is still on the queue there, which is what lets a disabled device's
        // request stay on it (:877).

        // The queue gate — C `queueRequest` (asynManager.c:1543-1554).
        // [`queue_gate`] says whether it runs at all (C queues this op or
        // performs it as a direct manager call) and, when it does, which of its
        // two independent refusals the request may waive. The *connected* waiver
        // follows the request's own user (`AsynUser::connect_check`, C's
        // `checkPortConnect` at :1538-1540); the *enabled* refusal is waivable by
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
        let multi_device = self.driver.base().flags.multi_device;
        if let Some(connect) = queue_gate(&msg.op, &msg.user, multi_device) {
            if !self.driver.base().flags.can_block {
                // A lend runs C's synchronous branch on its own thread
                // ([`Self::lent_gate`]), as C does.
                if !matches!(msg.reply, Reply::Lend(_))
                    && let Err(e) = self.synchronous_gate(&msg.user, msg.priority, connect)
                {
                    msg.refuse(e);
                    return None;
                }
            } else {
                // C drains the Connect queue *before* `autoConnectDevice`
                // (asynManager.c:813-857): a waived request runs on the dead line
                // with no connect attempt in front of it. The attempt C makes
                // *after* that drain (:858-863) is the actor loop's.
                if connect == ConnectCheck::Required {
                    self.auto_connect_device(msg.user.addr, msg.user.reason);
                }
                // Every queued request passed the port-level half of `queueRequest`
                // on its caller's thread. A port disabled or down since then is the
                // port thread's to answer, and it answers by leaving the request
                // queued until the state changes back or its timer fires (:808-810,
                // :858-863).
                if self.driver.base().queue_gate().check(connect).is_err() {
                    self.parked.push(msg);
                    return None;
                }
                if let Err(e) = self.driver.base().check_queue(msg.user.addr, connect) {
                    msg.refuse(e);
                    return None;
                }
                // C `portThread`'s per-request DEVICE checks (asynManager.c:877-888):
                // the half of the device state `check_queue` deliberately leaves to
                // the thread on a CANBLOCK port, because C answers them here in a way
                // `queueRequest` cannot — by *not answering*.
                //
                // The scope is the requests C leaves in the lower-priority loop: the
                // Connect-priority queue is drained above it with no device checks at
                // all (:813-857), and every waived request C queues rides that queue
                // (asynShellCommands.c:127,174,245,296; asynRecord.c:562-571). Hence
                // `Queued` dispatch and a `Required` connect check — the same
                // requests, asked the same way.
                if connect == ConnectCheck::Required
                    && self.driver.base().flags.can_block
                    && Self::c_dispatch(&msg.op) == CDispatch::Queued
                {
                    // :877 `if(!pdpCommon->enabled) continue;` — the request keeps
                    // its place in the queue. It runs when the device is re-enabled,
                    // and its queue timer still expires under it if it never is.
                    if self
                        .driver
                        .base()
                        .check_device_enabled(msg.user.addr)
                        .is_err()
                    {
                        self.parked.push(msg);
                        return None;
                    }
                    // :879-888 — auto-connect has already run above (C calls it at
                    // :879); a device still down diverts the request to its
                    // *timeout* callback rather than `processUser`, but only when
                    // one was registered (`:886` guards on `timeoutUser != 0`;
                    // `createAsynUser`'s second argument is optional —
                    // asynRecord.c:307-308 passes one, devAsynInt32.c:212 passes
                    // 0). A request that asked for a timeout always has one:
                    // `queueRequest` refuses `timeout > 0.0` without it
                    // (:1592-1597). Here the reply channel *is* that callback, so
                    // the request reports the same queue timeout its own timer
                    // would.
                    if msg.timeout_user()
                        && self
                            .driver
                            .base()
                            .check_device_connected(msg.user.addr)
                            .is_err()
                    {
                        msg.time_out(self.driver.base().port_name.to_string());
                        return None;
                    }
                }
            }
        }

        // Claim the request for execution. This is the C dequeue under
        // `asynManagerLock` (:889-891): `begin_running` transitions the cancel
        // token `Queued -> Running`, which closes the window in which an `AQR`
        // `cancelRequest` could report `wasQueued==1` (asynManager.c:1663-1668).
        // It fails only if the request left the queue without running — an `AQR`
        // cancel, or a queue-wait deadline the *waiter* resolved first (the
        // async path arms its own timer, `PortHandle::submit_cancellable`) — in
        // which case it must be dropped, reporting the outcome that won.
        match msg.claim() {
            Claim::Run => {}
            Claim::Gone => return None,
            Claim::Left => {
                let e = if msg.is_timed_out() {
                    AsynError::QueueTimeout {
                        port: self.driver.base().port_name.to_string(),
                    }
                } else {
                    AsynError::Status {
                        status: AsynStatus::Error,
                        message: "request cancelled".into(),
                    }
                };
                msg.refuse(e);
                return None;
            }
        }
        let ActorMessage {
            op,
            mut user,
            cancel,
            reply,
            priority,
            ..
        } = msg;
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

        // C `portThread` announces the callback it is about to run
        // (asynManager.c:904) — a `queueLockPort` callback too, before it waits
        // there for its caller. A direct manager call never reaches it, and a
        // port without ASYN_CANBLOCK has no thread: its line is the gate's.
        if Self::c_dispatch(&op) != CDispatch::Direct && self.driver.base().flags.can_block {
            self.link_user(&mut user);
            user.print(
                crate::trace::TraceMask::FLOW,
                file!(),
                line!(),
                format_args!(
                    "asynManager::portThread port={} callback\n",
                    self.driver.base().port_name
                ),
            );
        }
        let reply = match reply {
            Reply::Lend(reply) => {
                return Some(Handoff {
                    claimed: Claimed { op, user, priority },
                    reply,
                });
            }
            reply => reply,
        };
        let result = run_claimed(&mut Access::Owned(self), op, user);
        if let Reply::Result(reply) = reply {
            let _ = reply.send(result);
        }

        // The port-level auto-connect C's `portThread` runs after this
        // (asynManager.c:858-863) is not this function's — it is the actor
        // loop's, once per wake ([`Self::port_thread_auto_connect`]), because in
        // C it is the *thread*, not the request, that performs it. That is what
        // brings a dead line up on the new configuration a waived
        // `asynSetOption`/`asynSetEos`/HOSTINFO repoint just wrote, and equally
        // what brings a re-enabled port back up with no request at all (R15-46).
        None
    }

    /// Link a request's user to the port before any driver sees it: the trace
    /// context, and the address C's `getAddr` answers (asynManager.c:2005-2009),
    /// -1 on a port without `ASYN_MULTIDEVICE` whatever device the user names.
    /// No `PortDriver` method reads the address any other way.
    fn link_user(&self, user: &mut AsynUser) {
        user.trace = self.user_trace();
        user.addr = crate::port::dp_common_key(self.driver.base().flags.multi_device, user.addr)
            .unwrap_or(-1);
    }

    /// The port context [`AsynUser::trace`] carries — the driver's trace manager
    /// and the port's name, both `Arc`s, so stamping a request is two refcount
    /// bumps.
    fn user_trace(&self) -> Option<crate::user::UserTrace> {
        let base = self.driver.base();
        Some(crate::user::UserTrace {
            manager: base.trace.clone(),
            port: Arc::clone(&base.port_name),
        })
    }

    /// The **port** half of C `autoConnectDevice` (asynManager.c:706-723) —
    /// the single owner of every port-level auto-connect attempt, so the two
    /// callers cannot drift apart.
    ///
    /// C reaches it two ways, and both are here: through `autoConnectDevice`
    /// when a queued request needs the line up, and straight from `portThread`
    /// with `pdevice = 0` once the Connect-priority queue has drained
    /// (:858-863). One attempt per 2 s window either way
    /// (`auto_connect_throttle_ok`, C :714-715), every attempt restarting the
    /// window whether it succeeded or failed (`stamp_auto_connect_attempt`,
    /// C :720).
    ///
    /// Returns whether the port is connected afterwards (C's `:723` bail-out),
    /// or `false` outright when [`Self::connect_gate`] refuses — the whole
    /// device level below hangs off that `false` (:724-740), so one gate check
    /// covers both levels.
    fn auto_connect_port(&mut self) -> bool {
        // C `portThread` re-applies the queue gate's unconditional refusal
        // before it reaches *either* `autoConnectDevice` call site:
        // `if(!pport->dpc.enabled) continue;` (:808-811) sits above the
        // Connect-queue drain, above `autoConnectDevice(pport,0)` (:858-863)
        // and above the per-request one (:878-885).
        if self.connect_gate().is_err() {
            return false;
        }
        if !self.driver.base().is_connected() && self.driver.base().is_auto_connect() {
            if !self
                .driver
                .base()
                .auto_connect_throttle_ok(-1, Instant::now())
            {
                return false;
            }
            // Addr -1: this is the *port's* attempt. C derives it as
            // `addr = (pdevice ? pdevice->addr : -1)` in `connectAttempt`
            // (asynManager.c:754), and the port-level caller passes
            // `&pport->dpc`, i.e. no device (:718). A driver that reads the
            // address — prologix branches on it exactly as C's `prologixConnect`
            // does — must not see this as a device 0 connect.
            let held = self.sync_lock.hold();
            let result = self.driver.connect(&AsynUser::default().with_addr(-1));
            drop(held);
            self.report_connect_attempt(-1, &result);
            self.driver
                .base_mut()
                .stamp_auto_connect_attempt(-1, Instant::now());
        }
        self.driver.base().is_connected()
    }

    /// C `connectAttempt`'s report (asynManager.c:778-786) through
    /// `reportConnectStatus` (:469-478): an error-trace line only when the
    /// outcome differs from the port's last, so a dead line logs once per
    /// transition, not once per 2 s attempt.
    fn report_connect_attempt(&mut self, addr: i32, result: &AsynResult<()>) {
        if result.is_err() == self.connect_failure_reported {
            return;
        }
        self.connect_failure_reported = result.is_err();
        let mut user = AsynUser::default().with_addr(addr);
        user.trace = self.user_trace();
        let port = &self.driver.base().port_name;
        let mask = crate::trace::TraceMask::ERROR;
        // C prints through `pasynTrace->vprint`, which has no source (:476).
        match result {
            Err(e) => user.print(
                mask,
                "",
                0,
                format_args!(
                    "{port} {addr} autoConnect could not connect: {}\n",
                    e.message_display()
                ),
            ),
            Ok(()) => user.print(
                mask,
                "",
                0,
                format_args!("{port} {addr} port is now connected\n"),
            ),
        }
    }

    /// C `queueRequest`'s synchronous branch (asynManager.c:1543-1577), the
    /// whole answer on a port without `ASYN_CANBLOCK`: it has no queue, so a
    /// request it does not refuse runs now, and one it refuses is never parked.
    /// A down device is auto-connected and refused only below Connect priority
    /// and without `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED` (:1570-1577) —
    /// a Connect-priority request is how such a device comes up.
    fn synchronous_gate(
        &mut self,
        user: &AsynUser,
        priority: QueuePriority,
        connect: ConnectCheck,
    ) -> AsynResult<()> {
        let addr = user.addr;
        let base = self.driver.base();
        base.queue_gate()
            .check(connect)
            .map_err(|r| AsynError::from(r).into_queue_refusal())?;
        if connect != ConnectCheck::SynchronousLock {
            // :1561 sits between the port-level refusals and the device-level
            // ones, so a request the device refuses still prints it.
            if let Some(trace) = self.user_trace() {
                trace.print(
                    crate::port::dp_common_key(base.flags.multi_device, addr).unwrap_or(-1),
                    user.reason as i32,
                    crate::trace::TraceMask::FLOW,
                    file!(),
                    line!(),
                    format_args!("{} queueRequest synchronous\n", base.port_name),
                );
            }
            base.check_device_enabled(addr)
                .map_err(AsynError::into_queue_refusal)?;
        }
        if connect != ConnectCheck::SynchronousLock
            && priority < QueuePriority::Connect
            && user.reason != crate::user::ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED
            && !self.auto_connect_device(addr, user.reason)
        {
            return self
                .driver
                .base()
                .check_device_connected(addr)
                .map_err(AsynError::into_queue_refusal);
        }
        Ok(())
    }

    /// The gate a lent request passes on its caller's thread, with the actor
    /// in hand. On a port without `ASYN_CANBLOCK` it is C's whole
    /// synchronous branch, printed and auto-connected on the caller as
    /// asynManager.c:1555-1577 does; a `CANBLOCK` lend already passed the
    /// port thread's gate before the actor was handed over.
    fn lent_gate(
        &mut self,
        op: &RequestOp,
        user: &AsynUser,
        priority: QueuePriority,
    ) -> AsynResult<()> {
        self.driver.base_mut().sync_connection_edge();
        let base = self.driver.base();
        if base.flags.can_block {
            return Ok(());
        }
        match queue_gate(op, user, base.flags.multi_device) {
            Some(connect) => self.synchronous_gate(user, priority, connect),
            None => Ok(()),
        }
    }

    /// C `autoConnectDevice` (asynManager.c:706-741) — the single owner of
    /// every auto-connect attempt, port-level and device-level.
    ///
    /// The order is the whole point: C reconnects the **port** first
    /// (`connectAttempt(&pport->dpc)` at :718) and bails out if the port is
    /// still down (:723), only then reconnecting the device (:725-739). A
    /// multi-device port that skipped straight to the device would never
    /// reopen its transport — `PortDriver::connect_addr`'s default just
    /// flips the per-address `connected` flag (port.rs) and opens no socket,
    /// so the port-level `check_ready` that follows rejects the request
    /// forever.
    ///
    /// Both levels are throttled to one attempt per 2 s window
    /// (`auto_connect_throttle_ok`, C :714-715 / :731-732), and every attempt
    /// restarts the window whether it succeeded or failed
    /// (`stamp_auto_connect_attempt`, C :720 / :737) — so a burst of N queued
    /// requests to an offline port fires one attempt, not N. A port whose
    /// throttle window has not elapsed gives up for this request exactly as C
    /// does (`return FALSE` at :715), without falling through to the device.
    ///
    /// Returns whether the addressed device is connected afterwards. The
    /// single-threaded actor owns the driver throughout, so C's
    /// `autoConnectActive` re-entry guard has no observable analogue here.
    fn auto_connect_device(&mut self, addr: i32, reason: usize) -> bool {
        // --- Port level (C :707-723). Runs for single- and multi-device
        // ports alike: in C the port `dpCommon` is reconnected before any
        // device `dpCommon` is even looked at.
        if !self.auto_connect_port() {
            // C :723 — the port is still down, so the device cannot come up.
            return false;
        }
        if !self.driver.base().flags.multi_device {
            // C :724 `if(!pdevice) return TRUE`.
            return true;
        }

        // --- Device level (C :725-739).
        let dev_disconnected = !self.driver.base().dp_connected(addr);
        let dev_auto = self.driver.base().dp_auto_connect(addr);
        if dev_disconnected && dev_auto {
            if !self
                .driver
                .base()
                .auto_connect_throttle_ok(addr, Instant::now())
            {
                return false;
            }
            let connect_user = AsynUser::new(reason).with_addr(addr);
            let held = self.sync_lock.hold();
            let result = self.driver.connect_addr(&connect_user);
            drop(held);
            self.report_connect_attempt(addr, &result);
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
    fn octet_read(&mut self, user: &AsynUser, buf_size: usize) -> OctetRead {
        let mut buf = vec![0u8; buf_size];
        let r = crate::port::octet_read_chain(&mut *self.driver, user, &mut buf);
        OctetRead::from_chain(buf, r)
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
            RequestOp::OctetRead { buf_size, .. } => Ok(RequestResult::octet_read(
                self.octet_read(user, *buf_size),
                0,
            )),
            RequestOp::OctetWriteBinary { data } => {
                // C parity: asynRecord binary output (asynRecord.c:1528-1541).
                // Save the driver's output EOS, clear it for the raw write,
                // and restore it on every exit path so a configured OEOS does
                // not append terminator bytes to a binary payload. The actor
                // owns the bracket atomically under its serial dispatch.
                //
                // The bracket runs only when there IS a saved terminator, as
                // C's `if (saveEosLen)` guard does (:1534, :1539) — a driver
                // with no EOS methods answers `asynError` to both accessors
                // (asynOctetBase.c:401-415) and C explicitly tolerates that
                // (:1532 "getOutputEos can return an error if the driver does
                // not implement it"). Clearing unconditionally turned every
                // binary transfer on such a port into that refusal.
                // A refused getOutputEos leaves C's saveEosLen at 0.
                let saved = self.driver.get_output_eos(user).unwrap_or_default();
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
            RequestOp::OctetReadBinary { buf_size, .. } => {
                // C parity: asynRecord binary input (asynRecord.c:1564-1577).
                // Save the driver's input EOS, clear it for the read, and
                // restore it on every exit path so a configured IEOS does not
                // stop the read early or strip bytes belonging to the binary
                // payload. Guarded by `saveEosLen` exactly as C guards it
                // (:1570, :1575) — see `OctetWriteBinary` above.
                let saved = self.driver.get_input_eos(user).unwrap_or_default();
                let bracket = !saved.is_empty();
                if bracket {
                    self.set_input_eos(user, &[])?;
                }
                let read = self.octet_read(user, *buf_size);
                if bracket {
                    let _ = self.set_input_eos(user, &saved);
                }
                Ok(RequestResult::octet_read(read, 0))
            }
            RequestOp::OctetWriteRead {
                data,
                buf_size,
                flush,
                ..
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
                use crate::error::OctetStep;
                if *flush {
                    self.octet_flush(user)
                        .map_err(|e| e.in_octet_step(OctetStep::Flush))?;
                }
                let sent = self
                    .octet_write(user, data)
                    .map_err(|e| e.in_octet_step(OctetStep::Write))?;
                // devAsynOctet's writeIt ends the exchange on a short write
                // before readIt (devAsynOctet.c:680-686, :853-854): the result
                // carries no read data. SyncIO's writeRead reads regardless.
                if !*flush && sent != data.len() {
                    return Ok(RequestResult::write_n(sent));
                }
                let read = self.octet_read(user, *buf_size);
                Ok(RequestResult::octet_read(
                    read.in_octet_step(OctetStep::Read),
                    sent,
                ))
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
            // A single-device port has no device dpCommon: C's
            // `asynCommon->connect`/`disconnect` is the port's whatever the
            // user's addr (drvAsynIPPort.c:611 closes the socket at any addr).
            RequestOp::ConnectAddr if !self.driver.base().flags.multi_device => {
                self.driver.connect(user)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::DisconnectAddr if !self.driver.base().flags.multi_device => {
                self.driver.disconnect(user)?;
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
            RequestOp::ConnectUser => {
                let dp = self.driver.base_mut().connect_user(user.addr);
                Ok(RequestResult {
                    dp: Some(dp),
                    ..RequestResult::write_ok()
                })
            }
            RequestOp::SetEnable { yes } => {
                // C `enable(pasynUser, yesNo)` (asynManager.c:2224-2251):
                // `findDpCommon` resolves the slot from the requesting user's
                // addr (:538-545), so one op serves both scopes. A manager
                // call, not a driver method: no driver can override it.
                self.driver.base_mut().set_addr_enabled(user.addr, *yes)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::ExceptionDisconnect => {
                if !self.driver.base_mut().set_addr_connected(user.addr, false) {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!(
                            "{} addr {} asynManager:exceptionDisconnect but not connected",
                            self.driver.base().port_name,
                            self.driver.base().dp_addr(user.addr).unwrap_or(-1)
                        ),
                    });
                }
                Ok(RequestResult::write_ok())
            }
            RequestOp::ExceptionConnect => {
                if !self.driver.base_mut().set_addr_connected(user.addr, true) {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!(
                            "{} addr {} asynManager:exceptionConnect already connected",
                            self.driver.base().port_name,
                            self.driver.base().dp_addr(user.addr).unwrap_or(-1)
                        ),
                    });
                }
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetAutoConnect { yes } => {
                // C `autoConnectAsyn(pasynUser, value)` (asynManager.c:2312-2329)
                // — one call, whose `findDpCommon` hands it the DEVICE's
                // dpCommon when the user names one on a multi-device port
                // (:538-545). Fires `asynExceptionAutoConnect` unconditionally.
                self.driver
                    .base_mut()
                    .set_auto_connect_addr(user.addr, *yes);
                Ok(RequestResult::write_ok())
            }
            RequestOp::GetEnable => {
                // C `isEnabled` reads `findDpCommon(puserPvt)->enabled`
                // (asynManager.c:2345-2359): the addr on the requesting user
                // picks the slot, exactly as it does for the write above.
                let enabled = self.driver.base().dp_enabled(user.addr);
                Ok(RequestResult::int32_read(i32::from(enabled)))
            }
            RequestOp::GetAutoConnect => {
                // C `isAutoConnect` (asynManager.c:2361-2373), same resolution.
                let auto = self.driver.base().dp_auto_connect(user.addr);
                Ok(RequestResult::int32_read(i32::from(auto)))
            }
            RequestOp::PushEchoInterpose => {
                // C `asynInterposeEcho` (asynInterposeEcho.c:165-186) pushes the
                // layer onto the octet interface of the device its `addr`
                // argument names (:176), at any time after configure. The actor
                // owns the driver, so the install lands between transfers instead
                // of racing one; the addr rides the request's own user, which is
                // C's `pasynUser`-selects-the-device linkage.
                self.install_octet_interpose(
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
                self.install_octet_interpose(
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
                self.install_octet_interpose(
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
                self.install_octet_interpose(
                    user.addr,
                    Box::new(crate::interpose::flush::FlushTimeoutInterpose::new(
                        *flush_timeout,
                    )),
                );
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetTimeStampSource { name } => {
                // C `registerTimeStampSource` / `unregisterTimeStampSource`
                // (asynManager.c:334-335). The actor owns the driver, so the
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
                // its connect/disconnect request on (asynRecord.c:1090-1094,
                // 858-888).
                let connected = self.driver.base().dp_connected(user.addr);
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
                // C `blockProcessCallback` (asynManager.c:1694-1725) counts the
                // block; the holder is taken after this user's next callback
                // ([`Self::settle_block`]).
                let token = Self::block_identity(user)?;
                if !self.driver.base().flags.can_block {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "asynManager::blockProcessCallback blockProcessCallback \
                                  not supported because port is synchronous"
                            .into(),
                    });
                }
                if self.is_queued(token) {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "asynManager::blockProcessCallback is queued".into(),
                    });
                }
                self.cells.blocks().lock().block(token, *all_devices);
                Ok(RequestResult::write_ok())
            }
            RequestOp::UnblockProcess { all_devices } => {
                let token = Self::block_identity(user)?;
                if self.cells.blocks().lock().count(token, *all_devices) == 0 {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "asynManager::unblockProcessCallback but not locked".into(),
                    });
                }
                if self.is_queued(token) {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "asynManager::unblockProcessCallback is queued".into(),
                    });
                }
                let dp = self.dp_key(user.addr);
                self.cells
                    .blocks()
                    .lock()
                    .unblock(token, dp, *all_devices)
                    .map_err(|message| AsynError::Status {
                        status: AsynStatus::Error,
                        message: message.into(),
                    })?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::DrvUserCreate(req) => {
                let info = self.driver.drv_user_create(req)?;
                Ok(RequestResult::drv_user_create(info))
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
            RequestOp::Int32ArrayRead { buf, max_elements } => {
                let buf = read_in_place(buf, *max_elements, |b| {
                    self.driver.read_int32_array(user, b)
                })?;
                Ok(RequestResult::int32_array_read(buf))
            }
            RequestOp::Int32ArrayWrite { data } => {
                self.driver.write_int32_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Float64ArrayRead { buf, max_elements } => {
                let buf = read_in_place(buf, *max_elements, |b| {
                    self.driver.read_float64_array(user, b)
                })?;
                Ok(RequestResult::float64_array_read(buf))
            }
            RequestOp::Float64ArrayWrite { data } => {
                self.driver.write_float64_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int8ArrayRead { buf, max_elements } => {
                let buf =
                    read_in_place(buf, *max_elements, |b| self.driver.read_int8_array(user, b))?;
                Ok(RequestResult::int8_array_read(buf))
            }
            RequestOp::Int8ArrayWrite { data } => {
                self.driver.write_int8_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int16ArrayRead { buf, max_elements } => {
                let buf = read_in_place(buf, *max_elements, |b| {
                    self.driver.read_int16_array(user, b)
                })?;
                Ok(RequestResult::int16_array_read(buf))
            }
            RequestOp::Int16ArrayWrite { data } => {
                self.driver.write_int16_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Int64ArrayRead { buf, max_elements } => {
                let buf = read_in_place(buf, *max_elements, |b| {
                    self.driver.read_int64_array(user, b)
                })?;
                Ok(RequestResult::int64_array_read(buf))
            }
            RequestOp::Int64ArrayWrite { data } => {
                self.driver.write_int64_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Float32ArrayRead { buf, max_elements } => {
                let buf = read_in_place(buf, *max_elements, |b| {
                    self.driver.read_float32_array(user, b)
                })?;
                Ok(RequestResult::float32_array_read(buf))
            }
            RequestOp::Float32ArrayWrite { data } => {
                self.driver.write_float32_array(user, data)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::CallParamCallbacks {
                addr,
                updates,
                time_stamp,
                gate,
            } => {
                let base = self.driver.base_mut();
                if let Some(ts) = time_stamp {
                    base.set_time_stamp(*ts);
                }
                // A failed set does not strand the good ones: they still post.
                let mut failed = set_params(base, updates);
                // Flush every address list this batch wrote, not just the
                // request's. C has no bundled set-then-flush: a driver calls
                // `setXxxParam` per list and then `callParamCallbacks(addr)`
                // once for each list it touched (asynPortDriver.cpp:1785-1794). This
                // carrier bundles the two, so it owes a flush for every list it
                // just wrote — `call_param_callbacks` consumes the changed flags
                // of one list only, so flushing the request address alone stores
                // the other lists' values while firing no interrupt for them,
                // and a multi-device driver's per-address records never update
                // at all (six joint angles written at addresses 0..6, one
                // I/O Intr). Request address first, then each further address in
                // the order the batch first wrote it.
                //
                // One list that cannot be flushed must not strand the rest, for
                // the same reason one bad update does not strand the good ones
                // above; the first error still reaches whoever waits on the
                // reply.
                match gate {
                    Some(gate) => {
                        if gate.owe(base, *addr, updates)
                            && let Some(e) = gate.fire(base)
                        {
                            failed.get_or_insert(e);
                        }
                    }
                    None => {
                        let mut flush = |list| {
                            if let Err(e) = base.call_param_callbacks(list) {
                                failed.get_or_insert(e);
                            }
                        };
                        flush(*addr);
                        let mut further = Vec::new();
                        crate::request::push_further_lists(*addr, updates, &mut further);
                        further.into_iter().for_each(flush);
                    }
                }
                match failed {
                    Some(e) => Err(e),
                    None => Ok(RequestResult::write_ok()),
                }
            }
            RequestOp::SetParams { updates } => match set_params(self.driver.base_mut(), updates) {
                Some(e) => Err(e),
                None => Ok(RequestResult::write_ok()),
            },
            RequestOp::GetOption { key } => {
                // The request's own AsynUser, as `SetOption` below.
                let val = self.driver.get_option(user, key)?;
                Ok(RequestResult::option_read(val))
            }
            RequestOp::SetOption { key, value } => {
                // The request's own AsynUser, as C hands `setOption` the caller's
                // pasynUser (asynRecord.c:1758-1832 passes the record's, whose
                // timeout is TMOT; asynShellCommands.c:119 passes iocsh's 2 s).
                // A COM port's negotiation is bounded by exactly that timeout.
                self.driver.set_option(user, key, value)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::Report { level, sync_locked } => {
                // C parity: `asynManager::report` (asynManager.c:1138-1173) walks
                // every registered port and hands each to `reportPrintPort`
                // (:1020-1124), which prints the *manager's* view of the port and
                // only then calls the driver's `pasynCommon->report` (:1123-1125).
                // The iocsh wrapper (`asynReport`) does the per-port loop; here we
                // dispatch the per-port report from the actor thread so the driver
                // observes its own state under the actor's serial ownership.
                let mut result = RequestResult::write_ok();
                result.data = Some(self.report_port(*level, *sync_locked).into_bytes());
                Ok(result)
            }
            // The ops that make calls of their own; [`dispatch`] owns them.
            RequestOp::Cell(_) | RequestOp::CellInline(_) | RequestOp::LockPort(_) => {
                dispatch(&mut Access::Owned(self), user, op)
            }
            RequestOp::WithDriver(call) | RequestOp::WithDriverAfterQueue(call) => {
                match call.take() {
                    Some(f) => {
                        f(self.driver.as_mut());
                        Ok(RequestResult::write_ok())
                    }
                    None => Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "driver call carries no closure".into(),
                    }),
                }
            }
            RequestOp::SetInputEos { eos } => {
                // C parity: asynRecord IEOS write at asynRecord.c:1964-1967
                // calls `pasynOctet->setInputEos(pasynUser, eos, len)`.
                // Route through the driver trait so the EOS interpose
                // layer (interpose/eos.rs) reads from a single source
                // of truth (`PortDriverBase::input_eos`) rather than
                // the orphaned options HashMap. The terminator belongs to the
                // device the request addressed, so the user goes with it.
                self.set_input_eos(user, eos)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::SetOutputEos { eos } => {
                self.driver.set_output_eos(user, eos)?;
                Ok(RequestResult::write_ok())
            }
            RequestOp::GetInputEos => {
                let eos = self.driver.get_input_eos(user)?;
                Ok(RequestResult::eos_read(eos))
            }
            RequestOp::GetOutputEos => {
                let eos = self.driver.get_output_eos(user)?;
                Ok(RequestResult::eos_read(eos))
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

        // Attach alarm/timestamp metadata on successful reads; a failed octet
        // read carries its status inside the reply.
        match result {
            Ok(r) if is_read && r.octet_error().is_none() => {
                let (status, alarm_status, alarm_severity) = self
                    .driver
                    .base()
                    .params
                    .get_param_status(user.reason, user.addr)
                    .unwrap_or((crate::error::AsynStatus::Success, 0, 0));
                // C's parameter reads set `pasynUser->timestamp` from
                // getTimeStamp (asynPortDriver.cpp:1970-1979).
                let ts = Some(self.driver.base().get_time_stamp());
                // C devAsynInt32.c:844-847 — a non-success read/param status
                // maps to a record alarm (asynStatusToEpicsAlarm) and is
                // recGblSetSevr'd together with the explicit setParamAlarm.
                // The old `_` discarded the status, so a setParamStatus(error
                // /timeout) read returned clean (no INVALID, UDF-clearing).
                let (alarm_status, alarm_severity) =
                    combine_read_alarm(status, alarm_status, alarm_severity);
                // A non-success status fails the read, value kept, as C's
                // getXxxParam does: device support then stores nothing
                // (devAsynInt32.c:848-855) and a plain caller sees the failure.
                let out = r.with_alarm(alarm_status, alarm_severity, ts);
                self.driver.base().param_status_read(user, op, out, status)
            }
            other => other,
        }
    }
}

/// The port a [`RequestOp::Cell`] runs against: each call is dispatched on
/// whichever thread holds the actor, with no queue and no gate, as C's
/// callback calls the driver's interfaces directly while it owns the port.
/// Each call is made as the running request's user — C's callback passes its
/// own `pasynUser` — so a block it takes is held by that request's block
/// identity.
struct ActorCalls<'b, 'a>(&'b mut Access<'a>, Option<u64>);

impl PortCalls for ActorCalls<'_, '_> {
    fn call(&mut self, op: &RequestOp, mut user: AsynUser) -> AsynResult<RequestResult> {
        user.block_token = self.1;
        self.0.with(|actor| actor.link_user(&mut user))?;
        dispatch(self.0, &mut user, op)
    }
}

/// C `setXxxParam` of each update: one dispatch for every parameter type
/// (`ParamList::set_value`), so the carriers and the store cannot support
/// different type sets. A set that fails (a value the parameter cannot hold)
/// is reported, not swallowed, as C `asynPortDriver::setIntegerParam`
/// asynPrints the paramList error at ASYN_TRACE_ERROR; the remaining updates
/// still land, and the first error is returned.
fn set_params(
    base: &mut crate::port::PortDriverBase,
    updates: &[crate::request::ParamSetValue],
) -> Option<AsynError> {
    let mut failed = None;
    for u in updates {
        let outcome = base.set_param(u);
        // The setter printed C's reportSetParamErrors line.
        if let Err(e) = outcome {
            failed.get_or_insert(e);
        }
    }
    failed
}

/// An array read into the caller's buffer: the driver sees its contents, as
/// a C driver sees the record's `bptr`, and a driver that leaves them alone
/// returns them. A buffer still shared with its owner is copied once.
fn read_in_place<T: Clone + Default>(
    buf: &SharedArray<T>,
    max_elements: usize,
    read: impl FnOnce(&mut [T]) -> AsynResult<usize>,
) -> AsynResult<SharedArray<T>> {
    let mut buf = buf.clone();
    let n = read(buf.make_mut_resized(max_elements, T::default()))?;
    buf.truncate(n);
    Ok(buf)
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
/// by `libcom/src/misc/alarm.h:39-45,62-85` and part of the EPICS record ABI.
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
    use crate::port::InterfaceMask;
    use crate::port::{PortDriverBase, PortFlags};
    use crate::request::{DriverCall, OctetReadOp};
    use std::sync::Arc;
    use std::time::Duration;

    struct TestDriver {
        base: PortDriverBase,
    }

    impl TestDriver {
        fn new() -> Self {
            let mut base = PortDriverBase::new(
                "actor_test",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
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

    /// Take a port through C's `shutdownPort` (asynManager.c:2253-2310) — the
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

    /// [`spawn_actor`] with registration's 2 s throttle window already run out,
    /// for the tests that count the attempts a later wake makes.
    fn spawn_actor_past_registration(
        driver: impl PortDriver,
    ) -> mpsc::UnboundedSender<ActorMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut actor = PortActor::new(Box::new(driver), rx);
        actor.register();
        let past = Instant::now() - Duration::from_secs(3);
        actor.driver.base_mut().stamp_auto_connect_attempt(-1, past);
        std::thread::spawn(move || actor.run());
        tx
    }

    fn spawn_actor(driver: impl PortDriver) -> mpsc::UnboundedSender<ActorMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        std::thread::Builder::new()
            .name("test-actor".into())
            .spawn(actor.registered())
            .unwrap();
        tx
    }

    /// An actor and C `queueRequest`'s gate in front of it, as a
    /// [`crate::port_handle::PortHandle`] holds them.
    struct GatedTx {
        tx: mpsc::UnboundedSender<ActorMessage>,
        gate: QueueGate,
        multi_device: bool,
    }

    fn spawn_gated(driver: impl PortDriver) -> GatedTx {
        let gate = driver.base().queue_gate();
        let multi_device = driver.base().flags.multi_device;
        GatedTx {
            tx: spawn_actor(driver),
            gate,
            multi_device,
        }
    }

    /// [`send_and_wait`] behind `queueRequest`'s refusals; `send_and_wait`
    /// alone is a request that was queued before the port's state changed.
    fn queue_and_wait(port: &GatedTx, op: RequestOp, user: AsynUser) -> AsynResult<RequestResult> {
        queue_request_gate(&port.gate, &op, &user, port.multi_device)?;
        send_and_wait(&port.tx, op, user)
    }

    fn send_and_wait(
        tx: &mpsc::UnboundedSender<ActorMessage>,
        op: RequestOp,
        user: AsynUser,
    ) -> AsynResult<RequestResult> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = ActorMessage::new(op, user, CancelToken::new(), reply_tx);
        tx.send(msg).expect("actor channel closed");
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

    /// A driver that writes the parameter at the address it is handed and
    /// calls back on it, as specs-analyser's `write_int32` does.
    struct AddrDriver {
        base: PortDriverBase,
        seen: Log<i32>,
    }

    impl PortDriver for AddrDriver {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
            self.seen.lock().push(user.addr);
            self.base.set_int32_param(user.reason, user.addr, value)?;
            self.base.call_param_callbacks(user.addr)
        }
    }

    type Log<T> = Arc<parking_lot::Mutex<Vec<T>>>;

    /// An [`AddrDriver`] port with interrupts accepted: the addresses its
    /// driver was handed, and the `(addr, value)` of every callback a list-0
    /// client of `VAL` received.
    struct AddrPort {
        tx: mpsc::UnboundedSender<ActorMessage>,
        seen: Log<i32>,
        got: Log<(i32, i32)>,
        _sub: crate::interrupt::SyncCallbackSubscription,
    }

    fn addr_port(max_addr: usize) -> AddrPort {
        use crate::interrupt::{InterruptFilter, InterruptValue};
        use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;
        let mut base = PortDriverBase::new(
            "addr_test",
            max_addr,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.own_interrupts(InterruptGate::opened());
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        let got = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = got.clone();
        let sub = base
            .interrupts
            .register_sync_callback(
                InterruptFilter {
                    reason: Some(val),
                    addr: Some(0),
                    uint32_mask: None,
                    iface: None,
                    ..Default::default()
                },
                move |iv: &InterruptValue| {
                    sink.lock().push((iv.addr, iv.value.as_int32().unwrap()))
                },
            )
            .unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let driver = AddrDriver {
            base,
            seen: seen.clone(),
        };
        AddrPort {
            tx: spawn_actor(driver),
            seen,
            got,
            _sub: sub,
        }
    }

    fn write_at(tx: &mpsc::UnboundedSender<ActorMessage>, addr: i32, value: i32) -> AsynResult<()> {
        let user = AsynUser::new(0)
            .with_addr(addr)
            .with_timeout(Duration::from_secs(1));
        send_and_wait(tx, RequestOp::Int32Write { value }, user).map(|_| ())
    }

    fn read_at(tx: &mpsc::UnboundedSender<ActorMessage>, addr: i32) -> AsynResult<Option<i32>> {
        let user = AsynUser::new(0)
            .with_addr(addr)
            .with_timeout(Duration::from_secs(1));
        send_and_wait(tx, RequestOp::Int32Read, user).map(|r| r.int_val)
    }

    /// C `getAddr` is -1 on a port without `ASYN_MULTIDEVICE` whatever device
    /// the user names (asynManager.c:2005-2009), so a driver reading the
    /// address it is handed uses list 0 and its callback reaches list 0.
    #[test]
    fn a_single_device_driver_is_handed_getaddrs_minus_one() {
        let AddrPort {
            tx,
            seen,
            got,
            _sub,
        } = addr_port(1);
        write_at(&tx, 1, 11).unwrap();
        write_at(&tx, 0, 12).unwrap();
        write_at(&tx, -1, 13).unwrap();
        assert_eq!(*seen.lock(), vec![-1, -1, -1]);
        assert_eq!(*got.lock(), vec![(0, 11), (0, 12), (0, 13)]);
        assert_eq!(read_at(&tx, 3).unwrap(), Some(13));
    }

    /// On a multi-device port the driver is handed the device's address, and
    /// one past `max_addr` is refused as C's `setIntegerParam` refuses it.
    #[test]
    fn a_multi_device_driver_is_handed_the_device_address() {
        let AddrPort {
            tx,
            seen,
            got,
            _sub,
        } = addr_port(2);
        write_at(&tx, 1, 21).unwrap();
        write_at(&tx, 0, 22).unwrap();
        write_at(&tx, -1, 23).unwrap();
        assert!(matches!(
            write_at(&tx, 2, 24),
            Err(AsynError::ParamInvalidList(2))
        ));
        assert_eq!(*seen.lock(), vec![1, 0, -1, 2]);
        // List 1's callback is not list 0's; -1 calls back on list 0.
        assert_eq!(*got.lock(), vec![(0, 22), (0, 23)]);
        assert_eq!(read_at(&tx, 1).unwrap(), Some(21));
        assert_eq!(read_at(&tx, 0).unwrap(), Some(23));
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
    fn actor_read_carries_the_port_time_stamp() {
        // C readInt32 sets pasynUser->timestamp from getTimeStamp
        // (asynPortDriver.cpp:1970-1979), never a fresh clock.
        let mut drv = TestDriver::new();
        drv.base.params.set_int32(0, 0, 5).unwrap();
        let ts = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_234_567);
        drv.base.set_time_stamp(ts);
        let tx = spawn_actor(drv);

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user).unwrap();
        assert_eq!(result.timestamp, Some(ts));
    }

    #[test]
    fn actor_read_surfaces_param_status_alarm() {
        use super::alarm as al;
        // A defined param flagged setParamStatus(Error) with no explicit
        // setParamAlarm: the read must surface READ_ALARM/INVALID (C
        // devAsynInt32.c:844-847), not a clean (UDF-clearing) read.
        let mut drv = TestDriver::new();
        drv.base.params.set_int32(0, 0, 5).unwrap(); // define VAL
        drv.base.params.set_status(0, 0, AsynStatus::Error).unwrap();
        let tx = spawn_actor(drv);

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let e = send_and_wait(&tx, RequestOp::Int32Read, user).unwrap_err();
        // The read fails with the parameter's status and keeps its value, as
        // C getIntegerParam returns both (asynPortDriver.cpp:307-309).
        assert_eq!(e.status(), AsynStatus::Error);
        assert_eq!(
            e.message(),
            "asynPortDriver:readInt32: status=3, function=0, name=VAL, value=5"
        );
        let result = e.failed_read().unwrap();
        assert_eq!(result.int_val, Some(5));
        assert_eq!(result.alarm_status, al::READ_ALARM);
        assert_eq!(result.alarm_severity, al::SEV_INVALID);
    }

    /// A parameter's status fails an octet read inside the read's own status,
    /// the copied string kept (asynPortDriver.cpp:2615-2623).
    #[test]
    fn actor_octet_read_fails_on_param_status() {
        let mut drv = TestDriver::new();
        drv.base.params.set_string(2, 0, "abc").unwrap();
        drv.base
            .params
            .set_status(2, 0, AsynStatus::Timeout)
            .unwrap();
        let tx = spawn_actor(drv);

        let user = AsynUser::new(2).with_timeout(Duration::from_secs(1));
        let op = crate::request::OctetReadOp::Read { buf_size: 40 }.into_op();
        let read = crate::request::octet_outcome(send_and_wait(&tx, op, user));
        let (data, status) = read.into_parts();
        assert_eq!(data, b"abc\0");
        let e = status.unwrap_err();
        assert_eq!(e.status(), AsynStatus::Timeout);
        assert_eq!(
            e.message(),
            "asynPortDriver:readOctet: status=1, function=2, name=MSG, value=abc"
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
        let result =
            send_and_wait(&tx, OctetReadOp::Read { buf_size: 256 }.into_op(), user).unwrap();
        let (data, status) = result.into_read().into_parts();
        status.unwrap();
        assert_eq!(&data[..5], b"hello");
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
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                *self.read_eos.lock() = self.base().input_eos(_user.addr).to_vec();
                let resp = [0x01u8, 0x02];
                let n = resp.len().min(buf.len());
                buf[..n].copy_from_slice(&resp[..n]);
                crate::interpose::OctetReadResult::ok(n, crate::interpose::EomReason::empty())
            }
        }

        let write_eos = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let read_eos = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut eos_base = PortDriverBase::new(
            "eos_test",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
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
        let read = send_and_wait(
            &tx,
            OctetReadOp::ReadBinary { buf_size: 16 }.into_op(),
            mk(),
        );
        read.unwrap().into_read().status().as_ref().unwrap();
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
        let read = send_and_wait(&tx, OctetReadOp::Read { buf_size: 16 }.into_op(), mk());
        read.unwrap().into_read().status().as_ref().unwrap();
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
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                self.sequence.lock().push("read");
                let resp = b"RSP";
                let n = resp.len().min(buf.len());
                buf[..n].copy_from_slice(&resp[..n]);
                crate::interpose::OctetReadResult::ok(n, crate::interpose::EomReason::empty())
            }
        }

        let flush_calls = Arc::new(AtomicUsize::new(0));
        let write_calls = Arc::new(AtomicUsize::new(0));
        let sequence = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let drv = FlushTracker {
            base: PortDriverBase::new(
                "flush_test",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            ),
            flush_calls: flush_calls.clone(),
            write_calls: write_calls.clone(),
            sequence: sequence.clone(),
        };
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(
            &tx,
            OctetReadOp::WriteRead {
                data: b"CMD".to_vec(),
                buf_size: 16,
            }
            .into_op(),
            user,
        )
        .unwrap();
        let (data, status) = result.into_read().into_parts();
        status.unwrap();
        assert_eq!(data, b"RSP");
        assert_eq!(flush_calls.load(Ordering::Relaxed), 1, "flush called once");
        assert_eq!(write_calls.load(Ordering::Relaxed), 1);
        // Order must be: flush, then write, then read.
        let seq = sequence.lock().clone();
        assert_eq!(seq, vec!["flush", "write", "read"]);
    }

    /// C parity: `asynOctet::read` returns `nbytes` together with
    /// `int *eomReason` (`interfaces/asynOctet.h:38-40`). Drivers
    /// that override `io_read_octet` must have their EOM flags
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
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                let payload = b"ABC";
                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                // Driver explicitly reports both EOS match and END indicator
                // (e.g. a GPIB END line plus terminator detection).
                crate::interpose::OctetReadResult::ok(n, EomReason::EOS | EomReason::END)
            }
        }
        let drv = EomDriver {
            base: PortDriverBase::new(
                "eom_test",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            ),
        };
        let tx = spawn_actor(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let read = send_and_wait(&tx, OctetReadOp::Read { buf_size: 16 }.into_op(), user)
            .unwrap()
            .into_read();
        let eom = read.eom();
        let (data, status) = read.into_parts();
        status.unwrap();
        assert_eq!(data, b"ABC");
        assert!(eom.contains(EomReason::EOS));
        assert!(eom.contains(EomReason::END));
        assert!(!eom.contains(EomReason::CNT));
    }

    /// A "\r\n" split across two OctetReads: the second reply takes back
    /// the "\r" the first delivered (asynInterposeEos.c:201-204).
    #[test]
    fn actor_octet_read_carries_eos_taken_back() {
        use crate::interpose::eos::{EosConfig, EosInterpose};

        struct LineDriver {
            base: PortDriverBase,
        }
        impl PortDriver for LineDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                buf[..4].copy_from_slice(b"AB\r\n");
                crate::interpose::OctetReadResult::ok(4, crate::interpose::EomReason::empty())
            }
        }
        let mut drv = LineDriver {
            base: PortDriverBase::new(
                "takeback_test",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            ),
        };
        drv.base
            .install_octet_interpose(Box::new(EosInterpose::new(EosConfig {
                input_eos: b"\r\n".to_vec(),
                output_eos: Vec::new(),
            })));
        let tx = spawn_actor(drv);
        let user = || AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let read = || {
            let read = send_and_wait(&tx, OctetReadOp::Read { buf_size: 3 }.into_op(), user());
            let read = read.unwrap().into_read();
            (read.nbytes_transferred(), read.taken_back())
        };
        assert_eq!(read(), (3, 0));
        assert_eq!(read(), (0, 1));
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
                cancel: Some(CancelToken::new()),
                reply: Reply::Result(reply),
                seq,
                priority: QueuePriority::Medium,
                rank: Rank::Band(QueuePriority::Medium),
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
    /// asynManager.c:813-857), and every C caller of connect/disconnect
    /// queues there — `asynCommonSyncIO` passes the constant
    /// (asynCommonSyncIO.c:142, :154), `asynRecord` derives it from the
    /// callback type (asynRecord.c:561-565). Here the band comes from
    /// [`PortActor::c_dispatch`] for the same reason, so a `CNCT` put built
    /// on a `#[default]` Medium user still overtakes a sustained High stream
    /// instead of starving behind it.
    #[test]
    fn a_connect_overtakes_a_sustained_high_stream() {
        let mk = |op: RequestOp, user: AsynUser| {
            let (reply, _rx) = oneshot::channel();
            ActorMessage::new(op, user, CancelToken::new(), reply)
        };
        // Bands exist only on a CANBLOCK port.
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut actor = PortActor::new(Box::new(multi_device_driver(true)), rx);

        // A High scan stream already queued...
        for _ in 0..8 {
            actor.enqueue_message(mk(
                RequestOp::Int32Read,
                AsynUser::new(0).with_priority(QueuePriority::High),
            ));
        }
        // ...and then the operator's CNCT put, exactly as
        // `PortHandle::connect_blocking` builds it: no explicit priority.
        let connect_seq = {
            let m = mk(RequestOp::Connect, AsynUser::new(0).with_addr(-1));
            let seq = m.seq;
            actor.enqueue_message(m);
            seq
        };
        // ...and more of the stream behind it.
        for _ in 0..8 {
            actor.enqueue_message(mk(
                RequestOp::Int32Read,
                AsynUser::new(0).with_priority(QueuePriority::High),
            ));
        }

        let first = actor.heap.pop().unwrap();
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

    /// On a CANBLOCK port C `lockPort` and the other direct calls never join
    /// `queueRequest`'s bands, so they wait only for the request in progress:
    /// a Direct op pops before any queued one, at any band, and Direct ops
    /// keep FIFO order. On any other port C runs every request at once in the
    /// caller's thread (asynManager.c:1555-1584): no bands, arrival order only.
    #[test]
    fn a_port_without_canblock_keeps_arrival_order() {
        let mk = |op: RequestOp, priority: QueuePriority| {
            let (reply, _rx) = oneshot::channel();
            ActorMessage::new(
                op,
                AsynUser::new(0).with_priority(priority),
                CancelToken::new(),
                reply,
            )
        };
        let order = |can_block: bool| {
            let (_tx, rx) = mpsc::unbounded_channel();
            let mut actor = PortActor::new(Box::new(multi_device_driver(can_block)), rx);
            let mut seqs = Vec::new();
            for msg in [
                mk(RequestOp::Int32Read, QueuePriority::High),
                mk(RequestOp::Connect, QueuePriority::Medium),
                mk(
                    RequestOp::LockPort(crate::request::PortLockCall::spent()),
                    QueuePriority::Low,
                ),
                mk(RequestOp::GetConnected, QueuePriority::Low),
                mk(RequestOp::Int32Read, QueuePriority::High),
            ] {
                seqs.push(msg.seq);
                actor.enqueue_message(msg);
            }
            let popped: Vec<usize> = std::iter::from_fn(|| actor.heap.pop())
                .map(|m| seqs.iter().position(|&s| s == m.seq).unwrap())
                .collect();
            popped
        };
        assert_eq!(order(true), vec![2, 3, 1, 0, 4]);
        assert_eq!(order(false), vec![0, 1, 2, 3, 4]);
    }

    /// C's interrupt-edge sweep finds the requests queued before the edge
    /// already run (asynPortDriver.cpp:923-937): it pops behind every one of
    /// them, at any band, and ahead of a Low request queued after it. A
    /// direct call still overtakes them all.
    #[test]
    fn a_sweep_pops_behind_what_was_queued_before_it() {
        let mk = |op: RequestOp, priority: QueuePriority| {
            let (reply, _rx) = oneshot::channel();
            ActorMessage::new(
                op,
                AsynUser::new(0).with_priority(priority),
                CancelToken::new(),
                reply,
            )
        };
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut actor = PortActor::new(Box::new(multi_device_driver(true)), rx);
        let mut seqs = Vec::new();
        for msg in [
            mk(RequestOp::Int32Write { value: 1 }, QueuePriority::Low),
            mk(RequestOp::Int32Read, QueuePriority::High),
            mk(
                RequestOp::WithDriverAfterQueue(DriverCall::spent()),
                QueuePriority::Medium,
            ),
            mk(RequestOp::Int32Read, QueuePriority::Low),
            mk(
                RequestOp::WithDriver(DriverCall::spent()),
                QueuePriority::Low,
            ),
        ] {
            seqs.push(msg.seq);
            actor.enqueue_message(msg);
        }
        let popped: Vec<usize> = std::iter::from_fn(|| actor.heap.pop())
            .map(|m| seqs.iter().position(|&s| s == m.seq).unwrap())
            .collect();
        assert_eq!(popped, vec![4, 1, 0, 2, 3]);
    }

    /// What the port thread leaves queued does not hold the sweep up, since
    /// C's `callbackThread` is no `queueRequest` (asynPortDriver.cpp:923-937):
    /// it runs on a port that is down or disabled, past a disabled device's
    /// parked request and a dead device's timed-out one, and under another
    /// user's block.
    #[test]
    fn a_sweep_runs_past_what_the_port_thread_leaves_queued() {
        // A Low write queued ahead, then the sweep: whether it ran, and the
        // write's reply as the sweep left it, read while `tx` keeps the actor
        // alive (an exiting actor drops a parked write unanswered).
        let behind = |tx: &mpsc::UnboundedSender<ActorMessage>, addr: i32| {
            let mut ahead = send(
                tx,
                RequestOp::Int32Write { value: 9 },
                AsynUser::new(0)
                    .with_addr(addr)
                    .with_priority(QueuePriority::Low),
            );
            let (ran_tx, ran) = std::sync::mpsc::channel();
            let call = DriverCall::new(move |_| {
                let _ = ran_tx.send(());
            });
            let _reply = send(
                tx,
                RequestOp::WithDriverAfterQueue(call),
                AsynUser::default(),
            );
            // Bounded, so a sweep that parks fails the case instead of hanging.
            let ran = ran.recv_timeout(Duration::from_secs(5)).is_ok();
            (ran, ahead.try_recv())
        };
        let still_queued =
            |write: Result<_, _>| matches!(write, Err(oneshot::error::TryRecvError::Empty));

        let mut drv = multi_device_driver(true);
        drv.base.init_auto_connect(false);
        drv.base.init_connected(false);
        let (ran, write) = behind(&spawn_actor(drv), 0);
        assert!(ran && still_queued(write), "port down");

        let mut drv = multi_device_driver(true);
        drv.base.set_enabled(false).unwrap();
        let (ran, write) = behind(&spawn_actor(drv), 0);
        assert!(ran && still_queued(write), "port disabled");

        let mut drv = multi_device_driver(true);
        drv.base.disable_addr(1);
        let (ran, write) = behind(&spawn_actor(drv), 1);
        assert!(ran && still_queued(write), "device disabled");

        let mut drv = multi_device_driver(true);
        drv.base.init_auto_connect(false);
        drv.base.init_connected(true);
        drv.base.device_state(1).put_auto_connect(false);
        drv.base.device_state(1).put_connected(false);
        let (ran, write) = behind(&spawn_actor(drv), 1);
        assert!(ran, "device down");
        assert!(
            write.unwrap().unwrap_err().is_queue_timeout(),
            "the dead device's write is answered ahead of the sweep"
        );

        let tx = spawn_actor(multi_device_driver(true));
        let mut owner = AsynUser::new(0);
        owner.block_token = Some(42);
        send_and_wait(&tx, RequestOp::BlockProcess { all_devices: true }, owner).unwrap();
        // The holder is taken when the owner's next callback returns.
        let mut owner = AsynUser::new(0);
        owner.block_token = Some(42);
        send_and_wait(&tx, RequestOp::Int32Read, owner).unwrap();
        let (ran, write) = behind(&tx, 0);
        assert!(ran && still_queued(write), "another user's block");
    }

    #[test]
    fn heap_pops_higher_priority_first() {
        // Priority dominates the seq FIFO tiebreaker.
        let mk = |seq: u64, priority: QueuePriority| {
            let (reply, _rx) = oneshot::channel();
            ActorMessage {
                op: RequestOp::Int32Read,
                user: AsynUser::new(0),
                cancel: Some(CancelToken::new()),
                reply: Reply::Result(reply),
                seq,
                priority,
                rank: Rank::Band(priority),
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
        tx.send(msg).unwrap();
        let result = reply_rx.blocking_recv().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn actor_dequeued_request_runs_despite_elapsed_timeout() {
        // C parity: a request that reaches the head of the queue always
        // executes I/O — the queue-wait timer (when armed at all) is
        // cancelled at dequeue (asynManager.c:908), and standard device
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
        drv.base.set_enabled(false).unwrap();
        let tx = spawn_gated(drv);
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = queue_and_wait(&tx, RequestOp::Int32Read, user);
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
        // owner itself (C `enable`, asynManager.c:2238-2243). A defunct
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
        // `queueRequest` (asynManager.c:2238-2243).
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
        // A request's auto-connect is `portThread`'s (asynManager.c:858-863):
        // a synchronous port's `queueRequest` refuses a down port (:1549).
        drv.base.flags.can_block = true;
        drv.base.init_connected(false);
        drv.base.init_auto_connect(true);
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
        // Without the 2s throttle (C autoConnectDevice, asynManager.c:715),
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
        let flags = PortFlags {
            can_block: true,
            ..PortFlags::default()
        };
        let mut base = PortDriverBase::new(
            "throttle_test",
            1,
            flags,
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.create_param("VAL", ParamType::Int32).unwrap();
        base.init_connected(false);
        base.init_auto_connect(true);
        let drv = ThrottleDriver {
            base,
            connect_calls: connect_calls.clone(),
        };
        let tx = spawn_actor_past_registration(drv);
        // Registration's attempt and its pass's own (asynManager.c:2135,
        // :858-863).
        let registered = connect_calls.load(Ordering::SeqCst);
        assert_eq!(registered, 2);

        // Three reads already queued when the port dropped. Each dequeue, and
        // each rescan of the parked ones, asks autoConnectDevice
        // (asynManager.c:879); the port never comes back, so they stay queued.
        let replies: Vec<_> = (0..3)
            .map(|_| send(&tx, RequestOp::Int32Read, AsynUser::new(0)))
            .collect();
        let start = Instant::now();
        while connect_calls.load(Ordering::SeqCst) == registered {
            assert!(start.elapsed() < Duration::from_secs(5), "no attempt");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(100));

        // Only the first attempt fired; every later one fell inside the 2s
        // throttle window (C autoConnectDevice 2.0s gate, asynManager.c:714-715).
        assert_eq!(connect_calls.load(Ordering::SeqCst), registered + 1);
        for mut reply in replies {
            assert!(reply.try_recv().is_err(), "still queued, not refused");
        }
    }

    /// A connection-waived request is followed by the port auto-connect C's
    /// `portThread` performs.
    ///
    /// C queues the waived requests — `asynSetOption`/`asynSetEos` from
    /// `st.cmd`, the record's HOSTINFO repoint (asynRecord.c:566-569) — at
    /// `asynQueuePriorityConnect`. `portThread` drains that queue and then, at
    /// asynManager.c:858-863, runs `autoConnectDevice(pport,0)` whenever the
    /// port is still disconnected. That is what brings a dead line up on its
    /// *new* configuration immediately, instead of leaving it down until some
    /// later I/O request happens to force a connect.
    ///
    /// Boundary 1 (here): the waived request runs on the dead line AND the port
    /// connect attempt follows it.
    /// Boundary 2 (below): an explicit Disconnect is not undone by the same
    /// pass — C's 2 s throttle (:714-715) anchors on the transition it just
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
        let mut base = PortDriverBase::new(
            "waived_test",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.init_connected(false);
        base.init_auto_connect(true);
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
             queue has drained (asynManager.c:858-862)"
        );
    }

    /// Boundary 2 of the above: the auto-connect that follows a waived request
    /// must not undo an explicit Disconnect. C's throttle (asynManager.c:714-715)
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
        let mut base = PortDriverBase::new(
            "disconnect_test",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.init_connected(true);
        base.init_auto_connect(true);
        let tx = spawn_actor(DisconnectDriver {
            base,
            connect_calls: connect_calls.clone(),
        });

        send_and_wait(&tx, RequestOp::Disconnect, AsynUser::default())
            .expect("an explicit disconnect succeeds");

        // The auto-connect the Disconnect must NOT trigger runs on the actor's
        // tail after the reply, so a probe is needed as a sequencing barrier
        // before the counter can be trusted: its reply proves the tail ran.
        // The probe is a direct manager call, so it asks no auto-connect.
        send_and_wait(&tx, RequestOp::GetAutoConnect, AsynUser::new(0)).unwrap();

        assert_eq!(
            connect_calls.load(Ordering::SeqCst),
            0,
            "the transition the Disconnect just stamped keeps the auto-connect \
             inside its 2 s window (asynManager.c:714-715)"
        );
    }

    /// C's **port-level** auto-connect reaches the driver with `addr == -1`:
    /// `autoConnectDevice` calls `connectAttempt(&pport->dpc)`
    /// (asynManager.c:718) with `pdevice == 0`, and `connectAttempt` derives
    /// `addr = (pdevice ? pdevice->addr : -1)` (:754) before handing that user
    /// to `pasynCommon->connect` (:777). The device-level attempt is the
    /// separate `connectAttempt(&pdevice->dpc)` (:735), which carries the real
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

        let flags = PortFlags {
            can_block: true,
            ..PortFlags::default()
        };
        let mut base = PortDriverBase::new(
            "addr_ac",
            1,
            flags,
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.create_param("VAL", ParamType::Int32).unwrap();
        base.params.set_int32(0, 0, 7).unwrap();
        base.init_connected(false);
        base.init_auto_connect(true);
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
            "the port-level connect must carry addr -1 (C connectAttempt, asynManager.c:754)"
        );
    }

    /// R6-49: C `autoConnectDevice` reconnects the PORT first
    /// (`connectAttempt(&pport->dpc)`, asynManager.c:718) and only then the
    /// device (:725-739). A multi-device port whose transport dropped used to
    /// call `connect_addr` alone — whose default merely flips the per-address
    /// flag and opens no socket — so the port-level `check_ready` rejected
    /// every subsequent request forever.
    ///
    /// Boundary 1 (here): the port's connect succeeds, so the device connect
    /// runs after it and the request goes through.
    /// Boundary 2 (below): the port's connect fails, so the device connect
    /// must NOT run (C :723 `return FALSE`).
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
                    can_block: true,
                    ..PortFlags::default()
                },
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.params.set_int32(0, 1, 7).unwrap();
            // The transport dropped: port down, and the device with it.
            base.init_connected(false);
            base.init_auto_connect(true);
            base.device_state(1).put_connected(false);
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
            "the PORT must be reconnected before the device (C asynManager.c:718 then :725)"
        );

        // Boundary 2: the port stays down → C bails at :723 before touching
        // the device, and the request fails Disconnected.
        let (drv, calls) = mk(false);
        let tx = spawn_actor(drv);
        let mut reply = send(&tx, RequestOp::Int32Read, AsynUser::new(0).with_addr(1));
        // A direct call outranks every band (`Rank::Direct`), so a probe that
        // lands in the same drain as the read is answered before it: wait for
        // the port attempt, then probe to let the pass it ran in finish.
        let deadline = Instant::now() + Duration::from_secs(5);
        while calls.lock().is_empty() {
            assert!(Instant::now() < deadline, "no port connect was attempted");
            std::thread::sleep(Duration::from_millis(1));
        }
        send_and_wait(&tx, RequestOp::GetAutoConnect, AsynUser::new(0)).unwrap();
        assert!(reply.try_recv().is_err(), "the port is down: still queued");
        // Registration's attempt, then the request's own (asynManager.c:718).
        assert_eq!(
            calls.lock().clone(),
            vec!["connect", "connect"],
            "a device connect must not be attempted while the port is still down"
        );
    }

    /// R6-47: an auto-connect port that drops must reconnect on its own, with
    /// NO queued traffic. C `exceptionDisconnect` arms the port's connect
    /// timer at .01 s (asynManager.c:2183-2184); `portConnectTimerCallback`
    /// then posts a Connect-priority request (:3283-3297) and
    /// `portConnectProcessCallback` calls the driver's connect (:3299-3314).
    /// Before this, `port_actor`'s only auto-connect attempt lived inside
    /// `process_one` — it fired only when a request was dequeued, so an
    /// I/O-Intr-only or quiescent port stayed dead forever.
    ///
    /// Uses the real runtime (`create_port_runtime`), because the timer lives
    /// in the actor's idle `select!` — the whole point is that nothing else
    /// wakes it.
    #[test]
    fn idle_auto_connect_port_reconnects_without_queued_traffic() {
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
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
        let mut base = PortDriverBase::new(
            "retry_test",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.create_param("VAL", ParamType::Int32).unwrap();
        base.init_auto_connect(true);
        // Shorten C's 20 s back-off (asynManager.c:48) so the second attempt
        // lands inside the test. The first attempt always uses C's .01 s.
        base.seconds_between_port_connect = Duration::from_millis(30);
        let drv = FlappyDriver {
            base,
            connect_calls: connect_calls.clone(),
            link_up: link_up.clone(),
            fail_first: 1,
        };
        let (handle, _jh) = create_port_runtime_unclaimed(drv, RuntimeConfig::default())
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
            "the failed first attempt must re-arm the timer (C asynManager.c:3312), got {} call(s)",
            connect_calls.load(Ordering::SeqCst)
        );
    }

    /// R14-50: no connect attempt on a port the queue gate would refuse.
    ///
    /// C's retry timer does not call `pasynCommon->connect` — it posts a
    /// `queueRequest` (asynManager.c:3290), which a disabled or defunct port
    /// refuses with `asynDisabled` (:1543-1548); the process callback that would
    /// re-arm the timer (:3312) never runs. `portThread` applies the same
    /// refusal above `autoConnectDevice` (:808-811). So `asynEnable(port,0)` and
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
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.init_auto_connect(true);
            base.set_connected(false);
            base.set_enabled(enabled).unwrap();
            if defunct {
                // C `shutdownPort` clears `enabled` on its way to `defunct`
                // (:2285-2286), so the gate refuses the port as a disabled one.
                shut_down(&mut base);
            }
            base.connect_retry_at = Some(Instant::now() - Duration::from_millis(1));
            let drv = CountingDriver {
                base,
                connect_calls: calls.clone(),
                connect_addr_calls: addr_calls.clone(),
            };
            let (_tx, rx) = mpsc::unbounded_channel();
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
                "{label}: a refused queueRequest never re-arms the timer (C :3312)"
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

    /// C `connectAttempt` reports through `reportConnectStatus`
    /// (asynManager.c:469-478, :778-786): one error-trace line per change of
    /// outcome, the port's and its devices' attempts sharing the one
    /// `previousConnectStatus`, and the retry timer's attempt
    /// (`portConnectProcessCallback`, :3299) reports nothing. It prints
    /// through `pasynTrace->vprint` on the user `connectAttempt` connected to
    /// the device (:747-755), so the source is C's `[:0]`.
    #[test]
    fn auto_connect_reports_once_per_change_of_outcome() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Flaky {
            base: PortDriverBase,
            fail: Arc<AtomicBool>,
        }
        impl PortDriver for Flaky {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                if self.fail.load(Ordering::SeqCst) {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "no line".into(),
                    });
                }
                self.base.set_connected(true);
                Ok(())
            }
            fn connect_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
                if self.fail.load(Ordering::SeqCst) {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "no device".into(),
                    });
                }
                self.base.connect_addr(user.addr);
                Ok(())
            }
        }

        let services = crate::services::PortServices::new();
        let mut base = PortDriverBase::new(
            "acrep",
            2,
            PortFlags {
                multi_device: true,
                ..PortFlags::default()
            },
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        services.bind(&mut base);
        let trace = services.trace();
        trace.register_port("acrep", true);
        trace.set_trace_mask(Some("acrep"), crate::trace::TraceMask::ERROR);
        trace.set_trace_info_mask(
            Some("acrep"),
            crate::trace::TraceInfoMask::PORT | crate::trace::TraceInfoMask::SOURCE,
        );
        let dir = tempfile::tempdir().expect("fixture root");
        let path = dir.path().join("acrep.log");
        trace.set_trace_file(
            Some("acrep"),
            crate::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(
                std::fs::File::create(&path).unwrap(),
            ))),
        );
        base.init_auto_connect(true);
        base.set_connected(false);
        let fail = Arc::new(AtomicBool::new(true));
        let drv = Flaky {
            base,
            fail: fail.clone(),
        };
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut actor = PortActor::new(Box::new(drv), rx);
        let log = || std::fs::read_to_string(&path).unwrap();
        // Each attempt outside the 2 s throttle window.
        let attempt = |actor: &mut PortActor| {
            let past = Instant::now() - Duration::from_secs(3);
            actor.driver.base_mut().stamp_auto_connect_attempt(-1, past);
            actor.driver.base_mut().stamp_auto_connect_attempt(1, past);
            actor.auto_connect_device(1, 0)
        };

        actor.driver.base_mut().connect_retry_at = Some(Instant::now() - Duration::from_millis(1));
        actor.service_connect_timer();
        assert_eq!(log(), "", "the retry timer's attempt is silent");

        assert!(!attempt(&mut actor));
        assert!(!attempt(&mut actor));
        assert_eq!(
            log()
                .matches("[acrep,-1,0] [:0] acrep -1 autoConnect could not connect: no line\n")
                .count(),
            1,
            "{:?}",
            log()
        );

        // The port comes up; then its device fails and comes up.
        fail.store(false, Ordering::SeqCst);
        let past = Instant::now() - Duration::from_secs(3);
        actor.driver.base_mut().stamp_auto_connect_attempt(-1, past);
        assert!(actor.auto_connect_port());
        assert_eq!(
            log()
                .matches("[acrep,-1,0] [:0] acrep -1 port is now connected\n")
                .count(),
            1
        );
        fail.store(true, Ordering::SeqCst);
        assert!(!attempt(&mut actor));
        assert_eq!(
            log()
                .matches("[acrep,1,0] [:0] acrep 1 autoConnect could not connect: no device\n")
                .count(),
            1,
            "{:?}",
            log()
        );
        fail.store(false, Ordering::SeqCst);
        assert!(attempt(&mut actor));
        assert!(attempt(&mut actor));
        assert_eq!(
            log()
                .matches("[acrep,1,0] [:0] acrep 1 port is now connected\n")
                .count(),
            1
        );
        assert_eq!(log().lines().count(), 4, "{:?}", log());
    }

    /// Boundary: auto-connect OFF. C arms the timer only when
    /// `pport->dpc.autoConnect` is set (asynManager.c:2183), so a manual port
    /// that drops must stay down until something explicitly connects it.
    #[test]
    fn disconnect_does_not_arm_retry_when_auto_connect_is_off() {
        let mut base = PortDriverBase::new(
            "no_auto",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.init_auto_connect(false);
        base.init_connected(true);
        assert!(base.set_connected(false));
        assert!(
            base.connect_retry_at.is_none(),
            "a non-auto-connect port must not schedule an autonomous reconnect"
        );

        // ...and with auto-connect on, the same edge arms it.
        let mut base = PortDriverBase::new(
            "auto",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.init_auto_connect(true);
        base.init_connected(true);
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
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
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
        tx: &mpsc::UnboundedSender<ActorMessage>,
        op: RequestOp,
        user: AsynUser,
    ) -> oneshot::Receiver<AsynResult<RequestResult>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(ActorMessage::new(op, user, CancelToken::new(), reply_tx))
            .expect("actor channel closed");
        reply_rx
    }

    /// R15-47, boundary 1: on a CANBLOCK port a disabled device's request
    /// **parks** — it keeps its place in the queue and runs when the device is
    /// re-enabled.
    ///
    /// C's device-level refusals live inside `if(!(pport->attributes &
    /// ASYN_CANBLOCK))` in `queueRequest` (asynManager.c:1555-1585), so a
    /// CANBLOCK port never returns them. Its `portThread` answers the same
    /// condition by skipping the request — `if(!pdpCommon->enabled) continue;`
    /// (:877) — leaving it queued. Every real transport port is CANBLOCK.
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
        send_and_wait(
            &tx,
            RequestOp::SetEnable { yes: true },
            AsynUser::new(0).with_addr(1),
        )
        .unwrap();
        let r = parked
            .blocking_recv()
            .expect("actor dropped the parked reply")
            .expect("the parked request runs once its device is enabled");
        assert_eq!(r.int_val, Some(7));
    }

    /// R15-47, boundary 2: a parked request still expires on its own queue
    /// timer. C arms an `epicsTimer` at `queueRequest` (asynManager.c:1619-1625)
    /// whose callback unlinks the request wherever the port thread left it
    /// (:649-703) — so a device that is never re-enabled does not hang the
    /// caller, it times out.
    ///
    /// Uses the real runtime: the deadline of a parked request has no other
    /// clock (a blocking caller with no Tokio reactor arms no timer of its own),
    /// so the actor's own `select!` must wake for it.
    #[test]
    fn a_parked_request_still_times_out_on_its_queue_timer() {
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        let mut drv = multi_device_driver(true);
        drv.base.disable_addr(1);
        let (handle, _jh) = create_port_runtime_unclaimed(drv, RuntimeConfig::default())
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
        // C's double space, verbatim (asynManager.c:1566).
        assert!(
            err.to_string()
                .contains("port dev_gate  or device 1 not enabled"),
            "got {err}"
        );
    }

    /// R15-47, boundary 4: a disconnected device on a CANBLOCK port. C's
    /// `portThread` auto-connects it (:879) and, if it is still down, hands the
    /// request to its *timeout* callback (:886-887, :917-921) — it does not
    /// refuse it with `asynDisconnected`, which is what `queueRequest` would have
    /// done had this been a synchronous port.
    #[test]
    fn a_disconnected_device_on_a_canblock_port_takes_the_timeout_path() {
        let mut drv = multi_device_driver(true);
        // No auto-connect: nothing can bring the device back, so the request
        // reaches the timeout path rather than the connect that precedes it.
        // The port itself is up, as a manual connect would leave it.
        drv.base.init_auto_connect(false);
        drv.base.init_connected(true);
        drv.base.device_state(1).put_auto_connect(false);
        drv.base.device_state(1).put_connected(false);
        let tx = spawn_actor(drv);

        let err = send_and_wait(&tx, RequestOp::Int32Read, AsynUser::new(0).with_addr(1))
            .expect_err("a device that will not connect cannot serve the read");
        assert!(
            err.is_queue_timeout(),
            "C portThread calls the request's timeoutUser, not queueRequest's \
             asynDisconnected refusal: {err:?}"
        );

        // The synchronous port is the one that answers with the refusal, and its
        // text is C's (:1575).
        let mut drv = multi_device_driver(false);
        drv.base.init_auto_connect(false);
        drv.base.init_connected(true);
        drv.base.device_state(1).put_auto_connect(false);
        drv.base.device_state(1).put_connected(false);
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

    /// Boundary 4 with no `timeoutUser`: C's `:886` diverts only a request
    /// that has one, so a request nobody waits on runs on the down device.
    #[test]
    fn a_request_nobody_waits_on_runs_on_a_disconnected_device() {
        let mut drv = multi_device_driver(true);
        drv.base.init_auto_connect(false);
        drv.base.init_connected(true);
        drv.base.device_state(1).put_auto_connect(false);
        drv.base.device_state(1).put_connected(false);
        let tx = spawn_actor(drv);

        let write = RequestOp::Int32Write { value: 9 };
        tx.send(ActorMessage::unanswered(
            write,
            AsynUser::new(0).with_addr(1),
        ))
        .unwrap();
        // Device 0's read queues behind the write.
        send_and_wait(&tx, RequestOp::Int32Read, AsynUser::new(0).with_addr(0)).unwrap();
        let (val_tx, val_rx) = std::sync::mpsc::channel();
        let read = DriverCall::new(move |d| {
            let _ = val_tx.send(d.base().params.get_int32(0, 1));
        });
        send_and_wait(&tx, RequestOp::WithDriver(read), AsynUser::new(0)).unwrap();
        assert_eq!(val_rx.recv().unwrap().unwrap(), 9);
    }

    /// R15-47, boundary 5: the two **port**-level refusals are unchanged on a
    /// CANBLOCK port — they are the only ones C's `queueRequest` applies to it
    /// (asynManager.c:1543-1554).
    #[test]
    fn a_canblock_port_still_applies_the_port_level_refusals() {
        let mut drv = multi_device_driver(true);
        drv.base.set_enabled(false).unwrap();
        let tx = spawn_gated(drv);
        let err = queue_and_wait(&tx, RequestOp::Int32Read, AsynUser::new(0).with_addr(1))
            .expect_err("a disabled port refuses everything");
        assert!(err.is_queue_refused(), "got {err:?}");
        assert_eq!(err.status(), AsynStatus::Disabled);
        assert!(
            err.to_string().contains("port dev_gate disabled"),
            "got {err}"
        );

        let mut drv = multi_device_driver(true);
        drv.base.init_auto_connect(false);
        drv.base.init_connected(false);
        let tx = spawn_gated(drv);
        let err = queue_and_wait(&tx, RequestOp::Int32Read, AsynUser::new(0).with_addr(1))
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
    /// which lands the layer on the device's `dpCommon` (asynManager.c:2204-2208).
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
            fn read(&mut self, _u: &AsynUser, _b: &mut [u8]) -> OctetReadResult {
                OctetReadResult {
                    nbytes_transferred: 0,
                    eom_reason: crate::interpose::EomReason::CNT,
                    taken_back: 0,
                    status: Ok(()),
                }
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
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
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
    /// C `enable` ends in `exceptionOccurred` (asynManager.c:2249);
    /// `announceExceptionOccurred` signals `notifyPortThread` for every exception
    /// on a CANBLOCK port (:637-638); the woken `portThread` finds
    /// `!pport->dpc.connected` and runs the 2 s-throttled
    /// `autoConnectDevice(pport,0)` (:858-863). So `asynEnable(port,1)` brings a
    /// port that went down while disabled back up, with no I/O request at all.
    ///
    /// One case per gate boundary: re-enabled (must attempt), disabled (must
    /// not), defunct (must not), and an exception storm (throttled to one).
    #[test]
    fn re_enabling_a_down_port_reconnects_it_with_no_io_request() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct EnableDriver {
            base: PortDriverBase,
            connect_calls: Arc<AtomicUsize>,
            /// A link that never comes up, so the counter measures attempts
            /// rather than the one transition that ends them.
            comes_up: Arc<AtomicBool>,
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
                if self.comes_up.load(Ordering::SeqCst) {
                    self.base.set_connected(true);
                }
                Ok(())
            }
        }

        // A down, enabled, auto-connect port with no throttle window running
        // (`init_connected` does not stamp a transition).
        let spawn = |enabled: bool, defunct: bool, comes_up: bool| {
            let calls = Arc::new(AtomicUsize::new(0));
            let mut base = PortDriverBase::new(
                "enable_test",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.init_connected(false);
            base.init_auto_connect(true);
            base.set_enabled(enabled).unwrap();
            if defunct {
                shut_down(&mut base);
            }
            // Registration's attempt finds the link down and is not counted.
            let up = Arc::new(AtomicBool::new(false));
            let tx = spawn_actor_past_registration(EnableDriver {
                base,
                connect_calls: calls.clone(),
                comes_up: up.clone(),
            });
            up.store(comes_up, Ordering::SeqCst);
            calls.store(0, Ordering::SeqCst);
            (tx, calls)
        };

        // Boundary 1 — disabled: the wake finds the queue gate refusing, so no
        // attempt reaches the hardware (C `portThread` :808-811; R14-50).
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
        // (C `shutdownPort`, asynManager.c:2285-2286).
        let (tx, calls) = spawn(true, true, true);
        let _ = send_and_wait(&tx, RequestOp::GetEnable, AsynUser::default());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a defunct port makes no connect attempt"
        );

        // Boundary 4 — exception storm on a link that stays down: C's 2 s
        // throttle (:714-715) bounds it to one attempt per window, however many
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
    /// `isConnected` (asynManager.c:2331-2342), `isEnabled` (:2345),
    /// `isAutoConnect` (:2361) and `report` (:1138) signal nothing — they take
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
        let mut base = PortDriverBase::new(
            "status_test",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.create_param("VAL", ParamType::Int32).unwrap();
        // Down, enabled, auto-connect, and with no throttle window running
        // (`init_connected` stamps no transition) — so the *first* attempt any
        // pass wanted to make would be permitted. No transition means no retry
        // timer either, so nothing but a wake can produce an attempt.
        base.init_connected(false);
        base.init_auto_connect(true);
        let tx = spawn_actor_past_registration(StatusDriver {
            base,
            connect_calls: calls.clone(),
        });
        // Registration's attempt and its pass's own (asynManager.c:2135,
        // :858-863), before any query.
        assert_eq!(calls.swap(0, Ordering::SeqCst), 2);

        for op in [
            RequestOp::GetConnected,
            RequestOp::GetEnable,
            RequestOp::GetAutoConnect,
            RequestOp::Report {
                level: 1,
                sync_locked: false,
            },
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

        // The control — `enable` ends in `exceptionOccurred` (:2249), which *is* a
        // wake (:637-638), so this pass reaches the auto-connect tail.
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
        drv.base.set_enabled(false).unwrap();
        drv.base.init_auto_connect(true);
        let tx = spawn_actor(drv);

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let r = send_and_wait(&tx, RequestOp::GetEnable, user).unwrap();
        assert_eq!(r.int_val, Some(0));

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let r = send_and_wait(&tx, RequestOp::GetAutoConnect, user).unwrap();
        assert_eq!(r.int_val, Some(1));
    }

    /// C `queueRequest` (asynManager.c:1538-1554): a request queued at
    /// `asynQueuePriorityConnect` carrying `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED`
    /// skips the `!pport->dpc.connected → asynDisconnected` refusal. Bare Connect
    /// priority does not (unless the user carries no device) — that is what
    /// keeps the record's Low-priority EOS readback (asynRecord.c:1296) blank on
    /// a down port while its option readback (:1277-1280) fills in. Multi-device,
    /// so that addr 0 is a device.
    #[test]
    fn queue_even_if_not_connected_reason_waives_only_the_connected_refusal() {
        let mut drv = multi_device_driver(true);
        drv.base.init_auto_connect(false); // no reconnect can rescue the request
        drv.base.set_connected(false);
        let tx = spawn_gated(drv);

        // No waiver: refused, exactly as C's :1549-1554.
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let err = queue_and_wait(
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

        // Connect priority alone is not the waiver: the user carries device 0,
        // so C's :1538-1540 leaves `checkPortConnect` set.
        let user = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(1))
            .with_priority(QueuePriority::Connect);
        let err = queue_and_wait(
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
        // operator's only route to repoint a wrong or moved host. The base
        // driver keeps no options, so its own error is the proof it ran.
        let user = AsynUser::new(0)
            .with_timeout(Duration::from_secs(1))
            .queue_even_if_not_connected();
        let err = queue_and_wait(
            &tx,
            RequestOp::SetOption {
                key: "hostinfo".into(),
                value: "newhost:5000".into(),
            },
            user,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "asyn: Error - asynPortDriver:writeOption not implemented",
            "QUEUE_EVEN_IF_NOT_CONNECTED must reach the driver on a down port"
        );

        // And the readback reaches it off the same dead line.
        let user = AsynUser::new(0)
            .with_timeout(Duration::from_secs(1))
            .queue_even_if_not_connected();
        let err = queue_and_wait(
            &tx,
            RequestOp::GetOption {
                key: "hostinfo".into(),
            },
            user,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "asyn: Error - asynPortDriver:readOption not implemented",
            "the connect-time option readback is queued with the same waiver"
        );
    }

    /// C `queueRequest`'s waiver as a production [`PortHandle`] applies it
    /// (asynManager.c:1538-1554): the connected refusal is skipped only at
    /// Connect priority for a user with no device or the sentinel reason; the
    /// enabled refusal is skipped by nobody. A waived request on a disconnected
    /// port is queued and runs — the route the operator's reconnect takes.
    ///
    /// [`PortHandle`]: crate::port_handle::PortHandle
    #[test]
    fn a_port_handle_waives_only_cs_connect_queue_pair() {
        use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

        let mut drv = multi_device_driver(true);
        drv.base.init_auto_connect(false);
        drv.base.set_connected(false);
        let (runtime, _jh) = create_port_runtime_unclaimed(drv, RuntimeConfig::default()).unwrap();
        let handle = runtime.port_handle();
        let get = |user: AsynUser| {
            let user = user.with_queue_timeout(Duration::from_secs(1));
            handle.submit_blocking(RequestOp::GetOption { key: "k".into() }, user)
        };
        let refused = |r: AsynResult<RequestResult>| match r {
            Err(e) if e.is_queue_refused() => Some(e.status()),
            Err(AsynError::QueueTimeout { .. }) => panic!("parked instead of refused"),
            _ => None,
        };
        let connect = || AsynUser::default().with_priority(QueuePriority::Connect);

        assert_eq!(refused(get(connect())), None, "no device: waived, runs");
        assert_eq!(
            refused(get(connect().with_addr(0).queue_even_if_not_connected())),
            None,
            "the sentinel at a device: waived, runs"
        );
        assert_eq!(
            refused(get(connect().with_addr(0))),
            Some(AsynStatus::Disconnected),
            "a device at Connect priority without the sentinel"
        );
        assert_eq!(
            refused(get(AsynUser::default())),
            Some(AsynStatus::Disconnected),
            "no device below Connect priority"
        );

        handle.set_enable(-1, false).unwrap();
        assert_eq!(
            refused(get(connect())),
            Some(AsynStatus::Disabled),
            "nothing waives the enabled refusal"
        );
    }

    /// R13-47: `asynPortDriver::callParamCallbacks` (asynPortDriver.cpp:1785-1794)
    /// is a plain method call — `getParamList(list)` then `pList->callCallbacks(addr)`.
    /// It is invoked from driver code, never through `queueRequest`, so neither
    /// the `asynDisabled` (asynManager.c:1543-1548) nor the `asynDisconnected`
    /// (:1549-1554) refusal is reachable from it. A driver's parameter publish
    /// must land on a disconnected *and* on a disabled port — that is precisely
    /// when a driver publishes its status/health parameters.
    #[test]
    fn a_parameter_publish_lands_on_a_disconnected_or_disabled_port() {
        for (connected, enabled) in [(false, true), (true, false), (false, false)] {
            let mut drv = TestDriver::new();
            drv.base.init_auto_connect(false); // no reconnect can rescue the update
            drv.base.set_connected(connected);
            drv.base.set_enabled(enabled).unwrap();
            let val = drv.base.find_param("VAL").unwrap();
            let tx = spawn_actor(drv);

            // C: the driver's own publish, on a port whose line is down / disabled.
            let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
            send_and_wait(
                &tx,
                RequestOp::CallParamCallbacks {
                    addr: 0,
                    time_stamp: None,
                    updates: vec![crate::request::ParamSetValue::new(
                        val,
                        0,
                        crate::param::ParamValue::Int32(42),
                    )],
                    gate: None,
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
            // the disconnected port.
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
    /// they wrote (asynPortDriver.cpp:1785-1794). This carrier bundles the sets and
    /// the flush, so it owes a flush for every list the batch wrote: flushing
    /// only the request address stored the other lists' values and fired no
    /// interrupt for them, so a multi-device driver publishing six joint angles
    /// at addresses 0..6 in one call left five records at UDF for the life of
    /// the IOC while the array parameter carrying the same six numbers updated
    /// normally.
    #[test]
    fn a_publish_flushes_every_address_list_it_wrote() {
        // Post-`iocInit` precondition: the scan facility has opened the
        // interruptAccept gate. See `TestDriver::new` in `port.rs`.
        let mut base = PortDriverBase::new(
            "multi_addr",
            3,
            PortFlags {
                multi_device: true,
                ..PortFlags::default()
            },
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let _sub = base
            .interrupts
            .register_sync_callback(
                InterruptFilter {
                    reason: Some(val),
                    ..InterruptFilter::default()
                },
                move |iv| {
                    recorder
                        .lock()
                        .push((iv.addr, iv.value.as_int32().unwrap()))
                },
            )
            .unwrap();
        let tx = spawn_actor(TestDriverBase(base));

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(
            &tx,
            RequestOp::CallParamCallbacks {
                addr: 0,
                time_stamp: None,
                updates: (0..3)
                    .map(|a| {
                        crate::request::ParamSetValue::new(
                            val,
                            a,
                            crate::param::ParamValue::Int32(10 + a),
                        )
                    })
                    .collect(),
                gate: None,
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

    /// A batch's array is C `doCallbacksXxxArray` where it stands
    /// (asynPortDriver.cpp:1829-1866): posted ahead of the flush and typed,
    /// and its address list is not flushed for it.
    #[test]
    fn a_batch_array_posts_where_it_stands_and_flushes_no_list() {
        let mut base = PortDriverBase::new(
            "array_pub",
            2,
            PortFlags {
                multi_device: true,
                ..PortFlags::default()
            },
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        let wave = base.create_param("WAVE", ParamType::Float64Array).unwrap();
        base.set_int32_param(val, 1, 7).unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let _sub = base
            .interrupts
            .register_sync_callback(InterruptFilter::default(), move |iv| {
                recorder.lock().push((iv.reason, iv.addr, iv.iface))
            })
            .unwrap();
        let tx = spawn_actor(TestDriverBase(base));

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(
            &tx,
            RequestOp::CallParamCallbacks {
                addr: 0,
                time_stamp: None,
                updates: vec![
                    crate::request::ParamSetValue::new(val, 0, ParamValue::Int32(1)),
                    crate::request::ParamSetValue::array(
                        wave,
                        1,
                        ParamValue::Float64Array(vec![4.0, 5.0].into()),
                    ),
                ],
                gate: None,
            },
            user,
        )
        .unwrap();
        assert_eq!(
            *seen.lock(),
            vec![
                (
                    wave,
                    1,
                    Some(crate::interfaces::InterfaceType::Float64Array)
                ),
                (val, 0, None),
            ]
        );
    }

    /// A batch's `callParamCallbacks(addr)` flushes that list where it
    /// stands, as NDPluginROIStat.cpp:311 flushes each ROI before its time
    /// series posts (:319); the rest flush at the end.
    #[test]
    fn a_batch_callbacks_entry_flushes_its_list_where_it_stands() {
        let mut base = PortDriverBase::new(
            "callbacks_pub",
            2,
            PortFlags {
                multi_device: true,
                ..PortFlags::default()
            },
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        let wave = base.create_param("WAVE", ParamType::Float64Array).unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let _sub = base
            .interrupts
            .register_sync_callback(InterruptFilter::default(), move |iv| {
                recorder.lock().push((iv.reason, iv.addr))
            })
            .unwrap();
        let tx = spawn_actor(TestDriverBase(base));

        use crate::request::ParamSetValue as P;
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(
            &tx,
            RequestOp::CallParamCallbacks {
                addr: 0,
                time_stamp: None,
                updates: vec![
                    P::new(val, 0, ParamValue::Int32(1)),
                    P::new(val, 1, ParamValue::Int32(2)),
                    P::callbacks(1),
                    P::array(wave, 1, ParamValue::Float64Array(vec![3.0].into())),
                ],
                gate: None,
            },
            user,
        )
        .unwrap();
        assert_eq!(*seen.lock(), vec![(val, 1), (wave, 1), (val, 0)]);
    }

    /// NDPluginDriver.cpp:232 `setTimeStamp` before the cycle's sets: the
    /// batch's callbacks carry it, and so does a later unstamped flush.
    #[test]
    fn a_stamped_batch_sets_the_port_time_stamp_before_its_callbacks() {
        let mut base = PortDriverBase::new(
            "stamp_pub",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let _sub = base
            .interrupts
            .register_sync_callback(
                InterruptFilter {
                    reason: Some(val),
                    ..InterruptFilter::default()
                },
                move |iv| recorder.lock().push(iv.timestamp),
            )
            .unwrap();
        let tx = spawn_actor(TestDriverBase(base));
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for (n, time_stamp) in [(1, Some(ts)), (2, None)] {
            send_and_wait(
                &tx,
                RequestOp::CallParamCallbacks {
                    addr: 0,
                    updates: vec![crate::request::ParamSetValue::new(
                        val,
                        0,
                        crate::param::ParamValue::Int32(n),
                    )],
                    time_stamp,
                    gate: None,
                },
                AsynUser::new(0).with_timeout(Duration::from_secs(1)),
            )
            .unwrap();
        }
        assert_eq!(*seen.lock(), vec![ts, ts]);
    }

    /// `ParamSetValue::Status` is the background-thread half of C's alarm
    /// push — `lock(); setParamStatus(..); callParamCallbacks(); unlock()`.
    /// A status-only transition marks the param changed (paramVal.cpp:71-78),
    /// so the flush delivers the stored triplet on the `InterruptValue` while
    /// the value stays what it was; the recovery transition back to `Success`
    /// is delivered the same way.
    #[test]
    fn a_status_publish_delivers_the_alarm_without_touching_the_value() {
        // Post-`iocInit` precondition: the scan facility has opened the
        // interruptAccept gate. See `TestDriver::new` in `port.rs`.
        let mut base = PortDriverBase::new(
            "status_pub",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let _sub = base
            .interrupts
            .register_sync_callback(
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
            )
            .unwrap();
        let tx = spawn_actor(TestDriverBase(base));

        let publish = |updates| {
            let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
            send_and_wait(
                &tx,
                RequestOp::CallParamCallbacks {
                    addr: 0,
                    updates,
                    time_stamp: None,
                    gate: None,
                },
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
        )]);
        publish(vec![crate::request::ParamSetValue::status(
            val,
            0,
            crate::error::AsynStatus::Success,
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

    /// A batch's doCallbacksXxxArray posts where the batch reaches it, typed,
    /// before the flush that posts the batch's sets.
    #[test]
    fn a_batch_array_posts_in_place_ahead_of_the_flush() {
        use crate::interfaces::InterfaceType;
        use crate::param::ParamValue;
        use crate::request::ParamSetValue;
        let mut base = PortDriverBase::new(
            "batch_array",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        let wf = base.create_param("WF", ParamType::Float64Array).unwrap();
        base.call_param_callbacks(0).unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let _sub = base
            .interrupts
            .register_sync_callback(InterruptFilter::default(), move |iv| {
                recorder.lock().push((iv.reason, iv.iface))
            })
            .unwrap();
        let tx = spawn_actor(TestDriverBase(base));
        let user = || AsynUser::new(wf).with_timeout(Duration::from_secs(1));
        let batch = RequestOp::CallParamCallbacks {
            addr: 0,
            updates: vec![
                ParamSetValue::new(val, 0, ParamValue::Int32(7)),
                ParamSetValue::array(wf, 0, ParamValue::Float64Array(vec![1.5].into())),
            ],
            time_stamp: None,
            gate: None,
        };
        send_and_wait(&tx, batch, user()).unwrap();
        assert_eq!(
            *seen.lock(),
            vec![(wf, Some(InterfaceType::Float64Array)), (val, None)]
        );
    }

    /// doCallbacksXxxArray flushes no list (asynPortDriver.cpp:1829-1866): a
    /// batch's array at another address leaves that list's pending sets for
    /// its own callParamCallbacks.
    #[test]
    fn a_batch_array_flushes_no_list_of_its_own() {
        use crate::interfaces::InterfaceType;
        use crate::param::ParamValue;
        use crate::request::ParamSetValue;
        let mut base = PortDriverBase::new(
            "batch_array_list",
            2,
            PortFlags {
                multi_device: true,
                ..PortFlags::default()
            },
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        let wf = base.create_param("WF", ParamType::Float64Array).unwrap();
        for addr in 0..2 {
            base.call_param_callbacks(addr).unwrap();
        }
        base.set_int32_param(val, 1, 7).unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let _sub = base
            .interrupts
            .register_sync_callback(InterruptFilter::default(), move |iv| {
                recorder.lock().push((iv.reason, iv.addr, iv.iface))
            })
            .unwrap();
        let tx = spawn_actor(TestDriverBase(base));
        let user = AsynUser::new(wf).with_timeout(Duration::from_secs(1));
        let batch = RequestOp::CallParamCallbacks {
            addr: 0,
            updates: vec![ParamSetValue::array(
                wf,
                1,
                ParamValue::Float64Array(vec![1.5].into()),
            )],
            time_stamp: None,
            gate: None,
        };
        send_and_wait(&tx, batch, user).unwrap();
        assert_eq!(
            *seen.lock(),
            vec![(wf, 1, Some(InterfaceType::Float64Array))]
        );
    }

    /// C `setIntegerParam` alone writes the store, so a read sees the value
    /// at once, and only the next `callParamCallbacks` posts it
    /// (asynPortDriver.cpp:1785-1794). Neither goes through `queueRequest`,
    /// so a disconnected port takes the set too.
    #[test]
    fn a_set_alone_is_read_at_once_and_posted_by_the_next_callback() {
        let mut base = PortDriverBase::new(
            "set_alone",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let val = base.create_param("VAL", ParamType::Int32).unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let _sub = base
            .interrupts
            .register_sync_callback(
                InterruptFilter {
                    reason: Some(val),
                    ..InterruptFilter::default()
                },
                move |iv| recorder.lock().push(iv.value.as_int32().unwrap()),
            )
            .unwrap();
        let tx = spawn_actor(TestDriverBase(base));
        let user = || AsynUser::new(val).with_timeout(Duration::from_secs(1));
        let set = |n| RequestOp::SetParams {
            updates: vec![crate::request::ParamSetValue::new(
                val,
                0,
                crate::param::ParamValue::Int32(n),
            )],
        };
        send_and_wait(&tx, set(7), user()).unwrap();
        let read = send_and_wait(&tx, RequestOp::Int32Read, user()).unwrap();
        assert_eq!(read.int_val, Some(7));
        assert!(seen.lock().is_empty(), "a set alone posts nothing");
        let flush = RequestOp::CallParamCallbacks {
            addr: 0,
            updates: vec![],
            time_stamp: None,
            gate: None,
        };
        send_and_wait(&tx, flush, user()).unwrap();
        assert_eq!(*seen.lock(), vec![7]);

        let mut drv = TestDriver::new();
        drv.base.init_auto_connect(false);
        drv.base.set_connected(false);
        let tx = spawn_actor(drv);
        send_and_wait(&tx, set(8), user()).unwrap();
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
            drv.base.init_auto_connect(false);
            drv.base.set_connected(connected);
            drv.base.set_enabled(enabled).unwrap();
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
    /// C `reportPrintPort` (asynManager.c:1020-1124) prints, in order: the
    /// multiDevice/canBlock/autoConnect header; at `details >= 1` the
    /// enabled/connected/numberConnects line, the nDevices/nQueued/blocked line,
    /// the two lock states, the exception counts and the trace masks; at
    /// `details >= 2` the interpose and interface lists; then the per-address
    /// block (suppressed by a *negative* `details`, :1034-1037) — and only then
    /// the driver's own `pasynCommon->report`.
    ///
    /// One case per boundary of C's `details` rules: 0, 1, 2, negative, defunct.
    /// C's `asynReport` header prints `multiDevice:` straight off
    /// `pport->attributes & ASYN_MULTIDEVICE` (asynManager.c:1045-1049), and the
    /// IP server listener is registered with plain `ASYN_CANBLOCK`
    /// (drvAsynIPServerPort.c:646-647) — a TCP listener's asynOctet table stays
    /// the all-NULL initializer (:118-123; C fills read/write/flush in only for
    /// SOCK_DGRAM, :673-677), so it is an interrupt source, not an addressable
    /// port. The
    /// per-client ports are separate single-device registrations (:709-715).
    /// The connect registration queues is a connect edge, which C's
    /// `exceptionConnect` counts (asynManager.c:2160): a port that came up
    /// reports `numberConnects 1`.
    #[test]
    fn registration_counts_the_connect_that_brings_a_port_up() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut actor = PortActor::new(Box::new(TestDriver::new()), rx);
        assert!(
            actor
                .manager_report(1, false)
                .contains("connected:No numberConnects 0")
        );
        actor.register();
        let r = actor.manager_report(1, false);
        assert!(r.contains("connected:Yes numberConnects 1"), "{r}");
    }

    #[test]
    fn the_ip_server_listener_reports_multidevice_no_like_c() {
        let srv =
            crate::drivers::ip_server_port::DrvAsynIPServerPort::new("ipsrv_rep", "127.0.0.1:0")
                .expect("server");
        let (_tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(srv), rx);

        assert_eq!(
            actor.manager_report(0, false).lines().next().unwrap(),
            "ipsrv_rep multiDevice:No canBlock:Yes autoConnect:Yes",
            "C's header line for the listener (asynManager.c:1045-1049)"
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
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.init_auto_connect(true);
            base.init_connected(false);
            base.set_connected(true); // one published connect → numberConnects 1
            base.device_state(0).put_connected(true);
            base.device_state(1).put_connected(false); // a down device
            base.install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
            base.install_octet_interpose_addr(
                1,
                Box::new(crate::interpose::eos::EosInterpose::default()),
            );
            let (_tx, rx) = mpsc::unbounded_channel();
            (PortActor::new(Box::new(Plain(base)), rx), _tx)
        };

        // details = 0: the header, and the down device — nothing else. C prints a
        // disconnected device even at 0 (:1087), which is what the operator ran
        // the report for.
        let (actor, _tx) = build();
        let r0 = actor.manager_report(0, false);
        assert_eq!(
            r0.lines().next().unwrap(),
            "rep_block multiDevice:Yes canBlock:Yes autoConnect:Yes",
            "C's header line (asynManager.c:1045-1049)"
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
        let r1 = actor.manager_report(1, false);
        assert!(
            r1.contains("    enabled:Yes connected:Yes numberConnects 1"),
            "C :1059-1062 — including the count of connects: {r1}"
        );
        assert!(
            r1.contains("    nDevices 2 nQueued 0 blocked:No"),
            "C :1063-1066: {r1}"
        );
        assert!(
            r1.contains("    asynManagerLock:No synchronousLock:No"),
            "C :1067-1069: {r1}"
        );
        // Each EOS interpose registers an exception callback on its own
        // dpCommon (asynInterposeEos.c:110): one on the port, one on device 1.
        assert!(
            r1.contains("    exceptionActive:No exceptionUsers 1 exceptionNotifys 0"),
            "C :1070-1074: {r1}"
        );
        assert!(
            r1.contains(
                "connected Yes exceptionActive No\n        exceptionActive No exceptionUsers 0 "
            ) && r1.contains(
                "connected No exceptionActive No\n        exceptionActive No exceptionUsers 1 "
            ),
            "device 0 has no interpose, device 1 its own: {r1}"
        );
        assert!(
            r1.contains("    traceMask:0x"),
            "C :1075-1076 prints the port's three trace masks: {r1}"
        );
        assert!(
            r1.contains("addr 0 autoConnect Yes enabled Yes connected Yes"),
            "at details 1 every device prints, connected or not (C :1087): {r1}"
        );

        // details = 2: the interpose and interface lists (C :1078-1082).
        let r2 = actor.manager_report(2, false);
        assert!(
            r2.contains("    interposeInterfaceList\n        asynOctet"),
            "the installed EOS interpose is an asynOctet interpose: {r2}"
        );
        assert!(
            r2.contains("    interfaceList\n        asynCommon"),
            "the driver's registered interfaces: {r2}"
        );
        assert!(r2.contains("        asynOctet\n"), "…asynOctet among them");

        // A negative details is C's "detail, but no devices" (:1034-1037).
        let rneg = actor.manager_report(-1, false);
        assert!(
            rneg.contains("    enabled:Yes connected:Yes numberConnects 1"),
            "a negative details still prints the detail lines: {rneg}"
        );
        assert!(
            !rneg.contains("addr "),
            "…and suppresses the per-address block: {rneg}"
        );

        // A destroyed port: one line, and C never reaches the driver (:1040-1044).
        let (mut actor, _tx) = build();
        actor.driver.base_mut().flags.destructible = true;
        actor.driver.base_mut().shutdown_lifecycle().unwrap();
        assert_eq!(actor.manager_report(2, false), "rep_block destroyed\n");
    }

    /// W10-D6: C's `report` (asynManager.c:1138-1173) spawns a `reportPort` thread
    /// and calls `pasynCommon->report(drvPvt, fp, details)` directly (:1123-1125).
    /// It never queues, so no gate runs: a disabled port reports `enabled:No` and
    /// a disconnected one `connected:No` (:1059-1062). Queue-gating it made
    /// `asynReport` on a down port print a refusal instead of the diagnostic the
    /// operator opened it for.
    ///
    /// The one port C refuses to report on is a destroyed one: `reportPrintPort`
    /// prints "<name> destroyed" and returns without touching the driver
    /// (:1040-1044).
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
            fn report(&mut self, _out: &mut dyn std::fmt::Write, _level: i32) {
                self.reports
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        for (connected, enabled) in [(false, true), (true, false), (false, false)] {
            let reports = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut base = PortDriverBase::new(
                "rep",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.init_auto_connect(false);
            base.set_connected(connected);
            base.set_enabled(enabled).unwrap();
            let tx = spawn_actor(ReportSpy {
                base,
                reports: reports.clone(),
            });

            let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
            send_and_wait(
                &tx,
                RequestOp::Report {
                    level: 1,
                    sync_locked: false,
                },
                user,
            )
            .unwrap_or_else(|e| {
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
        // driver's interfaces (asynManager.c:1041-1043).
        let reports = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut base = PortDriverBase::new(
            "gone",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        shut_down(&mut base);
        let tx = spawn_actor(ReportSpy {
            base,
            reports: reports.clone(),
        });
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        send_and_wait(
            &tx,
            RequestOp::Report {
                level: 1,
                sync_locked: false,
            },
            user,
        )
        .unwrap();
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
    /// from the port (`dpCommonInit(pport, pdevice, pport->dpc.autoConnect)`, :586).
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
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.init_auto_connect(port_auto_connect);
            base.device_state(0);
            base.device_state(1);
            let (_tx, rx) = mpsc::unbounded_channel();
            (PortActor::new(Box::new(TestDriverBase(base)), rx), _tx)
        };

        // Boundary 1: the port auto-connects, one device does not.
        let (mut actor, _tx) = build(true);
        actor.driver.base_mut().set_auto_connect_addr(1, false);
        let r = actor.manager_report(1, false);
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
        let r = actor.manager_report(1, false);
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
            let mut base = PortDriverBase::new(
                "drv_rep",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
            let mut drv = Drv(base, caps);
            drv.set_input_eos(&AsynUser::default(), b"\r\n").unwrap();
            drv
        };

        // No octet interface → no EOS lines. C reads them off
        // `pasynStdInterfaces->octet.pinterface`, which such a port never set.
        let mut int32_only = build(vec![Capability::Int32Read]);
        let mut out = String::new();
        int32_only.report(&mut out, 1);
        assert!(out.starts_with("Port: drv_rep\n"), "{out}");
        assert!(
            out.contains("  Timestamp: <undefined>\n"),
            "C prints the port's timestamp at details >= 1 (:3682-3684), an unset \
             one as epicsTimeToStrftime's <undefined>: {out}"
        );
        assert!(
            !out.contains("EOS["),
            "a port with no octet interface has no EOS to report: {out}"
        );
        int32_only.base_mut().update_time_stamp();
        let mut stamped = String::new();
        int32_only.report(&mut stamped, 1);
        assert!(
            !stamped.contains("<undefined>") && stamped.contains("  Timestamp: 2"),
            "a set stamp prints its date: {stamped}"
        );
        // C `reportParams(fp, details)` with the level unshifted (:3693): at
        // details 1 the whole parameter block is already there — list 0, the
        // count, and every parameter with its value (:1804-1807,
        // paramVal.cpp:296-372).
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
        let mut octet = build(crate::interfaces::octet_transport_capabilities());
        let mut out = String::new();
        octet.report(&mut out, 1);
        assert!(
            out.contains("  Input EOS[2]: \\r\\n") && out.contains("  Output EOS[0]: "),
            "an octet port prints its EOS pair: {out}"
        );

        // details >= 3: one line per interrupt client, and none below 3.
        let mut with_client = build(vec![Capability::Int32Read]);
        let (_sub, _rx) = with_client
            .0
            .interrupts
            .register_interrupt_user(
                InterruptFilter {
                    reason: Some(0),
                    addr: Some(0),
                    iface: Some(InterfaceType::Int32),
                    ..InterruptFilter::default()
                },
                4096,
            )
            .unwrap();
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

    /// R12-48: C conditions **only** `checkPortConnect` on the priority
    /// (asynManager.c:1538-1540). `if(!pport->dpc.enabled) return asynDisabled`
    /// (:1543-1548) sits above it and every queued request takes it — and the
    /// port thread will not drain even the Connect queue on a disabled port
    /// (:808-811). So a `CNCT=1` put on a port disabled with `asynEnable(port,0)`
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
        let mut base = PortDriverBase::new(
            "disabled_port",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.init_auto_connect(false);
        base.set_connected(false);
        base.set_enabled(false).unwrap(); // C `asynEnable(port, 0)`
        let tx = spawn_gated(ConnectSpy {
            base,
            connects: connects.clone(),
        });

        // CNCT=1 on a disabled port: asynDisabled, and the driver's connect() is
        // never called — the hardware is not touched.
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let err = queue_and_wait(&tx, RequestOp::Connect, user).unwrap_err();
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

        // Not even the option waiver buys the enabled refusal off (C's :1543-1548
        // is above `checkPortConnect`, not beside it).
        let user = AsynUser::new(0)
            .with_timeout(Duration::from_secs(1))
            .queue_even_if_not_connected();
        let err = queue_and_wait(
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
        queue_and_wait(&tx, RequestOp::SetEnable { yes: true }, user).unwrap();

        // …and once enabled, the same CNCT=1 put opens the disconnected
        // single-device port: its user has no device at addr 0
        // (asynManager.c:576), so the Connect-queue waiver holds (:1538-1540).
        let user = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(1));
        queue_and_wait(&tx, RequestOp::Connect, user).unwrap();
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

        // Port is now disconnected; the Connect queue takes the reconnect.
        let user = AsynUser::new(0)
            .with_addr(-1)
            .with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Connect, user).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = send_and_wait(&tx, RequestOp::Int32Read, user);
        assert!(result.is_ok());
    }

    /// Boundaries of C's Connect-queue waiver (asynManager.c:1538-1540): on a
    /// disconnected, non-autoconnect port a Connect-priority request runs iff
    /// its user has no device or carries the sentinel. A user has a device
    /// only on an `ASYN_MULTIDEVICE` port at `addr >= 0` (`locateDevice`,
    /// :576), so asynRecord's CNCT put (its special user at the record's ADDR,
    /// no sentinel) is refused `asynDisconnected` only there.
    #[test]
    fn device_addressed_connect_is_refused_only_on_a_multi_device_port() {
        // Boundary 0: single-device port, addr 0 → no device, waived, runs.
        let mut drv = TestDriver::new();
        drv.base.init_auto_connect(false);
        drv.base.set_connected(false);
        let tx = spawn_gated(drv);
        let user = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(1));
        queue_and_wait(&tx, RequestOp::Connect, user)
            .expect("a single-device port has no device at addr 0, so the waiver holds");

        let mut drv = multi_device_driver(true);
        drv.base.init_auto_connect(false);
        drv.base.set_connected(false);
        let tx = spawn_gated(drv);

        // Boundary 1: multi-device, addr >= 0, no sentinel → refused.
        let user = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(1));
        let err = queue_and_wait(&tx, RequestOp::Connect, user).unwrap_err();
        assert!(
            err.is_queue_refused(),
            "the gate's refusal is a QueueRefused, not a driver error: {err:?}"
        );
        assert_eq!(err.status(), AsynStatus::Disconnected);

        // Boundary 2: addr >= 0 WITH the sentinel → waived, runs.
        let user = AsynUser::new(0)
            .with_timeout(Duration::from_secs(1))
            .queue_even_if_not_connected();
        queue_and_wait(&tx, RequestOp::Connect, user)
            .expect("the sentinel waives the connected refusal at any addr");

        // Reset for boundary 3.
        let user = AsynUser::new(0)
            .with_addr(-1)
            .with_timeout(Duration::from_secs(1));
        queue_and_wait(&tx, RequestOp::Disconnect, user).unwrap();

        // Boundary 3: addr == -1, no sentinel → waived, runs.
        let user = AsynUser::new(0)
            .with_addr(-1)
            .with_timeout(Duration::from_secs(1));
        queue_and_wait(&tx, RequestOp::Connect, user)
            .expect("a port-level user is waived on the Connect queue");
    }

    /// Boundary 4, and the reason the other three can be written at all: a user
    /// that never chose a device is *port-level*, so it clears C's
    /// `checkPortConnect` waiver (`addr == -1`, asynManager.c:1538-1540)
    /// without the caller spelling `-1` anywhere.
    ///
    /// C carries the distinction as a pointer, not a number:
    /// `connectDevice` allocates `puserPvt->pdevice` only for `addr >= 0`
    /// (asynManager.c:1351-1354), `findDpCommon` returns `&pport->dpc` whenever
    /// that pointer is NULL (:538-546), and `getAddr` renders a NULL `pdevice`
    /// as -1 (:2006-2010) — the one place the number appears, at the driver
    /// boundary. So "no device chosen" and "device 0" are different states in
    /// C, and `AsynUser::default()` is the Rust spelling of the first. A
    /// default that said 0 would make every framework-constructed user silently
    /// device-addressed, and each new reader of `user.addr` would need its own
    /// `-1` patch.
    #[test]
    fn a_user_that_never_chose_a_device_is_port_level() {
        let mut drv = TestDriver::new();
        drv.base.init_auto_connect(false);
        drv.base.set_connected(false);
        let tx = spawn_actor(drv);

        let user = AsynUser::default().with_timeout(Duration::from_secs(1));
        send_and_wait(&tx, RequestOp::Connect, user)
            .expect("a defaulted user is port-level, so the Connect gate waives it");

        assert_eq!(AsynUser::default().addr, -1);
    }

    #[test]
    fn actor_block_unblock_process() {
        // C refuses a block on a synchronous port (asynManager.c:1708-1713).
        let mut drv = TestDriver::new();
        drv.base.flags.can_block = true;
        let tx = spawn_actor(drv);

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
    /// are NULL-or-it (asynManager.c:889-892, :931-934). devGpib takes the
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
        // The holder is taken when the owner's next callback returns
        // (asynManager.c:931-934).
        let mut owner = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(1));
        owner.block_token = Some(42);
        send_and_wait(&tx, RequestOp::Int32Read, owner).unwrap();

        // Address 1 — a different dpCommon — is not gated by it. Probed
        // without blocking on the reply: under a port-wide holder this read
        // is diverted and never answers, and a test that waited on it would
        // hang rather than fail. A second read on address 1 is the barrier —
        // same band, so FIFO, it can only answer once the actor has dealt with
        // the first. A direct call is no barrier: it jumps every band.
        let (other_tx, mut other_rx) = oneshot::channel();
        tx.send(ActorMessage::new(
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
            RequestOp::Int32Read,
            AsynUser::new(0)
                .with_addr(1)
                .with_timeout(Duration::from_secs(1)),
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
        tx.send(ActorMessage::new(
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
        // Both holders are taken when the owner's next callback returns
        // (asynManager.c:931-934).
        send_and_wait(&tx, RequestOp::Int32Read, u(7, 0)).unwrap();

        // A non-owner on address 1: only the PORT holder gates it.
        let (reply_tx, mut reply_rx) = oneshot::channel();
        tx.send(ActorMessage::new(
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
        // I/O) is aborted by a queue deadline (C parity, asynManager.c:908).
        let mut drv = TestDriver::new();
        // Seed VAL so the post-unblock read probe returns a value rather than
        // the C-parity ParamUndefined for an unset param; this test asserts
        // the unblock lifecycle, not undefined-read state.
        drv.base.params.set_int32(0, 0, 0).unwrap();
        drv.base.flags.can_block = true;
        let tx = spawn_actor(drv);

        let owner = || {
            let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
            user.block_token = Some(7);
            user
        };
        send_and_wait(&tx, RequestOp::BlockProcess { all_devices: true }, owner()).unwrap();
        send_and_wait(&tx, RequestOp::Int32Read, owner()).unwrap();

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
        let mut drv = TestDriver::new();
        drv.base.flags.can_block = true;
        drv.base.params.set_int32(0, 0, 0).unwrap();
        let tx = spawn_actor(drv);

        // Block the port with owner token 42; the holder is taken when the
        // owner's next callback returns.
        let owner = || {
            let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
            user.block_token = Some(42);
            user
        };
        send_and_wait(&tx, RequestOp::BlockProcess { all_devices: true }, owner()).unwrap();
        send_and_wait(&tx, RequestOp::Int32Read, owner()).unwrap();

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
        let mut base = PortDriverBase::new(
            "serial_actor",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
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
    /// asynManager.c:548-553) — an interpose has no other handle on the port
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
        mgr.register_port_unclaimed(
            TraceSpy(PortDriverBase::new(
                "trace_link",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            )),
            &crate::registry::open_owner(),
        )
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
