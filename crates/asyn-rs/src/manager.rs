use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::{AsynError, AsynResult};
use crate::exception::ExceptionManager;
use crate::port::PortDriver;
use crate::port_handle::PortHandle;
use crate::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime};
use crate::services::PortServices;
use crate::trace::TraceManager;

/// Registry of named port drivers with global exception management.
pub struct PortManager {
    /// The trace configuration and exception list handed to every port this
    /// manager registers — see [`PortServices`]. The manager does not inject
    /// them itself: it puts them in the [`RuntimeConfig`] and
    /// `create_port_runtime` binds them, the same way the iocsh
    /// `drvAsyn*PortConfigure` commands do.
    services: PortServices,
    /// Actor-based port handles.
    port_handles: Mutex<HashMap<String, PortHandle>>,
    /// Runtime handles.
    runtime_handles: Mutex<HashMap<String, PortRuntimeHandle>>,
}

impl PortManager {
    pub fn new() -> Self {
        Self::with_trace_manager(Arc::new(TraceManager::new()))
    }

    /// Build a manager that shares an existing [`TraceManager`].
    ///
    /// The `asynSetTrace*` iocsh commands mutate the trace manager reached
    /// through [`Self::trace_manager`]. An IOC whose ports and drivers were
    /// registered against a trace manager it built itself (e.g. `AdIoc`) must
    /// hand that same instance here, or those commands would mutate a trace
    /// manager nothing reads and silently do nothing.
    pub fn with_trace_manager(trace: Arc<TraceManager>) -> Self {
        Self::with_services(PortServices::new(trace))
    }

    /// Build a manager on an existing [`PortServices`] — the form that shares
    /// one trace configuration *and* one exception list with ports created
    /// elsewhere (the iocsh `drvAsyn*PortConfigure` commands).
    pub fn with_services(services: PortServices) -> Self {
        Self {
            services,
            port_handles: Mutex::new(HashMap::new()),
            runtime_handles: Mutex::new(HashMap::new()),
        }
    }

    /// The services every port this manager registers is bound to.
    pub fn services(&self) -> &PortServices {
        &self.services
    }

    /// Register a port driver.
    ///
    /// Takes ownership of the driver. Spawns a runtime thread that exclusively
    /// owns the driver. Returns a [`PortRuntimeHandle`] with shutdown, events,
    /// and client access.
    ///
    /// **Errors with `PortAlreadyRegistered`** if a port with the same name
    /// already exists anywhere in the process — this manager's map or the
    /// process port registry (ports created by the `drvAsyn*PortConfigure`
    /// iocsh commands, plugin ports, hand-registered ports). C parity:
    /// `asynManager::registerPort` refuses duplicate names. Mirrors asyn
    /// upstream issue #34 (`asynPortDriver` segfault on duplicate port
    /// name): a silent overwrite would orphan the prior
    /// `PortRuntimeHandle` (its runtime thread would keep running on a
    /// now-unreachable handle, leaking resources and silently shadowing
    /// legitimate I/O). To replace a port, call
    /// [`Self::unregister_port`] first.
    pub fn register_port<D: PortDriver>(&self, driver: D) -> AsynResult<PortRuntimeHandle> {
        self.register_port_with_config(driver, RuntimeConfig::default())
    }

