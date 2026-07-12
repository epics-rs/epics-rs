//! Async-friendly handle for submitting requests to a port actor.
//!
//! [`PortHandle`] is a lightweight, cloneable handle that sends requests to the
//! actor via an mpsc channel and receives replies via oneshot channels.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interfaces::{Capability, InterfaceType};
use crate::interrupt::InterruptManager;
use crate::port::DrvUserInfo;
use crate::port_actor::ActorMessage;
use crate::request::{CancelToken, RequestOp, RequestResult};
use crate::user::AsynUser;

/// Async completion handle returned by [`PortHandle::try_submit`].
///
/// Implements `Future` for async waiting, plus `wait_blocking()` for sync callers.
pub struct AsyncCompletionHandle {
    rx: oneshot::Receiver<AsynResult<RequestResult>>,
}

impl AsyncCompletionHandle {
    /// Block the current thread until the result arrives or timeout.
    pub fn wait_blocking(self, _timeout: Duration) -> AsynResult<RequestResult> {
        match self.rx.blocking_recv() {
            Ok(result) => result,
            Err(_) => Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "actor dropped reply channel".into(),
            }),
        }
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
    port_name: String,
    interrupts: Arc<InterruptManager>,
    can_block: bool,
    multi_device: bool,
    max_addr: i32,
    /// The port's interface registry — the driver's own
    /// [`crate::port::PortDriver::capabilities`] declaration, recorded when the
    /// port is registered. C's asynManager keeps the same thing as the list of
    /// `registerInterface` calls the driver made, and clients ask for it with
    /// `findInterface` (asynManager.c:1352-1372); asynRecord uses it to fill
    /// OCTETIV / I32IV / UI32IV / F64IV / OPTIONIV / GPIBIV
    /// (asynRecord.c:1177-1240). Registration-time, not a runtime query: a
    /// driver cannot gain or lose an interface after `registerInterface`.
    interfaces: Arc<[Capability]>,
}

impl PortHandle {
    pub(crate) fn new(
        tx: mpsc::Sender<ActorMessage>,
        port_name: String,
        interrupts: Arc<InterruptManager>,
    ) -> Self {
        Self {
            tx,
            port_name,
            interrupts,
            can_block: false,
            multi_device: false,
            max_addr: 1,
            interfaces: crate::interfaces::default_capabilities().into(),
        }
    }

    /// Record the driver's declared interface set — see [`Self::interfaces`].
    /// Called by the runtime layer at handle construction, from the driver's own
    /// [`crate::port::PortDriver::capabilities`].
    pub fn set_interfaces(&mut self, interfaces: Vec<Capability>) {
        self.interfaces = interfaces.into();
    }

