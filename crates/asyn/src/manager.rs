use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::watch;

use crate::autoconnect::{spawn_auto_connect, AutoConnectConfig};
use crate::error::{AsynError, AsynResult};
use crate::exception::ExceptionManager;
use crate::interrupt::InterruptManager;
use crate::port::{PortDriver, QueueSubmit};
use crate::port_actor::PortActor;
use crate::port_handle::PortHandle;
use crate::port_worker::{PortWorkerHandle, SynchronousQueue};
use crate::request::{CancelToken, RequestOp};
use crate::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime};
use crate::trace::TraceManager;
use crate::user::AsynUser;

/// Handle for a running auto-connect background task.
struct AutoConnectHandle {
    join_handle: tokio::task::JoinHandle<()>,
    shutdown_tx: watch::Sender<bool>,
}

/// Execution backend for a port: either a threaded worker or synchronous inline execution.
enum PortExecutor {
    /// Port with `can_block=true`: dedicated worker thread.
    Threaded(PortWorkerHandle),
    /// Port with `can_block=false`: requests execute inline on the caller's thread.
    Synchronous { queue: Arc<SynchronousQueue> },
}

impl PortExecutor {
    fn queue_submit(&self) -> Arc<dyn QueueSubmit> {
        match self {
            PortExecutor::Threaded(handle) => handle.queue_submit(),
            PortExecutor::Synchronous { queue } => queue.clone(),
        }
    }

    fn shutdown(&mut self) {
        if let PortExecutor::Threaded(handle) = self {
            handle.shutdown();
        }
    }
}

/// Registry of named port drivers with global exception management
/// and auto-connect task lifecycle control.
pub struct PortManager {
    ports: Mutex<HashMap<String, Arc<Mutex<dyn PortDriver>>>>,
    executors: Mutex<HashMap<String, PortExecutor>>,
    exceptions: Arc<ExceptionManager>,
    trace: Arc<TraceManager>,
    auto_connect_handles: Mutex<HashMap<String, AutoConnectHandle>>,
    /// Actor-based port handles (Phase 2).
    port_handles: Mutex<HashMap<String, PortHandle>>,
    /// Runtime handles (Phase 5).
    runtime_handles: Mutex<HashMap<String, PortRuntimeHandle>>,
}

impl PortManager {
    pub fn new() -> Self {
        Self {
            ports: Mutex::new(HashMap::new()),
            executors: Mutex::new(HashMap::new()),
            exceptions: Arc::new(ExceptionManager::new()),
            trace: Arc::new(TraceManager::new()),
            auto_connect_handles: Mutex::new(HashMap::new()),
            port_handles: Mutex::new(HashMap::new()),
            runtime_handles: Mutex::new(HashMap::new()),
        }
    }

    /// Register a port driver. Injects the exception sink and spawns execution backend.
    ///
    /// If `can_block=true`, spawns a dedicated worker thread.
    /// If `can_block=false`, creates a synchronous queue (inline execution, no thread).
    pub fn register_port<D: PortDriver>(&self, mut driver: D) -> Arc<Mutex<dyn PortDriver>> {
        driver.base_mut().exception_sink = Some(self.exceptions.clone());
        driver.base_mut().trace = Some(self.trace.clone());
        let name = driver.base().port_name.clone();
        let can_block = driver.base().flags.can_block;
        let arc: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(driver));
        self.ports.lock().insert(name.clone(), arc.clone());

        let executor = if can_block {
            let worker = PortWorkerHandle::spawn(arc.clone(), &name);
            let queue_submit = worker.queue_submit();
            arc.lock().base_mut().worker_queue = Some(queue_submit);
            PortExecutor::Threaded(worker)
        } else {
            let queue = Arc::new(SynchronousQueue::new(arc.clone()));
            arc.lock().base_mut().worker_queue = Some(queue.clone());
            PortExecutor::Synchronous { queue }
        };
        self.executors.lock().insert(name, executor);

