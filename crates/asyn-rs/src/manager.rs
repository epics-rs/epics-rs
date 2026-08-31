use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::{AsynError, AsynResult};
use crate::exception::ExceptionManager;
use crate::port::PortDriver;
use crate::port_handle::PortHandle;
use crate::registry::PortEntry;
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

/// The failure a manager *device* call returns: C's bare `asynError`.
///
/// Deliberately carries no message. C's manager writes its diagnostic into
/// `pasynUser->errorMessage` and returns only the status
/// (asynManager.c:1331-1346, :2331-2373), so the last call to touch a user owns
/// what the caller reads: `asynRecord`'s `special()` splices the buffer *after*
/// `connectDevice`'s tail has run `monitorStatus` over it, and therefore
/// reports the `isEnabled` text for a failed connect (asynRecord.c:515). A
/// message attached to this value would be a second, private copy no later call
/// can update, and a caller splicing it would diverge from C on exactly the
/// paths the shared buffer exists to model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceError;

/// C's `pasynUser` as the manager's device calls see it: the port the user is
/// connected to (`userPvt->pport`, asynManager.c:1345-1352) beside the
/// `errorMessage` buffer those calls write their diagnostic into.
///
/// One object, both fields private, because the binding is what decides whether
/// a query answers or writes the buffer — C resolves it with
/// `findDpCommon(puserPvt)` and every `is*` fails with "asynUser not connected
/// to device" when that comes back null (asynManager.c:2331-2373). A holder
/// therefore cannot attach itself to a port except through
/// [`Self::connect_device`], cannot detach except through [`Self::disconnect`],
/// and cannot ask about a port it is not bound to.
///
/// Not [`crate::user::AsynUser`] itself: that type carries
/// `user_data: Box<dyn Any + Send>` and so is not `Sync`, while a record that
/// keeps this for its lifetime must be (`Record: Send + Sync`). This is the
/// long-lived half of C's `pasynRecPvt->pasynUser` — the binding and the buffer
/// that outlive any one call; the per-request halves stay on the `AsynUser`
/// each call builds.
///
/// The address is part of the binding, not a per-call argument: C stores it as
/// `userPvt->pdevice` when the user attaches (asynManager.c:1349-1352) and every
/// later `findDpCommon` reads it back, so a caller cannot ask about one device
/// on a user bound to another.
pub struct DeviceUser {
    device: Option<PortEntry>,
    /// C `userPvt->pdevice`, as the address that selects it. `-1` is C's null
    /// `pdevice`: `connectDevice` creates a device node only for `addr >= 0`
    /// (asynManager.c:1349-1352), and `findDpCommon` then answers from the
    /// port's own `dpc` (:541-544).
    addr: i32,
    error_message: String,
}

impl Default for DeviceUser {
    fn default() -> Self {
        Self {
            device: None,
            // C `pasynManager->createAsynUser` leaves `pdevice` null until
            // `connectDevice` sets it; `-1` is that null in port addressing.
            addr: -1,
            error_message: String::new(),
        }
    }
}

impl DeviceUser {
    /// The port this user is connected to — C `userPvt->pport`.
    pub fn device(&self) -> Option<&PortEntry> {
        self.device.as_ref()
    }

    /// The last diagnostic any call below left on this user — C
    /// `pasynUser->errorMessage`.
    ///
    /// Never cleared: C's buffer is only ever overwritten by the next layer to
    /// write one, which is what makes the *order* of the manager calls
    /// observable in the text a caller splices.
    pub fn error_message(&self) -> &str {
        &self.error_message
    }

    /// C `connectDevice` (asynManager.c:1324-1355), in C's order: no port name,
    /// then the registry lookup (`locatePort`), then a user that is already
    /// connected to a device.
    pub fn connect_device(&mut self, port_name: &str, addr: i32) -> Result<PortEntry, DeviceError> {
        if port_name.is_empty() {
            return Err(self.fail("asynManager:connectDevice no port name provided".to_string()));
        }
        let Some(entry) = crate::registry::get_port(port_name) else {
            return Err(self.fail(format!(
                "asynManager:connectDevice port {port_name} not found"
            )));
        };
        if self.device.is_some() {
            return Err(
                self.fail("asynManager:connectDevice already connected to device".to_string())
            );
        }
        self.device = Some(entry.clone());
        self.addr = addr;
        Ok(entry)
    }

    /// C `disconnect` (asynManager.c:1359-1391): sever the binding.
    ///
    /// C refuses while the user has a queued request, holds a block, or is on
    /// the exception list, and reports each refusal in the buffer. A record
    /// reaches it only after `exceptionCallbackRemove` and with no request of
    /// its own outstanding (asynRecord.c:1153-1154, :522-524), so the path it
    /// takes is the only one modelled.
    pub fn disconnect(&mut self) {
        // C clears both halves of the binding: `puserPvt->pport = 0;
        // puserPvt->pdevice = 0` (asynManager.c:1386-1387).
        self.device = None;
        self.addr = -1;
    }

    /// C `isConnected` (asynManager.c:2331-2343).
    ///
    /// C reads `pdpCommon->connected`, a field read that cannot fail once the
    /// user is bound. The actor query behind [`PortHandle`] can — a shut-down
    /// port, or a call made from the actor's own thread, where waiting would be
    /// the actor waiting on itself — and those answer `false` rather than
    /// writing the buffer: they are not C's "not connected to device", and a
    /// diagnostic C never writes must not displace one it did. Same for the two
    /// below.
    pub fn is_connected(&mut self) -> Result<bool, DeviceError> {
        let addr = self.addr;
        Ok(self
            .handle_for("isConnected")?
            .is_connected_blocking(addr)
            .unwrap_or(false))
    }

