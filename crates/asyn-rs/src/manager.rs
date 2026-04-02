use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::{AsynError, AsynResult};
use crate::exception::ExceptionManager;
use crate::port::PortDriver;
use crate::port_handle::PortHandle;
use crate::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime};
use crate::trace::TraceManager;

/// Registry of named port drivers with global exception management.
pub struct PortManager {
    exceptions: Arc<ExceptionManager>,
    trace: Arc<TraceManager>,
    /// Actor-based port handles.
    port_handles: Mutex<HashMap<String, PortHandle>>,
    /// Runtime handles.
    runtime_handles: Mutex<HashMap<String, PortRuntimeHandle>>,
}

impl PortManager {
    pub fn new() -> Self {
        Self {
            exceptions: Arc::new(ExceptionManager::new()),
            trace: Arc::new(TraceManager::new()),
            port_handles: Mutex::new(HashMap::new()),
            runtime_handles: Mutex::new(HashMap::new()),
        }
    }

    /// Register a port driver.
    ///
    /// Takes ownership of the driver. Spawns a runtime thread that exclusively
    /// owns the driver. Returns a [`PortRuntimeHandle`] with shutdown, events,
    /// and client access.
    pub fn register_port<D: PortDriver>(&self, driver: D) -> PortRuntimeHandle {
        self.register_port_with_config(driver, RuntimeConfig::default())
    }

    /// Register a port driver with custom runtime config.
    pub fn register_port_with_config<D: PortDriver>(
        &self,
        mut driver: D,
        config: RuntimeConfig,
    ) -> PortRuntimeHandle {
        driver.base_mut().exception_sink = Some(self.exceptions.clone());
        driver.base_mut().trace = Some(self.trace.clone());
        let name = driver.base().port_name.clone();

        let (handle, _jh) = create_port_runtime(driver, config);

        self.port_handles
            .lock()
            .insert(name.clone(), handle.port_handle().clone());
        self.runtime_handles.lock().insert(name, handle.clone());

        handle
    }

    /// Find a port handle by name.
    pub fn find_port_handle(&self, name: &str) -> AsynResult<PortHandle> {
        self.port_handles
            .lock()
            .get(name)
            .cloned()
            .ok_or_else(|| AsynError::PortNotFound(name.to_string()))
    }

    /// Find a runtime handle by name.
    pub fn find_runtime_handle(&self, name: &str) -> AsynResult<PortRuntimeHandle> {
        self.runtime_handles
            .lock()
            .get(name)
            .cloned()
            .ok_or_else(|| AsynError::PortNotFound(name.to_string()))
    }

    /// Unregister a port. Shuts down its runtime.
    pub fn unregister_port(&self, name: &str) {
        self.port_handles.lock().remove(name);

        if let Some(runtime_handle) = self.runtime_handles.lock().remove(name) {
            runtime_handle.shutdown();
        }
    }

    /// Get a reference to the global exception manager (for registering callbacks).
    pub fn exception_manager(&self) -> &Arc<ExceptionManager> {
        &self.exceptions
    }

    /// Get a reference to the global trace manager.
    pub fn trace_manager(&self) -> &Arc<TraceManager> {
        &self.trace
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
    use crate::param::ParamType;
    use crate::port::{PortDriverBase, PortFlags};
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        let mut drv = DummyDriver::new("port1");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port(drv);

        assert!(mgr.find_port_handle("port1").is_ok());
        assert!(mgr.find_port_handle("nope").is_err());
    }

    #[test]
    fn test_register_and_use() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("testport");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        let handle = mgr.register_port(drv);

        handle.port_handle().write_int32_blocking(0, 0, 42).unwrap();
        assert_eq!(handle.port_handle().read_int32_blocking(0, 0).unwrap(), 42);
    }

    #[test]
    fn test_find_port_handle() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("findme");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port(drv);

        let handle = mgr.find_port_handle("findme").unwrap();
        handle.write_int32_blocking(0, 0, 99).unwrap();
        assert_eq!(handle.read_int32_blocking(0, 0).unwrap(), 99);

        assert!(mgr.find_port_handle("nope").is_err());
    }

    #[test]
    fn test_find_runtime_handle() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("rt_find");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port(drv);

        let handle = mgr.find_runtime_handle("rt_find").unwrap();
        handle.port_handle().write_int32_blocking(0, 0, 77).unwrap();
        assert_eq!(handle.port_handle().read_int32_blocking(0, 0).unwrap(), 77);

        assert!(mgr.find_runtime_handle("nope").is_err());
    }

    #[test]
    fn test_exception_sink_injected() {
        let mgr = PortManager::new();
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();

        mgr.exception_manager().add_callback(move |_event| {
            count2.fetch_add(1, Ordering::Relaxed);
        });

        let mut drv = DummyDriver::new("exctest");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port(drv);

        // The runtime sends a Started event but not via the exception manager.
        // Exception manager is injected for driver-level exceptions.
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_unregister_port() {
        let mgr = PortManager::new();
        mgr.register_port(DummyDriver::new("removeme"));
        assert!(mgr.find_port_handle("removeme").is_ok());
        mgr.unregister_port("removeme");
        assert!(mgr.find_port_handle("removeme").is_err());
    }

    #[test]
    fn test_float64() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("f64_port");
        drv.base.create_param("TEMP", ParamType::Float64).unwrap();
        let handle = mgr.register_port(drv);

        handle
            .port_handle()
            .write_float64_blocking(0, 0, 98.6)
            .unwrap();
        assert!((handle.port_handle().read_float64_blocking(0, 0).unwrap() - 98.6).abs() < 1e-10);
    }

    #[test]
    fn test_shutdown_via_handle() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("shutme");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        let handle = mgr.register_port(drv);

        handle.port_handle().write_int32_blocking(0, 0, 42).unwrap();
        handle.shutdown_and_wait();

        // After shutdown, operations should fail
        assert!(handle.port_handle().write_int32_blocking(0, 0, 1).is_err());
    }
}
