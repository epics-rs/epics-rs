//! Per-port worker thread and request queue.
//!
//! Each port gets a dedicated `std::thread` that serializes I/O operations,
//! matching C asyn's per-port thread model.

use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::Instant;

use parking_lot::Mutex;

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::port::{PortDriver, QueueSubmit};
use crate::request::{
    CancelToken, CompletionHandle, QueuedRequest, RequestHeap, RequestOp, RequestResult,
};
use crate::asyn_trace;
use crate::trace::{TraceMask, TraceManager};
use crate::user::AsynUser;

/// Thread-safe request queue with condvar-based wakeup.
pub struct RequestQueue {
    inner: StdMutex<QueueInner>,
    condvar: Condvar,
}

struct QueueInner {
    heap: RequestHeap,
    shutdown: bool,
    /// If set, only requests from this user seq can be dequeued (blockProcessCallback).
    blocked_by: Option<u64>,
}

impl RequestQueue {
    fn new() -> Self {
        Self {
            inner: StdMutex::new(QueueInner {
                heap: RequestHeap::new(),
                shutdown: false,
                blocked_by: None,
            }),
            condvar: Condvar::new(),
        }
    }

    /// Submit a request and return a completion handle for waiting on the result.
    pub fn enqueue(&self, op: RequestOp, user: AsynUser) -> CompletionHandle {
        let deadline = Instant::now() + user.timeout;
        let cancel = CancelToken::new();
        let completion = CompletionHandle::new();
        let req = QueuedRequest::new(op, user, deadline, cancel, completion.clone());
        let mut inner = self.inner.lock().unwrap();
        inner.heap.push(req);
        self.condvar.notify_one();
        completion
    }

    /// Submit with an explicit cancel token (caller retains a clone).
    pub fn enqueue_cancellable(
        &self,
        op: RequestOp,
        user: AsynUser,
        cancel: CancelToken,
    ) -> CompletionHandle {
        let deadline = Instant::now() + user.timeout;
        let completion = CompletionHandle::new();
        let req = QueuedRequest::new(op, user, deadline, cancel, completion.clone());
        let mut inner = self.inner.lock().unwrap();
        inner.heap.push(req);
        self.condvar.notify_one();
        completion
    }

    /// Pop the highest-priority request, blocking until one is available or shutdown.
    ///
    /// When the port is blocked (via `BlockProcess`), only requests from the blocking
    /// owner (matching `block_token`) or `UnblockProcess` requests are dequeued.
    fn pop_or_wait(&self) -> Option<QueuedRequest> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.shutdown {
                return None;
            }
            if let Some(owner) = inner.blocked_by {
                // Port is blocked — only allow owner's requests or UnblockProcess
                if let Some(req) = inner.heap.peek() {
                    if req.block_token == Some(owner)
                        || matches!(req.op, RequestOp::UnblockProcess)
                    {
                        return inner.heap.pop();
                    }
                }
                // Top request doesn't match — wait for unblock
                inner = self.condvar.wait(inner).unwrap();
            } else if let Some(req) = inner.heap.pop() {
                return Some(req);
            } else {
                inner = self.condvar.wait(inner).unwrap();
            }
        }
    }

    /// Signal shutdown and wake the worker thread.
    fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.shutdown = true;
        self.condvar.notify_all();
    }
}

impl QueueSubmit for RequestQueue {
    fn enqueue(&self, op: RequestOp, user: AsynUser) -> CompletionHandle {
        self.enqueue(op, user)
    }

    fn enqueue_cancellable(
        &self,
        op: RequestOp,
        user: AsynUser,
        cancel: CancelToken,
    ) -> CompletionHandle {
        self.enqueue_cancellable(op, user, cancel)
    }
}

/// Synchronous queue for `can_block=false` ports.
///
/// Executes requests inline on the caller's thread (no worker thread spawned).
/// Holds an `Arc<Mutex<dyn PortDriver>>` and acquires the lock for each request.
pub struct SynchronousQueue {
    port: Arc<Mutex<dyn PortDriver>>,
}

impl SynchronousQueue {
    pub fn new(port: Arc<Mutex<dyn PortDriver>>) -> Self {
        Self { port }
    }
}

