//! Async-friendly handle for submitting requests to a port actor.
//!
//! [`PortHandle`] is a lightweight, cloneable handle that sends requests to the
//! actor via an mpsc channel and receives replies via oneshot channels.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interrupt::InterruptManager;
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
        Ok(AsyncCompletionHandle { rx: reply_rx })
    }

    /// Submit a request and block until completion (for sync callers).
    ///
    /// Works both from plain threads and from within a tokio runtime context
    /// (uses `block_in_place` when called from an async context).
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
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = ActorMessage::new(op, user, CancelToken::new(), reply_tx);
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

    pub async fn write_octet(&self, reason: usize, addr: i32, data: Vec<u8>) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_async(RequestOp::OctetWrite { data }, user)
            .await?;
        Ok(())
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

    pub async fn drv_user_create(&self, drv_info: &str) -> AsynResult<usize> {
        let user = AsynUser::default();
        let result = self
            .submit_async(
                RequestOp::DrvUserCreate {
                    drv_info: drv_info.to_string(),
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

    pub fn drv_user_create_blocking(&self, drv_info: &str) -> AsynResult<usize> {
        let user = AsynUser::default();
        let result = self.submit_blocking(
            RequestOp::DrvUserCreate {
                drv_info: drv_info.to_string(),
            },
            user,
        )?;
        result.reason.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "drv_user_create returned no reason".into(),
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
}
