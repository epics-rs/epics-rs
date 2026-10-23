use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

use crate::error::{AsynError, AsynResult};
use crate::exception::ExceptionManager;
use crate::fixed_text::FixedText;
use crate::port::{DpCells, PortDriver};
use crate::port_handle::PortHandle;
use crate::registry::{PortClaim, PortEntry};
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
        Self::with_services(PortServices::new())
    }

    /// The process's one port table — C's `pasynBase->asynPortList`, which
    /// every `registerPort` adds to and every shell command reads
    /// (asynManager.c:2096). It is bound to [`PortServices::global`],
    /// the services a port built with `RuntimeConfig::default()` gets, so a
    /// port created from a driver's own configure command and one created by
    /// `drvAsynIPPortConfigure` share the trace file, the masks and the
    /// exception list. `adapter::register_asyn_device_support` (feature `epics`)
    /// registers the asyn iocsh commands on this manager.
    pub fn global() -> Arc<PortManager> {
        static GLOBAL: OnceLock<Arc<PortManager>> = OnceLock::new();
        GLOBAL
            .get_or_init(|| Arc::new(PortManager::with_services(PortServices::global())))
            .clone()
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
    /// already exists anywhere in the process — the claim refuses a name in
    /// the process port registry (ports created by the `drvAsyn*PortConfigure`
    /// iocsh commands, plugin ports, hand-registered ports), this call one
    /// left in the manager's map. C parity:
    /// `asynManager::registerPort` refuses duplicate names. Mirrors asyn
    /// upstream issue #34 (`asynPortDriver` segfault on duplicate port
    /// name): a silent overwrite would orphan the prior
    /// `PortRuntimeHandle` (its runtime thread would keep running on a
    /// now-unreachable handle, leaking resources and silently shadowing
    /// legitimate I/O). To replace a port, call
    /// [`Self::unregister_port`] first.
    ///
    /// `claim` is the driver's name, held before the driver was built
    /// ([`claim_port_driver`]), as for [`create_port_runtime`]. `owner` is
    /// the gate of the database the port serves (see
    /// [`PortClaim::publish`](crate::registry::PortClaim::publish)); a
    /// manager has no database of its own, so the caller names it.
    pub fn register_port<D: PortDriver>(
        &self,
        claim: PortClaim,
        driver: D,
        owner: &Arc<InterruptGate>,
    ) -> AsynResult<PortRuntimeHandle> {
        self.register_port_with_config(claim, driver, RuntimeConfig::default(), owner)
    }

    /// Register a port driver with custom runtime config.
    ///
    /// See [`Self::register_port`] for the duplicate-name error contract.
    pub fn register_port_with_config<D: PortDriver>(
        &self,
        claim: PortClaim,
        driver: D,
        config: RuntimeConfig,
        owner: &Arc<InterruptGate>,
    ) -> AsynResult<PortRuntimeHandle> {
        let name = claim.name().to_string();
        // The `asynPortDriver` constructor's `registerPort` failure: the line
        // on the thread's stdout, then the error is the constructor's throw
        // (asynPortDriver.cpp:4036-4040).
        self.register_port_inner(claim, driver, config, owner, &name)
            .inspect_err(|_| {
                report_port_driver_register_failure(&name);
            })
    }

    /// [`Self::register_port`] claiming the driver's own name, for a fixture.
    #[cfg(any(test, feature = "test-support"))]
    pub fn register_port_unclaimed<D: PortDriver>(
        &self,
        driver: D,
        owner: &Arc<InterruptGate>,
    ) -> AsynResult<PortRuntimeHandle> {
        self.register_port_unclaimed_with_config(driver, RuntimeConfig::default(), owner)
    }

    /// [`Self::register_port_with_config`] claiming the driver's own name, for
    /// a fixture.
    #[cfg(any(test, feature = "test-support"))]
    pub fn register_port_unclaimed_with_config<D: PortDriver>(
        &self,
        driver: D,
        config: RuntimeConfig,
        owner: &Arc<InterruptGate>,
    ) -> AsynResult<PortRuntimeHandle> {
        let claim = claim_port_driver(&driver.base().port_name)
            .map_err(|_| AsynError::PortAlreadyRegistered(driver.base().port_name.to_string()))?;
        self.register_port_with_config(claim, driver, config, owner)
    }

    fn register_port_inner<D: PortDriver>(
        &self,
        claim: PortClaim,
        driver: D,
        mut config: RuntimeConfig,
        owner: &Arc<InterruptGate>,
        name: &str,
    ) -> AsynResult<PortRuntimeHandle> {
        // Refuse before the runtime thread exists. The manager map catches a
        // stale manager-owned entry whose registry entry was withdrawn
        // externally; the claim refuses a name any other creator holds.
        {
            let ph = self.port_handles.lock();
            if ph.contains_key(name) {
                crate::registry::report_already_registered(name);
                return Err(AsynError::PortAlreadyRegistered(name.to_string()));
            }
        }
        // The manager's services, not the config's default global ones — a
        // caller-supplied `RuntimeConfig` cannot detach a port from the trace
        // manager whose `asynSetTrace*` commands are the ones bound to this IOC.
        config.services = self.services.clone();

        // `?` is the whole registration contract on this path: a port whose
        // actor thread the OS refused is not a port, so the name below is never
        // claimed for it (C `registerDriver` returns `asynError` ahead of
        // `ellAdd(&pasynBase->asynPortList,...)`, asynManager.c:2084-2097).
        let (handle, _jh) = create_port_runtime(&claim, driver, config)?;

        if let Err(e) = claim.publish(handle.port_handle().clone(), owner) {
            handle.shutdown();
            return Err(e);
        }
        let mut ph = self.port_handles.lock();
        let mut rh = self.runtime_handles.lock();
        ph.insert(name.to_string(), handle.port_handle().clone());
        rh.insert(name.to_string(), handle.clone());
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
    /// C `asynManager::shutdownPort` at asynManager.c:2253-2310.
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
    ///
    /// In registration order, C's `asynPortList` order; a manager port whose
    /// registry entry was withdrawn externally follows.
    pub fn list_port_names(&self) -> Vec<String> {
        let mut names = crate::registry::port_names();
        let mut stale: Vec<String> = (self.port_handles.lock().keys())
            .filter(|name| !names.contains(name))
            .cloned()
            .collect();
        stale.sort_unstable();
        names.append(&mut stale);
        names
    }
}

/// An `asynPortDriver` constructor's `registerPort` failure. C prints the
/// message on stdout and then throws it as the `runtime_error` iocsh reports
/// as `C++ error: ...` (asynPortDriver.cpp:4036-4040); `Display` is that
/// `what()`, so a `*Configure` command hands this to
/// `CommandContext::cxx_error`.
#[derive(Debug)]
pub struct PortDriverRegisterFailed {
    port_name: String,
}

impl std::fmt::Display for PortDriverRegisterFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "asynPortDriver:asynPortDriver ERROR: Can't register port: {}",
            self.port_name
        )
    }
}

