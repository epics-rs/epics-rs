use std::sync::Arc;

use asyn_rs::asyn_record::PortClaim;
use asyn_rs::manager::{PortDriverRegisterFailed, report_port_driver_register_failure};

use crate::plugin::runtime::PluginRuntimeHandle;
use crate::plugin::wiring::WiringRegistry;

use super::DriverContext;

/// Manages areaDetector plugin lifecycle: registration, port wiring, report.
///
/// All records use standard asyn DTYPs with @asyn(PORT,...) links, handled by
/// the universal asyn device support factory. The PluginManager no longer
/// provides a device support factory — it only manages plugin creation,
/// port registration, and NDArray pipeline wiring.
pub struct PluginManager {
    driver: parking_lot::Mutex<Option<Arc<dyn DriverContext>>>,
    plugin_handles: parking_lot::Mutex<Vec<PluginRuntimeHandle>>,
    wiring: Arc<WiringRegistry>,
}

impl PluginManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            driver: parking_lot::Mutex::new(None),
            plugin_handles: parking_lot::Mutex::new(Vec::new()),
            wiring: Arc::new(WiringRegistry::new()),
        })
    }

    /// Access the shared wiring registry.
    pub fn wiring(&self) -> &Arc<WiringRegistry> {
        &self.wiring
    }

    /// Set the driver context. Called when the driver config command runs.
    pub fn set_driver(&self, driver: Arc<dyn DriverContext>) {
        *self.driver.lock() = Some(driver);
    }

    /// Get the driver context, or error if not yet configured.
    pub fn driver(&self) -> Result<Arc<dyn DriverContext>, String> {
        self.driver
            .lock()
            .clone()
            .ok_or_else(|| "driver must be configured first".into())
    }

    /// Register a plugin under the name its `*Configure` claimed first
    /// ([`asyn_rs::manager::claim_port_driver`]), in the global asyn port
    /// registry the universal asyn device support factory reads.
    ///
    /// `blocking_callbacks` is the `*Configure` line's `blockingCallbacks`
    /// argument, the `NDPluginDriver` constructor argument every plugin has.
    ///
    /// `owner` is the gate of the database the plugin's records load into
    /// (the configure command's `ctx.db().scan_control().interrupt_gate()`).
    pub fn add_plugin(
        &self,
        claim: PortClaim,
        handle: &PluginRuntimeHandle,
        blocking_callbacks: i32,
        owner: &std::sync::Arc<epics_libcom_rs::runtime::interrupt_accept::InterruptGate>,
    ) -> Result<(), PortDriverRegisterFailed> {
        let port_handle = handle.port_runtime().port_handle().clone();
        let port_name = port_handle.port_name().to_string();
        debug_assert_eq!(claim.name(), port_name);
        handle.set_blocking_callbacks(blocking_callbacks);
        claim
            .publish(port_handle, owner)
            .map_err(|_| report_port_driver_register_failure(&port_name))?;

        // Register this plugin's output in the wiring registry
        self.wiring
            .register_output(&port_name, handle.array_output().clone());

        self.plugin_handles.lock().push(handle.clone());
        Ok(())
    }

    /// The registered plugin whose port is `port_name`.
    pub fn plugin(&self, port_name: &str) -> Option<PluginRuntimeHandle> {
        self.plugin_handles
            .lock()
            .iter()
            .find(|h| h.port_runtime().port_handle().port_name() == port_name)
            .cloned()
    }

    /// Print a report of registered plugins.
    pub fn report(&self) {
        let handles = self.plugin_handles.lock();
        println!("  Plugins: {}", handles.len());
        for h in handles.iter() {
            println!("    - {}", h.port_runtime().port_handle().port_name());
        }
    }
}
