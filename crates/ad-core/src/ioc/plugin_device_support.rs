use std::sync::Arc;

use asyn_rs::adapter::AsynDeviceSupport;
use asyn_rs::port_handle::PortHandle;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceSupport, WriteCompletion};
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::EpicsValue;

use crate::ndarray::NDArray;
use crate::plugin::registry::{ParamRegistry, RegistryParamType};

/// Handle to the latest NDArray data from a StdArrays plugin.
pub type ArrayDataHandle = Arc<parking_lot::Mutex<Option<Arc<NDArray>>>>;

/// Device support for any areaDetector plugin.
///
/// Bridges EPICS records to a plugin's asyn port via PortHandle + ParamRegistry.
/// Records whose suffix has no param mapping are treated as no-ops.
/// For StdArrays plugins, the "ArrayData" suffix reads raw pixel data.
pub struct PluginDeviceSupport {
    inner: AsynDeviceSupport,
    registry: Arc<ParamRegistry>,
    dtyp_name: String,
    mapped: bool,
    array_data: Option<ArrayDataHandle>,
    is_array_data: bool,
}

impl PluginDeviceSupport {
    pub fn new(
        handle: PortHandle,
        registry: Arc<ParamRegistry>,
        dtyp_name: &str,
        array_data: Option<ArrayDataHandle>,
    ) -> Self {
        Self::with_addr(handle, registry, dtyp_name, array_data, 0)
    }

    pub fn with_addr(
        handle: PortHandle,
        registry: Arc<ParamRegistry>,
        dtyp_name: &str,
        array_data: Option<ArrayDataHandle>,
        addr: i32,
    ) -> Self {
        use asyn_rs::adapter::AsynLink;
        let link = AsynLink {
            port_name: String::new(),
            addr,
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

        if suffix == "ArrayData" && self.array_data.is_some() {
            self.is_array_data = true;
            // Register for I/O Intr on ArrayCounter_RBV param so the waveform
            // gets processed each time the plugin receives a new array.
            if let Some(counter_info) = self.registry.get("ArrayCounter_RBV") {
                self.inner.set_drv_info(&counter_info.drv_info);
                self.inner.set_reason(counter_info.param_index);
                self.inner.set_iface_type("asynInt32");
                self.mapped = true;
                self.inner.set_record_info(name, scan);
            }
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
