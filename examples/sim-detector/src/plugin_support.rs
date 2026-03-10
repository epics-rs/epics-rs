//! Plugin device support for the SimDetector IOC.
//!
//! Provides a generic `PluginDeviceSupport` that bridges EPICS records to
//! any plugin's asyn port via PortHandle + ParamRegistry.
//!
//! The core types (`ParamInfo`, `ParamRegistry`, `build_plugin_base_registry`)
//! live in `ad_core::plugin::registry`. This module re-exports them for
//! convenience and provides the EPICS device support wrapper.

use std::sync::Arc;

use asyn_rs::adapter::AsynDeviceSupport;
use asyn_rs::port_handle::PortHandle;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceSupport, WriteCompletion};
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::EpicsValue;

use ad_core::ndarray::NDArray;
use ad_core::plugin::registry::{ParamInfo, ParamRegistry, RegistryParamType};

// Re-export so existing IOC code doesn't need to change import paths too much.
pub use ad_core::plugin::registry::build_plugin_base_registry;

/// Handle to the latest NDArray data from a StdArrays plugin.
pub type ArrayDataHandle = Arc<parking_lot::Mutex<Option<Arc<NDArray>>>>;

/// Device support for any areaDetector plugin.
/// Wraps AsynDeviceSupport with a PortHandle and ParamRegistry.
/// Records whose suffix has no param mapping are treated as no-ops.
/// For StdArrays plugins, the "ArrayData" suffix is handled specially
/// by reading raw pixel data from the plugin's data handle.
pub struct PluginDeviceSupport {
    inner: AsynDeviceSupport,
    registry: Arc<ParamRegistry>,
    dtyp_name: String,
    /// True if this record's suffix was found in the param registry.
    mapped: bool,
    /// Handle to latest NDArray data (only set for StdArrays plugins).
    array_data: Option<ArrayDataHandle>,
    /// True if this record is the ArrayData waveform.
    is_array_data: bool,
}

impl PluginDeviceSupport {
    pub fn new(
        handle: PortHandle,
        registry: Arc<ParamRegistry>,
        dtyp_name: &str,
        array_data: Option<ArrayDataHandle>,
    ) -> Self {
        use asyn_rs::adapter::AsynLink;
        let link = AsynLink {
            port_name: String::new(),
            addr: 0,
            timeout: std::time::Duration::from_secs(1),
            drv_info: String::new(),
        };
        Self {
            inner: AsynDeviceSupport::from_handle(handle, link, "asynInt32")
                .with_initial_readback(),
            registry,
            dtyp_name: dtyp_name.to_string(),
            mapped: false,
            array_data,
            is_array_data: false,
        }
    }
}

impl DeviceSupport for PluginDeviceSupport {
    fn dtyp(&self) -> &str {
        &self.dtyp_name
    }

    fn set_record_info(&mut self, name: &str, scan: ScanType) {
        let suffix = name.rsplit(':').next().unwrap_or(name);

        // ArrayData waveform: read pixel data directly from data handle
        if suffix == "ArrayData" && self.array_data.is_some() {
            self.is_array_data = true;
            return;
        }

        if let Some(info) = self.registry.get(suffix) {
            self.inner.set_drv_info(&info.drv_info);
            self.inner.set_reason(info.param_index);
            let iface = match info.param_type {
                RegistryParamType::Int32 => "asynInt32",
                RegistryParamType::Float64 => "asynFloat64",
                RegistryParamType::Float64Array => "asynFloat64Array",
                RegistryParamType::OctetString => "asynOctet",
            };
            self.inner.set_iface_type(iface);
            self.mapped = true;
            self.inner.set_record_info(name, scan);
        }
        // Unmapped suffixes are silently ignored — no asyn wiring, reads/writes are no-ops.
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        if self.is_array_data || !self.mapped { Ok(()) } else { self.inner.init(record) }
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<()> {
        if self.is_array_data {
            if let Some(ref data_handle) = self.array_data {
                let guard = data_handle.lock();
                if let Some(ref array) = *guard {
                    let bytes = array.data.as_u8_slice();
                    record.set_val(EpicsValue::CharArray(bytes.to_vec()))?;
                }
            }
            return Ok(());
        }
        if self.mapped { self.inner.read(record) } else { Ok(()) }
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()> {
        if self.mapped { self.inner.write(record) } else { Ok(()) }
    }

    fn write_begin(&mut self, record: &mut dyn Record) -> CaResult<Option<Box<dyn WriteCompletion>>> {
        if self.mapped { self.inner.write_begin(record) } else { Ok(None) }
    }

    fn last_alarm(&self) -> Option<(u16, u16)> {
        if self.mapped { self.inner.last_alarm() } else { None }
    }

    fn last_timestamp(&self) -> Option<std::time::SystemTime> {
        if self.mapped { self.inner.last_timestamp() } else { None }
    }

    fn io_intr_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<()>> {
        if self.mapped { self.inner.io_intr_receiver() } else { None }
    }
}
