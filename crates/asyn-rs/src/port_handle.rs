//! Async-friendly handle for submitting requests to a port actor.
//!
//! [`PortHandle`] is a lightweight, cloneable handle that sends requests to the
//! actor via an mpsc channel and receives replies via oneshot channels.

// RTEMS-EXEC-MODEL-ALLOW(8): checked, not waived — all 8 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p asyn-rs
// --all-features`, 1081/1081). asyn-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use crate::param::ParamValue;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use tokio::runtime::RuntimeFlavor;
use tokio::sync::{mpsc, oneshot};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interfaces::{Capability, InterfaceType};
use crate::interrupt::InterruptManager;
use crate::port::{DrvUserInfo, DrvUserRequest};
use crate::port_actor::{ActorId, ActorMessage};
use crate::request::{CancelToken, RequestOp, RequestResult};
use crate::user::AsynUser;

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
fn park_on<F: Future>(fut: F, deadline: Option<Instant>) -> Option<F::Output> {
    struct ParkWaker(std::thread::Thread);
    impl Wake for ParkWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let mut fut = pin!(fut);
    let waker = Waker::from(Arc::new(ParkWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        // Polled before the deadline is consulted, so a reply that landed
        // exactly at the deadline is delivered rather than discarded.
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return Some(value),
            // A spurious unpark just re-polls, which is always sound.
            Poll::Pending => match deadline {
                None => std::thread::park(),
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return None;
                    }
                    std::thread::park_timeout(remaining);
                }
            },
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
    /// (`asynManager.c:647-701`) leaves a dequeued request alone — it returns
    /// at once on `!puserPvt->isQueued` (`:655-661`). What expiry
    /// buys the caller is the ability to report — `WriteCompletion::wait`
    /// (`epics-base-rs/src/server/device_support.rs`) turns the `asynTimeout`
    /// into the record's completion, where an unbounded park left the record
    /// in `PACT` for good and never released the thread.
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
    tx: mpsc::Sender<ActorMessage>,
    port_name: Arc<str>,
    /// Identity of the actor on the receiving end of `tx`. Every blocking entry
    /// point compares it against the calling thread — see [`guard_reentrant`].
    actor: ActorId,
    interrupts: Arc<InterruptManager>,
    can_block: bool,
    multi_device: bool,
    max_addr: i32,
    /// The port's interface registry — the driver's own
    /// [`crate::port::PortDriver::capabilities`] declaration, recorded when the
    /// port is registered. C's asynManager keeps the same thing as the list of
    /// `registerInterface` calls the driver made, and clients ask for it with
    /// `findInterface` (asynManager.c:1468-1512); asynRecord uses it to fill
    /// OCTETIV / I32IV / UI32IV / F64IV / OPTIONIV / GPIBIV
    /// (asynRecord.c:1177-1240). Registration-time, not a runtime query: a
    /// driver cannot gain or lose an interface after `registerInterface`.
    interfaces: Arc<[Capability]>,
}

impl PortHandle {
    /// `actor` must be the id of the actor owning `tx`'s receiving end
    /// (`PortActor::id`), so that a blocking caller can recognise a re-entrant
    /// call from that actor's own thread.
    pub(crate) fn new(
        tx: mpsc::Sender<ActorMessage>,
        port_name: String,
        interrupts: Arc<InterruptManager>,
        actor: ActorId,
    ) -> Self {
        Self {
            tx,
            port_name: port_name.into(),
            actor,
            interrupts,
            can_block: false,
            multi_device: false,
            max_addr: 1,
            interfaces: crate::interfaces::default_capabilities().into(),
        }
    }

    /// Record the driver's declared interface set — see `Self::interfaces`.
    /// Called by the runtime layer at handle construction, from the driver's own
    /// [`crate::port::PortDriver::capabilities`].
    pub fn set_interfaces(&mut self, interfaces: Vec<Capability>) {
        self.interfaces = interfaces.into();
    }

    /// Does the port implement this interface? The port's `findInterface`
    /// (asynManager.c:1468-1512).
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