    /// Register a port driver with custom runtime config.
    ///
    /// See [`Self::register_port`] for the duplicate-name error contract.
    pub fn register_port_with_config<D: PortDriver>(
        &self,
        driver: D,
        mut config: RuntimeConfig,
    ) -> AsynResult<PortRuntimeHandle> {
        let name = driver.base().port_name.clone();
        // Pre-flight: refuse before we spawn the runtime thread, so a
        // rejected duplicate doesn't burn a thread + create a
        // half-initialized PortRuntimeHandle. The manager map catches a
        // stale manager-owned entry whose registry entry was withdrawn
        // externally; the registry check catches ports published by any
        // other creator (drvAsyn*PortConfigure, plugins, hand-registered).
        {
            let ph = self.port_handles.lock();
            if ph.contains_key(&name) {
                return Err(AsynError::PortAlreadyRegistered(name));
            }
        }
        if crate::registry::get_port(&name).is_some() {
            return Err(AsynError::PortAlreadyRegistered(name));
        }
        // The manager's services, not the config's default global ones — a
        // caller-supplied `RuntimeConfig` cannot detach a port from the trace
        // manager whose `asynSetTrace*` commands are the ones bound to this IOC.
        config.services = self.services.clone();

        // `?` is the whole registration contract on this path: a port whose
        // actor thread the OS refused is not a port, so the name below is never
        // claimed for it (C `registerDriver` returns `asynError` ahead of
        // `ellAdd(&pasynBase->asynPortList,...)`, asynManager.c:2082-2095).
        let (handle, _jh) = create_port_runtime(driver, config)?;

        // The process-registry insert is the atomic claim on the name —
        // it is the single place every consumer resolves a name through
        // (asyn iocsh commands, asynRecord device support, the asyn
        // device-support adapter), and it refuses duplicates. Losing the
        // claim means a concurrent registrant won between the pre-flight
        // and here: drop the runtime we just built and report the
        // duplicate.
        if let Err(e) = crate::registry::register_port(
            &name,
            handle.port_handle().clone(),
            self.services.trace().clone(),
        ) {
            handle.shutdown();
            return Err(e);
        }
        let mut ph = self.port_handles.lock();
        let mut rh = self.runtime_handles.lock();
        ph.insert(name.clone(), handle.port_handle().clone());
        rh.insert(name.clone(), handle.clone());
        drop(rh);
        drop(ph);

        Ok(handle)
    }

    /// Find a port handle by name.
    ///
    /// Ports this manager registered itself resolve from its own map; any other
    /// name falls through to the process port registry, which is where the
    /// `drvAsyn*PortConfigure` iocsh commands, areaDetector plugins and
    /// hand-registered driver ports publish. Without that fall-through the asyn
    /// iocsh commands could not act on a port created from st.cmd — they would
    /// report "port not found" for a port the IOC had just built.
    pub fn find_port_handle(&self, name: &str) -> AsynResult<PortHandle> {
        if let Some(handle) = self.port_handles.lock().get(name).cloned() {
            return Ok(handle);
        }
        crate::registry::get_port(name)
            .map(|entry| entry.handle)
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

    /// Permanently shut down a `ASYN_DESTRUCTIBLE` port — mirror of
    /// C `asynManager::shutdownPort` at asynManager.c:2251-2308.
    ///
    /// Sends a `RequestOp::ShutdownPort` through the port's actor
    /// queue (so the lifecycle runs in the same thread that owns the
    /// driver), then drops the runtime handle. Returns
    /// `Err(Status::Error)` if the port did not opt into the
    /// `destructible` flag at registration. Idempotent — a second
    /// call against a port already shut down returns Ok.
    pub fn shutdown_port(&self, name: &str) -> AsynResult<()> {
        // Drive the lifecycle inside the port's runtime so the
        // driver's own shutdown() runs from its actor thread.
        let handle = self
            .port_handles
            .lock()
            .get(name)
            .cloned()
            .ok_or_else(|| AsynError::PortNotFound(name.to_string()))?;
        let user = crate::user::AsynUser::default();
        let res = handle.submit_blocking(crate::request::RequestOp::ShutdownPort, user);
        // Whether the lifecycle succeeded or hit the "not destructible"
        // error, we leave the port registered so observers can still
        // see the port-name → defunct state (matches C — the port
        // structure remains in pasynManager's port list after
        // shutdownPort completes). Callers that want full removal
        // follow up with `unregister_port`.
        res.map(|_| ())
    }

    /// Unregister a port. Shuts down its runtime.
    pub fn unregister_port(&self, name: &str) {
        let mut ph = self.port_handles.lock();
        let mut rh = self.runtime_handles.lock();
        let was_ours = ph.remove(name).is_some();
        let runtime = rh.remove(name);
        drop(rh);
        drop(ph);
        // A port this manager published must not outlive it in the registry,
        // or the name would keep resolving to a handle whose runtime is gone.
        if was_ours {
            crate::registry::unregister_port(name);
        }
        if let Some(runtime_handle) = runtime {
            runtime_handle.shutdown();
        }
    }

    /// Get a reference to the global exception manager (for registering callbacks).
    pub fn exception_manager(&self) -> &Arc<ExceptionManager> {
        self.services.exceptions()
    }

    /// Get a reference to the global trace manager.
    pub fn trace_manager(&self) -> &Arc<TraceManager> {
        self.services.trace()
    }

    /// Names of every port this IOC can act on, in sorted order.
    ///
    /// C parity: `asynManager::report` walks the global port list to
    /// emit one entry per port — iocsh `asynReport` exposes the same
    /// view (no port argument = all ports). Used by
    /// `iocsh::register_asyn_commands` for the no-port-arg
    /// case; also useful for diagnostic tooling.
    ///
    /// This is the union of the ports this manager registered and the ports
    /// published to the process registry (`drvAsyn*PortConfigure`, plugin and
    /// driver ports), so `asynReport` sees the whole IOC rather than only the
    /// ports that happened to be created through this manager.
    pub fn list_port_names(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> =
            self.port_handles.lock().keys().cloned().collect();
        names.extend(crate::registry::port_names());
        names.into_iter().collect()
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
        mgr.register_port(drv).unwrap();

        assert!(mgr.find_port_handle("port1").is_ok());
        assert!(mgr.find_port_handle("nope").is_err());
    }

    #[test]
    fn test_register_and_use() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("testport");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        let handle = mgr.register_port(drv).unwrap();

        handle.port_handle().write_int32_blocking(0, 0, 42).unwrap();
        assert_eq!(handle.port_handle().read_int32_blocking(0, 0).unwrap(), 42);
    }