        arc
    }

    /// Register a port and spawn an auto-connect background task.
    pub fn register_port_with_auto_connect<D: PortDriver>(
        &self,
        driver: D,
        config: AutoConnectConfig,
    ) -> Arc<Mutex<dyn PortDriver>> {
        let port = self.register_port(driver);
        let name = port.lock().base().port_name.clone();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = spawn_auto_connect(port.clone(), config, shutdown_rx);

        self.auto_connect_handles.lock().insert(
            name,
            AutoConnectHandle {
                join_handle: handle,
                shutdown_tx,
            },
        );

        port
    }

    /// Register a port driver using the actor model (Phase 2).
    ///
    /// Takes ownership of the driver (no `Arc<Mutex>`). Spawns an actor thread
    /// that exclusively owns the driver. Returns a cloneable [`PortHandle`].
    pub fn register_port_actor<D: PortDriver>(&self, mut driver: D) -> PortHandle {
        driver.base_mut().exception_sink = Some(self.exceptions.clone());
        driver.base_mut().trace = Some(self.trace.clone());
        let name = driver.base().port_name.clone();

        // Clone the broadcast sender before the driver is moved into the actor.
        // This lets the PortHandle subscribe to the same interrupt stream.
        let broadcast_tx = driver.base().interrupts.broadcast_sender();
        let handle_interrupts = Arc::new(InterruptManager::from_broadcast_sender(broadcast_tx));

        let (tx, rx) = tokio::sync::mpsc::channel(1024);
        let actor = PortActor::new(Box::new(driver), rx);

        let port_name = name.clone();
        std::thread::Builder::new()
            .name(format!("asyn-actor-{port_name}"))
            .spawn(move || actor.run())
            .expect("failed to spawn port actor thread");

        let handle = PortHandle::new(tx, name.clone(), handle_interrupts);
        self.port_handles.lock().insert(name, handle.clone());
        handle
    }

    /// Find a port handle by name (actor model).
    pub fn find_port_handle(&self, name: &str) -> AsynResult<PortHandle> {
        self.port_handles
            .lock()
            .get(name)
            .cloned()
            .ok_or_else(|| AsynError::PortNotFound(name.to_string()))
    }

    /// Register a port driver using the runtime model.
    ///
    /// Takes ownership of the driver. Spawns a runtime thread that exclusively
    /// owns the driver. Returns a [`PortRuntimeHandle`] with shutdown, events,
    /// and client access.
    pub fn register_port_runtime<D: PortDriver>(&self, mut driver: D) -> PortRuntimeHandle {
        self.register_port_runtime_with_config(driver, RuntimeConfig::default())
    }

    /// Register a port driver using the runtime model with custom config.
    pub fn register_port_runtime_with_config<D: PortDriver>(
        &self,
        mut driver: D,
        config: RuntimeConfig,
    ) -> PortRuntimeHandle {
        driver.base_mut().exception_sink = Some(self.exceptions.clone());
        driver.base_mut().trace = Some(self.trace.clone());
        let name = driver.base().port_name.clone();

        let (handle, _jh) = create_port_runtime(driver, config);

        // Also register the port handle for backwards compatibility
        self.port_handles
            .lock()
            .insert(name.clone(), handle.port_handle().clone());
        self.runtime_handles
            .lock()
            .insert(name, handle.clone());

        handle
    }

    /// Find a runtime handle by name.
    pub fn find_port_runtime_handle(&self, name: &str) -> AsynResult<PortRuntimeHandle> {
        self.runtime_handles
            .lock()
            .get(name)
            .cloned()
            .ok_or_else(|| AsynError::PortNotFound(name.to_string()))
    }

    /// Unregister a port. Shuts down its worker thread (if any) and auto-connect task.
    pub fn unregister_port(&self, name: &str) {
        self.ports.lock().remove(name);

        if let Some(mut executor) = self.executors.lock().remove(name) {
            executor.shutdown();
        }

        if let Some(handle) = self.auto_connect_handles.lock().remove(name) {
            let _ = handle.shutdown_tx.send(true);
            handle.join_handle.abort();
        }
    }

    /// Submit a request to a port's execution backend.
    pub fn queue_request(
        &self,
        port_name: &str,
        op: RequestOp,
        user: AsynUser,
    ) -> AsynResult<crate::request::CompletionHandle> {
        let executors = self.executors.lock();
        let executor = executors
            .get(port_name)
            .ok_or_else(|| AsynError::PortNotFound(port_name.to_string()))?;
        Ok(executor.queue_submit().enqueue(op, user))
    }

    /// Submit a cancellable request to a port's execution backend.
    pub fn queue_request_cancellable(
        &self,
        port_name: &str,
        op: RequestOp,
        user: AsynUser,
        cancel: CancelToken,
    ) -> AsynResult<crate::request::CompletionHandle> {
        let executors = self.executors.lock();
        let executor = executors
            .get(port_name)
            .ok_or_else(|| AsynError::PortNotFound(port_name.to_string()))?;
        Ok(executor.queue_submit().enqueue_cancellable(op, user, cancel))
    }

    /// Find a port by name.
    pub fn find_port(&self, name: &str) -> AsynResult<Arc<Mutex<dyn PortDriver>>> {
        self.ports
            .lock()
            .get(name)
            .cloned()
            .ok_or_else(|| AsynError::PortNotFound(name.to_string()))
    }

    /// List all registered port names.
    pub fn port_names(&self) -> Vec<String> {
        self.ports.lock().keys().cloned().collect()
    }

    /// Get a reference to the global exception manager (for registering callbacks).
    pub fn exception_manager(&self) -> &Arc<ExceptionManager> {
        &self.exceptions
    }

    /// Get a reference to the global trace manager.
    pub fn trace_manager(&self) -> &Arc<TraceManager> {
        &self.trace
    }

    /// Wait for a port to become connected, with timeout.
    /// Polls the port state, watching for Connect exceptions.
    pub fn wait_connect(&self, port_name: &str, timeout: Duration) -> AsynResult<()> {
        let port = self.find_port(port_name)?;
        let deadline = Instant::now() + timeout;

        // Fast path: already connected
        if port.lock().base().connected {
            return Ok(());
        }

        // Register an exception callback to get notified
        let (tx, rx) = std::sync::mpsc::channel();
        let name = port_name.to_string();
        let cb_id = self.exceptions.add_callback(move |event| {
            if event.port_name == name
                && event.exception == crate::exception::AsynException::Connect
            {
                let _ = tx.send(());
            }
        });

        let result = loop {
            if port.lock().base().connected {
                break Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break Err(AsynError::Status {
                    status: crate::error::AsynStatus::Timeout,
                    message: format!("wait_connect timed out for port {port_name}"),
                });
            }
            match rx.recv_timeout(remaining) {
                Ok(()) => {
                    if port.lock().base().connected {
                        break Ok(());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    break Err(AsynError::Status {
                        status: crate::error::AsynStatus::Timeout,
                        message: format!("wait_connect timed out for port {port_name}"),
                    });
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(AsynError::Status {
                        status: crate::error::AsynStatus::Error,
                        message: "exception channel closed".into(),
                    });
                }
            }
        };

        self.exceptions.remove_callback(cb_id);
        result
    }

    /// Shutdown a port: disable, announce Shutdown exception, then unregister.
    /// Only works if port has `destructible` flag set.
    pub fn shutdown_port(&self, name: &str) -> AsynResult<()> {
        let port = self.find_port(name)?;
        {
            let mut p = port.lock();
            if !p.base().flags.destructible {
                return Err(AsynError::Status {
                    status: crate::error::AsynStatus::Error,
                    message: format!("port {name} is not destructible"),
                });
            }
            p.base_mut().enabled = false;
            p.base().announce_exception(crate::exception::AsynException::Shutdown, -1);
        }
        self.unregister_port(name);
        Ok(())
    }
}