    /// Maximum sub-device address (exclusive upper bound).
    /// Mirrors C `pport->numAddr` / the `maxAddr` constructor
    /// argument of `asynPortDriver`. Always at least 1.
    pub fn max_addr(&self) -> i32 {
        self.max_addr
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
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = ActorMessage::new(op, user, cancel, reply_tx);
        self.tx.try_send(msg).map_err(|e| {
            let detail = match e {
                mpsc::error::TrySendError::Full(_) => "full",
                mpsc::error::TrySendError::Closed(_) => "closed",
            };
            AsynError::Status {
                status: AsynStatus::Error,
                message: format!("actor channel {} for port {}", detail, self.port_name),
            }
        })?;
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
    /// [`AsynUser::queue_timeout`] deadline is enforced on every path: the
    /// reply wait wakes at the deadline, and the actor refuses to run a
    /// request that waited past it
    /// (`PortActor::process_one`), replying [`AsynError::QueueTimeout`].
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
    /// C parity: `asynManager::queueRequest` (`asynManager.c:1514`) enqueues
    /// the request on the port thread and the caller's callback runs later;
    /// `asynManager::cancelRequest` (`asynManager.c:1630`) removes a request
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
        let queue_timeout = user.queue_timeout;
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = ActorMessage::new(op, user, cancel.clone(), reply_tx);
        self.tx.send(msg).await.map_err(|_| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("actor channel closed for port {}", self.port_name),
        })?;
        self.await_reply(reply_rx, cancel, queue_timeout).await
    }

    /// Wait for the actor's reply, honouring the request's queue-wait deadline —
    /// C's `epicsTimer` armed by `queueRequest` (asynManager.c:1617-1623).
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
    /// wait. The actor re-checks the same deadline at dequeue as well, which is
    /// C's own belt-and-braces (`asynManager.c:906` cancels the timer as the
    /// port thread dequeues) rather than a fallback for a caller this timer
    /// cannot serve — there is no such caller.
    async fn await_reply(
        &self,
        mut reply_rx: oneshot::Receiver<AsynResult<RequestResult>>,
        cancel: CancelToken,
        queue_timeout: Option<Duration>,
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
            match epics_libcom_rs::runtime::task::timeout(limit, &mut reply_rx).await {
                Ok(reply) => return reply.map_err(|_| dropped())?,
                Err(_elapsed) => {
                    if cancel.time_out_if_queued() {
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
        let user = AsynUser::new(reason).with_addr(addr);
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
    /// `*nbytesTransfered`).
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
    ) -> AsynResult<Vec<i32>> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self
            .submit_async(RequestOp::Int32ArrayRead { max_elements }, user)
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
    ) -> AsynResult<Vec<f64>> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self
            .submit_async(RequestOp::Float64ArrayRead { max_elements }, user)
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
            },
            user,
        )
        .await?;
        Ok(())
    }

    // --- Sync convenience methods ---

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

    /// Flush changed parameters as interrupt notifications (blocking).
    pub fn call_param_callbacks_blocking(&self, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        self.submit_blocking(
            RequestOp::CallParamCallbacks {
                addr,
                updates: vec![],
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
        self.enqueue(RequestOp::CallParamCallbacks { addr, updates }, addr)
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
            self.enqueue(RequestOp::CallParamCallbacks { addr, updates }, addr),
        )
    }

    /// Enqueue-only async helper: guaranteed delivery, no reply wait.
    async fn enqueue(&self, op: RequestOp, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        let (reply_tx, _) = oneshot::channel();
        let msg = ActorMessage::new(op, user, CancelToken::new(), reply_tx);
        self.tx.send(msg).await.map_err(|_| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("actor channel closed for port {}", self.port_name),
        })
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
    /// causes (an RFC 2217 negotiation on a COM port, `asynInterposeCom.c:475`).
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

    /// Run the port driver's `report(level)` callback on the actor
    /// thread. Mirrors C asyn `asynReport` (asynShellCommands.c:586)
    /// which calls `pasynManager->report` to walk each port's
    /// `pasynCommon->report` interface. Output is written to stderr
    /// by the driver itself.
    pub fn report_blocking(&self, level: i32) -> AsynResult<()> {
        let user = AsynUser::default();
        self.submit_blocking(RequestOp::Report { level }, user)?;
        Ok(())
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

    pub async fn get_option(&self, key: &str) -> AsynResult<String> {
        let user = AsynUser::default();
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

    /// C `pasynManager->enable(pasynUser, enable)` (asynManager.c:2224-2251),
    /// fired by `asynRecord` `ENBL` field writes (`asynRecord.c:484-486`).
    ///
    /// `addr` is the address of the caller's user, not a scope switch: C has
    /// one `enable`, and `findDpCommon` (:538-545) makes it land on the DEVICE's
    /// `dpCommon` for a multi-device port the caller addressed, on the port's
    /// own otherwise. `-1` names the port explicitly.
    pub fn set_enable_blocking(&self, addr: i32, enable: bool) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        self.submit_blocking(RequestOp::SetEnable { yes: enable }, user)?;
        Ok(())
    }

    /// C `pasynManager->autoConnect(pasynUser, autoConnect)`
    /// (asynManager.c:2312-2329), fired by `asynRecord` `AUCT` field writes
    /// (`asynRecord.c:481-482`). Emits `asynExceptionAutoConnect`
    /// unconditionally; `addr` resolves as in [`Self::set_enable_blocking`].
    pub fn set_auto_connect_blocking(&self, addr: i32, yes: bool) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        self.submit_blocking(RequestOp::SetAutoConnect { yes }, user)?;
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
    /// (:538-544), so a per-device `asynEnable P 1 0` is invisible at addr 0
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
    /// Connect-queue waiver applies (asynManager.c:1536-1538) and the request
    /// runs on a disconnected port — this is the route that brings a down
    /// line up. A device-addressed connect (asynRecord CNCT at the record's
    /// ADDR) goes through [`Self::submit_blocking`] with the caller's own
    /// user and is refused on a disconnected port, as C refuses it (W10-D1).
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
        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(Box::new(driver), rx);
        let actor_id = actor.id();
        std::thread::Builder::new()
            .name("test-handle-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        PortHandle::new(tx, "handle_test".into(), interrupts, actor_id)
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
    /// per-port `enabled` flag and emits `asynExceptionEnable`. The
    /// Rust analogue routes through `RequestOp::SetEnable` →
    /// `PortDriver::enable/disable` → `PortDriverBase::set_enabled`.
    #[test]
    fn handle_set_enable_blocking_propagates_to_driver() {
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
        handle.set_enable_blocking(-1, false).unwrap();
        handle.set_enable_blocking(-1, true).unwrap();
        // C `enable` fires on every transition; both calls land.
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

        let mut handle = make_handle(GpibDrv(PortDriverBase::new(
            "gpib_vtable",
            1,
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
    /// writes. The Rust analogue routes through
    /// `RequestOp::SetAutoConnect` → `set_auto_connect`.
    #[test]
    fn handle_set_auto_connect_blocking_propagates_to_driver() {
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
        handle.set_auto_connect_blocking(-1, true).unwrap();
        handle.set_auto_connect_blocking(-1, false).unwrap();
        handle.set_auto_connect_blocking(-1, false).unwrap();
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

    /// R10-49. C `queueRequest(pasynUser, priority, timeout)` arms a timer
    /// (asynManager.c:1617-1623) whose callback unlinks a still-queued request
    /// (:647-700) and runs the caller's `timeoutUser` **instead of** its
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
    /// (asynManager.c:655-661), and the port thread cancels the timer as it
    /// dequeues (:906). A request whose driver call outlives the deadline
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
            rd(RequestOp::Int8ArrayRead { max_elements: 8 }, 1)
                .await
                .int8_array
                .unwrap(),
            vec![-1i8, 2]
        );
        assert_eq!(
            rd(RequestOp::Int16ArrayRead { max_elements: 8 }, 2)
                .await
                .int16_array
                .unwrap(),
            vec![7i16, -8]
        );
        assert_eq!(
            rd(RequestOp::Int64ArrayRead { max_elements: 8 }, 3)
                .await
                .int64_array
                .unwrap(),
            vec![1i64 << 40]
        );
        assert_eq!(
            rd(RequestOp::Float32ArrayRead { max_elements: 8 }, 4)
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
                handle.read_float64_blocking(1, 0),
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
}