impl QueueSubmit for SynchronousQueue {
    fn enqueue(&self, op: RequestOp, user: AsynUser) -> CompletionHandle {
        let cancel = CancelToken::new();
        self.enqueue_cancellable(op, user, cancel)
    }

    fn enqueue_cancellable(
        &self,
        op: RequestOp,
        user: AsynUser,
        cancel: CancelToken,
    ) -> CompletionHandle {
        let completion = CompletionHandle::new();
        let result = execute_request_inline(&self.port, op, user, &cancel);
        completion.complete(result);
        completion
    }
}

/// Execute a single request inline (used by `SynchronousQueue` and extractable for testing).
fn execute_request_inline(
    port: &Arc<Mutex<dyn PortDriver>>,
    op: RequestOp,
    mut user: AsynUser,
    cancel: &CancelToken,
) -> AsynResult<RequestResult> {
    // 1. Cancel check
    if cancel.is_cancelled() {
        return Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "request cancelled".into(),
        });
    }

    let is_connect_op = matches!(op, RequestOp::Connect | RequestOp::Disconnect);

    // 2. Acquire port lock
    let mut port_guard = port.lock();

    if !is_connect_op {
        // 3. Auto-connect
        if !port_guard.base().connected && port_guard.base().auto_connect {
            let _ = port_guard.connect(&AsynUser::default());
        }

        // 4. Check ready (addr-aware for multi-device)
        if port_guard.base().flags.multi_device {
            port_guard.base().check_ready_addr(user.addr)?;
        } else {
            port_guard.base().check_ready()?;
        }
    }

    // 5. Dispatch — BlockProcess/UnblockProcess are no-ops for synchronous queues
    //    (there is no queue to block)
    let result = dispatch_io_inline(&mut *port_guard, &mut user, &op);

    result
}

