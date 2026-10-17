//! Async-friendly handle for submitting requests to a port actor.
//!
//! [`PortHandle`] is a lightweight, cloneable handle that sends requests to the
//! actor via an mpsc channel and receives replies via oneshot channels.

// RTEMS-EXEC-MODEL-ALLOW(9): checked, not waived — all 9 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p asyn-rs
// --all-features`, 1266/1266). asyn-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use epics_libcom_rs::SharedArray;

use crate::param::ParamValue;
use std::any::Any;
use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant, SystemTime};

use tokio::runtime::RuntimeFlavor;
use tokio::sync::{mpsc, oneshot};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interfaces::{Capability, InterfaceType};
use crate::interrupt::InterruptManager;
use crate::port::QueueGate;
use crate::port::{DrvUserInfo, DrvUserRequest, PortDriver};
use crate::port_actor::{ActorId, ActorLink, ActorMessage};
use crate::request::{CancelToken, DriverCall, PortCalls, PortLockCall, RequestOp, RequestResult};
use crate::trace::TraceManager;
use crate::user::{AsynUser, QueueDeadline};

/// Park the calling thread until `fut` resolves or `deadline` passes, from
/// *any* context.
///
/// `deadline` is what makes a blocking wait bounded, and it is taken here
/// rather than layered above because this parking executor drives no reactor:
/// there is no timer to race the future against, so the bound has to live in
/// the park itself (`park_timeout`). A `None` deadline parks until the future
/// resolves and is only correct where something *else* already bounds it —
/// [`PortHandle::blocking`] names the one such caller and why.
///
/// Deliberately not one of tokio's blocking shims. Every one of them vetoes
/// some context this framework actually runs in:
///
/// - `Handle::block_on`, `mpsc::blocking_send` and `oneshot::blocking_recv`
///   panic when called from inside a runtime;
/// - `task::block_in_place` panics unless the runtime is *multi-threaded* —
///   and the framework's own runtimes are not. Both `PortActor` (port_actor.rs)
///   and `ad_core_rs::runtime::run_thread_named` build `new_current_thread`
///   runtimes, so every blocking device-I/O call made from a driver's own task
///   panicked that thread.
///
/// The futures driven here are `tokio::sync` primitives (an mpsc send and a
/// oneshot receive), which are documented as runtime-agnostic: polling them
/// needs no reactor and no timer, so a bare parking executor is both sufficient
/// and incapable of panicking on a runtime-flavour check.
///
/// The future runs outside the enclosing tokio task's coop budget. Inside a
/// task, a `tokio::sync` poll past the budget returns `Pending` and hands its
/// waker to the scheduler, and the scheduler never runs while this thread is
/// parked, so the wait would never end.
fn park_on<F: Future>(fut: F, deadline: Option<Instant>) -> Option<F::Output> {
    let mut fut = pin!(tokio::task::unconstrained(fut));
    let drive = crate::runtime::task::ThreadDrive::enter();
    let mut cx = Context::from_waker(drive.waker());
    loop {
        // Polled before the deadline is consulted, so a reply that landed
        // exactly at the deadline is delivered rather than discarded.
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return Some(value),
            // A spurious unpark just re-polls, which is always sound.
            Poll::Pending => {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return None;
                }
                drive.park(deadline);
            }
        }
    }
}

/// Block the calling thread on a reply that only the port actor can produce.
///
/// The actor runs on its own OS thread, so the reply arrives no matter what the
/// caller's runtime is doing — parking the caller can stall the caller's own
/// runtime, but never the thread that owes us the answer. The one context where
/// that reasoning fails (the caller *is* the actor) is refused up front by
/// [`guard_reentrant`], never reached here.
fn block_on_reply<F: Future>(fut: F, deadline: Option<Instant>) -> Option<F::Output> {
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        // A multi-threaded worker: hand this worker's other tasks to a sibling
        // before we park it. Purely a scheduling courtesy — correctness does
        // not depend on it.
        Ok(RuntimeFlavor::MultiThread) => tokio::task::block_in_place(|| park_on(fut, deadline)),
        // A current-thread runtime, or no runtime at all. `block_in_place` is
        // unavailable here (it panics), and parking directly is correct.
        _ => park_on(fut, deadline),
    }
}

/// Block until `fut` resolves or `deadline` passes; `None` means it passed.
///
/// The bound is a parameter, not an option, so a caller cannot reach an
/// unbounded wait by leaving something out — the only way to park without a
/// deadline is to name [`block_on_prebounded_reply`] and satisfy its
/// precondition.
fn block_on_reply_until<F: Future>(fut: F, deadline: Instant) -> Option<F::Output> {
    block_on_reply(fut, Some(deadline))
}

/// Block until `fut` resolves, with no deadline of our own.
///
/// Precondition, which the name exists to make the caller state out loud:
/// `fut` must already resolve on its own bound. The single caller is
/// [`PortHandle::blocking`], whose future is `submit_cancellable` — that one
/// resolves at the request's `AsynUser::queue_timeout`, so a deadline here
/// would be a second, competing bound on the same wait.
fn block_on_prebounded_reply<F: Future>(fut: F) -> F::Output {
    // Not a guard against an impossible state: `park_on` returns `None` only
    // for a `Some` deadline, and this function is the sole `None` site.
    block_on_reply(fut, None).expect("a park with no deadline cannot expire")
}

/// Refuse a blocking wait the calling thread could never be woken from.
///
/// **Invariant:** a blocking call MUST NOT park a thread whose progress the
/// reply depends on. Exactly one thread produces the reply — port `port_name`'s
/// actor — so the question is not *"am I inside a runtime?"* (the old, wrong
/// predicate: it is neither necessary nor sufficient) but *"am I this port's
/// actor?"*. If it is us, the request we would block on is the request we would
/// have to run, and no amount of waiting can resolve that.
///
/// This is reported as an error rather than being made unrepresentable by the
/// type system: a `PortHandle` reaches a driver through shared state
/// (registries, `Arc`s, callbacks set up long after the port is built), so
/// "this handle points at me" is not knowable at any call site the compiler can
/// see. The error names the fix.
fn guard_reentrant(actor: ActorId, port_name: &str, what: &str) -> AsynResult<()> {
    if HELD.with(|held| held.borrow().contains(&actor)) {
        return Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!(
                "{what} called on port {port_name} by a thread holding its driver: the \
                 request would wait for its own release. Run the step through \
                 DriverHold::run instead."
            ),
        });
    }
    if crate::port_actor::current_actor() == Some(actor) {
        return Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!(
                "{what} called on port {port_name} from that port's own actor thread: \
                 a driver cannot make a blocking request to itself — the actor would \
                 have to service the very request it is blocked on. Call the driver \
                 method directly instead of going back through the port, or submit \
                 the request from another thread or task."
            ),
        });
    }
    Ok(())
}

thread_local! {
    /// The ports whose driver this thread holds through a [`DriverHold`].
    static HELD: std::cell::RefCell<Vec<ActorId>> = const { std::cell::RefCell::new(Vec::new()) };
}

type HeldOp<D> = Box<dyn FnOnce(&mut D) + Send>;

/// The driver lock [`PortHandle::hold_driver_blocking`] took; dropping it is
/// C's `unlock()`. Tied to the thread that took it, as a C mutex is.
pub struct DriverHold<D: PortDriver> {
    port: PortHandle,
    /// `None` once a re-take after [`Self::unlocked`] found the port gone.
    ops: Option<std::sync::mpsc::Sender<HeldOp<D>>>,
    _thread: std::marker::PhantomData<*const ()>,
}

impl<D: PortDriver> DriverHold<D> {
    /// Run `f` on the driver inside the hold and return what it returns.
    pub fn run<R: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut D) -> R + Send + 'static,
    ) -> AsynResult<R> {
        let ended = || AsynError::Status {
            status: AsynStatus::Error,
            message: format!("port {}: the driver hold ended", self.port.port_name),
        };
        let ops = self.ops.as_ref().ok_or_else(ended)?;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        ops.send(Box::new(move |d| {
            let _ = tx.send(f(d));
        }))
        .map_err(|_| ended())?;
        rx.recv().map_err(|_| ended())
    }

    /// C `unlock()` … `lock()` around `wait`. A port gone by the re-take
    /// leaves the hold ended: every later [`Self::run`] errors.
    pub fn unlocked<R>(&mut self, wait: impl FnOnce() -> R) -> R {
        self.release();
        let r = wait();
        self.ops = self.enter().ok();
        r
    }

    fn enter(&self) -> AsynResult<std::sync::mpsc::Sender<HeldOp<D>>> {
        let port = &self.port;
        guard_reentrant(port.actor, &port.port_name, "hold_driver_blocking")?;
        let (ops, session) = std::sync::mpsc::channel::<HeldOp<D>>();
        let (entered, taken) = std::sync::mpsc::sync_channel(1);
        let call = DriverCall::new(move |driver| {
            let driver: &mut dyn Any = driver;
            let Some(driver) = driver.downcast_mut::<D>() else {
                return;
            };
            let _ = entered.send(());
            for op in session {
                op(driver);
            }
        });
        let done = port.try_submit(RequestOp::WithDriver(call), AsynUser::default())?;
        if taken.recv().is_ok() {
            HELD.with(|held| held.borrow_mut().push(port.actor));
            return Ok(ops);
        }
        // The call never entered: the actor's refusal, or a driver of
        // another type.
        port.blocking("hold_driver_blocking", done)?;
        Err(port.wrong_driver::<D>())
    }

    fn release(&mut self) {
        if self.ops.take().is_some() {
            HELD.with(|held| {
                let mut held = held.borrow_mut();
                if let Some(at) = held.iter().position(|a| *a == self.port.actor) {
                    held.swap_remove(at);
                }
            });
        }
    }
}

impl<D: PortDriver> Drop for DriverHold<D> {
    fn drop(&mut self) {
        self.release();
    }
}

/// Async completion handle returned by [`PortHandle::try_submit`].
///
/// Implements `Future` for async waiting, plus `wait_blocking()` for sync callers.
pub struct AsyncCompletionHandle {
    rx: oneshot::Receiver<AsynResult<RequestResult>>,
    /// The actor that owes this reply — see [`guard_reentrant`].
    actor: ActorId,
    port_name: Arc<str>,
}

impl AsyncCompletionHandle {
    /// Block the current thread until the result arrives or `timeout` expires.
    ///
    /// `timeout` bounds the wait, not the request: a request the actor has
    /// already begun keeps running, exactly as C's `queueTimeoutCallback`
    /// (`asynManager.c:649-703`) leaves a dequeued request alone — it returns
    /// at once on `!puserPvt->isQueued` (`:657-663`). What expiry
    /// buys the caller is an `asynTimeout` to report instead of a thread
    /// parked for good.
    ///
    /// Dropping the reply receiver on expiry is safe: the actor sends its
    /// reply with `let _ = reply.send(..)` and does not care that nobody is
    /// listening.
    ///
    /// Errors instead of deadlocking if called from the port's own actor
    /// thread — that actor is the thread that would have to produce the reply.
    pub fn wait_blocking(self, timeout: Duration) -> AsynResult<RequestResult> {
        guard_reentrant(self.actor, &self.port_name, "wait_blocking")?;
        let port_name = self.port_name.clone();
        block_on_reply_until(self, Instant::now() + timeout).unwrap_or_else(|| {
            Err(AsynError::Status {
                status: AsynStatus::Timeout,
                message: format!(
                    "wait_blocking timed out after {timeout:?} waiting for port {port_name} \
                     to complete the request"
                ),
            })
        })
    }
}

impl std::future::Future for AsyncCompletionHandle {
    type Output = AsynResult<RequestResult>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match std::pin::Pin::new(&mut self.rx).poll(cx) {
            std::task::Poll::Ready(Ok(result)) => std::task::Poll::Ready(result),
            std::task::Poll::Ready(Err(_)) => std::task::Poll::Ready(Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "actor dropped reply channel".into(),
            })),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Cloneable async handle to a port actor.
///
/// All methods construct the appropriate [`RequestOp`], send it to the actor,
/// and return a completion handle.
#[derive(Clone)]
pub struct PortHandle {
    tx: mpsc::UnboundedSender<ActorMessage>,
    port_name: Arc<str>,
    /// Identity of the actor on the receiving end of `tx`. Every blocking entry
    /// point compares it against the calling thread — see [`guard_reentrant`].
    actor: ActorId,
    /// The actor's [`crate::request::RequestCell`] queue — see [`Self::new_cell`].
    cells: Arc<crate::request::CellQueue>,
    /// The port-level half of C `queueRequest`'s gate, over the actor's own
    /// cells — see [`Self::queue_gate`].
    gate: QueueGate,
    /// The port's device list, which C's `connectDevice` reaches on the
    /// binding thread — see [`Self::connect_device`].
    devices: crate::port::DeviceTable,
    interrupts: Arc<InterruptManager>,
    can_block: bool,
    multi_device: bool,
    max_addr: i32,
    /// The port's interface registry — the driver's own
    /// [`crate::port::PortDriver::capabilities`] declaration, recorded when the
    /// port is registered. C's asynManager keeps the same thing as the list of
    /// `registerInterface` calls the driver made, and clients ask for it with
    /// `findInterface` (asynManager.c:1470-1514); asynRecord uses it to fill
    /// OCTETIV / I32IV / UI32IV / F64IV / OPTIONIV / GPIBIV
    /// (asynRecord.c:1177-1240). Registration-time, not a runtime query: a
    /// driver cannot gain or lose an interface after `registerInterface`.
    interfaces: Arc<[Capability]>,
    /// The trace manager the port's driver was bound to
    /// (`PortServices::bind` at `create_port_runtime`) — C's per-port
    /// `tracePvt`, reached through the port. The registry publishes this one
    /// for `asynSetTraceMask` / asynRecord `TMSK`, so a mask set by name lands
    /// in the manager the driver's `trace_print` reads; handing the registry
    /// a second manager at registration is what made those masks silent.
    trace: Arc<TraceManager>,
    sync_lock: crate::port_actor::SyncLock,
}

/// A [`PortHandle`] that does not keep its port alive: the actor ends once
/// every `PortHandle` is gone, as it would without this one. For a port's own
/// helper thread, which C holds as `this` and so never keeps the port up.
#[derive(Clone)]
pub struct WeakPortHandle {
    tx: mpsc::WeakUnboundedSender<ActorMessage>,
    port_name: Arc<str>,
    actor: ActorId,
    cells: Arc<crate::request::CellQueue>,
    gate: QueueGate,
    devices: crate::port::DeviceTable,
    interrupts: Arc<InterruptManager>,
    can_block: bool,
    multi_device: bool,
    max_addr: i32,
    interfaces: Arc<[Capability]>,
    trace: Arc<TraceManager>,
    sync_lock: crate::port_actor::SyncLock,
}

impl WeakPortHandle {
    /// Wake the actor with nothing queued, so it publishes a connection edge
    /// the link's owner moved off the port thread
    /// ([`crate::port::PortDriverBase::sync_connection_edge`]).
    pub(crate) fn wake(&self) {
        self.cells.nudge();
    }

    /// The handle, or `None` once the last [`PortHandle`] has dropped.
    pub fn upgrade(&self) -> Option<PortHandle> {
        Some(PortHandle {
            tx: self.tx.upgrade()?,
            port_name: self.port_name.clone(),
            actor: self.actor,
            cells: self.cells.clone(),
            gate: self.gate.clone(),
            devices: self.devices.clone(),
            interrupts: self.interrupts.clone(),
            can_block: self.can_block,
            multi_device: self.multi_device,
            max_addr: self.max_addr,
            interfaces: self.interfaces.clone(),
            trace: self.trace.clone(),
            sync_lock: self.sync_lock.clone(),
        })
    }
}

/// A synchronous port's callback runs on the caller's thread, each call a
/// blocking round trip — C's `queueRequest` on a port without
/// `ASYN_CANBLOCK`. The request is a message to the port's actor, so it owns
/// a copy of `op`.
impl PortCalls for PortHandle {
    fn call(&mut self, op: &RequestOp, user: AsynUser) -> AsynResult<RequestResult> {
        self.submit_blocking(op.clone(), user)
    }
}

/// What [`PortHandle::queue_request`] did with the cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueOutcome {
    /// Linked on the port's queue; its body runs on the port thread.
    Queued,
    /// The port cannot block: the body already ran.
    Ran,
}

impl PortHandle {
    /// `actor` must be the link of the actor owning `tx`'s receiving end
    /// (`PortActor::link`), so that a blocking caller can recognise a
    /// re-entrant call from that actor's own thread and a queueing caller reads
    /// that port's gate.
    pub(crate) fn new(
        tx: mpsc::UnboundedSender<ActorMessage>,
        port_name: String,
        interrupts: Arc<InterruptManager>,
        actor: ActorLink,
        trace: Arc<TraceManager>,
    ) -> Self {
        // The one place a port meets the trace it reads, so its trace
        // exceptions reach its exception users from the moment it exists.
        trace.bind_announcer(&port_name, actor.devices.announcer());
        Self {
            tx,
            port_name: port_name.into(),
            actor: actor.id,
            cells: actor.cells,
            gate: actor.gate,
            devices: actor.devices,
            interrupts,
            can_block: false,
            multi_device: false,
            max_addr: 1,
            interfaces: crate::interfaces::default_capabilities().into(),
            trace,
            sync_lock: actor.sync_lock,
        }
    }

    /// A handle that does not keep the port alive — see [`WeakPortHandle`].
    pub fn downgrade(&self) -> WeakPortHandle {
        WeakPortHandle {
            tx: self.tx.downgrade(),
            port_name: self.port_name.clone(),
            actor: self.actor,
            cells: self.cells.clone(),
            gate: self.gate.clone(),
            devices: self.devices.clone(),
            interrupts: self.interrupts.clone(),
            can_block: self.can_block,
            multi_device: self.multi_device,
            max_addr: self.max_addr,
            interfaces: self.interfaces.clone(),
            trace: self.trace.clone(),
            sync_lock: self.sync_lock.clone(),
        }
    }

    /// The port's trace manager — see the field.
    pub fn trace(&self) -> &Arc<TraceManager> {
        &self.trace
    }