impl Default for PortManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exception::AsynException;
    use crate::param::ParamType;
    use crate::port::{PortDriverBase, PortFlags};
    use crate::user::AsynUser;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct DummyDriver {
        base: PortDriverBase,
    }

    impl DummyDriver {
        fn new(name: &str) -> Self {
            Self {
                base: PortDriverBase::new(name, 1, PortFlags::default()),
            }
        }
    }

    impl PortDriver for DummyDriver {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }

    #[test]
    fn test_register_and_find() {
        let mgr = PortManager::new();
        mgr.register_port(DummyDriver::new("port1"));
        mgr.register_port(DummyDriver::new("port2"));

        assert!(mgr.find_port("port1").is_ok());
        assert!(mgr.find_port("port2").is_ok());
        assert!(mgr.find_port("nope").is_err());
    }

    #[test]
    fn test_port_names() {
        let mgr = PortManager::new();
        mgr.register_port(DummyDriver::new("alpha"));
        mgr.register_port(DummyDriver::new("beta"));
        let mut names = mgr.port_names();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn test_use_through_manager() {
        let mgr = PortManager::new();
        let port = mgr.register_port(DummyDriver::new("testport"));
        {
            let mut p = port.lock();
            p.base_mut()
                .create_param("VAL", ParamType::Int32)
                .unwrap();
            let mut user = AsynUser::new(0);
            p.write_int32(&mut user, 77).unwrap();
        }
        {
            let found = mgr.find_port("testport").unwrap();
            let mut p = found.lock();
            let user = AsynUser::new(0);
            assert_eq!(p.read_int32(&user).unwrap(), 77);
        }
    }

    #[test]
    fn test_exception_sink_injected() {
        let mgr = PortManager::new();
        let port = mgr.register_port(DummyDriver::new("exctest"));
        let p = port.lock();
        assert!(p.base().exception_sink.is_some());
    }

    #[test]
    fn test_exception_callback_from_port() {
        let mgr = PortManager::new();
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();

        mgr.exception_manager().add_callback(move |event| {
            if event.exception == AsynException::Connect {
                count2.fetch_add(1, Ordering::Relaxed);
            }
        });

        let port = mgr.register_port(DummyDriver::new("excport"));
        {
            let mut p = port.lock();
            let user = AsynUser::default();
            p.disconnect(&user).unwrap();
        }
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_unregister_port() {
        let mgr = PortManager::new();
        mgr.register_port(DummyDriver::new("removeme"));
        assert!(mgr.find_port("removeme").is_ok());
        mgr.unregister_port("removeme");
        assert!(mgr.find_port("removeme").is_err());
    }

    #[test]
    fn test_worker_queue_injected() {
        let mgr = PortManager::new();
        let port = mgr.register_port(DummyDriver::new("wq_test"));
        assert!(port.lock().base().worker_queue.is_some());
    }

    #[test]
    fn test_worker_queue_injected_can_block_false() {
        let mgr = PortManager::new();
        let drv = DummyDriver::new("wq_noblock");
        // can_block is false by default — gets SynchronousQueue (no worker thread)
        assert!(!drv.base.flags.can_block);
        let port = mgr.register_port(drv);
        assert!(port.lock().base().worker_queue.is_some());
    }

    #[test]
    fn test_queue_request_roundtrip() {
        use crate::param::ParamType;
        use crate::request::RequestOp;

        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("qr_test");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port(drv);

        // Write via queue_request
        let user = crate::user::AsynUser::new(0).with_timeout(std::time::Duration::from_secs(1));
        let handle = mgr
            .queue_request("qr_test", RequestOp::Int32Write { value: 99 }, user)
            .unwrap();
        handle.wait(std::time::Duration::from_secs(1)).unwrap();

        // Read back via queue_request
        let user = crate::user::AsynUser::new(0).with_timeout(std::time::Duration::from_secs(1));
        let handle = mgr
            .queue_request("qr_test", RequestOp::Int32Read, user)
            .unwrap();
        let result = handle.wait(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(result.int_val, Some(99));
    }

    #[test]
    fn test_unregister_shuts_down_worker() {
        let mgr = PortManager::new();
        mgr.register_port(DummyDriver::new("unreg_wk"));
        mgr.unregister_port("unreg_wk");
        // Worker thread should be joined cleanly (no hang)
        assert!(mgr.find_port("unreg_wk").is_err());
    }

    #[tokio::test]
    async fn test_register_with_auto_connect() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("autoport");
        drv.base.connected = false;

        let port = mgr.register_port_with_auto_connect(
            drv,
            AutoConnectConfig {
                retry_interval: Duration::from_millis(10),
                enabled: true,
            },
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(port.lock().base().connected);

        mgr.unregister_port("autoport");
    }

    #[test]
    fn test_wait_connect_already_connected() {
        let mgr = PortManager::new();
        mgr.register_port(DummyDriver::new("wc_conn"));
        // Already connected by default
        mgr.wait_connect("wc_conn", Duration::from_millis(100)).unwrap();
    }

    #[test]
    fn test_wait_connect_timeout() {
        let mgr = PortManager::new();
        let port = mgr.register_port(DummyDriver::new("wc_timeout"));
        port.lock().base_mut().connected = false;
        let err = mgr.wait_connect("wc_timeout", Duration::from_millis(50)).unwrap_err();
        match err {
            AsynError::Status { status, .. } => {
                assert_eq!(status, crate::error::AsynStatus::Timeout);
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn test_wait_connect_succeeds_when_connected() {
        let mgr = PortManager::new();
        let port = mgr.register_port(DummyDriver::new("wc_success"));
        port.lock().base_mut().connected = false;

        let port2 = port.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let mut p = port2.lock();
            p.connect(&AsynUser::default()).unwrap();
        });

        mgr.wait_connect("wc_success", Duration::from_secs(1)).unwrap();
        assert!(port.lock().base().connected);
        handle.join().unwrap();
    }

    #[test]
    fn test_wait_connect_port_not_found() {
        let mgr = PortManager::new();
        assert!(mgr.wait_connect("nonexistent", Duration::from_millis(10)).is_err());
    }

    #[test]
    fn test_shutdown_port() {
        let mgr = PortManager::new();
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();

        mgr.exception_manager().add_callback(move |event| {
            if event.exception == crate::exception::AsynException::Shutdown {
                count2.fetch_add(1, Ordering::Relaxed);
            }
        });

        mgr.register_port(DummyDriver::new("shutme"));
        mgr.shutdown_port("shutme").unwrap();
        assert!(mgr.find_port("shutme").is_err());
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    // --- Actor model tests ---

    #[test]
    fn test_register_port_actor_write_read() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("actor_port");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        let handle = mgr.register_port_actor(drv);

        handle.write_int32_blocking(0, 0, 42).unwrap();
        assert_eq!(handle.read_int32_blocking(0, 0).unwrap(), 42);
    }

    #[test]
    fn test_find_port_handle() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("findme");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port_actor(drv);

        let handle = mgr.find_port_handle("findme").unwrap();
        handle.write_int32_blocking(0, 0, 99).unwrap();
        assert_eq!(handle.read_int32_blocking(0, 0).unwrap(), 99);

        assert!(mgr.find_port_handle("nope").is_err());
    }

    #[test]
    fn test_actor_float64() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("f64_actor");
        drv.base.create_param("TEMP", ParamType::Float64).unwrap();
        let handle = mgr.register_port_actor(drv);

        handle.write_float64_blocking(0, 0, 98.6).unwrap();
        assert!((handle.read_float64_blocking(0, 0).unwrap() - 98.6).abs() < 1e-10);
    }

    #[test]
    fn test_shutdown_port_not_destructible() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("nodestr");
        drv.base.flags.destructible = false;
        mgr.register_port(drv);
        let err = mgr.shutdown_port("nodestr").unwrap_err();
        assert!(format!("{err}").contains("not destructible"));
        // Port should still exist
        assert!(mgr.find_port("nodestr").is_ok());
    }

    // --- Phase 1A: can_block=false creates SynchronousQueue (no worker thread) ---

    #[test]
    fn test_can_block_false_inline_execution() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("sync_test");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        assert!(!drv.base.flags.can_block);
        mgr.register_port(drv);

        // Write via queue_request — executes inline
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = mgr
            .queue_request("sync_test", RequestOp::Int32Write { value: 55 }, user)
            .unwrap();
        handle.wait(Duration::from_millis(100)).unwrap();

        // Read back
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = mgr
            .queue_request("sync_test", RequestOp::Int32Read, user)
            .unwrap();
        let result = handle.wait(Duration::from_millis(100)).unwrap();
        assert_eq!(result.int_val, Some(55));
    }

    #[test]
    fn test_can_block_true_creates_worker() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("block_test");
        drv.base.flags.can_block = true;
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port(drv);

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = mgr
            .queue_request("block_test", RequestOp::Int32Write { value: 88 }, user)
            .unwrap();
        handle.wait(Duration::from_secs(1)).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = mgr
            .queue_request("block_test", RequestOp::Int32Read, user)
            .unwrap();
        let result = handle.wait(Duration::from_secs(1)).unwrap();
        assert_eq!(result.int_val, Some(88));
    }

    // --- Phase 1C: cancelRequest at manager level ---

    #[test]
    fn test_queue_request_cancellable() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("cancel_test");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port(drv);

        let cancel = crate::request::CancelToken::new();
        cancel.cancel(); // cancel immediately
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let handle = mgr
            .queue_request_cancellable("cancel_test", RequestOp::Int32Read, user, cancel)
            .unwrap();
        let err = handle.wait(Duration::from_millis(100)).unwrap_err();
        match err {
            AsynError::Status { status, .. } => {
                assert_eq!(status, crate::error::AsynStatus::Error);
            }
            other => panic!("expected Error (cancelled), got {other:?}"),
        }
    }

    // --- Runtime model tests ---

    #[test]
    fn test_register_port_runtime() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("rt_mgr");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        let handle = mgr.register_port_runtime(drv);

        handle.port_handle().write_int32_blocking(0, 0, 55).unwrap();
        assert_eq!(handle.port_handle().read_int32_blocking(0, 0).unwrap(), 55);
    }

    #[test]
    fn test_find_port_runtime_handle() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("rt_find");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port_runtime(drv);

        let handle = mgr.find_port_runtime_handle("rt_find").unwrap();
        handle.port_handle().write_int32_blocking(0, 0, 77).unwrap();
        assert_eq!(handle.port_handle().read_int32_blocking(0, 0).unwrap(), 77);

        assert!(mgr.find_port_runtime_handle("nope").is_err());
    }

    #[test]
    fn test_runtime_also_registers_port_handle() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("rt_compat");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port_runtime(drv);

        // Should be findable via find_port_handle too
        let handle = mgr.find_port_handle("rt_compat").unwrap();
        handle.write_int32_blocking(0, 0, 33).unwrap();
        assert_eq!(handle.read_int32_blocking(0, 0).unwrap(), 33);
    }

    #[test]
    fn test_queue_request_cancellable_port_not_found() {
        let mgr = PortManager::new();
        let cancel = crate::request::CancelToken::new();
        let user = AsynUser::new(0);
        assert!(mgr
            .queue_request_cancellable("nope", RequestOp::Int32Read, user, cancel)
            .is_err());
    }
}
