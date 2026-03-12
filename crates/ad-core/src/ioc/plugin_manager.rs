use std::sync::Arc;

use asyn_rs::port_handle::PortHandle;
use asyn_rs::trace::TraceManager;
use epics_base_rs::server::ioc_app::IocApplication;

use crate::plugin::registry::{build_plugin_base_registry, ParamRegistry};
use crate::plugin::runtime::PluginRuntimeHandle;
use crate::plugin::wiring::WiringRegistry;

use super::plugin_device_support::{ArrayDataHandle, PluginDeviceSupport};
use super::DriverContext;

/// Info about a configured plugin, stored for device support wiring at iocInit.
pub struct PluginInfo {
    pub dtyp_name: String,
    pub port_handle: PortHandle,
    pub registry: Arc<ParamRegistry>,
    pub array_data: Option<ArrayDataHandle>,
}

/// Manages areaDetector plugin lifecycle: registration, port wiring, device support dispatch.
///
/// Shared between startup commands (which create plugins) and the device support
/// factory (which wires EPICS records to plugin ports at iocInit).
pub struct PluginManager {
    driver: parking_lot::Mutex<Option<Arc<dyn DriverContext>>>,
    plugins: parking_lot::Mutex<Vec<PluginInfo>>,
    plugin_handles: parking_lot::Mutex<Vec<PluginRuntimeHandle>>,
    port_runtimes: parking_lot::Mutex<Vec<asyn_rs::runtime::port::PortRuntimeHandle>>,
    trace: Arc<TraceManager>,
    wiring: Arc<WiringRegistry>,
}