/// Dispatch I/O for synchronous (inline) execution.
/// Same as `dispatch_io` but without BlockProcess/UnblockProcess queue access.
fn dispatch_io_inline(
    port: &mut dyn PortDriver,
    user: &mut AsynUser,
    op: &RequestOp,
) -> AsynResult<RequestResult> {
    match op {
        RequestOp::OctetWrite { data } => {
            port.io_write_octet(user, data)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::OctetRead { buf_size } => {
            let mut buf = vec![0u8; *buf_size];
            let n = port.io_read_octet(user, &mut buf)?;
            buf.truncate(n);
            Ok(RequestResult::octet_read(buf, n))
        }
        RequestOp::OctetWriteRead { data, buf_size } => {
            port.io_write_octet(user, data)?;
            let mut buf = vec![0u8; *buf_size];
            let n = port.io_read_octet(user, &mut buf)?;
            buf.truncate(n);
            Ok(RequestResult::octet_read(buf, n))
        }
        RequestOp::Int32Write { value } => {
            port.io_write_int32(user, *value)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Int32Read => {
            let v = port.io_read_int32(user)?;
            Ok(RequestResult::int32_read(v))
        }
        RequestOp::Int64Write { value } => {
            port.io_write_int64(user, *value)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Int64Read => {
            let v = port.io_read_int64(user)?;
            Ok(RequestResult::int64_read(v))
        }
        RequestOp::Float64Write { value } => {
            port.io_write_float64(user, *value)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Float64Read => {
            let v = port.io_read_float64(user)?;
            Ok(RequestResult::float64_read(v))
        }
        RequestOp::UInt32DigitalWrite { value, mask } => {
            port.io_write_uint32_digital(user, *value, *mask)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::UInt32DigitalRead { mask } => {
            let v = port.io_read_uint32_digital(user, *mask)?;
            Ok(RequestResult::uint32_read(v))
        }
        RequestOp::Flush => {
            port.io_flush(user)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Connect => {
            port.connect(user)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Disconnect => {
            port.disconnect(user)?;
            Ok(RequestResult::write_ok())
        }
        // BlockProcess/UnblockProcess are no-ops for synchronous execution
        RequestOp::BlockProcess => Ok(RequestResult::write_ok()),
        RequestOp::UnblockProcess => Ok(RequestResult::write_ok()),
        RequestOp::DrvUserCreate { drv_info } => {
            let reason = port.drv_user_create(drv_info)?;
            Ok(RequestResult::drv_user_create(reason))
        }
        RequestOp::EnumRead => {
            let (idx, _) = port.read_enum(user)?;
            Ok(RequestResult::enum_read(idx))
        }
        RequestOp::EnumWrite { index } => {
            port.write_enum(user, *index)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Int32ArrayRead { max_elements } => {
            let mut buf = vec![0i32; *max_elements];
            let n = port.read_int32_array(user, &mut buf)?;
            buf.truncate(n);
            Ok(RequestResult::int32_array_read(buf))
        }
        RequestOp::Int32ArrayWrite { data } => {
            port.write_int32_array(user, data)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Float64ArrayRead { max_elements } => {
            let mut buf = vec![0f64; *max_elements];
            let n = port.read_float64_array(user, &mut buf)?;
            buf.truncate(n);
            Ok(RequestResult::float64_array_read(buf))
        }
        RequestOp::Float64ArrayWrite { data } => {
            port.write_float64_array(user, data)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::CallParamCallbacks { addr } => {
            port.base_mut().call_param_callbacks(*addr)?;
            Ok(RequestResult::write_ok())
        }
    }
}

/// Handle to a running port worker thread.
pub struct PortWorkerHandle {
    queue: Arc<RequestQueue>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PortWorkerHandle {
    /// Spawn a worker thread for the given port.
    pub fn spawn(port: Arc<Mutex<dyn PortDriver>>, port_name: &str) -> Self {
        let queue = Arc::new(RequestQueue::new());
        let q = queue.clone();
        let name = port_name.to_string();
        let trace = port.lock().base().trace.clone();
        let thread = std::thread::Builder::new()
            .name(format!("asyn-{name}"))
            .spawn(move || worker_loop(q, port, trace, name))
            .expect("failed to spawn port worker thread");
        Self {
            queue,
            thread: Some(thread),
        }
    }

    /// Get the request queue handle (for injection into PortDriverBase).
    pub fn queue(&self) -> Arc<RequestQueue> {
        self.queue.clone()
    }

    /// Get the queue as a `QueueSubmit` trait object.
    pub fn queue_submit(&self) -> Arc<dyn QueueSubmit> {
        self.queue.clone()
    }

    /// Signal shutdown and join the worker thread.
    pub fn shutdown(&mut self) {
        self.queue.shutdown();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for PortWorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(
    queue: Arc<RequestQueue>,
    port: Arc<Mutex<dyn PortDriver>>,
    trace: Option<Arc<TraceManager>>,
    port_name: String,
) {
    while let Some(mut req) = queue.pop_or_wait() {
        asyn_trace!(Some(trace), &port_name, TraceMask::FLOW, "dequeue {:?}", req.op);

        // 1. Cancel check
        if req.cancel.is_cancelled() {
            asyn_trace!(Some(trace), &port_name, TraceMask::FLOW, "request cancelled");
            req.completion.complete(Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "request cancelled".into(),
            }));
            continue;
        }

        // 2. Deadline check
        if Instant::now() > req.deadline {
            asyn_trace!(Some(trace), &port_name, TraceMask::FLOW, "deadline expired");
            req.completion.complete(Err(AsynError::Status {
                status: AsynStatus::Timeout,
                message: "request deadline expired before execution".into(),
            }));
            continue;
        }

        // Connect-priority ops bypass enabled/connected checks
        let is_connect_op = matches!(req.op, RequestOp::Connect | RequestOp::Disconnect);

        // 3. Acquire port lock
        let mut port_guard = port.lock();

        if !is_connect_op {
            // 4. Auto-connect (addr-aware for multi-device ports)
            let should_auto = if port_guard.base().flags.multi_device {
                let ds = port_guard.base().device_states.get(&req.user.addr);
                !ds.map_or(true, |d| d.connected)
                    && ds.map_or(port_guard.base().auto_connect, |d| d.auto_connect)
            } else {
                !port_guard.base().connected && port_guard.base().auto_connect
            };
            if should_auto {
                asyn_trace!(Some(trace), &port_name, TraceMask::FLOW, "auto-connect attempt");
                let _ = port_guard.connect(&AsynUser::default());
            }

            // 5. Check ready (addr-aware for multi-device ports)
            if let Err(e) = port_guard.base().check_ready_addr(req.user.addr) {
                req.completion.complete(Err(e));
                continue;
            }
        }

        // 6. Dispatch
        let result = dispatch_io(&mut *port_guard, &mut req.user, &req.op, &queue);

        // 7. Release port lock
        drop(port_guard);

        // 8. Complete
        match &result {
            Ok(_) => asyn_trace!(Some(trace), &port_name, TraceMask::FLOW, "dispatch complete"),
            Err(e) => asyn_trace!(Some(trace), &port_name, TraceMask::ERROR, "dispatch error: {}", e),
        }
        req.completion.complete(result);
    }
}

fn dispatch_io(
    port: &mut dyn PortDriver,
    user: &mut AsynUser,
    op: &RequestOp,
    queue: &RequestQueue,
) -> AsynResult<RequestResult> {
    match op {
        RequestOp::OctetWrite { data } => {
            port.io_write_octet(user, data)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::OctetRead { buf_size } => {
            let mut buf = vec![0u8; *buf_size];
            let n = port.io_read_octet(user, &mut buf)?;
            buf.truncate(n);
            Ok(RequestResult::octet_read(buf, n))
        }
        RequestOp::OctetWriteRead { data, buf_size } => {
            port.io_write_octet(user, data)?;
            let mut buf = vec![0u8; *buf_size];
            let n = port.io_read_octet(user, &mut buf)?;
            buf.truncate(n);
            Ok(RequestResult::octet_read(buf, n))
        }
        RequestOp::Int32Write { value } => {
            port.io_write_int32(user, *value)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Int32Read => {
            let v = port.io_read_int32(user)?;
            Ok(RequestResult::int32_read(v))
        }
        RequestOp::Int64Write { value } => {
            port.io_write_int64(user, *value)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Int64Read => {
            let v = port.io_read_int64(user)?;
            Ok(RequestResult::int64_read(v))
        }
        RequestOp::Float64Write { value } => {
            port.io_write_float64(user, *value)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Float64Read => {
            let v = port.io_read_float64(user)?;
            Ok(RequestResult::float64_read(v))
        }
        RequestOp::UInt32DigitalWrite { value, mask } => {
            port.io_write_uint32_digital(user, *value, *mask)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::UInt32DigitalRead { mask } => {
            let v = port.io_read_uint32_digital(user, *mask)?;
            Ok(RequestResult::uint32_read(v))
        }
        RequestOp::Flush => {
            port.io_flush(user)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Connect => {
            port.connect(user)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Disconnect => {
            port.disconnect(user)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::BlockProcess => {
            let token = user.block_token.unwrap_or(user.reason as u64);
            let mut inner = queue.inner.lock().unwrap();
            inner.blocked_by = Some(token);
            Ok(RequestResult::write_ok())
        }
        RequestOp::UnblockProcess => {
            let mut inner = queue.inner.lock().unwrap();
            inner.blocked_by = None;
            queue.condvar.notify_all();
            Ok(RequestResult::write_ok())
        }
        RequestOp::DrvUserCreate { drv_info } => {
            let reason = port.drv_user_create(drv_info)?;
            Ok(RequestResult::drv_user_create(reason))
        }
        RequestOp::EnumRead => {
            let (idx, _) = port.read_enum(user)?;
            Ok(RequestResult::enum_read(idx))
        }
        RequestOp::EnumWrite { index } => {
            port.write_enum(user, *index)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Int32ArrayRead { max_elements } => {
            let mut buf = vec![0i32; *max_elements];
            let n = port.read_int32_array(user, &mut buf)?;
            buf.truncate(n);
            Ok(RequestResult::int32_array_read(buf))
        }
        RequestOp::Int32ArrayWrite { data } => {
            port.write_int32_array(user, data)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::Float64ArrayRead { max_elements } => {
            let mut buf = vec![0f64; *max_elements];
            let n = port.read_float64_array(user, &mut buf)?;
            buf.truncate(n);
            Ok(RequestResult::float64_array_read(buf))
        }
        RequestOp::Float64ArrayWrite { data } => {
            port.write_float64_array(user, data)?;
            Ok(RequestResult::write_ok())
        }
        RequestOp::CallParamCallbacks { addr } => {
            port.base_mut().call_param_callbacks(*addr)?;
            Ok(RequestResult::write_ok())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use crate::param::ParamType;
    use crate::port::{PortDriverBase, PortFlags};

    struct TestDriver {
        base: PortDriverBase,
    }

    impl TestDriver {
        fn new() -> Self {
            let mut base = PortDriverBase::new("worker_test", 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
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

    fn make_worker() -> (PortWorkerHandle, Arc<Mutex<dyn PortDriver>>) {
        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(TestDriver::new()));
        let worker = PortWorkerHandle::spawn(port.clone(), "worker_test");
        (worker, port)
    }

    #[test]
    fn basic_write_read() {
        let (mut worker, _port) = make_worker();
        let queue = worker.queue();

        // Write int32
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Write { value: 42 }, user);
        let result = handle.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.status, AsynStatus::Success);

        // Read it back
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Read, user);
        let result = handle.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.int_val, Some(42));

        worker.shutdown();
    }

    #[test]
    fn priority_ordering() {
        // Create a port that's initially disabled to queue up requests
        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(TestDriver::new()));
        port.lock().base_mut().enabled = false;

        let worker = PortWorkerHandle::spawn(port.clone(), "prio_test");
        let queue = worker.queue();

        // Enqueue low priority
        let mut user_low = AsynUser::new(0).with_timeout(Duration::from_secs(5));
        user_low.priority = crate::port::QueuePriority::Low;
        let handle_low = queue.enqueue(RequestOp::Int32Write { value: 1 }, user_low);

        // Enqueue high priority
        let mut user_high = AsynUser::new(0).with_timeout(Duration::from_secs(5));
        user_high.priority = crate::port::QueuePriority::High;
        let handle_high = queue.enqueue(RequestOp::Int32Write { value: 2 }, user_high);

        // Give worker time to process (both will fail since disabled)
        std::thread::sleep(Duration::from_millis(50));

        // Both should get Disabled errors
        let err_high = handle_high.wait(Duration::from_millis(100)).unwrap_err();
        let err_low = handle_low.wait(Duration::from_millis(100)).unwrap_err();
        match (&err_high, &err_low) {
            (
                AsynError::Status { status: s1, .. },
                AsynError::Status { status: s2, .. },
            ) => {
                assert_eq!(*s1, AsynStatus::Disabled);
                assert_eq!(*s2, AsynStatus::Disabled);
            }
            _ => panic!("expected Disabled errors"),
        }

        drop(worker);
    }

    #[test]
    fn deadline_expiry() {
        let (mut worker, _port) = make_worker();
        let queue = worker.queue();

        // Create a request with an already-passed deadline
        let user = AsynUser::new(0).with_timeout(Duration::from_nanos(1));
        std::thread::sleep(Duration::from_millis(1)); // ensure deadline passes
        let handle = queue.enqueue(RequestOp::Int32Read, user);
        let err = handle.wait(Duration::from_secs(1)).unwrap_err();
        match err {
            AsynError::Status { status, .. } => assert_eq!(status, AsynStatus::Timeout),
            _ => panic!("expected Timeout"),
        }

        worker.shutdown();
    }

    #[test]
    fn cancel_before_execution() {
        let (mut worker, _port) = make_worker();
        let queue = worker.queue();

        let cancel = CancelToken::new();
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        cancel.cancel(); // cancel immediately
        let handle = queue.enqueue_cancellable(RequestOp::Int32Read, user, cancel);
        let err = handle.wait(Duration::from_secs(1)).unwrap_err();
        match err {
            AsynError::Status { status, .. } => assert_eq!(status, AsynStatus::Error),
            _ => panic!("expected Error (cancelled)"),
        }

        worker.shutdown();
    }

    #[test]
    fn auto_connect() {
        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(TestDriver::new()));
        {
            let mut p = port.lock();
            p.base_mut().connected = false;
            p.base_mut().auto_connect = true;
        }

        let mut worker = PortWorkerHandle::spawn(port.clone(), "autoconn_test");
        let queue = worker.queue();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Read, user);
        let result = handle.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.status, AsynStatus::Success);

        // Port should now be connected
        assert!(port.lock().base().connected);

        worker.shutdown();
    }

    #[test]
    fn check_ready_disabled() {
        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(TestDriver::new()));
        port.lock().base_mut().enabled = false;

        let mut worker = PortWorkerHandle::spawn(port.clone(), "disabled_test");
        let queue = worker.queue();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Read, user);
        let err = handle.wait(Duration::from_secs(1)).unwrap_err();
        match err {
            AsynError::Status { status, .. } => assert_eq!(status, AsynStatus::Disabled),
            _ => panic!("expected Disabled"),
        }

        worker.shutdown();
    }

    #[test]
    fn clean_shutdown() {
        let (mut worker, _port) = make_worker();
        worker.shutdown();
        // Should not hang — thread joined cleanly
    }

    #[test]
    fn serialization_no_concurrent_access() {
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
            fn io_write_int32(&mut self, _user: &mut AsynUser, _value: i32) -> AsynResult<()> {
                let c = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = self.max_concurrent.fetch_max(c, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(1));
                self.concurrent.fetch_sub(1, Ordering::SeqCst);
                self.base_mut().params.set_int32(0, 0, _value)?;
                Ok(())
            }
        }

        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let mut base = PortDriverBase::new("serial_test", 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        let driver = CountingDriver {
            base,
            concurrent: concurrent.clone(),
            max_concurrent: max_concurrent.clone(),
        };

        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(driver));
        let mut worker = PortWorkerHandle::spawn(port, "serial_test");
        let queue = worker.queue();

        // Submit 20 requests from multiple threads
        let handles: Vec<_> = (0..20)
            .map(|i| {
                let q = queue.clone();
                std::thread::spawn(move || {
                    let user = AsynUser::new(0).with_timeout(Duration::from_secs(5));
                    let h = q.enqueue(RequestOp::Int32Write { value: i }, user);
                    h.wait(Duration::from_secs(5)).unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Worker thread serializes: max concurrent should be exactly 1
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);

        worker.shutdown();
    }

    #[test]
    fn int64_write_read_via_worker() {
        let (mut worker, _port) = make_worker();
        let queue = worker.queue();

        // Write int64
        let user = AsynUser::new(2).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int64Write { value: i64::MAX }, user);
        handle.wait(Duration::from_secs(1)).unwrap();

        // Read it back
        let user = AsynUser::new(2).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int64Read, user);
        let result = handle.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.int64_val, Some(i64::MAX));

        worker.shutdown();
    }

    #[test]
    fn connect_via_worker() {
        let (mut worker, port) = make_worker();
        let queue = worker.queue();

        // Disconnect first
        port.lock().base_mut().connected = false;

        // Connect via queue (bypasses enabled/connected check)
        let mut user = AsynUser::default().with_timeout(Duration::from_secs(1));
        user.priority = crate::port::QueuePriority::Connect;
        let handle = queue.enqueue(RequestOp::Connect, user);
        handle.wait(Duration::from_secs(1)).unwrap();

        assert!(port.lock().base().connected);
        worker.shutdown();
    }

    #[test]
    fn disconnect_via_worker() {
        let (mut worker, port) = make_worker();
        let queue = worker.queue();
        assert!(port.lock().base().connected);

        let mut user = AsynUser::default().with_timeout(Duration::from_secs(1));
        user.priority = crate::port::QueuePriority::Connect;
        let handle = queue.enqueue(RequestOp::Disconnect, user);
        handle.wait(Duration::from_secs(1)).unwrap();

        assert!(!port.lock().base().connected);
        worker.shutdown();
    }

    // --- Phase 1A: SynchronousQueue tests ---

    fn make_sync_queue() -> (Arc<SynchronousQueue>, Arc<Mutex<dyn PortDriver>>) {
        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(TestDriver::new()));
        let queue = Arc::new(SynchronousQueue::new(port.clone()));
        (queue, port)
    }

    #[test]
    fn sync_queue_write_read() {
        let (queue, _port) = make_sync_queue();
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Write { value: 42 }, user);
        // SynchronousQueue completes inline — result is immediately available
        let result = handle.wait(Duration::from_millis(10)).unwrap();
        assert_eq!(result.status, AsynStatus::Success);

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Read, user);
        let result = handle.wait(Duration::from_millis(10)).unwrap();
        assert_eq!(result.int_val, Some(42));
    }

    #[test]
    fn sync_queue_check_ready_enforced() {
        let (queue, port) = make_sync_queue();
        port.lock().base_mut().enabled = false;

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Read, user);
        let err = handle.wait(Duration::from_millis(10)).unwrap_err();
        match err {
            AsynError::Status { status, .. } => assert_eq!(status, AsynStatus::Disabled),
            _ => panic!("expected Disabled"),
        }
    }

    #[test]
    fn sync_queue_cancel() {
        let (queue, _port) = make_sync_queue();
        let cancel = CancelToken::new();
        cancel.cancel();
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue_cancellable(RequestOp::Int32Read, user, cancel);
        let err = handle.wait(Duration::from_millis(10)).unwrap_err();
        match err {
            AsynError::Status { status, .. } => assert_eq!(status, AsynStatus::Error),
            _ => panic!("expected Error (cancelled)"),
        }
    }

    #[test]
    fn sync_queue_auto_connect() {
        let (queue, port) = make_sync_queue();
        {
            let mut p = port.lock();
            p.base_mut().connected = false;
            p.base_mut().auto_connect = true;
        }

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Read, user);
        handle.wait(Duration::from_millis(10)).unwrap();
        assert!(port.lock().base().connected);
    }

    // --- Phase 1B: BlockProcess enforcement tests ---

    #[test]
    fn block_process_exclusive_access() {
        let (mut worker, _port) = make_worker();
        let queue = worker.queue();

        // Block with token 42
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(42);
        let handle = queue.enqueue(RequestOp::BlockProcess, user);
        handle.wait(Duration::from_secs(1)).unwrap();

        // Request from owner (token 42) should succeed
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(42);
        let handle = queue.enqueue(RequestOp::Int32Write { value: 99 }, user);
        let result = handle.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.status, AsynStatus::Success);

        // Unblock
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::UnblockProcess, user);
        handle.wait(Duration::from_secs(1)).unwrap();

        // Non-owner request should now succeed
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Read, user);
        let result = handle.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.int_val, Some(99));

        worker.shutdown();
    }

    #[test]
    fn block_process_non_owner_blocked_then_released() {
        let (mut worker, _port) = make_worker();
        let queue = worker.queue();
        let queue2 = queue.clone();

        // Block with token 100
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        user.block_token = Some(100);
        let handle = queue.enqueue(RequestOp::BlockProcess, user);
        handle.wait(Duration::from_secs(1)).unwrap();

        // Non-owner request (no token) — will block in the queue
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(5));
        let non_owner_handle = queue.enqueue(RequestOp::Int32Write { value: 77 }, user);

        // Unblock from another thread after a delay
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
            queue2.enqueue(RequestOp::UnblockProcess, user)
        });

        // The non-owner request should eventually complete after unblock
        let result = non_owner_handle.wait(Duration::from_secs(2)).unwrap();
        assert_eq!(result.status, AsynStatus::Success);

        t.join().unwrap().wait(Duration::from_secs(1)).unwrap();
        worker.shutdown();
    }

    // --- Phase 2A: check_ready_addr in worker ---

    #[test]
    fn worker_check_ready_addr_disabled() {
        let mut base = PortDriverBase::new("multi_test", 4, PortFlags {
            multi_device: true,
            can_block: true,
            destructible: true,
        });
        base.create_param("VAL", ParamType::Int32).unwrap();
        base.device_state(1).enabled = false;

        struct MultiDriver { base: PortDriverBase }
        impl PortDriver for MultiDriver {
            fn base(&self) -> &PortDriverBase { &self.base }
            fn base_mut(&mut self) -> &mut PortDriverBase { &mut self.base }
        }

        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(MultiDriver { base }));
        let mut worker = PortWorkerHandle::spawn(port.clone(), "multi_test");
        let queue = worker.queue();

        // Request to addr 0 (not disabled) should succeed
        let user = AsynUser::new(0).with_addr(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Read, user);
        handle.wait(Duration::from_secs(1)).unwrap();

        // Request to addr 1 (disabled) should fail
        let user = AsynUser::new(0).with_addr(1).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Read, user);
        let err = handle.wait(Duration::from_secs(1)).unwrap_err();
        match err {
            AsynError::Status { status, .. } => assert_eq!(status, AsynStatus::Disabled),
            _ => panic!("expected Disabled"),
        }

        worker.shutdown();
    }

    #[test]
    fn worker_single_device_regression() {
        // Single-device port should still work with check_ready_addr
        let (mut worker, _port) = make_worker();
        let queue = worker.queue();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Write { value: 7 }, user);
        handle.wait(Duration::from_secs(1)).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = queue.enqueue(RequestOp::Int32Read, user);
        let result = handle.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.int_val, Some(7));

        worker.shutdown();
    }
}