    /// The trace linkage a user connected to this port carries — what
    /// `PortActor` stamps on every request it runs, made
    /// available to a layer that prints on the port's behalf without a
    /// request in flight (device support, C `pPvt->pasynUser`).
    pub fn user_trace(&self) -> crate::user::UserTrace {
        crate::user::UserTrace {
            manager: self.trace.clone(),
            port: self.port_name.clone(),
        }
    }

    /// Record the driver's declared interface set — see `Self::interfaces`.
    /// Called by the runtime layer at handle construction, from the driver's own
    /// [`crate::port::PortDriver::capabilities`].
    pub fn set_interfaces(&mut self, interfaces: Vec<Capability>) {
        self.interfaces = interfaces.into();
    }

    /// Does the port implement this interface? The port's `findInterface`
    /// (asynManager.c:1470-1514).
    pub fn has_interface(&self, iface: InterfaceType) -> bool {
        self.interfaces
            .iter()
            .any(|cap| cap.interface_type() == iface)
    }

    /// Set whether this port can perform blocking I/O.
    pub fn set_can_block(&mut self, can_block: bool) {
        self.can_block = can_block;
    }

    /// Whether this port can perform blocking I/O.
    pub fn can_block(&self) -> bool {
        self.can_block
    }

    /// Record the port's multi-device capability and address count.
    /// Called by the runtime layer at handle construction so the
    /// flags reflect the driver's `PortFlags::multi_device` /
    /// `PortDriverBase::max_addr`.
    pub fn set_capabilities(&mut self, multi_device: bool, max_addr: i32) {
        self.multi_device = multi_device;
        self.max_addr = max_addr.max(1);
    }

    /// Whether this port is multi-device. C parity:
    /// `pasynManager::isMultiDevice` (`asynManager.c::isMultiDevice`)
    /// — used by clients to decide whether to address sub-units
    /// individually or always pass `addr=0`.
    pub fn is_multi_device(&self) -> bool {
        self.multi_device
    }

    /// C `findDpCommon` (asynManager.c:538-545) as a key: which `dpCommon` an
    /// operation carrying `addr` lands on — `Some(addr)` for a device of a
    /// multi-device port, `None` for the port's own slot.
    ///
    /// Callers that need the distinction (an `asynRecord` deciding whether its
    /// trace writes address a device) ask here rather than re-deriving it from
    /// [`Self::is_multi_device`]; the ops themselves need not ask at all, since
    /// the actor resolves the addr they carry.
    pub fn dp_addr(&self, addr: i32) -> Option<i32> {
        crate::port::dp_common_key(self.multi_device, addr)
    }

    /// The parameter list a client at `addr` reads: [`crate::port::param_list`].
    pub fn param_list(&self, addr: i32) -> i32 {
        crate::port::param_list(self.multi_device, addr)
    }

    /// Maximum sub-device address (exclusive upper bound).
    /// Mirrors C `pport->numAddr` / the `maxAddr` constructor
    /// argument of `asynPortDriver`. Always at least 1.
    pub fn max_addr(&self) -> i32 {
        self.max_addr
    }