impl PluginManager {
    pub fn new(trace: Arc<TraceManager>) -> Arc<Self> {
        Arc::new(Self {
            driver: parking_lot::Mutex::new(None),
            plugins: parking_lot::Mutex::new(Vec::new()),
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

    /// Register a plugin with auto-generated base param registry.
    pub fn add_plugin(
        &self,
        dtyp: &str,
        handle: &PluginRuntimeHandle,
        array_data: Option<ArrayDataHandle>,
    ) {
        let registry = Arc::new(build_plugin_base_registry(handle));
        self.add_plugin_with_registry(dtyp, handle, registry, array_data);
    }

    /// Register a plugin with a custom param registry (e.g. Stats with extra params).
    pub fn add_plugin_with_registry(
        &self,
        dtyp: &str,
        handle: &PluginRuntimeHandle,
        registry: Arc<ParamRegistry>,
        array_data: Option<ArrayDataHandle>,
    ) {
        let port_handle = handle.port_runtime().port_handle().clone();
        let port_name = port_handle.port_name().to_string();

        // Register this plugin's output in the wiring registry
        self.wiring.register_output(&port_name, handle.array_output().clone());

        self.plugins.lock().push(PluginInfo {
            dtyp_name: dtyp.to_string(),
            port_handle: port_handle.clone(),
            registry,
            array_data,
        });
        self.plugin_handles.lock().push(handle.clone());
        asyn_rs::asyn_record::register_port(&port_name, port_handle, self.trace.clone());
    }

    /// Register a raw port (not a plugin runtime) for device support dispatch.
    /// Used for auxiliary ports like TimeSeries.
    ///
    /// The `PortRuntimeHandle` is stored to keep the actor thread alive.
    pub fn add_port(
        &self,
        dtyp: &str,
        runtime: asyn_rs::runtime::port::PortRuntimeHandle,
        registry: Arc<ParamRegistry>,
    ) {
        let port_handle = runtime.port_handle().clone();
        let port_name = port_handle.port_name().to_string();
        self.plugins.lock().push(PluginInfo {
            dtyp_name: dtyp.to_string(),
            port_handle: port_handle.clone(),
            registry,
            array_data: None,
        });
        self.port_runtimes.lock().push(runtime);
        asyn_rs::asyn_record::register_port(&port_name, port_handle, self.trace.clone());
    }

    /// Register dynamic device support on the IocApplication.
    ///
    /// Returns a factory that looks up DTYP names against registered plugins.
    pub fn register_device_support(
        self: &Arc<Self>,
        app: IocApplication,
    ) -> IocApplication {
        let mgr = self.clone();
        app.register_dynamic_device_support(move |ctx: &epics_base_rs::server::ioc_app::DeviceSupportContext| {
            let plugins = mgr.plugins.lock();
            // Exact DTYP match (port-based: "asynIMAGE1", "asynSTATS1", etc.)
            for p in plugins.iter() {
                if p.dtyp_name == ctx.dtyp {
                    let handle = p.port_handle.clone();
                    let registry = p.registry.clone();
                    let dtyp = p.dtyp_name.clone();
                    let array_data = p.array_data.clone();
                    return Some(
                        Box::new(PluginDeviceSupport::new(handle, registry, &dtyp, array_data))
                            as Box<dyn epics_base_rs::server::device_support::DeviceSupport>,
                    );
                }
            }
            // Generic asyn DTYP (asynInt32, asynFloat64, asynOctet, asynXxxArrayIn/Out, etc.)
            // Parse INP/OUT link to extract port name, then match by port name.
            // This handles NDTimeSeries, NDPluginBase, and NDStdArrays templates that use
            // generic asyn DTYPs with @asyn(PORT,...) links.
            if is_generic_asyn_dtyp(ctx.dtyp) {
                let link_str = if !ctx.inp.is_empty() { ctx.inp } else { ctx.out };
                if let Ok(link) = asyn_rs::adapter::parse_asyn_link(link_str) {
                    for p in plugins.iter() {
                        if p.port_handle.port_name() == link.port_name {
                            let handle = p.port_handle.clone();
                            let registry = p.registry.clone();
                            let dtyp = ctx.dtyp.to_string();
                            let array_data = p.array_data.clone();
                            return Some(
                                Box::new(PluginDeviceSupport::new(handle, registry, &dtyp, array_data))
                                    as Box<dyn epics_base_rs::server::device_support::DeviceSupport>,
                            );
                        }
                    }
                }
                // Fallback for array DTYPs: match first plugin with array_data
                if is_asyn_array_dtyp(ctx.dtyp) {
                    for p in plugins.iter() {
                        if p.array_data.is_some() {
                            let handle = p.port_handle.clone();
                            let registry = p.registry.clone();
                            let dtyp = ctx.dtyp.to_string();
                            let array_data = p.array_data.clone();
                            return Some(
                                Box::new(PluginDeviceSupport::new(handle, registry, &dtyp, array_data))
                                    as Box<dyn epics_base_rs::server::device_support::DeviceSupport>,
                            );
                        }
                    }
                }
            }
            None
        })
    }

    /// Print a report of registered plugins.
    pub fn report(&self) {
        let plugins = self.plugins.lock();
        println!("  Plugins: {}", plugins.len());
        for p in plugins.iter() {
            println!("    - {} (DTYP: {})", p.port_handle.port_name(), p.dtyp_name);
        }
    }
}

/// Check if a DTYP is a generic asyn interface type.
///
/// Any DTYP starting with "asyn" that wasn't matched by the exact port-based
/// lookup is a generic asyn DTYP (asynInt32, asynFloat64, asynOctetRead, etc.).
/// Since exact match runs first, this safely catches all standard asyn DTYPs
/// including those from NDPluginBase (asynOctetRead/Write) and NDTimeSeries.
fn is_generic_asyn_dtyp(dtyp: &str) -> bool {
    dtyp.starts_with("asyn")
}

/// Check if a DTYP name is a standard asyn array device support type.
///
/// These are DTYPs like "asynInt8ArrayIn", "asynFloat64ArrayOut", etc.
/// used by NDStdArrays.template via `DTYP="asyn$(TYPE)ArrayIn"`.
fn is_asyn_array_dtyp(dtyp: &str) -> bool {
    const TYPES: &[&str] = &[
        "Int8", "UInt8", "Int16", "UInt16", "Int32", "UInt32", "Float32", "Float64",
    ];
    for ty in TYPES {
        let in_name = format!("asyn{ty}ArrayIn");
        let out_name = format!("asyn{ty}ArrayOut");
        if dtyp == in_name || dtyp == out_name {
            return true;
        }
    }
    false
}