impl std::error::Error for PortDriverRegisterFailed {}

/// Print the line an `asynPortDriver` constructor prints when its
/// `registerPort` fails, and return the exception it then throws.
///
/// Called by [`PortManager::register_port_with_config`], by
/// `port_runtime_unavailable` (the constructor-shaped plugin runtimes that
/// abort), and through [`claim_port_driver`]. A driver C builds without
/// `asynPortDriver` (`drvAsynIPPort`, …) does not call it.
pub fn report_port_driver_register_failure(port_name: &str) -> PortDriverRegisterFailed {
    let failed = PortDriverRegisterFailed {
        port_name: port_name.to_string(),
    };
    epics_libcom_rs::epics_println!("{failed}");
    failed
}

/// The `registerPort` an `asynPortDriver` constructor makes before anything
/// else (asynPortDriver.cpp:4036-4040): claim `port_name` ahead of building
/// the driver, its runtime or any wiring, so a refused name builds nothing.
pub fn claim_port_driver(port_name: &str) -> Result<PortClaim, PortDriverRegisterFailed> {
    crate::registry::claim_port(port_name)
        .map_err(|_| report_port_driver_register_failure(port_name))
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
/// (asynManager.c:1331-1348, :2331-2373), so the last call to touch a user owns
/// what the caller reads: `asynRecord`'s `special()` splices the buffer *after*
/// `connectDevice`'s tail has run `monitorStatus` over it, and therefore
/// reports the `isEnabled` text for a failed connect (asynRecord.c:515). A
/// message attached to this value would be a second, private copy no later call
/// can update, and a caller splicing it would diverge from C on exactly the
/// paths the shared buffer exists to model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceError;

/// C `ERROR_MESSAGE_SIZE` (asynManager.c:44), the `errorMessageSize` every
/// `createAsynUser` hands out (:1194).
const ERROR_MESSAGE_SIZE: usize = 160;

/// C `pasynUser->errorMessage`: at most `ERROR_MESSAGE_SIZE - 1` bytes.
pub(crate) type ErrorMessage = FixedText<{ ERROR_MESSAGE_SIZE - 1 }>;

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
/// `userPvt->pdevice` when the user attaches (asynManager.c:1351-1353) and every
/// later `findDpCommon` reads it back, so a caller cannot ask about one device
/// on a user bound to another.
pub struct DeviceUser {
    /// C `userPvt->pport`, with the cells of the `dpCommon` C's `findDpCommon`
    /// resolves this user to — `pdevice`'s, or the port's own.
    device: Option<(PortEntry, DpCells)>,
    /// C `userPvt->pdevice`, as the address that selects it. `-1` is C's null
    /// `pdevice`: `connectDevice` creates a device node only for `addr >= 0`
    /// (asynManager.c:1349-1352), and `findDpCommon` then answers from the
    /// port's own `dpc` (:541-544).
    addr: i32,
    error_message: ErrorMessage,
}

impl Default for DeviceUser {
    fn default() -> Self {
        Self {
            device: None,
            // C `pasynManager->createAsynUser` leaves `pdevice` null until
            // `connectDevice` sets it; `-1` is that null in port addressing.
            addr: -1,
            error_message: FixedText::EMPTY,
        }
    }
}

impl DeviceUser {
    /// The port this user is connected to — C `userPvt->pport`.
    pub fn device(&self) -> Option<&PortEntry> {
        self.device.as_ref().map(|(entry, _)| entry)
    }

    /// The last diagnostic any call below left on this user — C
    /// `pasynUser->errorMessage`.
    ///
    /// Never cleared: C's buffer is only ever overwritten by the next layer to
    /// write one, which is what makes the *order* of the manager calls
    /// observable in the text a caller splices.
    pub fn error_message(&self) -> &str {
        self.error_message.as_str()
    }

    /// A layer below wrote `pasynUser->errorMessage`: a driver call or a
    /// manager refusal on a request this user made.
    pub(crate) fn set_error_message(&mut self, message: ErrorMessage) {
        self.error_message = message;
    }

    /// C `connectDevice` (asynManager.c:1324-1357), in C's order: no port name
    /// (C's NULL `portName`, `None` here), then the registry lookup
    /// (`locatePort`), which an empty name fails like any other, then a user
    /// that is already connected to a device.
    pub fn connect_device(
        &mut self,
        port_name: Option<&str>,
        addr: i32,
    ) -> Result<PortEntry, DeviceError> {
        let Some(port_name) = port_name else {
            return Err(self.fail(format_args!(
                "asynManager:connectDevice no port name provided"
            )));
        };
        let Some(entry) = crate::registry::get_port(port_name) else {
            return Err(self.fail(format_args!(
                "asynManager:connectDevice port {port_name} not found"
            )));
        };
        if self.device.is_some() {
            return Err(self.fail(format_args!(
                "asynManager:connectDevice already connected to device"
            )));
        }
        // C :1349-1352 under `asynManagerLock`: `locateDevice(pport, addr,
        // TRUE)` creates the device on a multi-device port, and cannot fail.
        let dp = entry.handle.connect_device(addr);
        self.device = Some((entry.clone(), dp));
        self.addr = addr;
        Ok(entry)
    }

    /// C `exceptionCallbackAdd(pasynUser, callback)` (asynManager.c:1364-1382):
    /// on the exception list of the `dpCommon` this user is bound to, so it
    /// hears that device's or port's exceptions and no other. `None` when the
    /// user is bound to nothing, C's "not connected" refusal.
    pub fn exception_callback_add<F>(
        &self,
        callback: F,
    ) -> Option<crate::exception::ExceptionCallbackGuard>
    where
        F: Fn(&crate::exception::ExceptionEvent<'_>) + Send + Sync + 'static,
    {
        let entry = self.device()?;
        Some(entry.handle.exception_callback_add(self.addr, callback))
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
        // puserPvt->pdevice = 0` (asynManager.c:1387-1388).
        self.device = None;
        self.addr = -1;
    }

    /// C `isConnected` (asynManager.c:2331-2343): a read of the bound
    /// `dpCommon`, which never queues — so a record may ask it under its lock
    /// while the port sits in a driver call.
    pub fn is_connected(&mut self) -> Result<bool, DeviceError> {
        Ok(self.dp("isConnected")?.connected())
    }

    /// C `isEnabled` (asynManager.c:2345-2359).
    pub fn is_enabled(&mut self) -> Result<bool, DeviceError> {
        Ok(self.dp("isEnabled")?.enabled())
    }

    /// C `isAutoConnect` (asynManager.c:2361-2373).
    pub fn is_auto_connect(&mut self) -> Result<bool, DeviceError> {
        Ok(self.dp("isAutoConnect")?.auto_connect())
    }

    /// C `findInterface(pasynUser, asynCommonType, ...)` (asynManager.c:1470-1513)
    /// as `connectDevice` asks it. A `PortDriver` cannot lack asynCommon, so only
    /// C's two refusals can answer differently: no binding, or a port shut down.
    pub fn find_common(&mut self) -> Result<(), DeviceError> {
        let defunct = match &self.device {
            None => {
                return Err(self.fail(format_args!("asynManager:findInterface: not connected")));
            }
            Some((entry, _)) => entry.handle.is_defunct(),
        };
        if defunct {
            return Err(self.fail(format_args!("asynManager:findInterface: port shut down")));
        }
        Ok(())
    }

    /// Bind to an already-resolved port, for tests that build a [`PortEntry`]
    /// by hand rather than publishing it to the process registry.
    #[cfg(all(test, feature = "epics"))]
    pub(crate) fn attach_for_test(&mut self, entry: PortEntry, addr: i32) {
        let dp = entry.handle.connect_device(addr);
        self.device = Some((entry, dp));
        self.addr = addr;
    }

    /// The bound port, or C's `findDpCommon` failure: the `is*` calls share one
    /// message shape and one buffer, so they share this.
    fn dp(&mut self, which: &str) -> Result<&DpCells, DeviceError> {
        if self.device.is_none() {
            return Err(self.fail(format_args!(
                "asynManager:{which} asynUser not connected to device"
            )));
        }
        Ok(&self.device.as_ref().unwrap().1)
    }

    fn fail(&mut self, message: std::fmt::Arguments<'_>) -> DeviceError {
        self.error_message = ErrorMessage::format(message);
        DeviceError
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::ParamType;
    use crate::port::InterfaceMask;
    use crate::port::{PortDriverBase, PortFlags};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DummyDriver {
        base: PortDriverBase,
    }

    impl DummyDriver {
        fn new(name: &str) -> Self {
            Self {
                base: PortDriverBase::new(
                    name,
                    1,
                    PortFlags::default(),
                    InterfaceMask::UNMIGRATED_ALL,
                    InterfaceMask::UNMIGRATED_INTERRUPTS,
                ),
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
        assert_eq!(user.find_common(), Err(DeviceError));
        assert_eq!(
            user.error_message(),
            "asynManager:findInterface: not connected"
        );
    }

    /// `connectDevice`'s three rejections in C's order — no port name, an
    /// unknown name, a user already connected — and the binding each leaves
    /// behind (asynManager.c:1324-1357).
    #[test]
    fn connect_device_binds_once_and_refuses_the_second_attempt() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("devuser_port_1");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port_unclaimed(drv, &crate::registry::open_owner())
            .unwrap();

        let mut user = DeviceUser::default();
        assert!(user.connect_device(None, 0).is_err());
        assert_eq!(
            user.error_message(),
            "asynManager:connectDevice no port name provided"
        );
        assert!(user.device().is_none(), "a rejected connect binds nothing");

        // an empty name is a name locatePort does not find (asynManager.c:1337-1342)
        assert!(user.connect_device(Some(""), 0).is_err());
        assert_eq!(
            user.error_message(),
            "asynManager:connectDevice port  not found"
        );
        assert!(user.device().is_none());

        assert!(
            user.connect_device(Some("devuser_no_such_port"), 0)
                .is_err()
        );
        assert_eq!(
            user.error_message(),
            "asynManager:connectDevice port devuser_no_such_port not found"
        );
        assert!(user.device().is_none());

        assert!(user.connect_device(Some("devuser_port_1"), 0).is_ok());
        assert_eq!(
            user.device()
                .map(|entry| entry.handle.port_name().to_string()),
            Some("devuser_port_1".to_string())
        );

        assert!(user.connect_device(Some("devuser_port_1"), 0).is_err());
        assert_eq!(
            user.error_message(),
            "asynManager:connectDevice already connected to device"
        );

        mgr.unregister_port("devuser_port_1");
    }

    /// C `connectDevice` is where a multi-device port's device comes into
    /// being (`locateDevice(pport, addr, TRUE)`, asynManager.c:1349-1352):
    /// `asynReport` counts it in `nDevices` and prints its block from that
    /// moment. The boundaries: a multi-device port with `addr >= 0` allocates;
    /// `addr < 0` on the same port binds the port's own dpCommon (:1351
    /// `if(addr>=0)`); a single-device port allocates nothing whatever the
    /// addr (:576 `!(attributes&ASYN_MULTIDEVICE)`).
    #[test]
    fn connect_device_creates_the_device_on_a_multi_device_port() {
        let mgr = PortManager::new();
        let mut multi = DummyDriver::new("devuser_multi");
        multi.base.flags.multi_device = true;
        multi.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port_unclaimed(multi, &crate::registry::open_owner())
            .unwrap();
        let mut single = DummyDriver::new("devuser_single");
        single.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port_unclaimed(single, &crate::registry::open_owner())
            .unwrap();

        let devices = |port: &str| {
            crate::registry::get_port(port)
                .unwrap()
                .handle
                .with_driver_blocking(|d: &mut DummyDriver| {
                    d.base
                        .devices()
                        .into_iter()
                        .map(|(addr, _)| addr)
                        .collect::<Vec<_>>()
                })
                .unwrap()
        };
        assert!(devices("devuser_multi").is_empty(), "nothing bound yet");

        let mut user = DeviceUser::default();
        user.connect_device(Some("devuser_multi"), 3).unwrap();
        assert_eq!(
            devices("devuser_multi"),
            vec![3],
            "the bound address is a device now"
        );

        let mut port_user = DeviceUser::default();
        port_user.connect_device(Some("devuser_multi"), -1).unwrap();
        assert_eq!(
            devices("devuser_multi"),
            vec![3],
            "addr -1 is the port's own dpCommon"
        );

        let mut single_user = DeviceUser::default();
        single_user
            .connect_device(Some("devuser_single"), 3)
            .unwrap();
        assert!(
            devices("devuser_single").is_empty(),
            "a single-device port has no device list to add to"
        );

        mgr.unregister_port("devuser_multi");
        mgr.unregister_port("devuser_single");
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
        mgr.register_port_unclaimed(drv, &crate::registry::open_owner())
            .unwrap();

        let mut user = DeviceUser::default();
        user.connect_device(Some("devuser_port_2"), 0).unwrap();
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
        mgr.register_port_unclaimed(drv, &crate::registry::open_owner())
            .unwrap();

        assert!(mgr.find_port_handle("port1").is_ok());
        assert!(mgr.find_port_handle("nope").is_err());
    }

    #[test]
    fn test_register_and_use() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("testport");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        let handle = mgr
            .register_port_unclaimed(drv, &crate::registry::open_owner())
            .unwrap();

        handle.port_handle().write_int32_blocking(0, 0, 42).unwrap();
        assert_eq!(handle.port_handle().read_int32_blocking(0, 0).unwrap(), 42);
    }

    #[test]
    fn test_find_port_handle() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("findme");
        drv.base.create_param("VAL", ParamType::Int32).unwrap();
        mgr.register_port_unclaimed(drv, &crate::registry::open_owner())
            .unwrap();

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
        mgr.register_port_unclaimed(drv, &crate::registry::open_owner())
            .unwrap();

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
        mgr.register_port_unclaimed(drv, &crate::registry::open_owner())
            .unwrap();

        // Registration's connect is the one exception: the port starts down
        // and its connect announces `asynExceptionConnect` (asynManager.c:523,
        // :2133-2137).
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_unregister_port() {
        let mgr = PortManager::new();
        mgr.register_port_unclaimed(DummyDriver::new("removeme"), &crate::registry::open_owner())
            .unwrap();
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
        mgr.register_port_unclaimed(DummyDriver::new("dup"), &crate::registry::open_owner())
            .unwrap();
        match mgr.register_port_unclaimed(DummyDriver::new("dup"), &crate::registry::open_owner()) {
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
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        // Stands in for a port published by another creator; no actor loop
        // runs behind it, so its `ActorId` is never current on any thread.
        let ext = crate::port_handle::PortHandle::new(
            tx,
            "extowned".to_string(),
            Arc::new(crate::interrupt::InterruptManager::new()),
            crate::port_actor::ActorLink::unowned(),
            Arc::new(crate::trace::TraceManager::new()),
        );
        crate::registry::register_port("extowned", ext, &crate::registry::open_owner()).unwrap();

        let mgr = PortManager::new();
        match mgr
            .register_port_unclaimed(DummyDriver::new("extowned"), &crate::registry::open_owner())
        {
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

    /// A name another creator holds, even before it publishes, refuses the
    /// manager before the driver's port thread exists, so the refused IP port
    /// never dials its host.
    #[test]
    fn a_claimed_name_refuses_before_the_port_thread_connects() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let held = crate::registry::claim_port("mgrclaimed").unwrap();

        let mgr = PortManager::new();
        let driver = crate::drivers::ip_port::DrvAsynIPPort::new("mgrclaimed", &host).unwrap();
        assert!(matches!(
            mgr.register_port_unclaimed(driver, &crate::registry::open_owner()),
            Err(AsynError::PortAlreadyRegistered(_))
        ));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(listener.accept().is_err(), "the refused port connected");
        drop(held);
        assert!(crate::registry::claim_port("mgrclaimed").is_ok());
    }

    #[test]
    fn duplicate_after_unregister_succeeds() {
        // Replace-via-unregister must work cleanly.
        let mgr = PortManager::new();
        mgr.register_port_unclaimed(DummyDriver::new("recycle"), &crate::registry::open_owner())
            .unwrap();
        mgr.unregister_port("recycle");
        assert!(
            mgr.register_port_unclaimed(
                DummyDriver::new("recycle"),
                &crate::registry::open_owner()
            )
            .is_ok(),
            "re-register after unregister must succeed"
        );
    }

    #[test]
    fn test_float64() {
        let mgr = PortManager::new();
        let mut drv = DummyDriver::new("f64_port");
        drv.base.create_param("TEMP", ParamType::Float64).unwrap();
        let handle = mgr
            .register_port_unclaimed(drv, &crate::registry::open_owner())
            .unwrap();

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
        let handle = mgr
            .register_port_unclaimed(drv, &crate::registry::open_owner())
            .unwrap();

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
        let handle = mgr
            .register_port_unclaimed(drv, &crate::registry::open_owner())
            .unwrap();

        handle.port_handle().write_int32_blocking(0, 0, 42).unwrap();
        handle.shutdown_and_wait();

        // After shutdown, operations should fail
        assert!(handle.port_handle().write_int32_blocking(0, 0, 1).is_err());
    }
}

#[cfg(test)]
mod register_port_line_tests {
    use super::*;
    use crate::param::ParamType;
    use crate::port::InterfaceMask;
    use crate::port::{PortDriver, PortDriverBase, PortFlags};

    struct P(PortDriverBase);
    impl PortDriver for P {
        fn base(&self) -> &PortDriverBase {
            &self.0
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.0
        }
    }

    fn port(name: &str) -> P {
        let mut base = PortDriverBase::new(
            name,
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.create_param("VAL", ParamType::Int32).unwrap();
        P(base)
    }

    /// A refused registerPort prints C's two stdout lines, the manager's
    /// (asynManager.c:2056) then the constructor's, and the error is the
    /// constructor's throw (asynPortDriver.cpp:4036-4040). The
    /// refusal here is a duplicate name, which `registerPort` also fails on.
    #[test]
    fn a_refused_register_port_prints_the_constructor_line() {
        const CHILD: &str = "ASYN_RS_REGISTER_LINE_CHILD";
        const TEST: &str = "manager::register_port_line_tests::a_refused_register_port_prints_the_constructor_line";
        if std::env::var_os(CHILD).is_some() {
            let mgr = PortManager::new();
            let owner = crate::registry::open_owner();
            let _first = mgr
                .register_port_unclaimed(port("dupLine"), &owner)
                .unwrap();
            match mgr.register_port_unclaimed(port("dupLine"), &owner) {
                Err(AsynError::PortAlreadyRegistered(n)) => println!("ERR={n}"),
                Err(e) => println!("OTHER={e}"),
                Ok(_) => println!("REGISTERED"),
            }
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("re-exec the test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stdout}{stderr}");
        let lines: Vec<&str> = stdout.lines().collect();
        let at = lines
            .iter()
            .position(|l| *l == "asynCommon:registerDriver dupLine already registered")
            .unwrap_or_else(|| panic!("no already-registered line:\n{stdout}"));
        assert_eq!(
            lines.get(at + 1).copied(),
            Some("asynPortDriver:asynPortDriver ERROR: Can't register port: dupLine"),
            "{stdout}"
        );
        assert!(stdout.lines().any(|l| l == "ERR=dupLine"), "{stdout}");
        assert!(!stderr.contains("Can't register port"), "{stderr}");
    }

    /// A port that registers prints nothing.
    #[test]
    fn a_registered_port_prints_no_constructor_line() {
        const CHILD: &str = "ASYN_RS_REGISTER_OK_CHILD";
        const TEST: &str =
            "manager::register_port_line_tests::a_registered_port_prints_no_constructor_line";
        if std::env::var_os(CHILD).is_some() {
            let mgr = PortManager::new();
            let _h = mgr
                .register_port_unclaimed(port("okLine"), &crate::registry::open_owner())
                .unwrap();
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("re-exec the test binary");
        assert!(out.status.success());
        assert!(!String::from_utf8_lossy(&out.stdout).contains("Can't register port"));
    }
}