    /// C `queueRequest(pasynUser, priority, …)`'s refusals
    /// (asynManager.c:1538-1554) for `user`, at the priority it carries,
    /// answered on the calling thread from the port's own state as C answers
    /// them under `asynManagerLock` before anything is queued. The refusal is
    /// an [`AsynError::QueueRefused`], as the actor's.
    #[cfg(feature = "epics")]
    pub(crate) fn queue_gate(&self, user: &AsynUser) -> Result<(), crate::error::QueueRefusal<'_>> {
        self.gate.check(user.connect_check(self.multi_device))
    }

    /// A preallocated request on this port that runs `body` — C
    /// `createAsynUser(processUser, timeoutUser)` + `connectDevice`. Queueing it
    /// ([`crate::request::RequestCell::queue`]) allocates nothing.
    pub fn new_cell(
        &self,
        body: Arc<dyn crate::request::CellBody>,
    ) -> Arc<crate::request::RequestCell> {
        self.cells.new_cell(body)
    }

    /// C `pasynManager->queueRequest` for a cell from [`Self::new_cell`]: the
    /// port's enabled/connected refusals on the calling thread, then
    /// [`crate::request::RequestCell::queue`]. A dset whose `processUser` makes
    /// several calls under one queue entry (devDigitelPump, devVacSen) queues
    /// through this and ends its record's cycle from the cell's body.
    ///
    /// On a port without `ASYN_CANBLOCK` the body has run, with nothing else
    /// on the port in between, when this returns [`QueueOutcome::Ran`]
    /// (asynManager.c:1555-1584; see [`RequestOp::CellInline`]); a refusal of
    /// that branch is returned here, as C's `queueRequest` returns it.
    pub fn queue_request(
        &self,
        cell: &Arc<crate::request::RequestCell>,
        user: &AsynUser,
    ) -> AsynResult<QueueOutcome> {
        self.gate.check(user.connect_check(self.multi_device))?;
        if !self.can_block {
            let user = AsynUser::new(user.reason)
                .with_addr(user.addr)
                .with_priority(user.priority);
            self.submit_blocking(RequestOp::CellInline(Arc::clone(cell)), user)?;
            return Ok(QueueOutcome::Ran);
        }
        cell.queue(user)?;
        Ok(QueueOutcome::Queued)
    }

    /// Whether `cell` queues on this port.
    pub fn owns_cell(&self, cell: &crate::request::RequestCell) -> bool {
        cell.queues_on(&self.cells)
    }

    /// Port name this handle is connected to.
    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    /// Whether the port actor is gone — its receiver has been dropped, so no
    /// further request can ever complete.
    ///
    /// A driver's background task (a periodic poller) uses this as its
    /// shutdown signal: the C analogue is the `modbusExiting_` /
    /// `pasynManager->shutdown` flag a driver thread tests to leave its loop
    /// (`drvModbusAsyn.cpp:1637`). It is the *only* condition that ends such a
    /// loop — a failing request is a device error, not a shutdown.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Access the interrupt manager for subscribing to interrupt callbacks.
    pub fn interrupts(&self) -> &Arc<InterruptManager> {
        &self.interrupts
    }

    /// Submit a request and return an async completion handle (non-blocking submission).
    pub fn try_submit(&self, op: RequestOp, user: AsynUser) -> AsynResult<AsyncCompletionHandle> {
        let cancel = CancelToken::new();
        self.try_submit_cancellable(op, user, cancel)
    }

    /// Submit a cancellable request and return an async completion handle.
    pub fn try_submit_cancellable(
        &self,
        op: RequestOp,
        user: AsynUser,
        cancel: CancelToken,
    ) -> AsynResult<AsyncCompletionHandle> {
        crate::port_actor::queue_request_gate(&self.gate, &op, &user, self.multi_device)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(ActorMessage::new(op, user, cancel, reply_tx))?;
        Ok(AsyncCompletionHandle {
            rx: reply_rx,
            actor: self.actor,
            port_name: self.port_name.clone(),
        })
    }

    /// The one gate every blocking entry point on this handle passes through.
    ///
    /// Refuses a re-entrant call from the port's own actor thread (which could
    /// never complete), and otherwise parks the calling thread on the actor's
    /// reply — correctly from a plain thread, from a current-thread runtime, or
    /// from a multi-threaded worker.
    fn blocking<T>(&self, what: &str, fut: impl Future<Output = AsynResult<T>>) -> AsynResult<T> {
        guard_reentrant(self.actor, &self.port_name, what)?;
        // A blocking port's actor answers only after the driver call it is in,
        // so waiting under a record's hold holds the record for that call — a
        // read's whole timeout. C waits there on a synchronous port's
        // `synchronousLock` (asynManager.c:1555-1584), and in a synchronous
        // port's driver call that drives a can-block port through SyncIO
        // (drvScaler974 under devScalerAsyn) — the `sync_driver_wait` callers.
        #[cfg(all(feature = "epics", debug_assertions))]
        assert!(
            !(self.can_block
                && epics_base_rs::server::database::scan_lock_held() == Some(true)
                && !epics_base_rs::server::database::in_sync_driver_wait()),
            "{what} on port {} under a record's hold",
            self.port_name
        );
        // `fut` is `submit_cancellable`, which already resolves at the
        // request's own `AsynUser::queue_timeout` — `await_reply` arms that
        // deadline on this thread like any other. That satisfies the
        // precondition; see the function's doc.
        block_on_prebounded_reply(fut)
    }

    /// Submit a request and block until completion (for sync callers).
    ///
    /// Valid from a plain thread, from a current-thread runtime (an `ad-core`
    /// driver task), and from a multi-threaded worker (device support under
    /// `#[epics_main]`).
    ///
    /// Returns an error, rather than deadlocking, when called from the port's
    /// own actor thread — i.e. when a `PortDriver` method calls back into its
    /// own port. The actor cannot service a request it is itself blocked on.
    ///
    /// The request goes through [`Self::submit_cancellable`], so an
    /// [`field@AsynUser::queue_timeout`] deadline is enforced on every path: the
    /// reply wait wakes at the deadline and takes a request still queued off
    /// the queue, replying [`AsynError::QueueTimeout`].
    pub fn submit_blocking(&self, op: RequestOp, user: AsynUser) -> AsynResult<RequestResult> {
        self.blocking(
            "submit_blocking",
            self.submit_cancellable(op, user, CancelToken::new()),
        )
    }

    /// Submit a request and await completion (reliable, for async callers).
    ///
    /// Waits for channel space and returns the
    /// actor's reply. Use this for operations where delivery must be guaranteed.
    pub async fn submit_async(&self, op: RequestOp, user: AsynUser) -> AsynResult<RequestResult> {
        self.submit_cancellable(op, user, CancelToken::new()).await
    }

    /// Submit a cancellable request and await completion, threading a
    /// caller-owned [`CancelToken`].
    ///
    /// C parity: `asynManager::queueRequest` (`asynManager.c:1516`) enqueues
    /// the request on the port thread and the caller's callback runs later;
    /// `asynManager::cancelRequest` (`asynManager.c:1632`) removes a request
    /// that is still queued. A caller that may need to abort the request
    /// (asynRecord `AQR`, `asynRecord.c:393-408`) keeps the token and calls
    /// [`CancelToken::cancel`]; the actor drops a still-queued message at its
    /// cancel check. Unlike [`Self::submit_async`] (which mints a throwaway
    /// token) the token belongs to the caller, so the same token both arms
    /// and later cancels the request.
    pub async fn submit_cancellable(
        &self,
        op: RequestOp,
        user: AsynUser,
        cancel: CancelToken,
    ) -> AsynResult<RequestResult> {
        let lock_port = user.lock_port == Some(crate::user::LockPort::Queued);
        let result = async {
            crate::port_actor::queue_request_gate(&self.gate, &op, &user, self.multi_device)?;
            let queue_timeout = user.queue_timeout;
            let (reply_tx, reply_rx) = oneshot::channel();
            self.send(ActorMessage::new(op, user, cancel.clone(), reply_tx))?;
            self.await_reply(reply_rx, cancel, queue_timeout).await
        }
        .await;
        if lock_port {
            result.map_err(AsynError::through_queue_lock_port)
        } else {
            result
        }
    }

    /// Wait for the actor's reply, honouring the request's queue-wait deadline —
    /// C's `epicsTimer` armed by `queueRequest` (asynManager.c:1619-1625).
    ///
    /// The timer is the caller's, as it is in C: `queueTimeoutCallback` runs the
    /// *user's* `timeoutUser`, so the deadline is only worth waking for where the
    /// caller has something to do at it — asynRecord's `pact` process request,
    /// which reports "process queueRequest timeout" and forces the record's
    /// completion 10 s after queueing, whatever the port is doing
    /// (asynRecord.c:919-927).
    ///
    /// Whether the deadline is honoured is not this timer's call: it hands the
    /// question to [`CancelToken::time_out_if_queued`], C's `if(!isQueued)`
    /// guard. A request the actor has already begun is *never* aborted — the
    /// wait simply resumes — so this timer cannot cut a transfer short, only a
    /// wait. The actor reads no deadline: a request with no waiter is timed
    /// by `RequestCell`'s queue timer instead.
    async fn await_reply(
        &self,
        mut reply_rx: oneshot::Receiver<AsynResult<RequestResult>>,
        cancel: CancelToken,
        queue_timeout: Option<QueueDeadline>,
    ) -> AsynResult<RequestResult> {
        let dropped = || AsynError::Status {
            status: AsynStatus::Error,
            message: "actor dropped reply channel".into(),
        };
        // No gate on whether a timer exists here: the seam's `timeout` arms
        // one the calling thread can reach, so `queue_timeout` means the same
        // thing on every path. It did not always — this used to ask
        // `Handle::try_current()`, which answers "is tokio the timer here" and
        // so dropped the deadline for every caller on `exec_backend` (no tokio
        // runtime exists in that process at all) and for plain-thread
        // `submit_blocking` on the hosted one. C arms the timer in
        // `queueRequest` regardless of what the caller is, and so does this.
        if let Some(limit) = queue_timeout {
            match epics_libcom_rs::runtime::task::timeout(limit.get(), &mut reply_rx).await {
                Ok(reply) => return reply.map_err(|_| dropped())?,
                Err(_elapsed) => {
                    if cancel.time_out_if_queued() {
                        // C's timer signals the port thread (asynManager.c:702)
                        // so the request's entry leaves the queue now.
                        self.cells.poke();
                        return Err(AsynError::QueueTimeout {
                            port: self.port_name.to_string(),
                        });
                    }
                    // Already running (C `!isQueued`): the timer fired too late.
                }
            }
        }
        reply_rx.await.map_err(|_| dropped())?
    }

    // --- Typed convenience methods ---

    pub async fn read_int32(&self, reason: usize, addr: i32) -> AsynResult<i32> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self.submit_async(RequestOp::Int32Read, user).await?;
        result.int_val.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "int32 read returned no value".into(),
        })
    }

    pub async fn write_int32(&self, reason: usize, addr: i32, value: i32) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_async(RequestOp::Int32Write { value }, user)
            .await?;
        Ok(())
    }

    pub async fn read_int64(&self, reason: usize, addr: i32) -> AsynResult<i64> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self.submit_async(RequestOp::Int64Read, user).await?;
        result.int64_val.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "int64 read returned no value".into(),
        })
    }

    pub async fn write_int64(&self, reason: usize, addr: i32, value: i64) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_async(RequestOp::Int64Write { value }, user)
            .await?;
        Ok(())
    }

    pub async fn read_float64(&self, reason: usize, addr: i32) -> AsynResult<f64> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self.submit_async(RequestOp::Float64Read, user).await?;
        result.float_val.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "float64 read returned no value".into(),
        })
    }

    pub async fn write_float64(&self, reason: usize, addr: i32, value: f64) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_async(RequestOp::Float64Write { value }, user)
            .await?;
        Ok(())
    }

    pub async fn read_octet(
        &self,
        reason: usize,
        addr: i32,
        buf_size: usize,
    ) -> AsynResult<Vec<u8>> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self
            .submit_async(RequestOp::OctetRead { buf_size }, user)
            .await?;
        result.data.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "octet read returned no data".into(),
        })
    }

    /// One locked, flush-first write/read pair — C
    /// `asynOctetSyncIO.c:231-276` `writeRead`.
    ///
    /// C brackets the whole exchange in `queueLockPort` (:246) …
    /// `queueUnlockPort` (:271), flushes INSIDE that lock and BEFORE the
    /// write (:250), and unlocks on every exit path via `goto bad`. All
    /// three properties hold here by construction: the pair is a single
    /// [`RequestOp::OctetWriteRead`], so the port actor — which owns the
    /// driver and runs one op to completion — is the lock, the flush is
    /// the op's own first step, and there is no exit path that can leave
    /// the port half-owned because ownership ends when the op returns.
    ///
    /// This is the primitive a protocol driver must use for a
    /// request/response transaction. Submitting `OctetWrite` and then
    /// `OctetRead` separately is NOT equivalent: another client of the same
    /// port can be served between the two, and a reply that arrived late
    /// from the previous exchange is still in the buffer.
    pub async fn write_read(
        &self,
        reason: usize,
        addr: i32,
        data: &[u8],
        buf_size: usize,
    ) -> AsynResult<(Vec<u8>, crate::interpose::EomReason)> {
        let user = AsynUser::new(reason).with_addr(addr).locking_port(self);
        let result = self
            .submit_async(
                RequestOp::OctetWriteRead {
                    data: data.to_vec(),
                    buf_size,
                    flush: true,
                },
                user,
            )
            .await?;
        let data = result.data.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "octet write/read returned no data".into(),
        })?;
        let eom = crate::interpose::EomReason::from_bits_truncate(result.eom_reason);
        Ok((data, eom))
    }

    /// Read variant that also surfaces the end-of-message reason
    /// flags. C parity: `asynOctet::read(... int *eomReason)`
    /// (`interfaces/asynOctet.h:38-40`) — consumers like asynRecord
    /// (`EOMR` field, `asynRecord.c`) need the flag bitmap to tell
    /// "byte count reached" from "EOS matched" from "END asserted".
    pub async fn read_octet_eom(
        &self,
        reason: usize,
        addr: i32,
        buf_size: usize,
    ) -> AsynResult<(Vec<u8>, crate::interpose::EomReason)> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self
            .submit_async(RequestOp::OctetRead { buf_size }, user)
            .await?;
        let data = result.data.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "octet read returned no data".into(),
        })?;
        let eom = crate::interpose::EomReason::from_bits_truncate(result.eom_reason);
        Ok((data, eom))
    }

    /// Returns the number of bytes transferred (C `asynOctet::write`'s
    /// `*nbytesTransferred`).
    pub async fn write_octet(&self, reason: usize, addr: i32, data: Vec<u8>) -> AsynResult<usize> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self
            .submit_async(RequestOp::OctetWrite { data }, user)
            .await?;
        Ok(result.nbytes)
    }

    pub async fn read_uint32_digital(
        &self,
        reason: usize,
        addr: i32,
        mask: u32,
    ) -> AsynResult<u32> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self
            .submit_async(RequestOp::UInt32DigitalRead { mask }, user)
            .await?;
        result.uint_val.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "uint32 read returned no value".into(),
        })
    }

    pub async fn write_uint32_digital(
        &self,
        reason: usize,
        addr: i32,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_async(RequestOp::UInt32DigitalWrite { value, mask }, user)
            .await?;
        Ok(())
    }

    pub async fn flush(&self, reason: usize, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_async(RequestOp::Flush, user).await?;
        Ok(())
    }

    /// Resolve a bind request ([`DrvUserRequest`]) to its shared reason index. The
    /// async port-handle path needs only the reason; the per-record [`DrvUserInfo`]
    /// (string-length cap etc.) is surfaced on the record-binding blocking path
    /// [`drv_user_create_blocking`](Self::drv_user_create_blocking).
    pub async fn drv_user_create(&self, req: &DrvUserRequest) -> AsynResult<usize> {
        let user = AsynUser::default().with_addr(req.addr);
        let result = self
            .submit_async(RequestOp::DrvUserCreate(req.clone()), user)
            .await?;
        result.reason.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "drv_user_create returned no reason".into(),
        })
    }

    pub async fn read_enum(&self, reason: usize, addr: i32) -> AsynResult<usize> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self.submit_async(RequestOp::EnumRead, user).await?;
        result.enum_index.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "enum read returned no index".into(),
        })
    }

    pub async fn write_enum(&self, reason: usize, addr: i32, index: usize) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_async(RequestOp::EnumWrite { index }, user)
            .await?;
        Ok(())
    }

    pub async fn read_int32_array(
        &self,
        reason: usize,
        addr: i32,
        max_elements: usize,
    ) -> AsynResult<SharedArray<i32>> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self
            .submit_async(
                RequestOp::Int32ArrayRead {
                    buf: SharedArray::default(),
                    max_elements,
                },
                user,
            )
            .await?;
        result.int32_array.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "int32 array read returned no data".into(),
        })
    }

    pub async fn write_int32_array(
        &self,
        reason: usize,
        addr: i32,
        data: Vec<i32>,
    ) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_async(RequestOp::Int32ArrayWrite { data }, user)
            .await?;
        Ok(())
    }

    pub async fn read_float64_array(
        &self,
        reason: usize,
        addr: i32,
        max_elements: usize,
    ) -> AsynResult<SharedArray<f64>> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self
            .submit_async(
                RequestOp::Float64ArrayRead {
                    buf: SharedArray::default(),
                    max_elements,
                },
                user,
            )
            .await?;
        result.float64_array.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "float64 array read returned no data".into(),
        })
    }

    pub async fn write_float64_array(
        &self,
        reason: usize,
        addr: i32,
        data: Vec<f64>,
    ) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_async(RequestOp::Float64ArrayWrite { data }, user)
            .await?;
        Ok(())
    }

    /// Flush changed parameters as interrupt notifications (async).
    pub async fn call_param_callbacks(&self, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        self.submit_async(
            RequestOp::CallParamCallbacks {
                addr,
                updates: vec![],
                time_stamp: None,
            },
            user,
        )
        .await?;
        Ok(())
    }

    // --- Sync convenience methods ---

    /// The one round trip allowed under a record's hold, until the driver model
    /// changes: asynRecord's DRVINFO resolution on a PORT/ADDR/DRVINFO put. C
    /// calls `pasynDrvUser->create` directly under `dbScanLock`
    /// (asynRecord.c:1249); here the driver lives on its actor, and a driver
    /// may create the parameter in it, so there is no direct form.
    /// A [`sync_driver_wait`](epics_base_rs::server::database::sync_driver_wait)
    /// caller, until the post-stage-3 driver-model item.
    #[cfg(feature = "epics")]
    pub(crate) fn drv_user_create_on_put(&self, req: &DrvUserRequest) -> AsynResult<DrvUserInfo> {
        epics_base_rs::server::database::sync_driver_wait(|| self.drv_user_create_blocking(req))
    }

    pub fn drv_user_create_blocking(&self, req: &DrvUserRequest) -> AsynResult<DrvUserInfo> {
        let user = AsynUser::default().with_addr(req.addr);
        let result = self.submit_blocking(RequestOp::DrvUserCreate(req.clone()), user)?;
        let reason = result.reason.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "drv_user_create returned no reason".into(),
        })?;
        Ok(DrvUserInfo {
            reason,
            max_octet_len: result.max_octet_len,
        })
    }

    pub fn read_int32_blocking(&self, reason: usize, addr: i32) -> AsynResult<i32> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self.submit_blocking(RequestOp::Int32Read, user)?;
        result.int_val.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "int32 read returned no value".into(),
        })
    }

    pub fn write_int32_blocking(&self, reason: usize, addr: i32, value: i32) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_blocking(RequestOp::Int32Write { value }, user)?;
        Ok(())
    }

    pub fn read_float64_blocking(&self, reason: usize, addr: i32) -> AsynResult<f64> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self.submit_blocking(RequestOp::Float64Read, user)?;
        result.float_val.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "float64 read returned no value".into(),
        })
    }

    pub fn write_float64_blocking(&self, reason: usize, addr: i32, value: f64) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_blocking(RequestOp::Float64Write { value }, user)?;
        Ok(())
    }

    /// The flush when interrupts open, on the actor; see
    /// [`PortDriverBase::boot_param_callbacks`](crate::port::PortDriverBase::boot_param_callbacks).
    pub(crate) fn boot_param_callbacks_blocking(&self, addr: i32) -> AsynResult<()> {
        let call = DriverCall::new(move |driver| {
            let _ = driver.base_mut().boot_param_callbacks(addr);
        });
        self.submit_blocking(RequestOp::WithDriver(call), AsynUser::default())?;
        Ok(())
    }

    /// Flush changed parameters as interrupt notifications (blocking).
    pub fn call_param_callbacks_blocking(&self, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        self.submit_blocking(
            RequestOp::CallParamCallbacks {
                addr,
                updates: vec![],
                time_stamp: None,
            },
            user,
        )?;
        Ok(())
    }

    /// Set parameters directly and fire callbacks atomically (reliable, blocking).
    ///
    /// This is the correct way for **background/acquisition threads** to update
    /// parameters. It mirrors the C ADCore pattern of:
    /// ```c
    /// setIntegerParam(reason, value);
    /// setDoubleParam(reason, value);
    /// callParamCallbacks();
    /// ```
    ///
    /// Delivery to the actor inbox is guaranteed (waits if channel is full).
    /// The actor processes messages in FIFO order.
    ///
    /// `addr` names the address list to flush; a batch that writes further
    /// lists gets those flushed too, in the order it first wrote them, so a
    /// multi-device driver can publish every address in one call.
    ///
    /// # Example
    /// ```ignore
    /// port_handle.set_params_and_notify(0, vec![
    ///     ParamSetValue::new(acquire_busy, 0, ParamValue::Int32(0)),
    ///     ParamSetValue::new(status, 0, ParamValue::Int32(ADStatus::Idle as i32)),
    /// ]).await?;
    /// ```
    ///
    /// Guarantees inbox enqueue (`send().await`). Does NOT wait for actor
    /// processing — returns as soon as the message is in the channel.
    /// Use `_blocking` variant from non-async contexts (std threads).
    pub async fn set_params_and_notify(
        &self,
        addr: i32,
        updates: Vec<crate::request::ParamSetValue>,
    ) -> AsynResult<()> {
        self.enqueue(
            RequestOp::CallParamCallbacks {
                addr,
                updates,
                time_stamp: None,
            },
            addr,
        )
        .await
    }

    /// [`Self::set_params_and_notify`] after C `setTimeStamp(ts)`, under the
    /// same lock: every callback of the batch, and every later one until the
    /// next update, carries `ts`.
    pub async fn set_time_stamp_params_and_notify(
        &self,
        addr: i32,
        ts: std::time::SystemTime,
        updates: Vec<crate::request::ParamSetValue>,
    ) -> AsynResult<()> {
        self.enqueue(
            RequestOp::CallParamCallbacks {
                addr,
                updates,
                time_stamp: Some(ts),
            },
            addr,
        )
        .await
    }

    /// Sync version of [`set_params_and_notify`](Self::set_params_and_notify)
    /// for non-async contexts (std threads, acquisition tasks).
    ///
    /// Blocks only until the update is accepted into the actor's inbox. Valid
    /// from a plain thread, from a current-thread runtime, and from a
    /// multi-threaded worker; errors, rather than deadlocking, when called from
    /// the port's own actor thread (a full inbox could only be drained by the
    /// actor we would be blocking).
    pub fn set_params_and_notify_blocking(
        &self,
        addr: i32,
        updates: Vec<crate::request::ParamSetValue>,
    ) -> AsynResult<()> {
        self.blocking(
            "set_params_and_notify_blocking",
            self.enqueue(
                RequestOp::CallParamCallbacks {
                    addr,
                    updates,
                    time_stamp: None,
                },
                addr,
            ),
        )
    }

    /// C `setXxxParam` of each update without the `callParamCallbacks`: the
    /// values are in the store for any request queued after this one, and
    /// marked changed until a [`Self::call_param_callbacks_blocking`] on
    /// their list posts them. Blocks only until the inbox accepts it, as
    /// [`Self::set_params_and_notify_blocking`].
    pub fn set_params_blocking(
        &self,
        addr: i32,
        updates: Vec<crate::request::ParamSetValue>,
    ) -> AsynResult<()> {
        self.blocking(
            "set_params_blocking",
            self.enqueue(RequestOp::SetParams { updates }, addr),
        )
    }

    /// Enqueue-only async helper: guaranteed delivery, no reply wait.
    async fn enqueue(&self, op: RequestOp, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        let (reply_tx, _) = oneshot::channel();
        self.send(ActorMessage::new(op, user, CancelToken::new(), reply_tx))
    }

    /// C `queueRequest`'s `ellAdd` + `epicsEventSignal` (asynManager.c:1615,
    /// :1628): the queue has no capacity to refuse on, so only a gone actor does.
    fn send(&self, mut msg: ActorMessage) -> AsynResult<()> {
        // The actor cannot answer while it holds the lock, so the try-lock is
        // taken here, on the submitting thread, for every route to a report.
        if let RequestOp::Report { sync_locked, .. } = &mut msg.op {
            *sync_locked = self.sync_lock.is_held();
        }
        self.tx.send(msg).map_err(|_| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("actor channel closed for port {}", self.port_name),
        })
    }

    // --- Driver access ---

    /// Run `f` against this port's driver, on the actor, and return what it
    /// returns — the analog of a C driver thread's `lock()` … `unlock()`
    /// (asynPortDriver.cpp:3858). `D` is the driver's concrete type; a port
    /// whose driver is some other type is an error, not a panic.
    pub async fn with_driver<D, R>(
        &self,
        f: impl FnOnce(&mut D) -> R + Send + 'static,
    ) -> AsynResult<R>
    where
        D: PortDriver,
        R: Send + 'static,
    {
        let (call, result) = self.driver_call(f);
        self.submit_async(RequestOp::WithDriver(call), AsynUser::default())
            .await?;
        self.driver_result::<D, R>(result)
    }

    /// [`Self::with_driver`] for sync callers; refuses the port's own actor
    /// thread like every blocking entry point.
    pub fn with_driver_blocking<D, R>(
        &self,
        f: impl FnOnce(&mut D) -> R + Send + 'static,
    ) -> AsynResult<R>
    where
        D: PortDriver,
        R: Send + 'static,
    {
        let (call, result) = self.driver_call(f);
        self.submit_blocking(RequestOp::WithDriver(call), AsynUser::default())?;
        self.driver_result::<D, R>(result)
    }

    /// A driver thread's C `lock()`, held across several steps until the
    /// returned [`DriverHold`] drops (`unlock()`): the actor stays inside one
    /// request running the steps [`DriverHold::run`] sends, so a client
    /// request issued meanwhile lands after the release. Returns once the
    /// lock is taken.
    pub fn hold_driver_blocking<D: PortDriver>(&self) -> AsynResult<DriverHold<D>> {
        let mut hold = DriverHold {
            port: self.clone(),
            ops: None,
            _thread: std::marker::PhantomData,
        };
        hold.ops = Some(hold.enter()?);
        Ok(hold)
    }

    /// C `updateTimeStamp` from a driver's own task: refresh the port's
    /// timestamp through its source, on the actor, and return it.
    pub async fn update_time_stamp(&self) -> AsynResult<SystemTime> {
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        let call = DriverCall::new(move |driver| {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(driver.base_mut().update_time_stamp());
        });
        self.submit_async(RequestOp::WithDriver(call), AsynUser::default())
            .await?;
        let stamp = result.lock().unwrap_or_else(|e| e.into_inner()).take();
        stamp.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("port {}: the timestamp update did not run", self.port_name),
        })
    }

    /// C `pasynManager->lockPort` … `unlockPort`: run `f` on the actor, where
    /// each [`PortCalls::call`] goes straight to the port's interfaces (through
    /// its interpose stack) with no queue gate and no autoconnect — so a read on
    /// a down link returns the driver's own error, as a C driver thread's
    /// `pasynOctet->read` between the two does. Several calls in one `f` run with
    /// nothing in between. A shut-down port refuses with C's `asynDisabled`.
    pub fn lock_port_blocking<R>(
        &self,
        f: impl FnOnce(&mut dyn PortCalls) -> R + Send + 'static,
    ) -> AsynResult<R>
    where
        R: Send + 'static,
    {
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        let call = PortLockCall::new(move |calls| {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(f(calls));
        });
        self.submit_blocking(RequestOp::LockPort(call), AsynUser::default())?;
        let value = result.lock().unwrap_or_else(|e| e.into_inner()).take();
        value.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("port {}: port lock call did not run", self.port_name),
        })
    }

    /// C `getStringParam(list, index, maxChars, value)` from a driver's own
    /// thread (asynPortDriver.cpp:543-553): the stored text up to its first
    /// NUL and at most `max_chars - 1` bytes, read on the actor. Not
    /// `readOctet`, whose count includes the NUL it appends.
    pub async fn get_string_param(
        &self,
        reason: usize,
        addr: i32,
        max_chars: usize,
    ) -> AsynResult<Vec<u8>> {
        let (call, result) = string_param_call(reason, addr, max_chars);
        self.submit_async(RequestOp::WithDriver(call), AsynUser::default())
            .await?;
        string_param_result(&result)
    }

    /// [`Self::get_string_param`] for sync callers.
    pub fn get_string_param_blocking(
        &self,
        reason: usize,
        addr: i32,
        max_chars: usize,
    ) -> AsynResult<Vec<u8>> {
        let (call, result) = string_param_call(reason, addr, max_chars);
        self.submit_blocking(RequestOp::WithDriver(call), AsynUser::default())?;
        string_param_result(&result)
    }

    fn driver_call<D, R>(
        &self,
        f: impl FnOnce(&mut D) -> R + Send + 'static,
    ) -> (DriverCall, Arc<Mutex<Option<R>>>)
    where
        D: PortDriver,
        R: Send + 'static,
    {
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        let call = DriverCall::new(move |driver| {
            let driver: &mut dyn Any = driver;
            if let Some(driver) = driver.downcast_mut::<D>() {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(f(driver));
            }
        });
        (call, result)
    }

    fn driver_result<D, R>(&self, result: Arc<Mutex<Option<R>>>) -> AsynResult<R> {
        let value = result.lock().unwrap_or_else(|e| e.into_inner()).take();
        value.ok_or_else(|| self.wrong_driver::<D>())
    }

    fn wrong_driver<D>(&self) -> AsynError {
        AsynError::Status {
            status: AsynStatus::Error,
            message: format!(
                "port {}: driver is not a {}",
                self.port_name,
                std::any::type_name::<D>()
            ),
        }
    }

    // --- Option convenience methods ---

    /// C `asynOption::getOption`, run under the caller's `AsynUser` for the same
    /// reason [`Self::set_option_blocking`] is: in C the read happens *inside*
    /// the caller's queued request (asynRecord's `callbackGetOption`,
    /// asynRecord.c:845-849), so it inherits that request's addr, its I/O timeout
    /// and its queue-wait deadline. A default user here would silently give the
    /// record's option readbacks a different (and unbounded) queue wait from the
    /// `setOption` they follow.
    pub fn get_option_blocking(&self, user: AsynUser, key: &str) -> AsynResult<String> {
        let result = self.submit_blocking(
            RequestOp::GetOption {
                key: key.to_string(),
            },
            user,
        )?;
        result.option_value.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "get_option returned no value".into(),
        })
    }

    /// C `asynOption::setOption` — the caller supplies the `AsynUser`, because in
    /// C the caller's `pasynUser` is what the option write runs under: its `addr`
    /// selects the device and its `timeout` bounds any wire traffic the write
    /// causes (an RFC 2217 negotiation on a COM port, `asynInterposeCom.c:488`).
    /// asynRecord passes the record's user (TMOT); iocsh `asynSetOption` passes
    /// its own, at 2 s (`asynShellCommands.c:119`). There is no default here to
    /// paper over the difference.
    pub fn set_option_blocking(&self, user: AsynUser, key: &str, value: &str) -> AsynResult<()> {
        self.submit_blocking(
            RequestOp::SetOption {
                key: key.to_string(),
                value: value.to_string(),
            },
            user,
        )?;
        Ok(())
    }

    /// The port's `reportPrintPort` text (asynManager.c:1020-1124): the
    /// manager block, then the driver's `report(level)`, run on the actor
    /// thread. The caller picks the stream, as C's caller passes `fp`.
    pub fn report_blocking(&self, level: i32) -> AsynResult<String> {
        let user = AsynUser::default();
        let op = RequestOp::Report {
            level,
            sync_locked: false,
        };
        let result = self.submit_blocking(op, user)?;
        Ok(String::from_utf8_lossy(result.data.as_deref().unwrap_or_default()).into_owned())
    }

    /// Apply input EOS bytes to the port through the actor — C
    /// `pasynOctet->setInputEos`. asynRecord IEOS writes (and any
    /// other caller that wants to change the in-driver EOS) MUST
    /// reach the driver via this path; the previous option-key
    /// route only wrote to `PortDriverBase::options` which no
    /// driver consumes.
    ///
    /// Takes the caller's `AsynUser` for the reason
    /// [`Self::get_option_blocking`] does: C runs the EOS write inside the
    /// caller's queued request (`callbackSetEos`, asynRecord.c:851-854).
    pub fn set_input_eos_blocking(&self, user: AsynUser, eos: &[u8]) -> AsynResult<()> {
        self.submit_blocking(RequestOp::SetInputEos { eos: eos.to_vec() }, user)?;
        Ok(())
    }

    /// Apply output EOS bytes — C `pasynOctet->setOutputEos`.
    pub fn set_output_eos_blocking(&self, user: AsynUser, eos: &[u8]) -> AsynResult<()> {
        self.submit_blocking(RequestOp::SetOutputEos { eos: eos.to_vec() }, user)?;
        Ok(())
    }

    /// Read back the driver's input EOS bytes — C `pasynOctet->getInputEos`,
    /// the read half asynRecord's `getEos` (asynRecord.c:1987-2025) runs after
    /// every IEOS/OEOS put.
    pub fn get_input_eos_blocking(&self, user: AsynUser) -> AsynResult<Vec<u8>> {
        let result = self.submit_blocking(RequestOp::GetInputEos, user)?;
        Ok(result.data.unwrap_or_default())
    }

    /// Read back the driver's output EOS bytes — C `pasynOctet->getOutputEos`.
    pub fn get_output_eos_blocking(&self, user: AsynUser) -> AsynResult<Vec<u8>> {
        let result = self.submit_blocking(RequestOp::GetOutputEos, user)?;
        Ok(result.data.unwrap_or_default())
    }

    pub async fn get_option(&self, user: AsynUser, key: &str) -> AsynResult<String> {
        let result = self
            .submit_async(
                RequestOp::GetOption {
                    key: key.to_string(),
                },
                user,
            )
            .await?;
        result.option_value.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "get_option returned no value".into(),
        })
    }

    /// Async [`Self::set_option_blocking`] — same rule: the caller owns the
    /// `AsynUser` that bounds the write.
    pub async fn set_option(&self, user: AsynUser, key: &str, value: &str) -> AsynResult<()> {
        self.submit_async(
            RequestOp::SetOption {
                key: key.to_string(),
                value: value.to_string(),
            },
            user,
        )
        .await?;
        Ok(())
    }

    // --- Multi-device convenience methods ---

    /// The port-side half of C `pasynManager->connectDevice(pasynUser, port,
    /// addr)` (asynManager.c:1324-1357): create the device's dpCommon on a
    /// multi-device port (`locateDevice(..., TRUE)`) so it is counted and
    /// reported from the moment a user binds to it. Every user bind — a
    /// record's device support at init, `asynRecord`'s PORT/ADDR connect —
    /// goes through here; it is not the transport `connect` of
    /// [`Self::connect_addr_blocking`]. C does it on the caller's thread under
    /// `asynManagerLock`, so this takes the port's device-list lock and asks
    /// nothing of the actor: `asynRecord` binds under the record's lock.
    /// Answers the cells of the `dpCommon` the user is bound to (see
    /// [`crate::port::DpCells`]).
    pub fn connect_device(&self, addr: i32) -> crate::port::DpCells {
        self.devices.connect_user(addr)
    }

    /// The port's announcement owner, which its exception lists hang off.
    pub(crate) fn announcer(&self) -> crate::port::ExceptionAnnouncer {
        self.devices.announcer()
    }

    /// C `exceptionCallbackAdd` (asynManager.c:1364-1382) for a user bound to
    /// `addr`; dropping the guard is `exceptionCallbackRemove`. The callback
    /// runs on the announcing thread — this port's actor, mid-transition — so
    /// it may read [`Self::connect_device`]'s cells but must not wait on this
    /// port.
    pub fn exception_callback_add<F>(
        &self,
        addr: i32,
        callback: F,
    ) -> crate::exception::ExceptionCallbackGuard
    where
        F: Fn(&crate::exception::ExceptionEvent<'_>) + Send + Sync + 'static,
    {
        self.devices.exception_callback_add(addr, callback)
    }

    /// C `exceptionDisconnect` (asynManager.c:2165-2190) for a user bound to
    /// `addr`. Queued without waiting: its caller is typically another port's
    /// exception callback, running on that port's actor, and C's callers
    /// ignore the status (drvLove.c:686).
    pub fn exception_disconnect(&self, addr: i32) -> AsynResult<()> {
        self.try_submit(
            RequestOp::ExceptionDisconnect,
            AsynUser::default().with_addr(addr),
        )
        .map(drop)
    }

    /// Was the port shut down? See [`crate::port::DeviceTable::is_defunct`].
    pub(crate) fn is_defunct(&self) -> bool {
        self.devices.is_defunct()
    }

    pub fn connect_addr_blocking(&self, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        self.submit_blocking(RequestOp::ConnectAddr, user)?;
        Ok(())
    }

    pub fn disconnect_addr_blocking(&self, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        self.submit_blocking(RequestOp::DisconnectAddr, user)?;
        Ok(())
    }

    /// The port's C `queueLockPortTimeout`: every SyncIO call queues with at
    /// least this deadline — see [`AsynUser::locking_port`].
    pub fn queue_lock_port_timeout(&self) -> Duration {
        self.devices.queue_lock_port_timeout()
    }

    /// C `pasynManager->setQueueLockPortTimeout` (asynManager.c:2393-2406),
    /// behind the iocsh `asynSetQueueLockPortTimeout`.
    pub fn set_queue_lock_port_timeout(&self, timeout: Duration) {
        self.devices.set_queue_lock_port_timeout(timeout);
    }

    /// C `pasynManager->enable(pasynUser, enable)` (asynManager.c:2224-2251),
    /// fired by `asynRecord` `ENBL` field writes (`asynRecord.c:484-486`).
    ///
    /// `addr` is the address of the caller's user, not a scope switch: C has
    /// one `enable`, and `findDpCommon` (:538-545) makes it land on the DEVICE's
    /// `dpCommon` for a multi-device port the caller addressed, on the port's
    /// own otherwise. `-1` names the port explicitly.
    ///
    /// C stores under `asynManagerLock` on the caller's thread and raises the
    /// exception there after unlocking, so asynRecord's `exceptCallback` runs
    /// inside `special()`, once per put (asynRecord.c:905-917). The actor is
    /// woken as `announceExceptionOccurred`'s `notifyPortThread` wakes it.
    pub fn set_enable(&self, addr: i32, enable: bool) -> AsynResult<()> {
        self.devices.enable(addr, enable)?;
        self.cells.nudge();
        Ok(())
    }

    /// C `pasynManager->autoConnect(pasynUser, autoConnect)`
    /// (asynManager.c:2312-2329), fired by `asynRecord` `AUCT` field writes
    /// (`asynRecord.c:481-482`). Emits `asynExceptionAutoConnect`
    /// unconditionally; `addr` resolves as in [`Self::set_enable`].
    /// Stored and announced here, as [`Self::set_enable`]; the connect timer it
    /// owes is the actor's.
    pub fn set_auto_connect(&self, addr: i32, yes: bool) -> AsynResult<()> {
        self.devices.auto_connect(addr, yes);
        self.cells.nudge();
        Ok(())
    }

    // --- Bounds convenience methods ---

    pub fn get_bounds_int32_blocking(&self, reason: usize, addr: i32) -> AsynResult<(i64, i64)> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self.submit_blocking(RequestOp::GetBoundsInt32, user)?;
        result.bounds.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "get_bounds returned no bounds".into(),
        })
    }

    pub fn get_bounds_int64_blocking(&self, reason: usize, addr: i32) -> AsynResult<(i64, i64)> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self.submit_blocking(RequestOp::GetBoundsInt64, user)?;
        result.bounds.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "get_bounds returned no bounds".into(),
        })
    }

    // --- Port-state query convenience methods ---

    /// C `pasynManager->isEnabled` (asynManager.c:2345-2359): the `enabled`
    /// flag of the `dpCommon` this port resolves `addr` to.
    ///
    /// The address is not decoration. `findDpCommon` hands back the DEVICE's
    /// slot on an `ASYN_MULTIDEVICE` port whenever the user names one
    /// (:538-546), so a per-device `asynEnable P 1 0` is invisible at addr 0
    /// and at the port. `-1` asks the port's own slot — the answer for every
    /// single-device port, whatever addr is passed.
    pub fn is_enabled_blocking(&self, addr: i32) -> AsynResult<bool> {
        let result =
            self.submit_blocking(RequestOp::GetEnable, AsynUser::new(0).with_addr(addr))?;
        Ok(result.int_val.unwrap_or(0) != 0)
    }

    /// C `pasynManager->isAutoConnect` (asynManager.c:2361-2373) — same
    /// per-address resolution as [`Self::is_enabled_blocking`].
    pub fn is_auto_connect_blocking(&self, addr: i32) -> AsynResult<bool> {
        let result =
            self.submit_blocking(RequestOp::GetAutoConnect, AsynUser::new(0).with_addr(addr))?;
        Ok(result.int_val.unwrap_or(0) != 0)
    }

    /// C `pasynManager->isConnected` (asynManager.c:2331-2343) — same
    /// per-address resolution as [`Self::is_enabled_blocking`].
    pub fn is_connected_blocking(&self, addr: i32) -> AsynResult<bool> {
        let result =
            self.submit_blocking(RequestOp::GetConnected, AsynUser::new(0).with_addr(addr))?;
        Ok(result.int_val.unwrap_or(0) != 0)
    }

    /// Async twins of the three port-state queries above. An `asynRecord`
    /// exception callback runs on the *port actor thread* (the driver announces
    /// the exception from inside its own op), so it cannot use the `_blocking`
    /// forms — a round trip to the actor from the actor would deadlock. The
    /// callback spawns the refresh onto the runtime instead and awaits these.
    pub async fn is_enabled(&self, addr: i32) -> AsynResult<bool> {
        let result = self
            .submit_async(RequestOp::GetEnable, AsynUser::new(0).with_addr(addr))
            .await?;
        Ok(result.int_val.unwrap_or(0) != 0)
    }

    /// Async twin of [`Self::is_auto_connect_blocking`].
    pub async fn is_auto_connect(&self, addr: i32) -> AsynResult<bool> {
        let result = self
            .submit_async(RequestOp::GetAutoConnect, AsynUser::new(0).with_addr(addr))
            .await?;
        Ok(result.int_val.unwrap_or(0) != 0)
    }

    /// Async twin of [`Self::is_connected_blocking`].
    pub async fn is_connected(&self, addr: i32) -> AsynResult<bool> {
        let result = self
            .submit_async(RequestOp::GetConnected, AsynUser::new(0).with_addr(addr))
            .await?;
        Ok(result.int_val.unwrap_or(0) != 0)
    }

    /// Connect the port's transport (blocking) — C `pasynCommon->connect`
    /// reached through a PORT-level user (`connectDevice(port, -1)`), so the
    /// Connect-queue waiver applies (asynManager.c:1538-1540) and the request
    /// runs on a disconnected port — this is the route that brings a down
    /// line up. A device-addressed connect (asynRecord CNCT at the record's
    /// ADDR) goes through [`Self::submit_blocking`] with the caller's own
    /// user, which a disconnected multi-device port refuses, as C does.
    pub fn connect_blocking(&self) -> AsynResult<()> {
        self.submit_blocking(RequestOp::Connect, AsynUser::new(0).with_addr(-1))?;
        Ok(())
    }

    /// Disconnect the port's transport (blocking) — C `pasynCommon->disconnect`
    /// through a PORT-level user; see [`Self::connect_blocking`].
    pub fn disconnect_blocking(&self) -> AsynResult<()> {
        self.submit_blocking(RequestOp::Disconnect, AsynUser::new(0).with_addr(-1))?;
        Ok(())
    }

    /// Install the echo interpose on the device `addr` names (blocking) — C
    /// `asynInterposeEcho(portName, addr)`, whose `addr` reaches
    /// `interposeInterface` unchanged (asynInterposeEcho.c:176). The user carries
    /// it, as it does in C: `interposeInterface` resolves the device from it.
    pub fn push_echo_interpose_blocking(&self, addr: i32) -> AsynResult<()> {
        self.submit_blocking(
            RequestOp::PushEchoInterpose,
            AsynUser::new(0).with_addr(addr),
        )?;
        Ok(())
    }

    /// Install the delay interpose on the device `addr` names (blocking) — C
    /// `asynInterposeDelay(portName, addr, delay)` (asynInterposeDelay.c:187,200).
    pub fn push_delay_interpose_blocking(
        &self,
        addr: i32,
        delay: std::time::Duration,
    ) -> AsynResult<()> {
        self.submit_blocking(
            RequestOp::PushDelayInterpose { delay },
            AsynUser::new(0).with_addr(addr),
        )?;
        Ok(())
    }

    /// Install the EOS interpose on `addr`'s octet stack — C
    /// `asynInterposeEosConfig(portName, addr, processEosIn, processEosOut)`.
    pub fn push_eos_interpose_blocking(
        &self,
        addr: i32,
        process_in: bool,
        process_out: bool,
    ) -> AsynResult<()> {
        self.submit_blocking(
            RequestOp::PushEosInterpose {
                process_in,
                process_out,
            },
            AsynUser::new(0).with_addr(addr),
        )?;
        Ok(())
    }

    /// Install the flush-timeout interpose on `addr`'s octet stack — C
    /// `asynInterposeFlushConfig(portName, addr, timeout)`.
    pub fn push_flush_interpose_blocking(
        &self,
        addr: i32,
        flush_timeout: std::time::Duration,
    ) -> AsynResult<()> {
        self.submit_blocking(
            RequestOp::PushFlushInterpose { flush_timeout },
            AsynUser::new(0).with_addr(addr),
        )?;
        Ok(())
    }

    /// Set the port's time-stamp source to the named one, or clear it with
    /// `None` — C `asynRegisterTimeStampSource` /
    /// `asynUnregisterTimeStampSource`. The name must have been published with
    /// [`crate::timestamp::register_time_stamp_source`], C's
    /// `registryFunctionAdd`.
    pub fn set_time_stamp_source_blocking(&self, name: Option<&str>) -> AsynResult<()> {
        self.submit_blocking(
            RequestOp::SetTimeStampSource {
                name: name.map(str::to_string),
            },
            AsynUser::default(),
        )?;
        Ok(())
    }

    // --- asynGpib convenience methods ---
    //
    // C's `pasynGpib` is a global vtable a gpib-aware user calls after
    // `findInterface(asynGpibType)` (asynGpibDriver.h:45-62); each method is
    // passed straight to the driver (asynGpib.c:472-496). Here the handle *is*
    // that vtable — ask [`Self::has_interface`] first (the `findInterface`), then
    // call. Each takes the caller's `AsynUser` because in C the caller's
    // `pasynUser` is what the command runs under: its `addr` selects the GPIB
    // device (`vxiAddressedCmd` reads it with `getAddr`, drvVxi11.c:1372) and its
    // `timeout` bounds the bus traffic.

    /// C `pasynGpib->universalCmd` — send one universal command byte
    /// (asynGpib.c:480-484). The byte comes from
    /// [`crate::interfaces::gpib::universal_cmd_byte`].
    pub fn gpib_universal_cmd_blocking(&self, user: AsynUser, cmd: u8) -> AsynResult<()> {
        self.submit_blocking(RequestOp::GpibUniversalCmd { cmd }, user)?;
        Ok(())
    }

    /// C `pasynGpib->addressedCmd` — send an addressed-command frame
    /// (asynGpib.c:472-478). The frame comes from
    /// [`crate::interfaces::gpib::addressed_request`].
    pub fn gpib_addressed_cmd_blocking(&self, user: AsynUser, data: Vec<u8>) -> AsynResult<()> {
        self.submit_blocking(RequestOp::GpibAddressedCmd { data }, user)?;
        Ok(())
    }

    /// C `pasynGpib->ifc` — assert Interface Clear (asynGpib.c:486-490).
    pub fn gpib_ifc_blocking(&self, user: AsynUser) -> AsynResult<()> {
        self.submit_blocking(RequestOp::GpibIfc, user)?;
        Ok(())
    }

    /// C `pasynGpib->ren` — set the Remote Enable line (asynGpib.c:492-496).
    pub fn gpib_ren_blocking(&self, user: AsynUser, enable: bool) -> AsynResult<()> {
        self.submit_blocking(RequestOp::GpibRen { enable }, user)?;
        Ok(())
    }
}