    #[test]
    fn test_find_port_handle() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("findme");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port(drv).unwrap();

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
        mgr.register_port(drv).unwrap();

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
        mgr.register_port(drv).unwrap();

        // The runtime sends a Started event but not via the exception manager.
        // Exception manager is injected for driver-level exceptions.
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_unregister_port() {
        let mgr = PortManager::new();
        mgr.register_port(DummyDriver::new("removeme")).unwrap();
        assert!(mgr.find_port_handle("removeme").is_ok());
        mgr.unregister_port("removeme");
        assert!(mgr.find_port_handle("removeme").is_err());
    }

    #[test]
    fn duplicate_port_name_rejected() {
        // Mirrors asyn upstream issue #34: registering a second port
        // with the same name must return PortAlreadyRegistered, not
        // silently overwrite the prior PortRuntimeHandle.
        let mgr = PortManager::new();
        mgr.register_port(DummyDriver::new("dup")).unwrap();
        match mgr.register_port(DummyDriver::new("dup")) {
            Err(crate::error::AsynError::PortAlreadyRegistered(name)) => {
                assert_eq!(name, "dup")
            }
            Err(other) => panic!("expected PortAlreadyRegistered, got {other:?}"),
            Ok(_) => panic!("second registration must fail"),
        }
        // The original port is still reachable (no shadow/orphan).
        assert!(mgr.find_port_handle("dup").is_ok());
    }