    /// C `isEnabled` (asynManager.c:2345-2359).
    pub fn is_enabled(&mut self) -> Result<bool, DeviceError> {
        let addr = self.addr;
        Ok(self
            .handle_for("isEnabled")?
            .is_enabled_blocking(addr)
            .unwrap_or(false))
    }

    /// C `isAutoConnect` (asynManager.c:2361-2373).
    pub fn is_auto_connect(&mut self) -> Result<bool, DeviceError> {
        let addr = self.addr;
        Ok(self
            .handle_for("isAutoConnect")?
            .is_auto_connect_blocking(addr)
            .unwrap_or(false))
    }

    /// Bind to an already-resolved port, for tests that build a [`PortEntry`]
    /// by hand rather than publishing it to the process registry.
    #[cfg(test)]
    pub(crate) fn attach_for_test(&mut self, entry: PortEntry, addr: i32) {
        self.device = Some(entry);
        self.addr = addr;
    }

    /// The bound port, or C's `findDpCommon` failure: the `is*` calls share one
    /// message shape and one buffer, so they share this.
    fn handle_for(&mut self, which: &str) -> Result<PortHandle, DeviceError> {
        if let Some(entry) = self.device.as_ref() {
            return Ok(entry.handle.clone());
        }
        Err(self.fail(format!(
            "asynManager:{which} asynUser not connected to device"
        )))
    }

    fn fail(&mut self, message: String) -> DeviceError {
        self.error_message = message;
        DeviceError
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

    /// The three device queries on a user bound to nothing: each fails with its
    /// own name in the shared buffer, so the *last* one asked is what a caller
    /// reads back. C `isAutoConnect` / `isConnected` / `isEnabled` all write
    /// `pasynUser->errorMessage` through `findDpCommon` (asynManager.c:2331-2373),
    /// and `monitorStatus` asks them in that order (asynRecord.c:1085-1097) —
    /// which is why `asynRecord`'s `special()` reports the `isEnabled` text for
    /// a failed *connect*.
    #[test]
    fn an_unbound_user_reports_the_query_that_asked_last() {
        let mut user = DeviceUser::default();
        assert!(user.device().is_none());

        assert_eq!(user.is_auto_connect(), Err(DeviceError));
        assert_eq!(
            user.error_message(),
            "asynManager:isAutoConnect asynUser not connected to device"
        );
        assert_eq!(user.is_connected(), Err(DeviceError));
        assert_eq!(
            user.error_message(),
            "asynManager:isConnected asynUser not connected to device"
        );
        assert_eq!(user.is_enabled(), Err(DeviceError));
        assert_eq!(
            user.error_message(),
            "asynManager:isEnabled asynUser not connected to device"
        );
    }

    /// `connectDevice`'s three rejections in C's order — no port name, an
    /// unknown name, a user already connected — and the binding each leaves
    /// behind (asynManager.c:1324-1355).
    #[test]
    fn connect_device_binds_once_and_refuses_the_second_attempt() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("devuser_port_1");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port(drv).unwrap();

        let mut user = DeviceUser::default();
        assert!(user.connect_device("", 0).is_err());
        assert_eq!(
            user.error_message(),
            "asynManager:connectDevice no port name provided"
        );
        assert!(user.device().is_none(), "a rejected connect binds nothing");

        assert!(user.connect_device("devuser_no_such_port", 0).is_err());
        assert_eq!(
            user.error_message(),
            "asynManager:connectDevice port devuser_no_such_port not found"
        );
        assert!(user.device().is_none());

        assert!(user.connect_device("devuser_port_1", 0).is_ok());
        assert_eq!(
            user.device()
                .map(|entry| entry.handle.port_name().to_string()),
            Some("devuser_port_1".to_string())
        );

        assert!(user.connect_device("devuser_port_1", 0).is_err());
        assert_eq!(
            user.error_message(),
            "asynManager:connectDevice already connected to device"
        );

        mgr.unregister_port("devuser_port_1");
    }

    /// A bound user answers the queries from the port and leaves the buffer
    /// alone — C writes `errorMessage` only on the `findDpCommon` failure — and
    /// `disconnect` puts it back on the failing arm. The second half is the
    /// regression: a holder that severed the binding by hand would keep
    /// answering from a port it is no longer connected to.
    #[test]
    fn disconnect_returns_the_queries_to_the_unbound_arm() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("devuser_port_2");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port(drv).unwrap();

        let mut user = DeviceUser::default();
        user.connect_device("devuser_port_2", 0).unwrap();
        assert_eq!(user.is_enabled(), Ok(true));
        assert_eq!(user.is_connected(), Ok(true));
        assert_eq!(
            user.error_message(),
            "",
            "an answered query writes no diagnostic"
        );

        user.disconnect();
        assert!(user.device().is_none());
        assert_eq!(user.is_enabled(), Err(DeviceError));
        assert_eq!(
            user.error_message(),
            "asynManager:isEnabled asynUser not connected to device"
        );

        mgr.unregister_port("devuser_port_2");
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
        // above the driver (asynOctetBase.c:170-172). A driver with no EOS
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