type StringParamSlot = Arc<Mutex<Option<AsynResult<Vec<u8>>>>>;

fn string_param_call(reason: usize, addr: i32, max_chars: usize) -> (DriverCall, StringParamSlot) {
    let result: StringParamSlot = Arc::new(Mutex::new(None));
    let slot = result.clone();
    let call = DriverCall::new(move |driver| {
        let value = driver.base().get_string_param(reason, addr).map(|bytes| {
            let text = &bytes[..bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len())];
            text[..text.len().min(max_chars.saturating_sub(1))].to_vec()
        });
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
    });
    (call, result)
}

fn string_param_result(result: &StringParamSlot) -> AsynResult<Vec<u8>> {
    result
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_else(|| {
            Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "getStringParam did not run".into(),
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::ParamType;
    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::port_actor::PortActor;

    struct TestDriver {
        base: PortDriverBase,
    }

    impl TestDriver {
        fn new() -> Self {
            let mut base = PortDriverBase::new("handle_test", 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.create_param("F64", ParamType::Float64).unwrap();
            base.create_param("MSG", ParamType::Octet).unwrap();
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

    fn make_handle(driver: impl PortDriver) -> PortHandle {
        let interrupts = Arc::new(InterruptManager::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_link = actor.link();
        std::thread::Builder::new()
            .name("test-handle-actor".into())
            .spawn(actor.registered())
            .unwrap();
        PortHandle::new(
            tx,
            "handle_test".into(),
            interrupts,
            actor_link,
            Arc::new(TraceManager::new()),
        )
    }

    /// C `queueRequest` refuses a disconnected port before queueing
    /// (asynManager.c:1549-1552), and a queued cell's body runs on the port.
    #[test]
    fn queue_request_gates_then_runs_the_cell() {
        use crate::request::{CellBody, Unrun};
        use std::sync::mpsc as std_mpsc;

        struct Ran(std_mpsc::SyncSender<()>);
        impl CellBody for Ran {
            fn process(&self, port: &mut dyn PortCalls) {
                let _ = port.call(&RequestOp::Int32Read, AsynUser::new(0));
                self.0.send(()).unwrap();
            }
            fn unrun(&self, why: Unrun) {
                panic!("the queued request did not run: {why:?}");
            }
            fn done(self: Arc<Self>) {}
        }

        // Down from the start (asynManager.c:523), and nothing connects it.
        let mut down = TestDriver::new();
        down.base.init_auto_connect(false);
        let handle = make_handle(down);
        let (tx, _rx) = std_mpsc::sync_channel(1);
        let cell = handle.new_cell(Arc::new(Ran(tx)));
        let e = handle.queue_request(&cell, &AsynUser::new(0)).unwrap_err();
        assert_eq!(e.status(), AsynStatus::Disconnected);
        assert_eq!(e.message(), "port handle_test not connected");

        let handle = make_handle(TestDriver::new());
        let (tx, rx) = std_mpsc::sync_channel(1);
        let cell = handle.new_cell(Arc::new(Ran(tx)));
        handle.queue_request(&cell, &AsynUser::new(0)).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    /// C `queueRequest` runs `processUser` inline on a port without
    /// `ASYN_CANBLOCK` (asynManager.c:1555-1584) and queues it on one with.
    #[test]
    fn queue_request_runs_the_cell_inline_only_on_a_port_that_cannot_block() {
        use crate::request::{CellBody, Unrun};
        use std::sync::mpsc as std_mpsc;

        struct Where(std_mpsc::SyncSender<Option<String>>);
        impl CellBody for Where {
            fn process(&self, port: &mut dyn PortCalls) {
                let _ = port.call(&RequestOp::Int32Read, AsynUser::new(0));
                let name = std::thread::current().name().map(str::to_owned);
                self.0.send(name).unwrap();
            }
            fn unrun(&self, why: Unrun) {
                panic!("the request did not run: {why:?}");
            }
            fn done(self: Arc<Self>) {}
        }

        let mut handle = make_handle(TestDriver::new());
        let (tx, rx) = std_mpsc::sync_channel(2);
        let cell = handle.new_cell(Arc::new(Where(tx)));
        let ran = handle.queue_request(&cell, &AsynUser::new(0)).unwrap();
        assert_eq!(ran, QueueOutcome::Ran);
        assert_eq!(
            rx.try_recv().unwrap().as_deref(),
            Some("test-handle-actor"),
            "ran before queue_request returned"
        );
        #[cfg(feature = "epics")]
        assert!(!cell.is_outstanding());

        handle.set_can_block(true);
        let queued = handle.queue_request(&cell, &AsynUser::new(0)).unwrap();
        assert_eq!(queued, QueueOutcome::Queued);
        let on = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(on.as_deref(), Some("test-handle-actor"));
    }

    /// C runs a synchronous port's `processUser` under `synchronousLock`
    /// (asynManager.c:1579-1581): a request made while the body is between
    /// its calls runs after the body, and a refusal of the synchronous
    /// branch is `queue_request`'s answer, with the body never run.
    #[test]
    fn a_synchronous_cell_body_holds_the_port_and_a_refusal_is_returned() {
        use crate::request::{CellBody, Unrun};
        use std::sync::mpsc as std_mpsc;

        struct Log {
            base: PortDriverBase,
            log: Arc<parking_lot::Mutex<Vec<usize>>>,
        }
        impl PortDriver for Log {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
                self.log.lock().push(user.reason);
                Ok(0)
            }
        }
        struct TwoCalls(std_mpsc::SyncSender<()>);
        impl CellBody for TwoCalls {
            fn process(&self, port: &mut dyn PortCalls) {
                port.call(&RequestOp::Int32Read, AsynUser::new(1)).unwrap();
                self.0.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(100));
                port.call(&RequestOp::Int32Read, AsynUser::new(2)).unwrap();
            }
            fn unrun(&self, why: Unrun) {
                panic!("a synchronous port runs or refuses, never unruns: {why:?}");
            }
            fn done(self: Arc<Self>) {}
        }

        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut base = PortDriverBase::new(
            "sync_hold",
            2,
            PortFlags {
                multi_device: true,
                ..PortFlags::default()
            },
        );
        base.set_addr_enabled(1, false).unwrap();
        let handle = make_handle(Log {
            base,
            log: log.clone(),
        });
        let (tx, between) = std_mpsc::sync_channel(1);
        let cell = handle.new_cell(Arc::new(TwoCalls(tx)));

        let other = handle.clone();
        let jam = std::thread::spawn(move || {
            between.recv().unwrap();
            other
                .submit_blocking(RequestOp::Int32Read, AsynUser::new(3))
                .unwrap();
        });
        assert_eq!(
            handle.queue_request(&cell, &AsynUser::new(0)).unwrap(),
            QueueOutcome::Ran
        );
        jam.join().unwrap();
        assert_eq!(*log.lock(), [1, 2, 3]);

        let e = handle
            .queue_request(&cell, &AsynUser::new(0).with_addr(1))
            .unwrap_err();
        assert_eq!(e.status(), AsynStatus::Disabled);
        assert_eq!(e.message(), "port sync_hold  or device 1 not enabled");
        assert_eq!(log.lock().len(), 3, "the refused body never ran");
    }

    /// asynManager.c:886: a request on a disconnected device goes to its
    /// `timeoutUser` only when it has one; without one, `processUser` runs.
    #[test]
    fn a_disconnected_device_runs_a_cell_without_a_timeout_user() {
        use crate::request::{CellBody, Unrun};
        use std::sync::mpsc as std_mpsc;

        struct Body {
            tx: std_mpsc::SyncSender<&'static str>,
            timeout_user: bool,
        }
        impl CellBody for Body {
            fn process(&self, _: &mut dyn PortCalls) {
                self.tx.send("process").unwrap();
            }
            fn unrun(&self, _: Unrun) {
                self.tx.send("unrun").unwrap();
            }
            fn done(self: Arc<Self>) {}
            fn timeout_user(&self) -> bool {
                self.timeout_user
            }
        }

        for (timeout_user, ran) in [(false, "process"), (true, "unrun")] {
            let mut base = PortDriverBase::new(
                "dev_down",
                2,
                PortFlags {
                    multi_device: true,
                    can_block: true,
                    ..PortFlags::default()
                },
            );
            base.set_auto_connect_addr(1, false);
            base.set_addr_connected(1, false);
            let mut handle = make_handle(TestDriver { base });
            handle.set_can_block(true);
            let (tx, rx) = std_mpsc::sync_channel(1);
            let cell = handle.new_cell(Arc::new(Body { tx, timeout_user }));
            handle
                .queue_request(&cell, &AsynUser::new(0).with_addr(1))
                .unwrap();
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), ran);
        }
    }

    /// C `queueRequest`'s synchronous branch (asynManager.c:1570-1577) refuses
    /// a down device, after its auto-connect, only below Connect priority and
    /// only without `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED`.
    #[test]
    fn a_synchronous_port_refuses_a_down_device_only_below_connect_priority() {
        let port = || {
            let mut base = PortDriverBase::new(
                "sync_dev",
                2,
                PortFlags {
                    multi_device: true,
                    ..PortFlags::default()
                },
            );
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.set_auto_connect_addr(1, false);
            base.set_addr_connected(1, false);
            make_handle(TestDriver { base })
        };
        let device = |reason| AsynUser::new(reason).with_addr(1);

        let handle = port();
        let e = handle
            .submit_blocking(RequestOp::Int32Read, device(0))
            .unwrap_err();
        assert_eq!(e.status(), AsynStatus::Disconnected);
        assert_eq!(e.message(), "port sync_dev or device 1 not connected");

        handle
            .submit_blocking(RequestOp::ConnectAddr, device(0))
            .expect("a Connect-priority request runs on a down device");
        handle
            .submit_blocking(RequestOp::NoIo, device(0))
            .expect("the device it connected passes the gate");

        let handle = port();
        handle
            .submit_blocking(
                RequestOp::NoIo,
                device(crate::user::ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED),
            )
            .expect("the reason waives the device refusal below Connect priority");
    }

    /// A synchronous port has no queue to park a request on: one whose port
    /// went down behind the request ahead of it is refused, not left waiting
    /// with its caller blocked.
    #[test]
    fn a_synchronous_port_refuses_rather_than_parks() {
        let (handle, _reads) = slow_port(Duration::from_millis(200));
        let jammer = handle.clone();
        let jam = std::thread::spawn(move || {
            jammer
                .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
                .unwrap()
        });
        std::thread::sleep(Duration::from_millis(50));
        let _disable = handle
            .try_submit(RequestOp::SetEnable { yes: false }, AsynUser::default())
            .unwrap();
        let read = handle
            .try_submit(RequestOp::Int32Read, AsynUser::new(0))
            .unwrap();
        let e = read.wait_blocking(Duration::from_secs(5)).unwrap_err();
        assert_eq!(e.status(), AsynStatus::Disabled, "{e}");
        jam.join().unwrap();
    }

    #[test]
    fn a_weak_handle_fails_to_upgrade_after_the_last_handle_drops() {
        let handle = make_handle(TestDriver::new());
        let weak = handle.downgrade();
        let again = weak.upgrade().expect("a live handle upgrades");
        drop(handle);
        assert!(weak.upgrade().is_some(), "one handle is still alive");
        drop(again);
        assert!(weak.upgrade().is_none());
    }

    /// C `connectDevice` runs on the caller's thread under `asynManagerLock`
    /// (asynManager.c:1324-1356): it binds on a port whose actor is not running,
    /// and the device it created is the one the actor reports.
    #[test]
    fn connect_device_binds_without_the_actor() {
        use std::sync::mpsc as std_mpsc;

        let mut driver = TestDriver::new();
        driver.base.flags.multi_device = true;
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let handle = PortHandle::new(
            tx,
            "cd_noactor".into(),
            Arc::new(InterruptManager::new()),
            actor.link(),
            Arc::new(TraceManager::new()),
        );
        let (done_tx, done) = std_mpsc::channel();
        std::thread::spawn(move || {
            let dp = handle.connect_device(3);
            done_tx.send((handle, dp.enabled())).unwrap();
        });
        let (handle, enabled) = done
            .recv_timeout(Duration::from_secs(5))
            .expect("connectDevice waited on the actor");
        assert!(enabled, "a new device is seeded from the port's dpCommon");

        std::thread::spawn(actor.registered());
        let devices = handle
            .with_driver_blocking(|d: &mut TestDriver| {
                d.base
                    .devices()
                    .into_iter()
                    .map(|(addr, _)| addr)
                    .collect::<Vec<_>>()
            })
            .unwrap();
        assert_eq!(devices, vec![3]);
    }

    /// C `enable` and `autoConnectAsyn` store on the caller's thread
    /// (asynManager.c:2224-2250, :2312-2329) and asynRecord calls them under the
    /// record's lock: both land on a port whose actor is not running, on the
    /// device's dpCommon, and each announces before it returns.
    #[test]
    fn enable_and_auto_connect_store_without_the_actor() {
        use crate::exception::{AsynException, ExceptionManager};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc as std_mpsc;

        let mut driver = TestDriver::new();
        driver.base.flags.multi_device = true;
        let exc_mgr = Arc::new(ExceptionManager::new());
        driver.base.bind_exception_sink(exc_mgr.clone());
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        exc_mgr.add_callback(move |event| {
            if event.addr == 2
                && matches!(
                    event.exception,
                    AsynException::Enable | AsynException::AutoConnect
                )
            {
                hits2.fetch_add(1, Ordering::Relaxed);
            }
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let handle = PortHandle::new(
            tx,
            "enbl_noactor".into(),
            Arc::new(InterruptManager::new()),
            actor.link(),
            Arc::new(TraceManager::new()),
        );
        let (done_tx, done) = std_mpsc::channel();
        std::thread::spawn(move || {
            handle.set_enable(2, false).unwrap();
            handle.set_auto_connect(2, false).unwrap();
            done_tx.send(handle).unwrap();
        });
        let handle = done
            .recv_timeout(Duration::from_secs(5))
            .expect("enable/autoConnect waited on the actor");
        let dp = handle.connect_device(2);
        assert!(!dp.enabled() && !dp.auto_connect());
        assert!(
            handle.connect_device(-1).enabled(),
            "the port's own is untouched"
        );
        assert_eq!(hits.load(Ordering::Relaxed), 2, "announced by the caller");
        drop(actor);
    }

    /// C `queueRequest` appends to an unbounded list (asynManager.c:1610-1617):
    /// however many requests arrive while the port thread is inside the driver,
    /// none is refused, and each runs once it comes out.
    #[test]
    fn a_port_busy_in_its_driver_refuses_no_request() {
        use std::sync::mpsc as std_mpsc;

        struct Busy {
            base: PortDriverBase,
            entered: std_mpsc::SyncSender<()>,
            release: std::sync::Mutex<std_mpsc::Receiver<()>>,
        }
        impl PortDriver for Busy {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
                if user.reason == 1 {
                    self.entered.send(()).unwrap();
                    self.release.lock().unwrap().recv().unwrap();
                }
                Ok(7)
            }
        }

        let (entered_tx, entered) = std_mpsc::sync_channel(1);
        let (release, release_rx) = std_mpsc::channel();
        let (runtime, _jh) = crate::runtime::create_port_runtime_unclaimed(
            Busy {
                base: PortDriverBase::new(
                    "busy",
                    1,
                    PortFlags {
                        can_block: true,
                        ..PortFlags::default()
                    },
                ),
                entered: entered_tx,
                release: std::sync::Mutex::new(release_rx),
            },
            crate::runtime::RuntimeConfig::default(),
        )
        .unwrap();
        let handle = runtime.port_handle();
        let jam = handle
            .try_submit(RequestOp::Int32Read, AsynUser::new(1))
            .unwrap();
        entered.recv().unwrap();
        let queued: Vec<_> = (0..1500)
            .map(|i| {
                handle
                    .try_submit(RequestOp::Int32Read, AsynUser::new(0))
                    .unwrap_or_else(|e| panic!("request {i} refused: {e}"))
            })
            .collect();
        release.send(()).unwrap();
        jam.wait_blocking(Duration::from_secs(5)).unwrap();
        for done in queued {
            assert_eq!(
                done.wait_blocking(Duration::from_secs(5)).unwrap().int_val,
                Some(7)
            );
        }
    }

    /// A waiter's request queued just before its port went down stays queued,
    /// as C `portThread` leaves it (asynManager.c:808-810, :858-863): it times
    /// out on its own queue timer, or runs once the port is back. Only one
    /// submitted while the port is down is refused, by `queueRequest`
    /// (:1549-1552).
    #[test]
    fn a_request_queued_before_its_port_went_down_stays_queued() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc as std_mpsc;

        struct Drops {
            base: PortDriverBase,
            entered: std_mpsc::SyncSender<()>,
            release: std::sync::Mutex<std_mpsc::Receiver<()>>,
            line_up: Arc<AtomicBool>,
        }
        impl PortDriver for Drops {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
                if user.reason == 1 {
                    self.entered.send(()).unwrap();
                    self.release.lock().unwrap().recv().unwrap();
                    // The line drops while the port thread is in the driver.
                    self.base.set_connected(false);
                }
                Ok(7)
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                if !self.line_up.load(Ordering::SeqCst) {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "line down".into(),
                    });
                }
                self.base.set_connected(true);
                Ok(())
            }
        }

        let (entered_tx, entered) = std_mpsc::sync_channel(1);
        let (release, release_rx) = std_mpsc::channel();
        let line_up = Arc::new(AtomicBool::new(false));
        // `portThread`'s parking: a CANBLOCK port.
        let mut base = PortDriverBase::new(
            "park_waiter",
            1,
            PortFlags {
                can_block: true,
                ..PortFlags::default()
            },
        );
        // The port comes back on its own retry timer (asynManager.c:3283-3314).
        base.seconds_between_port_connect = Duration::from_millis(20);
        // The handle queues into `queued`; the test hands each request on to
        // the actor, so it knows when one is queued.
        let (tx, mut queued) = mpsc::unbounded_channel();
        let (relay, rx) = mpsc::unbounded_channel();
        let mut hand_on = || relay.send(queued.blocking_recv().unwrap()).unwrap();
        let mut actor = PortActor::new(
            Box::new(Drops {
                base,
                entered: entered_tx,
                release: std::sync::Mutex::new(release_rx),
                line_up: line_up.clone(),
            }),
            rx,
        );
        // The line is up at registration and stays down once it drops.
        line_up.store(true, Ordering::SeqCst);
        actor.register();
        line_up.store(false, Ordering::SeqCst);
        let link = actor.link();
        // The production loop: the test loop runs no connect-retry timer.
        let (_shutdown, shutdown_rx) = mpsc::channel(1);
        std::thread::spawn(move || actor.run_with_shutdown(shutdown_rx));
        let mut handle = PortHandle::new(
            tx,
            "park_waiter".into(),
            Arc::new(InterruptManager::new()),
            link,
            Arc::new(TraceManager::new()),
        );
        handle.set_can_block(true);

        let submit = |user: AsynUser| {
            let handle = handle.clone();
            std::thread::spawn(move || handle.submit_blocking(RequestOp::Int32Read, user))
        };
        let jam = submit(AsynUser::new(1));
        hand_on();
        entered.recv().unwrap();
        let timed = submit(AsynUser::new(0).with_queue_timeout(Duration::from_millis(100)));
        let waits = submit(AsynUser::new(0));
        // Both are queued — the actor, inside the driver, has not taken them —
        // before the line drops.
        hand_on();
        hand_on();
        release.send(()).unwrap();
        jam.join().unwrap().unwrap();

        let err = timed.join().unwrap().unwrap_err();
        assert!(
            matches!(err, AsynError::QueueTimeout { .. }),
            "queued until its timer, not refused: {err:?}"
        );
        assert!(!waits.is_finished(), "still queued");

        let down = AsynUser::new(0).with_queue_timeout(Duration::from_secs(1));
        let err = handle
            .submit_blocking(RequestOp::Int32Read, down)
            .unwrap_err();
        assert!(err.is_queue_refused(), "queueRequest refuses: {err:?}");
        assert_eq!(err.status(), AsynStatus::Disconnected);

        line_up.store(true, Ordering::SeqCst);
        let r = waits.join().unwrap().expect("runs once the port is back");
        assert_eq!(r.int_val, Some(7));
    }

    /// C `updateTimeStamp` from a task: the source runs on the actor, its
    /// answer is the port's stamp, and the next read carries it.
    #[test]
    fn update_time_stamp_refreshes_the_port_stamp() {
        let fixed = SystemTime::UNIX_EPOCH + Duration::from_secs(4_242_424);
        let mut drv = TestDriver::new();
        drv.base.register_timestamp_source(move || fixed);
        drv.base.set_int32_param(0, 0, 1).unwrap();
        let handle = make_handle(drv);

        let stamp = epics_libcom_rs::runtime::task::block_on_sync(handle.update_time_stamp())
            .unwrap()
            .unwrap();
        assert_eq!(stamp, fixed);
        let read = handle
            .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
            .unwrap();
        assert_eq!(read.timestamp, Some(fixed));
    }

    /// `with_driver` hands the closure the driver's concrete type, runs it on
    /// the actor (a param set there is visible to the next queued read), and
    /// returns the closure's value.
    #[test]
    fn with_driver_runs_the_closure_on_the_concrete_driver() {
        let handle = make_handle(TestDriver::new());
        let name = handle
            .with_driver_blocking(|d: &mut TestDriver| {
                d.base.set_int32_param(0, 0, 41).unwrap();
                d.base.port_name.clone()
            })
            .unwrap();
        assert_eq!(&*name, "handle_test");
        assert_eq!(handle.read_int32_blocking(0, 0).unwrap(), 41);
    }

    /// Boundary: the port's driver is some other type — an error naming the
    /// type, and the closure never runs.
    #[test]
    fn with_driver_refuses_a_driver_of_another_type() {
        struct Other(PortDriverBase);
        impl PortDriver for Other {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }
        let handle = make_handle(TestDriver::new());
        let err = handle
            .with_driver_blocking(|_: &mut Other| unreachable!())
            .unwrap_err();
        assert!(err.to_string().contains("Other"), "{err}");
    }

    /// A client write issued between two steps of a held span lands after
    /// the release, as one queued behind a C driver thread's `lock()` does.
    #[test]
    fn a_write_issued_during_a_held_span_lands_after_it() {
        let handle = make_handle(TestDriver::new());
        let mut hold = handle.hold_driver_blocking::<TestDriver>().unwrap();
        hold.run(|d| d.base.set_int32_param(0, 0, 5).unwrap())
            .unwrap();
        let writer = {
            let handle = handle.clone();
            std::thread::spawn(move || handle.write_int32_blocking(0, 0, 7))
        };
        std::thread::sleep(Duration::from_millis(100));
        let seen = hold.run(|d| d.base.get_int32_param(0, 0).unwrap()).unwrap();
        assert_eq!(seen, 5, "the write reached the driver inside the span");
        assert!(!writer.is_finished());
        drop(hold);
        writer.join().unwrap().unwrap();
        assert_eq!(handle.read_int32_blocking(0, 0).unwrap(), 7);
    }

    /// `unlocked` is C's `unlock()` … `lock()`: a write queued in the window
    /// lands before the span resumes.
    #[test]
    fn a_write_lands_while_the_hold_is_unlocked() {
        let handle = make_handle(TestDriver::new());
        let mut hold = handle.hold_driver_blocking::<TestDriver>().unwrap();
        let writer = {
            let handle = handle.clone();
            std::thread::spawn(move || handle.write_int32_blocking(0, 0, 7))
        };
        hold.unlocked(|| writer.join().unwrap().unwrap());
        let seen = hold.run(|d| d.base.get_int32_param(0, 0).unwrap()).unwrap();
        assert_eq!(seen, 7);
    }

    /// Boundary: the holder's own blocking request would wait for its own
    /// release — refused, not deadlocked; after the release it runs.
    #[test]
    fn a_holders_own_blocking_request_is_refused() {
        let handle = make_handle(TestDriver::new());
        let hold = handle.hold_driver_blocking::<TestDriver>().unwrap();
        let err = handle.read_int32_blocking(0, 0).unwrap_err();
        assert!(err.to_string().contains("holding its driver"), "{err}");
        drop(hold);
        handle.write_int32_blocking(0, 0, 3).unwrap();
    }

    /// Boundary: a hold of another driver type errors and leaves the port
    /// free.
    #[test]
    fn a_hold_of_another_driver_type_is_refused() {
        struct Other(PortDriverBase);
        impl PortDriver for Other {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }
        let handle = make_handle(TestDriver::new());
        let err = handle.hold_driver_blocking::<Other>().err().unwrap();
        assert!(err.to_string().contains("Other"), "{err}");
        handle.write_int32_blocking(0, 0, 3).unwrap();
    }

    /// Boundary: a call with no closure — the second clone of a request, or one
    /// rebuilt from a `PortCommand` — is refused rather than reported done.
    #[test]
    fn a_spent_driver_call_is_refused() {
        let handle = make_handle(TestDriver::new());
        let op = RequestOp::WithDriver(DriverCall::new(|_| {}));
        handle
            .submit_blocking(op.clone(), AsynUser::default())
            .unwrap();
        assert!(handle.submit_blocking(op, AsynUser::default()).is_err());
    }

    /// C `lockPort` boundaries: a down link answers a locked read with the
    /// driver's own error and no connect attempt, a live one serves several
    /// calls in one hold, and a shut-down port refuses (asynManager.c:1778-1808).
    #[test]
    fn lock_port_reads_past_the_connect_gate() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Line {
            base: PortDriverBase,
            connects: Arc<AtomicUsize>,
        }
        impl PortDriver for Line {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
                self.connects.fetch_add(1, Ordering::SeqCst);
                Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "line down".into(),
                })
            }
            fn io_read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
                if !self.base.is_connected() {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "disconnected:".into(),
                    });
                }
                buf[0] = b'x';
                Ok(1)
            }
        }

        let read = |calls: &mut dyn PortCalls| {
            calls.call(&RequestOp::OctetRead { buf_size: 4 }, AsynUser::new(0))
        };
        let connects = Arc::new(AtomicUsize::new(0));
        let flags = PortFlags {
            destructible: true,
            ..PortFlags::default()
        };
        let base = PortDriverBase::new("lock_port", 1, flags);
        let dir = tempfile::tempdir().expect("fixture root");
        let trace_path = dir.path().join("lock_port_trace.txt");
        base.trace
            .set_trace_mask(Some("lock_port"), crate::trace::TraceMask::FLOW);
        base.trace.set_trace_file(
            Some("lock_port"),
            crate::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(
                std::fs::File::create(&trace_path).unwrap(),
            ))),
        );
        let handle = make_handle(Line {
            base,
            connects: connects.clone(),
        });
        // Registration's attempt and its pass's own (asynManager.c:2135,
        // :858-863).
        let registered = connects.load(Ordering::SeqCst);
        assert_eq!(registered, 2);

        // Down: the driver's error, not the gate's asynDisconnected.
        match handle.lock_port_blocking(read).unwrap() {
            Err(AsynError::Status { status, message }) => {
                assert_eq!(status, AsynStatus::Error);
                assert_eq!(message, "disconnected:");
            }
            other => panic!("expected the driver's error, got {other:?}"),
        }
        assert_eq!(
            connects.load(Ordering::SeqCst),
            registered,
            "lockPort never autoconnects"
        );

        // Up: two reads in one hold.
        handle
            .with_driver_blocking(|d: &mut Line| d.base.set_connected(true))
            .unwrap();
        let (a, b) = handle
            .lock_port_blocking(move |calls| (read(calls), read(calls)))
            .unwrap();
        assert_eq!(a.unwrap().data.as_deref(), Some(&b"x"[..]));
        assert_eq!(b.unwrap().data.as_deref(), Some(&b"x"[..]));
        // asynManager.c:1789/:1817, one pair per hold.
        let trace = std::fs::read_to_string(&trace_path).unwrap();
        assert_eq!(trace.matches("lock_port lockPort\n").count(), 2);
        assert_eq!(trace.matches("lock_port unlockPort\n").count(), 2);

        // Shut down: C's asynDisabled.
        handle
            .submit_blocking(RequestOp::ShutdownPort, AsynUser::default())
            .unwrap();
        match handle.lock_port_blocking(|_| ()) {
            Err(AsynError::Status { status, message }) => {
                assert_eq!(status, AsynStatus::Disabled);
                assert_eq!(message, "asynManager:lockPort: port has been shut down");
            }
            other => panic!("expected the shutdown refusal, got {other:?}"),
        }
    }

    #[test]
    fn handle_blocking_int32() {
        let handle = make_handle(TestDriver::new());
        handle.write_int32_blocking(0, 0, 42).unwrap();
        assert_eq!(handle.read_int32_blocking(0, 0).unwrap(), 42);
    }

    /// The blocking API must work from inside a **current-thread** runtime —
    /// the flavour the framework builds for the port actor itself
    /// (`PortActor::run_with_shutdown`) and for every `ad-core` driver thread
    /// (`ad_core_rs::runtime::run_thread_named`). `#[tokio::test]` is
    /// current-thread by default, so this test *is* that context.
    ///
    /// The old implementation reached for `tokio::task::block_in_place`, which
    /// panics here: "can call blocking only when running on the multi-threaded
    /// runtime".
    #[tokio::test]
    async fn handle_blocking_works_inside_current_thread_runtime() {
        let handle = make_handle(TestDriver::new());
        handle.write_int32_blocking(0, 0, 21).unwrap();
        assert_eq!(handle.read_int32_blocking(0, 0).unwrap(), 21);
        handle.call_param_callbacks_blocking(0).unwrap();
        // Enqueue-only path: previously refused outright inside any runtime.
        handle.set_params_and_notify_blocking(0, vec![]).unwrap();
    }

    /// A driver task makes many blocking requests without ever yielding to its
    /// runtime. Past tokio's per-task coop budget (128) the reply wait parked
    /// forever: the exhausted budget hands the waker to the scheduler, which
    /// never runs again because this task is parked.
    #[test]
    fn blocking_requests_outlast_the_tasks_coop_budget() {
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            rt.block_on(async {
                let handle = make_handle(TestDriver::new());
                for i in 0..300 {
                    handle.write_int32_blocking(0, 0, i).unwrap();
                    let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
                    let read = handle
                        .try_submit(RequestOp::Int32Read, user)
                        .unwrap()
                        .wait_blocking(Duration::from_secs(1))
                        .unwrap();
                    assert_eq!(read.int_val, Some(i));
                }
            });
            done_tx.send(()).unwrap();
        });
        done.recv_timeout(Duration::from_secs(10))
            .expect("a blocking request past the coop budget never completed");
    }

    /// The multi-threaded path (device support under `#[epics_main]`) keeps
    /// working — `block_in_place` still yields the worker before we park it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_blocking_works_inside_multi_thread_runtime() {
        let handle = make_handle(TestDriver::new());
        handle.write_int32_blocking(0, 0, 84).unwrap();
        assert_eq!(handle.read_int32_blocking(0, 0).unwrap(), 84);
    }

    /// `try_submit` + `wait_blocking` is the same blocking wait by another
    /// route: it too used a tokio shim (`oneshot::blocking_recv`) that panics
    /// inside a runtime.
    #[tokio::test]
    async fn wait_blocking_works_inside_current_thread_runtime() {
        let handle = make_handle(TestDriver::new());
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        handle
            .try_submit(RequestOp::Int32Write { value: 63 }, user)
            .unwrap()
            .wait_blocking(Duration::from_secs(1))
            .unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let result = handle
            .try_submit(RequestOp::Int32Read, user)
            .unwrap()
            .wait_blocking(Duration::from_secs(1))
            .unwrap();
        assert_eq!(result.int_val, Some(63));
    }

    #[test]
    fn handle_blocking_float64() {
        let handle = make_handle(TestDriver::new());
        handle.write_float64_blocking(1, 0, 2.718).unwrap();
        assert!((handle.read_float64_blocking(1, 0).unwrap() - 2.718).abs() < 1e-10);
    }

    #[tokio::test]
    async fn handle_async_int32() {
        let handle = make_handle(TestDriver::new());
        handle.write_int32(0, 0, 100).await.unwrap();
        assert_eq!(handle.read_int32(0, 0).await.unwrap(), 100);
    }

    #[tokio::test]
    async fn handle_async_float64() {
        let handle = make_handle(TestDriver::new());
        handle.write_float64(1, 0, 1.23).await.unwrap();
        assert!((handle.read_float64(1, 0).await.unwrap() - 1.23).abs() < 1e-10);
    }

    #[tokio::test]
    async fn handle_async_octet() {
        let handle = make_handle(TestDriver::new());
        handle.write_octet(2, 0, b"test".to_vec()).await.unwrap();
        let data = handle.read_octet(2, 0, 256).await.unwrap();
        assert_eq!(&data[..4], b"test");
    }

    #[test]
    fn handle_try_submit() {
        let handle = make_handle(TestDriver::new());
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let completion = handle
            .try_submit(RequestOp::Int32Write { value: 55 }, user)
            .unwrap();
        completion.wait_blocking(Duration::from_secs(1)).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let completion = handle.try_submit(RequestOp::Int32Read, user).unwrap();
        let result = completion.wait_blocking(Duration::from_secs(1)).unwrap();
        assert_eq!(result.int_val, Some(55));
    }

    #[test]
    fn handle_clone_works() {
        let handle = make_handle(TestDriver::new());
        let h2 = handle.clone();

        handle.write_int32_blocking(0, 0, 77).unwrap();
        assert_eq!(h2.read_int32_blocking(0, 0).unwrap(), 77);
    }

    #[test]
    fn handle_port_name() {
        let handle = make_handle(TestDriver::new());
        assert_eq!(handle.port_name(), "handle_test");
    }

    /// C parity: `pasynManager::isMultiDevice` reads back the
    /// port-level flag set at registration. The Rust handle must
    /// expose the same so clients can decide whether to issue
    /// addr-scoped requests or always use `addr=0`.
    #[test]
    fn handle_exposes_multi_device_and_max_addr() {
        let mut handle = make_handle(TestDriver::new());
        // Defaults — single-device, max_addr=1.
        assert!(!handle.is_multi_device());
        assert_eq!(handle.max_addr(), 1);

        handle.set_capabilities(true, 8);
        assert!(handle.is_multi_device());
        assert_eq!(handle.max_addr(), 8);

        // Floor guard: a degenerate `set_capabilities(_, 0)` must
        // still report at least 1 (C asyn enforces the same in
        // `paramList` constructor).
        handle.set_capabilities(false, 0);
        assert!(!handle.is_multi_device());
        assert_eq!(handle.max_addr(), 1);
    }

    /// C parity: `asynRecord` ENBL field writes call
    /// `pasynManager->enable(...)`, which in turn flips the
    /// per-port `enabled` flag and emits `asynExceptionEnable`.
    /// C `enable` and `autoConnectAsyn` raise their exception inline, one
    /// call at a time (asynManager.c:2224-2250, :2312-2329), so the callbacks
    /// see the calls' order, whatever dpCommon each was made on.
    #[test]
    fn enable_and_auto_connect_announce_in_call_order() {
        use crate::exception::{AsynException, ExceptionManager};
        let mut driver = TestDriver::new();
        driver.base.flags.multi_device = true;
        let exc_mgr = Arc::new(ExceptionManager::new());
        driver.base.bind_exception_sink(exc_mgr.clone());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        exc_mgr.add_callback(move |event| {
            if matches!(
                event.exception,
                AsynException::Enable | AsynException::AutoConnect
            ) {
                seen2.lock().unwrap().push((event.addr, event.exception));
            }
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = PortActor::new(Box::new(driver), rx);
        let handle = PortHandle::new(
            tx,
            "announce_order".into(),
            Arc::new(InterruptManager::new()),
            actor.link(),
            Arc::new(TraceManager::new()),
        );
        handle.set_auto_connect(2, false).unwrap();
        handle.set_enable(2, false).unwrap();
        handle.set_enable(-1, true).unwrap();
        std::thread::spawn(actor.registered());
        handle
            .with_driver_blocking(|_: &mut TestDriver| ())
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                (2, AsynException::AutoConnect),
                (2, AsynException::Enable),
                (-1, AsynException::Enable)
            ]
        );
    }

    #[test]
    fn handle_set_enable_propagates_to_driver() {
        use crate::exception::{AsynException, ExceptionManager};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut base = PortDriverBase::new("enbl_prop", 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        let exc_mgr = Arc::new(ExceptionManager::new());
        base.bind_exception_sink(exc_mgr.clone());
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        exc_mgr.add_callback(move |event| {
            if event.exception == AsynException::Enable {
                hits2.fetch_add(1, Ordering::Relaxed);
            }
        });

        struct Drv {
            base: PortDriverBase,
        }
        impl PortDriver for Drv {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let handle = make_handle(Drv { base });
        // C `enable` fires on every call, before it returns.
        for yes in [false, true] {
            handle.set_enable(-1, yes).unwrap();
        }
        assert_eq!(hits.load(Ordering::Relaxed), 2);
    }

    /// R10-55. The handle is the `pasynGpib` vtable: each of C's four command
    /// methods (asynGpibDriver.h:47-51) reaches the driver through the actor,
    /// carrying the caller's `AsynUser` (C passes the caller's `pasynUser`
    /// straight through, asynGpib.c:472-496).
    #[test]
    fn gpib_commands_reach_the_driver_through_the_actor() {
        use crate::interfaces::gpib::IBDCL;
        use std::sync::Mutex as StdMutex;

        static CALLS: StdMutex<Vec<String>> = StdMutex::new(Vec::new());

        struct GpibDrv(PortDriverBase);
        impl PortDriver for GpibDrv {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<Capability> {
                vec![
                    Capability::Gpib,
                    Capability::OctetRead,
                    Capability::OctetWrite,
                ]
            }
            fn gpib_universal_cmd(&mut self, user: &mut AsynUser, cmd: u8) -> AsynResult<()> {
                CALLS
                    .lock()
                    .unwrap()
                    .push(format!("universal {cmd:#04x} addr {}", user.addr));
                Ok(())
            }
            fn gpib_addressed_cmd(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
                CALLS.lock().unwrap().push(format!("addressed {data:02x?}"));
                Ok(())
            }
            fn gpib_ifc(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                CALLS.lock().unwrap().push("ifc".into());
                Ok(())
            }
            fn gpib_ren(&mut self, _user: &mut AsynUser, enable: bool) -> AsynResult<()> {
                CALLS.lock().unwrap().push(format!("ren {enable}"));
                Ok(())
            }
        }

        // A GPIB link is multi-device; a single-device port hands -1 (getAddr).
        let mut handle = make_handle(GpibDrv(PortDriverBase::new(
            "gpib_vtable",
            32,
            PortFlags::default(),
        )));
        handle.set_interfaces(vec![Capability::Gpib]);
        assert!(handle.has_interface(InterfaceType::Gpib), "findInterface");

        handle
            .gpib_universal_cmd_blocking(AsynUser::new(0).with_addr(7), IBDCL)
            .unwrap();
        handle
            .gpib_addressed_cmd_blocking(AsynUser::new(0), vec![0x5f, 0x3f, 0x27, 0x08])
            .unwrap();
        handle.gpib_ifc_blocking(AsynUser::new(0)).unwrap();
        handle.gpib_ren_blocking(AsynUser::new(0), true).unwrap();

        assert_eq!(
            CALLS.lock().unwrap().as_slice(),
            [
                "universal 0x14 addr 7",
                "addressed [5f, 3f, 27, 08]",
                "ifc",
                "ren true",
            ]
        );
    }

    /// A port that does not implement the interface refuses every GPIB command:
    /// in C there is no `asynGpib` interface to find at all, so the caller has
    /// nothing to call (asynRecord checks GPIBIV first, asynRecord.c:1647).
    #[test]
    fn a_port_without_the_gpib_interface_refuses_the_commands() {
        let handle = make_handle(TestDriver::new());
        assert!(!handle.has_interface(InterfaceType::Gpib));

        let err = handle
            .gpib_universal_cmd_blocking(AsynUser::new(0), 0x14)
            .unwrap_err();
        assert_eq!(err.message(), "port has no asynGpib interface");
        assert!(handle.gpib_ifc_blocking(AsynUser::new(0)).is_err());
    }

    /// C parity: `asynRecord` AUCT writes call
    /// `pasynManager->autoConnect(...)`, which always emits
    /// `asynExceptionAutoConnect` — even on no-op (same-value)
    /// writes, before it returns.
    #[test]
    fn handle_set_auto_connect_propagates_to_driver() {
        use crate::exception::{AsynException, ExceptionManager};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut base = PortDriverBase::new("auct_prop", 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        let exc_mgr = Arc::new(ExceptionManager::new());
        base.bind_exception_sink(exc_mgr.clone());
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        exc_mgr.add_callback(move |event| {
            if event.exception == AsynException::AutoConnect {
                hits2.fetch_add(1, Ordering::Relaxed);
            }
        });

        struct Drv {
            base: PortDriverBase,
        }
        impl PortDriver for Drv {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let handle = make_handle(Drv { base });
        // Both true→true and true→false fire (C unconditional).
        for yes in [true, false, false] {
            handle.set_auto_connect(-1, yes).unwrap();
        }
        assert_eq!(hits.load(Ordering::Relaxed), 3);
    }

    // ===== R10-49: the queue-wait deadline (C queueRequest's `timeout` arg) =====

    /// A port whose reads take `delay` and are counted, so "the request ran" is
    /// observable.
    struct SlowDriver {
        base: PortDriverBase,
        delay: Duration,
        reads: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl PortDriver for SlowDriver {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
            std::thread::sleep(self.delay);
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(7)
        }
    }

    fn slow_port(delay: Duration) -> (PortHandle, Arc<std::sync::atomic::AtomicUsize>) {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handle = make_handle(SlowDriver {
            base: PortDriverBase::new("handle_test", 1, PortFlags::default()),
            delay,
            reads: reads.clone(),
        });
        (handle, reads)
    }

    /// [`slow_port`] with `ASYN_CANBLOCK`, where a SyncIO call queues.
    fn blocking_slow_port(delay: Duration) -> (PortHandle, Arc<std::sync::atomic::AtomicUsize>) {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let flags = PortFlags {
            can_block: true,
            ..PortFlags::default()
        };
        let mut handle = make_handle(SlowDriver {
            base: PortDriverBase::new("handle_test", 1, flags),
            delay,
            reads: reads.clone(),
        });
        handle.set_can_block(true);
        (handle, reads)
    }

    /// A SyncIO call on a CANBLOCK port is C `queueLockPort`'s queued half
    /// (asynManager.c:1858-1912), which answers its caller in its own words:
    /// the `queueRequest` refusal behind its prefix, the queue deadline as
    /// `asynTimeout`, and a disabled port with the bare refusal it makes itself
    /// before queueing.
    #[test]
    fn a_syncio_call_that_never_ran_reports_as_queue_lock_port() {
        let sync = |handle: &PortHandle| {
            crate::sync_io::SyncIOHandle::from_handle(handle.clone(), 0, Duration::from_millis(10))
                .read_int32(0)
                .expect_err("the call never ran")
        };

        // Not connected, no autoconnect: queueRequest refuses.
        let flags = PortFlags {
            can_block: true,
            ..PortFlags::default()
        };
        let mut base = PortDriverBase::new("qlp", 1, flags);
        base.init_connected(false);
        base.init_auto_connect(false);
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handle = make_handle(SlowDriver {
            base,
            delay: Duration::ZERO,
            reads: reads.clone(),
        });
        handle.set_can_block(true);
        let err = sync(&handle);
        assert_eq!(err.status(), AsynStatus::Disconnected);
        assert_eq!(
            err.message(),
            "asynManager::queueLockPort queueRequest failed: port qlp not connected"
        );
        assert!(err.never_ran());
        // A plain queueRequest keeps the bare text.
        let plain = handle
            .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
            .expect_err("refused");
        assert_eq!(plain.message(), "port qlp not connected");

        // Disabled: queueLockPort's own refusal, unprefixed.
        handle
            .submit_blocking(RequestOp::SetEnable { yes: false }, AsynUser::default())
            .unwrap();
        let err = sync(&handle);
        assert_eq!(err.status(), AsynStatus::Disabled);
        assert_eq!(err.message(), "port qlp disabled");

        // The queue deadline: asynTimeout, never ran.
        let (handle, reads) = blocking_slow_port(Duration::from_millis(300));
        handle.set_queue_lock_port_timeout(Duration::from_millis(10));
        let jammer = handle.clone();
        let jam = std::thread::spawn(move || {
            jammer
                .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
                .unwrap()
        });
        std::thread::sleep(Duration::from_millis(50));
        let err = sync(&handle);
        assert_eq!(err.status(), AsynStatus::Timeout);
        assert_eq!(
            err.message(),
            "asynManager::queueLockPort queueRequest timed out"
        );
        assert!(err.is_queue_timeout());
        jam.join().unwrap();
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// C `queueLockPort` on a synchronous port refuses only a disabled port
    /// (asynManager.c:1851-1856) and then takes `synchronousLock`: a SyncIO
    /// call on a port that is down still reaches the driver.
    #[test]
    fn a_syncio_call_on_a_synchronous_port_skips_the_connect_gate() {
        let mut base = PortDriverBase::new("qlp", 1, PortFlags::default());
        base.init_connected(false);
        base.init_auto_connect(false);
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handle = make_handle(SlowDriver {
            base,
            delay: Duration::ZERO,
            reads: reads.clone(),
        });
        let syncio =
            crate::sync_io::SyncIOHandle::from_handle(handle.clone(), 0, Duration::from_millis(10));
        assert_eq!(syncio.read_int32(0).unwrap(), 7);
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);

        handle
            .submit_blocking(RequestOp::SetEnable { yes: false }, AsynUser::default())
            .unwrap();
        let err = syncio.read_int32(0).expect_err("disabled");
        assert_eq!(err.status(), AsynStatus::Disabled);
        assert_eq!(err.message(), "port qlp disabled");
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// On a synchronous port C `queueLockPort` only takes `synchronousLock`
    /// (asynManager.c:1913-1916): no queue deadline, so a SyncIO call behind
    /// a long request waits it out instead of timing out.
    #[test]
    fn a_syncio_call_on_a_synchronous_port_has_no_queue_deadline() {
        let (handle, _reads) = slow_port(Duration::from_millis(300));
        handle.set_queue_lock_port_timeout(Duration::from_millis(10));
        let syncio =
            crate::sync_io::SyncIOHandle::from_handle(handle.clone(), 0, Duration::from_millis(10));
        assert_eq!(syncio.user(0).queue_timeout(), None);
        let jammer = handle.clone();
        let jam = std::thread::spawn(move || {
            jammer
                .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
                .unwrap()
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(syncio.read_int32(0).unwrap(), 7);
        jam.join().unwrap();
    }

    /// R10-49. C `queueRequest(pasynUser, priority, timeout)` arms a timer
    /// (asynManager.c:1619-1625) whose callback unlinks a still-queued request
    /// (:649-703) and runs the caller's `timeoutUser` **instead of** its
    /// `processUser`. The port had no queue-wait deadline at all: a request
    /// behind a slow one waited as long as it took, whatever `queueRequest` was
    /// given.
    #[test]
    fn a_request_that_waited_past_its_queue_deadline_never_runs() {
        let (handle, reads) = slow_port(Duration::from_millis(300));

        // Jam the port with a request that carries no deadline (C's
        // `queueRequest(..., 0.0)` — device support).
        let jammer = handle.clone();
        let jam = std::thread::spawn(move || {
            jammer
                .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
                .unwrap()
        });
        std::thread::sleep(Duration::from_millis(50));

        // Queued behind it, with a deadline it cannot possibly meet.
        let started = std::time::Instant::now();
        let err = handle
            .submit_blocking(
                RequestOp::Int32Read,
                AsynUser::new(0).with_queue_timeout(Duration::from_millis(10)),
            )
            .expect_err("the queue wait outlived the deadline");
        assert!(err.is_queue_timeout(), "got {err:?}");
        // *When* it learns is the point, and it is the same answer on both
        // backends and on a thread with no runtime at all: the seam's
        // `timeout` arms a timer this thread can reach. It used to be the
        // 300 ms jam here and 30 ms of a 600 ms jam in the async test, which
        // is the actor's dequeue re-check reporting instead of the deadline.
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_millis(150),
            "the caller wakes at its own deadline, not when the jam clears \
             (waited {waited:?})"
        );

        jam.join().unwrap();
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "C removes the request from the queue — the driver never sees it"
        );
    }

    /// Negative control: the same jam, the same wait, no deadline asked for.
    /// C arms no timer at all for `queueRequest(..., 0.0)`, so the request waits
    /// its turn and runs.
    #[test]
    fn a_request_with_no_queue_deadline_waits_its_turn_and_runs() {
        let (handle, reads) = slow_port(Duration::from_millis(300));

        let jammer = handle.clone();
        let jam = std::thread::spawn(move || {
            jammer
                .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
                .unwrap()
        });
        std::thread::sleep(Duration::from_millis(50));

        let res = handle
            .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
            .expect("no deadline: the request waits and runs");
        assert_eq!(res.int_val, Some(7));

        jam.join().unwrap();
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// R10-49. The deadline bounds the *queue wait*, never the transfer: C's
    /// timer callback returns immediately when `!puserPvt->isQueued`
    /// (asynManager.c:657-663), and the port thread cancels the timer as it
    /// dequeues (:908). A request whose driver call outlives the deadline
    /// therefore completes normally — the previous behaviour this guards against
    /// is an I/O timeout masquerading as a queue timeout.
    #[test]
    fn a_running_request_is_never_aborted_by_its_queue_deadline() {
        let (handle, reads) = slow_port(Duration::from_millis(200));

        let res = handle
            .submit_blocking(
                RequestOp::Int32Read,
                AsynUser::new(0).with_queue_timeout(Duration::from_millis(20)),
            )
            .expect("dequeued at once: the deadline never applies");
        assert_eq!(res.int_val, Some(7));
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A SyncIO read queued behind a `jam`-long read on a port whose SyncIO
    /// floor is `port_floor`: its result and how long it waited.
    fn syncio_behind_a_jam(
        port_floor: Duration,
        user_timeout: Duration,
        jam: Duration,
    ) -> (AsynResult<i32>, Duration) {
        let (handle, _reads) = blocking_slow_port(jam);
        handle.set_queue_lock_port_timeout(port_floor);
        let jammer = handle.clone();
        let jam = std::thread::spawn(move || {
            jammer
                .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
                .unwrap()
        });
        std::thread::sleep(Duration::from_millis(50));
        let started = std::time::Instant::now();
        let read = crate::sync_io::SyncIOHandle::from_handle(handle, 0, user_timeout).read_int32(0);
        let waited = started.elapsed();
        jam.join().unwrap();
        (read, waited)
    }

    fn assert_queue_timeout_at(read: AsynResult<i32>, waited: Duration, at: Duration) {
        assert!(
            read.as_ref().is_err_and(|e| e.is_queue_timeout()),
            "got {read:?}"
        );
        assert!(
            waited >= at && waited < at + Duration::from_millis(400),
            "timed out after {waited:?}, deadline {at:?}"
        );
    }

    /// C `queueLockPort` queues with `max(queueLockPortTimeout, timeout)`
    /// (asynManager.c:1894-1896) and `queueRequest` arms a timer only for a
    /// positive one (:1619-1626). With both at zero there is no deadline: the
    /// call waits its turn instead of failing the moment it queues.
    #[test]
    fn a_zero_floor_and_zero_timeout_syncio_call_waits_without_a_deadline() {
        let (read, waited) =
            syncio_behind_a_jam(Duration::ZERO, Duration::ZERO, Duration::from_millis(400));
        assert_eq!(read.unwrap(), 7);
        assert!(waited >= Duration::from_millis(300), "waited {waited:?}");
    }

    #[test]
    fn a_zero_floor_syncio_call_times_out_at_its_own_timeout() {
        let (read, waited) = syncio_behind_a_jam(
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_millis(1600),
        );
        assert_queue_timeout_at(read, waited, Duration::from_secs(1));
    }

    #[test]
    fn a_zero_timeout_syncio_call_times_out_at_the_port_floor() {
        let (read, waited) = syncio_behind_a_jam(
            Duration::from_secs(2),
            Duration::ZERO,
            Duration::from_millis(2600),
        );
        assert_queue_timeout_at(read, waited, Duration::from_secs(2));
    }

    #[test]
    fn a_syncio_timeout_above_the_port_floor_sets_the_deadline() {
        let (read, waited) = syncio_behind_a_jam(
            Duration::from_secs(2),
            Duration::from_secs(5),
            Duration::from_millis(5600),
        );
        assert_queue_timeout_at(read, waited, Duration::from_secs(5));
    }

    /// R10-49, the async waiter. C's `queueTimeoutCallback` fires on its own
    /// timer queue, so the caller learns of the timeout **at the deadline** — not
    /// when the port next comes free. That is what lets
    /// `queueTimeoutCallbackProcess` complete a record stuck at `pact = TRUE` 10 s
    /// after queueing (asynRecord.c:919-927), whatever the port is doing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_async_waiter_reports_the_queue_timeout_at_the_deadline() {
        let (handle, reads) = slow_port(Duration::from_millis(600));

        let jammer = handle.clone();
        let jam = tokio::task::spawn_blocking(move || {
            jammer
                .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = std::time::Instant::now();
        let err = handle
            .submit_async(
                RequestOp::Int32Read,
                AsynUser::new(0).with_queue_timeout(Duration::from_millis(30)),
            )
            .await
            .expect_err("the queue wait outlived the deadline");
        assert!(err.is_queue_timeout(), "got {err:?}");
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "the waiter woke at its own deadline, not when the port came free \
             (waited {:?})",
            started.elapsed()
        );

        jam.await.unwrap();
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "and the timed-out request is still never executed"
        );
    }

    /// The same claim, asked of whichever backend this build compiled rather
    /// than of tokio. The test above pins itself to a tokio runtime, so it
    /// could not see that `await_reply` armed its deadline only when tokio was
    /// the timer: on `exec_backend` — where no tokio runtime exists anywhere in
    /// the process — every caller's deadline was dropped and the timeout was
    /// reported at 550 ms of a 30 ms deadline, when the port came free.
    #[epics_macros_rs::epics_test]
    async fn the_deadline_is_armed_from_the_backend_s_own_timer() {
        let (handle, reads) = slow_port(Duration::from_millis(600));

        let jammer = handle.clone();
        let jam = std::thread::spawn(move || {
            jammer
                .submit_blocking(RequestOp::Int32Read, AsynUser::new(0))
                .unwrap()
        });
        epics_libcom_rs::runtime::task::sleep(Duration::from_millis(50)).await;

        let started = std::time::Instant::now();
        let err = handle
            .submit_async(
                RequestOp::Int32Read,
                AsynUser::new(0).with_queue_timeout(Duration::from_millis(30)),
            )
            .await
            .expect_err("the queue wait outlived the deadline");
        assert!(err.is_queue_timeout(), "got {err:?}");
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "the waiter woke at its own deadline, not when the port came free \
             (waited {:?})",
            started.elapsed()
        );

        jam.join().unwrap();
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Every parameter type the store supports must survive the actor path.
    ///
    /// `ParamSetValue` used to enumerate its own subset (Int32/Float64/Octet/
    /// Int32Array/Float64Array/UInt32Digital), so a driver's background thread
    /// pushing an `Int64`, `Int8Array`, `Int16Array`, `Int64Array`,
    /// `Float32Array` or `Enum` — all of which `ParamList` stores and records
    /// bind — had no variant to carry it, and the update was simply lost. The
    /// carrier now holds a `ParamValue`, so the store's type set *is* the
    /// request's type set.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_path_carries_every_settable_param_type() {
        use crate::param::{EnumEntry, ParamValue};
        use crate::request::ParamSetValue;

        struct TypedDriver {
            base: PortDriverBase,
        }
        impl PortDriver for TypedDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            // Serve the array reasons from the parameter cache, as a real driver
            // does — this is what a bound waveform record reads.
            fn read_int8_array(&mut self, user: &AsynUser, buf: &mut [i8]) -> AsynResult<usize> {
                let a = self.base.params.get_int8_array(user.reason, user.addr)?;
                let n = a.len().min(buf.len());
                buf[..n].copy_from_slice(&a[..n]);
                Ok(n)
            }
            fn read_int16_array(&mut self, user: &AsynUser, buf: &mut [i16]) -> AsynResult<usize> {
                let a = self.base.params.get_int16_array(user.reason, user.addr)?;
                let n = a.len().min(buf.len());
                buf[..n].copy_from_slice(&a[..n]);
                Ok(n)
            }
            fn read_int64_array(&mut self, user: &AsynUser, buf: &mut [i64]) -> AsynResult<usize> {
                let a = self.base.params.get_int64_array(user.reason, user.addr)?;
                let n = a.len().min(buf.len());
                buf[..n].copy_from_slice(&a[..n]);
                Ok(n)
            }
            fn read_float32_array(
                &mut self,
                user: &AsynUser,
                buf: &mut [f32],
            ) -> AsynResult<usize> {
                let a = self.base.params.get_float32_array(user.reason, user.addr)?;
                let n = a.len().min(buf.len());
                buf[..n].copy_from_slice(&a[..n]);
                Ok(n)
            }
        }
        let mut base = PortDriverBase::new("handle_test", 1, PortFlags::default());
        // reason 0..=5, in the order pushed below.
        base.create_param("I64", ParamType::Int64).unwrap();
        base.create_param("I8A", ParamType::Int8Array).unwrap();
        base.create_param("I16A", ParamType::Int16Array).unwrap();
        base.create_param("I64A", ParamType::Int64Array).unwrap();
        base.create_param("F32A", ParamType::Float32Array).unwrap();
        base.create_param("ENUM", ParamType::Enum).unwrap();
        let handle = make_handle(TypedDriver { base });

        handle
            .set_params_and_notify(
                0,
                vec![
                    ParamSetValue::new(0, 0, ParamValue::Int64(-9_000_000_000)),
                    ParamSetValue::new(1, 0, ParamValue::Int8Array(vec![-1i8, 2].into())),
                    ParamSetValue::new(2, 0, ParamValue::Int16Array(vec![7i16, -8].into())),
                    ParamSetValue::new(3, 0, ParamValue::Int64Array(vec![1i64 << 40].into())),
                    ParamSetValue::new(4, 0, ParamValue::Float32Array(vec![1.5f32].into())),
                    ParamSetValue::new(
                        5,
                        0,
                        ParamValue::Enum {
                            index: 1,
                            choices: vec![
                                EnumEntry {
                                    string: "Off".into(),
                                    value: 0,
                                    severity: 0,
                                },
                                EnumEntry {
                                    string: "On".into(),
                                    value: 1,
                                    severity: 0,
                                },
                            ]
                            .into(),
                        },
                    ),
                ],
            )
            .await
            .unwrap();

        // Int64 lands in the store (read back through the actor, FIFO-ordered
        // behind the update).
        assert_eq!(
            handle.read_int64(0, 0).await.unwrap(),
            -9_000_000_000,
            "an Int64 pushed through the actor path must reach the store"
        );
        // Enum lands too.
        assert_eq!(handle.read_enum(5, 0).await.unwrap(), 1);

        // The four array types land in the store — read each back through the
        // actor, exactly as a bound waveform record would.
        let rd = |op, reason| {
            let h = handle.clone();
            async move {
                h.submit_async(op, AsynUser::new(reason).with_addr(0))
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            rd(
                RequestOp::Int8ArrayRead {
                    buf: Default::default(),
                    max_elements: 8
                },
                1
            )
            .await
            .int8_array
            .unwrap(),
            vec![-1i8, 2]
        );
        assert_eq!(
            rd(
                RequestOp::Int16ArrayRead {
                    buf: Default::default(),
                    max_elements: 8
                },
                2
            )
            .await
            .int16_array
            .unwrap(),
            vec![7i16, -8]
        );
        assert_eq!(
            rd(
                RequestOp::Int64ArrayRead {
                    buf: Default::default(),
                    max_elements: 8
                },
                3
            )
            .await
            .int64_array
            .unwrap(),
            vec![1i64 << 40]
        );
        assert_eq!(
            rd(
                RequestOp::Float32ArrayRead {
                    buf: Default::default(),
                    max_elements: 8
                },
                4
            )
            .await
            .float32_array
            .unwrap(),
            vec![1.5f32]
        );
    }

    /// A param set that FAILS must be reported, not swallowed. The actor used to
    /// apply every update as `let _ = base.set_xxx_param(..)`, so pushing a value
    /// the parameter cannot hold (here: an Int32 into a Float64 parameter)
    /// returned success while the value went nowhere.
    ///
    /// The failing update is reported (C `setIntegerParam` asynPrints the
    /// paramList error at ASYN_TRACE_ERROR and returns the status); the other
    /// updates in the batch still land and the callbacks still fire.
    #[test]
    fn failed_param_set_is_reported_not_swallowed() {
        use crate::param::ParamValue;
        use crate::request::ParamSetValue;

        let handle = make_handle(TestDriver::new());
        // TestDriver: reason 0 = VAL (Int32), 1 = F64 (Float64), 2 = MSG (Octet).
        let err = handle
            .submit_blocking(
                RequestOp::CallParamCallbacks {
                    addr: 0,
                    time_stamp: None,
                    updates: vec![
                        // Int32 into a Float64 param: the store cannot hold it.
                        ParamSetValue::new(1, 0, ParamValue::Int32(7)),
                        // A good update in the same batch must still land.
                        ParamSetValue::new(0, 0, ParamValue::Int32(42)),
                    ],
                },
                AsynUser::new(0),
            )
            .unwrap_err();
        assert!(
            matches!(err, AsynError::TypeMismatch { .. }),
            "a set the parameter cannot hold must surface, got {err:?}"
        );
        assert_eq!(
            handle.read_int32_blocking(0, 0).unwrap(),
            42,
            "the good update in the batch still lands"
        );
        assert!(
            matches!(
                handle
                    .read_float64_blocking(1, 0)
                    .as_ref()
                    .map_err(AsynError::param_failure),
                Err(AsynError::ParamUndefined(1))
            ),
            "the failed update stored nothing: the Float64 param is still undefined"
        );
    }

    /// A handle whose actor never answers. `ActorId::new()` mints an id that
    /// is published on no thread, so `guard_reentrant` treats this caller as
    /// a foreign thread — exactly the shape of a record's device support
    /// waiting on a port whose device has gone away.
    fn stuck_handle(
        port_name: &str,
    ) -> (
        oneshot::Sender<AsynResult<RequestResult>>,
        AsyncCompletionHandle,
    ) {
        let (tx, rx) = oneshot::channel();
        (
            tx,
            AsyncCompletionHandle {
                rx,
                actor: ActorId::new(),
                port_name: port_name.into(),
            },
        )
    }

    /// F2 regression: `wait_blocking` used to underscore its `timeout` and
    /// park forever, leaving the record in PACT and leaking the thread.
    ///
    /// The wait runs on its own thread and reports through a channel, so an
    /// unbounded park fails this test in seconds instead of hanging it.
    #[test]
    fn wait_blocking_expires_when_the_actor_never_replies() {
        // Held for the whole test: dropping it would close the oneshot and
        // resolve the wait for the wrong reason.
        let (_tx, handle) = stuck_handle("F2_STUCK");

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = handle.wait_blocking(Duration::from_millis(200));
            let _ = done_tx.send((started.elapsed(), result));
        });

        let (elapsed, result) = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("wait_blocking never returned: its timeout argument is being discarded");

        assert!(
            elapsed >= Duration::from_millis(200),
            "returned before the deadline it was given: {elapsed:?}"
        );
        match result {
            Err(AsynError::Status {
                status: AsynStatus::Timeout,
                message,
            }) => assert!(
                message.contains("F2_STUCK"),
                "the timeout must name the port that owes the reply, got {message:?}"
            ),
            other => panic!("expected an asynTimeout, got {other:?}"),
        }
    }

    /// The other side of the same boundary: a reply that beats the deadline
    /// is delivered, so the bound cannot be satisfied by simply timing out.
    /// This is also what proves `park_timeout` still wakes on `unpark`.
    #[test]
    fn wait_blocking_returns_a_reply_that_beats_the_deadline() {
        let (tx, handle) = stuck_handle("F2_PROMPT");
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let _ = tx.send(Ok(RequestResult::write_n(7)));
        });

        let result = handle
            .wait_blocking(Duration::from_secs(30))
            .expect("the reply arrived well inside the deadline");
        assert_eq!(result.nbytes, 7);
    }

    /// An already-expired budget is still one honest attempt at the reply,
    /// not an error the caller cannot distinguish from a real timeout.
    #[test]
    fn wait_blocking_with_a_zero_timeout_still_takes_a_reply_that_is_already_there() {
        let (tx, handle) = stuck_handle("F2_ZERO");
        tx.send(Ok(RequestResult::write_n(3))).unwrap();

        let result = handle
            .wait_blocking(Duration::ZERO)
            .expect("the reply was already queued");
        assert_eq!(result.nbytes, 3);
    }

    /// A down-counting driver for the exception-callback tests: C
    /// `exceptionDisconnect` must not reach the driver's `disconnect`.
    struct Counted {
        base: PortDriverBase,
        disconnects: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl PortDriver for Counted {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn disconnect(&mut self, user: &AsynUser) -> AsynResult<()> {
            self.disconnects
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.base_mut().set_addr_connected(user.addr, false);
            Ok(())
        }
    }

    fn counted_port(
        name: &str,
        max_addr: usize,
    ) -> (PortHandle, Arc<std::sync::atomic::AtomicUsize>) {
        let disconnects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handle = make_handle(Counted {
            base: PortDriverBase::new(name, max_addr, PortFlags::default()),
            disconnects: disconnects.clone(),
        });
        handle.connect_blocking().unwrap();
        (handle, disconnects)
    }

    type Events = Arc<parking_lot::Mutex<Vec<(crate::exception::AsynException, i32)>>>;

    fn recorder(
        events: &Events,
    ) -> impl Fn(&crate::exception::ExceptionEvent<'_>) + Send + Sync + 'static {
        let events = events.clone();
        move |e| events.lock().push((e.exception, e.addr))
    }

    /// The answer of a direct op queued behind `exception_disconnect`, which
    /// does not wait for its own.
    fn connected_after(handle: &PortHandle, addr: i32) -> bool {
        let r = handle
            .submit_blocking(RequestOp::GetConnected, AsynUser::default().with_addr(addr))
            .unwrap();
        r.int_val == Some(1)
    }

    /// C keeps one `exceptionUserList` per `dpCommon` (asynManager.c:1364-1382)
    /// and `announceExceptionOccurred` walks the list of the one that changed
    /// (:613-639): a port user hears the port, a device user its device.
    #[test]
    fn an_exception_callback_hears_only_its_own_dp_common() {
        use crate::exception::AsynException;
        let (handle, _) = counted_port("exc_dp", 4);
        handle.connect_addr_blocking(1).unwrap();
        handle.connect_addr_blocking(2).unwrap();
        let port = Events::default();
        let dev1 = Events::default();
        let _p = handle.exception_callback_add(-1, recorder(&port));
        let _d = handle.exception_callback_add(1, recorder(&dev1));

        handle.exception_disconnect(2).unwrap();
        handle.exception_disconnect(1).unwrap();
        assert!(!connected_after(&handle, 1));
        assert_eq!(*dev1.lock(), [(AsynException::Connect, 1)]);
        assert!(port.lock().is_empty(), "{:?}", port.lock());

        handle.disconnect_blocking().unwrap();
        assert_eq!(*port.lock(), [(AsynException::Connect, -1)]);
        assert_eq!(dev1.lock().len(), 1);
    }

    /// Dropping the guard is C `exceptionCallbackRemove` (asynManager.c:1384-1408),
    /// and `asynReport`'s `exceptionUsers` counts the list of the `dpCommon` it
    /// prints (:1070-1074, :1098).
    #[test]
    fn dropping_the_guard_removes_the_callback_from_its_dp_common() {
        let (handle, _) = counted_port("exc_drop", 4);
        handle.connect_addr_blocking(1).unwrap();
        let events = Events::default();
        let guard = handle.exception_callback_add(1, recorder(&events));
        let report = handle.report_blocking(1).unwrap();
        assert!(
            report.contains("exceptionActive:No exceptionUsers 0 exceptionNotifys 0"),
            "{report}"
        );
        assert!(
            report.contains("exceptionActive No exceptionUsers 1 exceptionNotifys 0"),
            "{report}"
        );

        drop(guard);
        handle.exception_disconnect(1).unwrap();
        assert!(!connected_after(&handle, 1));
        assert!(events.lock().is_empty(), "{:?}", events.lock());
        let report = handle.report_blocking(1).unwrap();
        assert!(!report.contains("exceptionUsers 1"), "{report}");
    }

    /// The report's lock line, sampled as `reportPrintPort`'s own thread
    /// try-locks `synchronousLock` (asynManager.c:1053) while `span` is inside
    /// the actor. `span` runs on another thread; it signals `entered` from
    /// inside the actor and stays there until `release` fires.
    fn lock_line_during(
        handle: &PortHandle,
        span: impl FnOnce(PortHandle, Box<dyn FnOnce() + Send>) + Send + 'static,
    ) -> String {
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let inside: Box<dyn FnOnce() + Send> = Box::new(move || {
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        let other = handle.clone();
        let worker = std::thread::spawn(move || span(other, inside));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let report = RequestOp::Report {
            level: 1,
            sync_locked: false,
        };
        let pending = handle.try_submit(report, AsynUser::default()).unwrap();
        release.send(()).unwrap();
        worker.join().unwrap();
        let r = pending.wait_blocking(Duration::from_secs(5)).unwrap();
        let text = String::from_utf8_lossy(r.data.as_deref().unwrap_or_default()).into_owned();
        text.lines()
            .find(|l| l.contains("synchronousLock"))
            .unwrap()
            .to_string()
    }

    /// C's port thread holds `synchronousLock` across a `lockPort` span and a
    /// queued callback (asynManager.c:909, :1581, :1800), so a report taken
    /// meanwhile prints Yes; an idle port, and a driver's own `lock()`
    /// (asynPortDriver.cpp:3858, a different mutex), print No.
    #[test]
    fn report_shows_synchronous_lock_held_only_inside_a_port_lock_or_callback() {
        use crate::request::{CellBody, Unrun};
        const NO: &str = "    asynManagerLock:No synchronousLock:No";
        const YES: &str = "    asynManagerLock:No synchronousLock:Yes";
        let (handle, _) = counted_port("sync_rep", 1);
        let idle = handle.report_blocking(1).unwrap();
        assert!(idle.contains(&format!("{NO}\n")), "{idle}");

        let lock_port = lock_line_during(&handle, |h, inside| {
            h.lock_port_blocking(move |_| inside()).unwrap();
        });
        assert_eq!(lock_port, YES);

        struct Body(parking_lot::Mutex<Option<Box<dyn FnOnce() + Send>>>);
        impl CellBody for Body {
            fn process(&self, _: &mut dyn PortCalls) {
                (self.0.lock().take().unwrap())();
            }
            fn unrun(&self, why: Unrun) {
                panic!("the callback did not run: {why:?}");
            }
            fn done(self: Arc<Self>) {}
        }
        let callback = lock_line_during(&handle, |h, inside| {
            let cell = h.new_cell(Arc::new(Body(parking_lot::Mutex::new(Some(inside)))));
            h.queue_request(&cell, &AsynUser::new(0)).unwrap();
        });
        assert_eq!(callback, YES);

        let driver_lock = lock_line_during(&handle, |h, inside| {
            h.with_driver_blocking::<Counted, _>(move |_| inside())
                .unwrap();
        });
        assert_eq!(driver_lock, NO);

        let after = handle.report_blocking(1).unwrap();
        assert!(after.contains(&format!("{NO}\n")), "{after}");
    }

    /// C `exceptionDisconnect` (asynManager.c:2165-2190) marks the `dpCommon`
    /// down and announces it without the driver's `disconnect`, and refuses a
    /// `dpCommon` that is already down.
    #[test]
    fn exception_disconnect_downs_the_port_without_the_driver() {
        use crate::exception::AsynException;
        let (handle, disconnects) = counted_port("exc_down", 1);
        let events = Events::default();
        let _g = handle.exception_callback_add(0, recorder(&events));

        handle
            .submit_blocking(RequestOp::ExceptionDisconnect, AsynUser::default())
            .unwrap();
        assert!(!connected_after(&handle, -1));
        assert_eq!(*events.lock(), [(AsynException::Connect, -1)]);
        assert_eq!(disconnects.load(std::sync::atomic::Ordering::Relaxed), 0);

        let e = handle
            .submit_blocking(RequestOp::ExceptionDisconnect, AsynUser::default())
            .unwrap_err();
        assert_eq!(
            e.message(),
            "exc_down addr -1 asynManager:exceptionDisconnect but not connected"
        );
        assert_eq!(events.lock().len(), 1);
    }

    /// drvLove.c:663-687: a layered port's callback on the serial port under it
    /// takes the layered port down when the serial port drops — from the serial
    /// port's actor, which must not wait on the layered one.
    #[test]
    fn a_layered_port_goes_down_when_the_port_under_it_drops() {
        let (serial, _) = counted_port("exc_s0", 1);
        let (layer, layer_disconnects) = counted_port("exc_l0", 1);
        let under = serial.connect_device(-1);
        let over = layer.connect_device(-1);
        let on_layer = layer.clone();
        let _g = serial.exception_callback_add(-1, move |_| {
            if !under.connected() && over.connected() {
                on_layer.exception_disconnect(-1).unwrap();
            }
        });

        assert!(connected_after(&layer, -1));
        serial.disconnect_blocking().unwrap();
        assert!(!connected_after(&layer, -1));
        assert_eq!(
            layer_disconnects.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }
}