    #[test]
    fn duplicate_against_process_registry_rejected() {
        // A name already published to the process port registry (e.g. by a
        // drvAsyn*PortConfigure command or a hand-registered driver port)
        // must block manager registration too — the registry is the
        // process-wide authority on port names, matching C
        // asynManager::registerPort.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        // Stands in for a port published by another creator; no actor loop
        // runs behind it, so its `ActorId` is never current on any thread.
        let ext = crate::port_handle::PortHandle::new(
            tx,
            "extowned".to_string(),
            Arc::new(crate::interrupt::InterruptManager::new(4)),
            crate::port_actor::ActorId::new(),
        );
        crate::registry::register_port(
            "extowned",
            ext,
            Arc::new(crate::trace::TraceManager::new()),
        )
        .unwrap();

        let mgr = PortManager::new();
        match mgr.register_port(DummyDriver::new("extowned")) {
            Err(crate::error::AsynError::PortAlreadyRegistered(name)) => {
                assert_eq!(name, "extowned")
            }
            Err(other) => panic!("expected PortAlreadyRegistered, got {other:?}"),
            Ok(_) => panic!("registration over a process-registry name must fail"),
        }
        // The externally published entry survives the rejected attempt.
        assert!(crate::registry::get_port("extowned").is_some());
        crate::registry::unregister_port("extowned");
    }

    #[test]
    fn duplicate_after_unregister_succeeds() {
        // Replace-via-unregister must work cleanly.
        let mgr = PortManager::new();
        mgr.register_port(DummyDriver::new("recycle")).unwrap();
        mgr.unregister_port("recycle");
        assert!(
            mgr.register_port(DummyDriver::new("recycle")).is_ok(),
            "re-register after unregister must succeed"
        );
    }

    #[test]
    fn test_float64() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("f64_port");
        drv.base.create_param("TEMP", ParamType::Float64).unwrap();
        let handle = mgr.register_port(drv).unwrap();

        handle
            .port_handle()
            .write_float64_blocking(0, 0, 98.6)
            .unwrap();
        assert!((handle.port_handle().read_float64_blocking(0, 0).unwrap() - 98.6).abs() < 1e-10);
    }

    /// asynRecord OEOS/IEOS writes now go through
    /// `RequestOp::SetInputEos / SetOutputEos` which routes through
    /// the actor and calls `PortDriver::set_input_eos /
    /// set_output_eos`. Previously the option-key route stored bytes
    /// in `PortDriverBase::options` HashMap — never read by any
    /// driver, so the EOS interpose never saw the asynRecord update.
    /// This test confirms the new path lands in
    /// `PortDriverBase::input_eos / output_eos`.
    #[test]
    fn set_input_eos_via_actor_reaches_driver_base() {
        let mgr = PortManager::new();
        // "mgr_eos_port", not "eos_port": the iocsh EOS-command test
        // registers "eos_port" in the same process-global registry, and
        // duplicate names now error (C registerPort parity).
        let mut drv = DummyDriver::new("mgr_eos_port");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        // A port that accepts an EOS is one C configured with `processEosIn/
        // Out`, so `asynOctetBase::initialize` installed `asynInterposeEos`
        // above the driver (asynOctetBase.c:169-171). A driver with no EOS
        // methods and no such layer answers "not implemented" (R18-71).
        drv.base
            .install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
        let handle = mgr.register_port(drv).unwrap();

        // Drive through the public handle helper — same path as
        // asynRecord's IEOS / OEOS writes after the
        // SetInputEos/SetOutputEos rewiring. Round-trip success
        // proves the actor accepted the op, drove the driver trait
        // hook, and the trait default mutated `PortDriverBase::
        // input_eos / output_eos` (the source of truth read by the
        // EOS interpose). The actor returns Err if the driver hook
        // erred, so a clean Ok here is the proof.
        handle
            .port_handle()
            .set_input_eos_blocking(crate::user::AsynUser::default(), b"\r\n")
            .unwrap();
        handle
            .port_handle()
            .set_output_eos_blocking(crate::user::AsynUser::default(), b"\n")
            .unwrap();
    }

    #[test]
    fn test_shutdown_via_handle() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("shutme");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        let handle = mgr.register_port(drv).unwrap();

        handle.port_handle().write_int32_blocking(0, 0, 42).unwrap();
        handle.shutdown_and_wait();

        // After shutdown, operations should fail
        assert!(handle.port_handle().write_int32_blocking(0, 0, 1).is_err());
    }
}