    /// Does the port implement this interface? The port's `findInterface`
    /// (asynManager.c:1352-1372).
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
        Ok(AsyncCompletionHandle { rx: reply_rx })
    }

    /// Submit a request and block until completion (for sync callers).
    ///
    /// Works both from plain threads and from within a tokio runtime context
    /// (uses `block_in_place` when called from an async context). Inside a
    /// runtime the request goes through [`Self::submit_async`], so a
    /// [`AsynUser::queue_timeout`] wakes the caller at its deadline. On a plain
    /// thread there is no timer service to wake it, and none is needed: the
    /// deadline is still enforced — the actor refuses to run a request that
    /// waited past it (`PortActor::process_one`) and replies
    /// [`AsynError::QueueTimeout`] — and a thread parked on the reply has no
    /// earlier action it could take, since it cannot report anything until it
    /// returns.
    pub fn submit_blocking(&self, op: RequestOp, user: AsynUser) -> AsynResult<RequestResult> {
        if tokio::runtime::Handle::try_current().is_ok() {
            // Inside a tokio runtime — use block_in_place to avoid panicking
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(self.submit_async(op, user))
            })
        } else {
            // Plain thread — use blocking_send/blocking_recv directly
            let (reply_tx, reply_rx) = oneshot::channel();
            let msg = ActorMessage::new(op, user, CancelToken::new(), reply_tx);
            self.tx.blocking_send(msg).map_err(|_| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("actor channel closed for port {}", self.port_name),
            })?;
            reply_rx.blocking_recv().map_err(|_| AsynError::Status {
                status: AsynStatus::Error,
                message: "actor dropped reply channel".into(),
            })?
        }
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
    /// completion 10 s after queueing, whatever the port is doing (:919-926).
    ///
    /// Whether the deadline is honoured is not this timer's call: it hands the
    /// question to [`CancelToken::time_out_if_queued`], C's `if(!isQueued)`
    /// guard. A request the actor has already begun is *never* aborted — the
    /// wait simply resumes — so this timer cannot cut a transfer short, only a
    /// wait. The actor re-checks the same deadline at dequeue, which is what
    /// covers the blocking submit paths (a thread parked in `blocking_recv` has
    /// no earlier action to take anyway: it cannot report until it returns).
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
        if let Some(limit) = queue_timeout {
            match tokio::time::timeout(limit, &mut reply_rx).await {
                Ok(reply) => return reply.map_err(|_| dropped())?,
                Err(_elapsed) => {
                    if cancel.time_out_if_queued() {
                        return Err(AsynError::QueueTimeout {
                            port: self.port_name.clone(),
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

    /// Sync version of [`set_params_and_notify`] for non-async contexts
    /// (std threads, acquisition tasks). Uses `blocking_send` internally.
    ///
    /// Returns an error if called from within a Tokio runtime — use the async
    /// version instead.
    pub fn set_params_and_notify_blocking(
        &self,
        addr: i32,
        updates: Vec<crate::request::ParamSetValue>,
    ) -> AsynResult<()> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "set_params_and_notify_blocking called inside Tokio runtime; \
                          use the async set_params_and_notify() instead"
                    .into(),
            });
        }
        self.enqueue_blocking(RequestOp::CallParamCallbacks { addr, updates }, addr)
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

    /// Enqueue-only sync helper: guaranteed delivery, no reply wait.
    fn enqueue_blocking(&self, op: RequestOp, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        let (reply_tx, _) = oneshot::channel();
        let msg = ActorMessage::new(op, user, CancelToken::new(), reply_tx);
        self.tx.blocking_send(msg).map_err(|_| AsynError::Status {
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
    /// the read half asynRecord's `getEos` (asynRecord.c:1985-2026) runs after
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

    /// Query whether the port's transport is connected (blocking) — C
    /// `pasynManager->isConnected`.
    pub fn is_connected_blocking(&self) -> AsynResult<bool> {
        let result = self.submit_blocking(RequestOp::GetConnected, AsynUser::new(0))?;
        Ok(result.int_val.unwrap_or(0) != 0)
    }

    /// Async twins of the three port-state queries above. An `asynRecord`
    /// exception callback runs on the *port actor thread* (the driver announces
    /// the exception from inside its own op), so it cannot use the `_blocking`
    /// forms — a round trip to the actor from the actor would deadlock. The
    /// callback spawns the refresh onto the runtime instead and awaits these.
    pub async fn is_enabled(&self) -> AsynResult<bool> {
        let result = self
            .submit_async(RequestOp::GetEnable, AsynUser::new(0))
            .await?;
        Ok(result.int_val.unwrap_or(0) != 0)
    }

    /// Async twin of [`Self::is_auto_connect_blocking`].
    pub async fn is_auto_connect(&self) -> AsynResult<bool> {
        let result = self
            .submit_async(RequestOp::GetAutoConnect, AsynUser::new(0))
            .await?;
        Ok(result.int_val.unwrap_or(0) != 0)
    }

    /// Async twin of [`Self::is_connected_blocking`].
    pub async fn is_connected(&self) -> AsynResult<bool> {
        let result = self
            .submit_async(RequestOp::GetConnected, AsynUser::new(0))
            .await?;
        Ok(result.int_val.unwrap_or(0) != 0)
    }

    /// Connect the port's transport (blocking) — C `pasynCommon->connect`.
    pub fn connect_blocking(&self) -> AsynResult<()> {
        self.submit_blocking(RequestOp::Connect, AsynUser::new(0))?;
        Ok(())
    }

    /// Disconnect the port's transport (blocking) — C `pasynCommon->disconnect`.
    pub fn disconnect_blocking(&self) -> AsynResult<()> {
        self.submit_blocking(RequestOp::Disconnect, AsynUser::new(0))?;
        Ok(())
    }

    /// Install the echo interpose on the port (blocking) — C
    /// `asynInterposeEcho(portName, addr)`.
    pub fn push_echo_interpose_blocking(&self) -> AsynResult<()> {
        self.submit_blocking(RequestOp::PushEchoInterpose, AsynUser::new(0))?;
        Ok(())
    }

    /// Install the delay interpose on the port (blocking) — C
    /// `asynInterposeDelay(portName, addr, delay)`.
    pub fn push_delay_interpose_blocking(&self, delay: std::time::Duration) -> AsynResult<()> {
        self.submit_blocking(RequestOp::PushDelayInterpose { delay }, AsynUser::new(0))?;
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
        std::thread::Builder::new()
            .name("test-handle-actor".into())
            .spawn(move || actor.run())
            .unwrap();
        PortHandle::new(tx, "handle_test".into(), interrupts)
    }

    #[test]
    fn handle_blocking_int32() {
        let handle = make_handle(TestDriver::new());
        handle.write_int32_blocking(0, 0, 42).unwrap();
        assert_eq!(handle.read_int32_blocking(0, 0).unwrap(), 42);
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
        let err = handle
            .submit_blocking(
                RequestOp::Int32Read,
                AsynUser::new(0).with_queue_timeout(Duration::from_millis(10)),
            )
            .expect_err("the queue wait outlived the deadline");
        assert!(err.is_queue_timeout(), "got {err:?}");

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
    /// after queueing (asynRecord.c:919-926), whatever the port is doing.
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
}
