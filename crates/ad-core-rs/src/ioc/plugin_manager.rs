use std::sync::Arc;

use asyn_rs::trace::TraceManager;

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
    port_runtimes: parking_lot::Mutex<Vec<asyn_rs::runtime::port::PortRuntimeHandle>>,
    trace: Arc<TraceManager>,
    wiring: Arc<WiringRegistry>,
}

impl PluginManager {
    pub fn new(trace: Arc<TraceManager>) -> Arc<Self> {
        Arc::new(Self {
            driver: parking_lot::Mutex::new(None),
            plugin_handles: parking_lot::Mutex::new(Vec::new()),
            port_runtimes: parking_lot::Mutex::new(Vec::new()),
            trace,
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

    /// The shared TraceManager for port registration.
    pub fn trace(&self) -> &Arc<TraceManager> {
        &self.trace
    }

    /// Register a plugin. The port is registered in the global asyn port registry
    /// so the universal asyn device support factory can find it.
    pub fn add_plugin(&self, _dtyp: &str, handle: &PluginRuntimeHandle) {
        let port_handle = handle.port_runtime().port_handle().clone();
        let port_name = port_handle.port_name().to_string();

        // Register this plugin's output in the wiring registry
        self.wiring
            .register_output(&port_name, handle.array_output().clone());

        self.plugin_handles.lock().push(handle.clone());
        asyn_rs::asyn_record::register_port(&port_name, port_handle, self.trace.clone());
    }

    /// Register a raw port (not a plugin runtime) for auxiliary ports like TimeSeries.
    ///
    /// The `PortRuntimeHandle` is stored to keep the actor thread alive.
    pub fn add_port(&self, _dtyp: &str, runtime: asyn_rs::runtime::port::PortRuntimeHandle) {
        let port_handle = runtime.port_handle().clone();
        let port_name = port_handle.port_name().to_string();
        self.port_runtimes.lock().push(runtime);
        asyn_rs::asyn_record::register_port(&port_name, port_handle, self.trace.clone());
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
