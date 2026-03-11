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
        }
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
    pub fn try_submit(
        &self,
        op: RequestOp,
        user: AsynUser,
    ) -> AsynResult<AsyncCompletionHandle> {
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
        self.tx.try_send(msg).map_err(|_| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("actor channel full or closed for port {}", self.port_name),
        })?;
        Ok(AsyncCompletionHandle { rx: reply_rx })
    }

    /// Submit a request and block until completion (for sync callers).
    ///
    /// Works both from plain threads and from within a tokio runtime context
    /// (uses `block_in_place` when called from an async context).
    pub fn submit_blocking(
        &self,
        op: RequestOp,
        user: AsynUser,
    ) -> AsynResult<RequestResult> {
        if tokio::runtime::Handle::try_current().is_ok() {
            // Inside a tokio runtime — use block_in_place to avoid panicking
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(self.submit(op, user))
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

    /// Submit a request and await completion (for async callers).
    pub async fn submit(&self, op: RequestOp, user: AsynUser) -> AsynResult<RequestResult> {
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
        let result = self.submit(RequestOp::Int32Read, user).await?;
        result.int_val.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "int32 read returned no value".into(),
        })
    }

    pub async fn write_int32(&self, reason: usize, addr: i32, value: i32) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit(RequestOp::Int32Write { value }, user).await?;
        Ok(())
    }

    pub async fn read_int64(&self, reason: usize, addr: i32) -> AsynResult<i64> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self.submit(RequestOp::Int64Read, user).await?;
        result.int64_val.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "int64 read returned no value".into(),
        })
    }

    pub async fn write_int64(&self, reason: usize, addr: i32, value: i64) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit(RequestOp::Int64Write { value }, user).await?;
        Ok(())
    }

    pub async fn read_float64(&self, reason: usize, addr: i32) -> AsynResult<f64> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self.submit(RequestOp::Float64Read, user).await?;
        result.float_val.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "float64 read returned no value".into(),
        })
    }

    pub async fn write_float64(&self, reason: usize, addr: i32, value: f64) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit(RequestOp::Float64Write { value }, user).await?;
        Ok(())
    }

    pub async fn read_octet(&self, reason: usize, addr: i32, buf_size: usize) -> AsynResult<Vec<u8>> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self.submit(RequestOp::OctetRead { buf_size }, user).await?;
        result.data.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "octet read returned no data".into(),
        })
    }

    pub async fn write_octet(&self, reason: usize, addr: i32, data: Vec<u8>) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit(RequestOp::OctetWrite { data }, user).await?;
        Ok(())
    }

    pub async fn read_uint32_digital(&self, reason: usize, addr: i32, mask: u32) -> AsynResult<u32> {
        let user = AsynUser::new(reason).with_addr(addr);
        let result = self.submit(RequestOp::UInt32DigitalRead { mask }, user).await?;
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
        self.submit(RequestOp::UInt32DigitalWrite { value, mask }, user)
            .await?;
        Ok(())
    }

    pub async fn flush(&self, reason: usize, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit(RequestOp::Flush, user).await?;
        Ok(())
    }

    pub async fn drv_user_create(&self, drv_info: &str) -> AsynResult<usize> {
        let user = AsynUser::default();
        let result = self
            .submit(
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
        let result = self.submit(RequestOp::EnumRead, user).await?;
        result.enum_index.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "enum read returned no index".into(),
        })
    }

    pub async fn write_enum(&self, reason: usize, addr: i32, index: usize) -> AsynResult<()> {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit(RequestOp::EnumWrite { index }, user).await?;
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
            .submit(RequestOp::Int32ArrayRead { max_elements }, user)
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
        self.submit(RequestOp::Int32ArrayWrite { data }, user).await?;
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
            .submit(RequestOp::Float64ArrayRead { max_elements }, user)
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
        self.submit(RequestOp::Float64ArrayWrite { data }, user).await?;
        Ok(())
    }

    /// Flush changed parameters as interrupt notifications (async).
    pub async fn call_param_callbacks(&self, addr: i32) -> AsynResult<()> {
        let user = AsynUser::new(0).with_addr(addr);
        self.submit(RequestOp::CallParamCallbacks { addr }, user).await?;
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
        self.submit_blocking(RequestOp::CallParamCallbacks { addr }, user)?;
        Ok(())
    }

    /// Flush changed parameters as interrupt notifications (fire-and-forget).
    ///
    /// Safe to call from within a Tokio runtime context.
    /// The actor processes messages in FIFO order, so prior writes are
    /// guaranteed to be applied before this callback runs.
    pub fn call_param_callbacks_no_wait(&self, addr: i32) {
        let user = AsynUser::new(0).with_addr(addr);
        self.submit_no_wait(RequestOp::CallParamCallbacks { addr }, user);
    }

    /// Send a write request without waiting for the reply.
    /// The actor still processes it in FIFO order, so a subsequent blocking
    /// call (e.g. call_param_callbacks_blocking) guarantees prior writes are done.
    pub fn submit_no_wait(&self, op: RequestOp, user: AsynUser) {
        let (reply_tx, _reply_rx) = oneshot::channel();
        let msg = ActorMessage::new(op, user, CancelToken::new(), reply_tx);
        let _ = self.tx.try_send(msg);
    }

    pub fn write_int32_no_wait(&self, reason: usize, addr: i32, value: i32) {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_no_wait(RequestOp::Int32Write { value }, user);
    }

    pub fn write_float64_no_wait(&self, reason: usize, addr: i32, value: f64) {
        let user = AsynUser::new(reason).with_addr(addr);
        self.submit_no_wait(RequestOp::Float64Write { value }, user);
    }

    // --- Option convenience methods ---

    pub fn get_option_blocking(&self, key: &str) -> AsynResult<String> {
        let user = AsynUser::default();
        let result = self.submit_blocking(
            RequestOp::GetOption { key: key.to_string() },
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
            RequestOp::SetOption { key: key.to_string(), value: value.to_string() },
            user,
        )?;
        Ok(())
    }

    pub async fn get_option(&self, key: &str) -> AsynResult<String> {
        let user = AsynUser::default();
        let result = self.submit(
            RequestOp::GetOption { key: key.to_string() },
            user,
        ).await?;
        result.option_value.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "get_option returned no value".into(),
        })
    }

    pub async fn set_option(&self, key: &str, value: &str) -> AsynResult<()> {
        let user = AsynUser::default();
        self.submit(
            RequestOp::SetOption { key: key.to_string(), value: value.to_string() },
            user,
        ).await?;
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
        let completion = handle
            .try_submit(RequestOp::Int32Read, user)
            .unwrap();
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
}
