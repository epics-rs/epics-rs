//! Async-friendly handle for submitting requests to a port actor.
//!
//! [`PortHandle`] is a lightweight, cloneable handle that sends requests to the
//! actor via an mpsc channel and receives replies via oneshot channels.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::runtime::RuntimeFlavor;
use tokio::sync::{mpsc, oneshot};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interrupt::InterruptManager;
use crate::port::DrvUserInfo;
use crate::port_actor::{ActorId, ActorMessage};
use crate::request::{CancelToken, RequestOp, RequestResult};
use crate::user::AsynUser;

/// Park the calling thread until `fut` resolves, from *any* context.
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
fn park_on<F: Future>(fut: F) -> F::Output {
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
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            // A spurious unpark just re-polls, which is always sound.
            Poll::Pending => std::thread::park(),
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
fn block_on_reply<F: Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        // A multi-threaded worker: hand this worker's other tasks to a sibling
        // before we park it. Purely a scheduling courtesy — correctness does
        // not depend on it.
        Ok(RuntimeFlavor::MultiThread) => tokio::task::block_in_place(|| park_on(fut)),
        // A current-thread runtime, or no runtime at all. `block_in_place` is
        // unavailable here (it panics), and parking directly is correct.
        _ => park_on(fut),
    }
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
    /// Block the current thread until the result arrives.
    ///
    /// Errors instead of deadlocking if called from the port's own actor
    /// thread — that actor is the thread that would have to produce the reply.
    pub fn wait_blocking(self, _timeout: Duration) -> AsynResult<RequestResult> {
        guard_reentrant(self.actor, &self.port_name, "wait_blocking")?;
        block_on_reply(self)
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
        }
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
        block_on_reply(fut)
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
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = ActorMessage::new(op, user, cancel, reply_tx);
        self.tx.send(msg).await.map_err(|_| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("actor channel closed for port {}", self.port_name),
        })?;
        reply_rx.await.map_err(|_| AsynError::Status {
            status: AsynStatus::Error,
            message: "actor dropped reply channel".into(),
        })?
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

    /// Resolve a drvInfo (and asyn `addr`) to its shared reason index. The async
    /// port-handle path needs only the reason; the per-record [`DrvUserInfo`]
    /// (string-length cap etc.) is surfaced on the record-binding blocking path
    /// [`drv_user_create_blocking`](Self::drv_user_create_blocking).
    pub async fn drv_user_create(&self, drv_info: &str, addr: i32) -> AsynResult<usize> {
        let user = AsynUser::default().with_addr(addr);
        let result = self
            .submit_async(
                RequestOp::DrvUserCreate {
                    drv_info: drv_info.to_string(),
                    addr,
                },
                user,
            )
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

    pub fn drv_user_create_blocking(&self, drv_info: &str, addr: i32) -> AsynResult<DrvUserInfo> {
        let user = AsynUser::default().with_addr(addr);
        let result = self.submit_blocking(
            RequestOp::DrvUserCreate {
                drv_info: drv_info.to_string(),
                addr,
            },
            user,
        )?;
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
    /// # Example
    /// ```ignore
    /// port_handle.set_params_and_notify(0, vec![
    ///     ParamSetValue::Int32 { reason: acquire_busy, addr: 0, value: 0 },
    ///     ParamSetValue::Int32 { reason: status, addr: 0, value: ADStatus::Idle as i32 },
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

    pub fn get_option_blocking(&self, key: &str) -> AsynResult<String> {
        let user = AsynUser::default();
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

    pub fn set_option_blocking(&self, key: &str, value: &str) -> AsynResult<()> {
        let user = AsynUser::default();
        self.submit_blocking(
            RequestOp::SetOption {
                key: key.to_string(),
                value: value.to_string(),
            },
            user,
        )?;
        Ok(())
    }

    /// Address-aware setOption — iocsh `asynSetOption portName addr key value`.
    /// The C surface (`asynShellCommands.c:604-606`) carries `addr`
    /// via `connectDevice`; here we attach it to the AsynUser so the
    /// driver can disambiguate device-scoped options.
    pub fn set_option_addr_blocking(&self, addr: i32, key: &str, value: &str) -> AsynResult<()> {
        let user = AsynUser::default().with_addr(addr);
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
    pub fn set_input_eos_blocking(&self, eos: &[u8]) -> AsynResult<()> {
        let user = AsynUser::default();
        self.submit_blocking(RequestOp::SetInputEos { eos: eos.to_vec() }, user)?;
        Ok(())
    }

    /// Apply output EOS bytes — C `pasynOctet->setOutputEos`.
    pub fn set_output_eos_blocking(&self, eos: &[u8]) -> AsynResult<()> {
        let user = AsynUser::default();
        self.submit_blocking(RequestOp::SetOutputEos { eos: eos.to_vec() }, user)?;
        Ok(())
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

    pub async fn set_option(&self, key: &str, value: &str) -> AsynResult<()> {
        let user = AsynUser::default();
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

    pub fn enable_addr_blocking(&self, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        self.submit_blocking(RequestOp::EnableAddr, user)?;
        Ok(())
    }

    pub fn disable_addr_blocking(&self, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        self.submit_blocking(RequestOp::DisableAddr, user)?;
        Ok(())
    }

    /// Enable or disable the entire port. C parity:
    /// `pasynManager->enable(pasynUser, enable)` — fired by
    /// `asynRecord` `ENBL` field writes (`asynRecord.c:484-486`).
    pub fn set_enable_blocking(&self, enable: bool) -> AsynResult<()> {
        let user = AsynUser::default();
        self.submit_blocking(RequestOp::SetEnable { yes: enable }, user)?;
        Ok(())
    }

    /// Enable or disable auto-connect for the port. C parity:
    /// `pasynManager->autoConnect(pasynUser, autoConnect)` — fired
    /// by `asynRecord` `AUCT` field writes
    /// (`asynRecord.c:481-482`). Emits `asynExceptionAutoConnect`
    /// unconditionally.
    pub fn set_auto_connect_blocking(&self, yes: bool) -> AsynResult<()> {
        let user = AsynUser::default();
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

    /// Query whether the port is currently enabled (blocking).
    pub fn is_enabled_blocking(&self) -> AsynResult<bool> {
        let result = self.submit_blocking(RequestOp::GetEnable, AsynUser::new(0))?;
        Ok(result.int_val.unwrap_or(0) != 0)
    }

    /// Query whether auto-connect is enabled for the port (blocking).
    pub fn is_auto_connect_blocking(&self) -> AsynResult<bool> {
        let result = self.submit_blocking(RequestOp::GetAutoConnect, AsynUser::new(0))?;
        Ok(result.int_val.unwrap_or(0) != 0)
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
        base.exception_sink = Some(exc_mgr.clone());
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
        handle.set_enable_blocking(false).unwrap();
        handle.set_enable_blocking(true).unwrap();
        // C `enable` fires on every transition; both calls land.
        assert_eq!(hits.load(Ordering::Relaxed), 2);
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
        base.exception_sink = Some(exc_mgr.clone());
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
        handle.set_auto_connect_blocking(true).unwrap();
        handle.set_auto_connect_blocking(false).unwrap();
        handle.set_auto_connect_blocking(false).unwrap();
        assert_eq!(hits.load(Ordering::Relaxed), 3);
    }
}
