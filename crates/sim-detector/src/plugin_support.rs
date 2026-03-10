//! Plugin device support for the SimDetector IOC.
//!
//! Provides a generic `PluginDeviceSupport` that bridges EPICS records to
//! any plugin's asyn port via PortHandle + ParamRegistry.

use std::collections::HashMap;
use std::sync::Arc;

use asyn_rs::adapter::AsynDeviceSupport;
use asyn_rs::port_handle::PortHandle;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceSupport, WriteCompletion};
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::EpicsValue;

use ad_core::ndarray::NDArray;
use ad_core::plugin::runtime::PluginRuntimeHandle;
use crate::ioc_support::{ParamInfo, ParamRegistry, ParamType};

/// Handle to the latest NDArray data from a StdArrays plugin.
pub type ArrayDataHandle = Arc<parking_lot::Mutex<Option<Arc<NDArray>>>>;

/// Build the parameter registry for plugin base params from a PluginRuntimeHandle.
pub fn build_plugin_base_registry(h: &PluginRuntimeHandle) -> ParamRegistry {
    let mut map = HashMap::new();
    let base = &h.ndarray_params;
    let plug = &h.plugin_params;

    // NDArrayDriverParams (subset relevant to plugins)
    map.insert("PortName_RBV".into(), ParamInfo::string(base.port_name_self, "PORT_NAME_SELF"));
    map.insert("ArrayCounter".into(), ParamInfo::int32(base.array_counter, "ARRAY_COUNTER"));
    map.insert("ArrayCounter_RBV".into(), ParamInfo::int32(base.array_counter, "ARRAY_COUNTER"));
    map.insert("ArrayCallbacks".into(), ParamInfo::int32(base.array_callbacks, "ARRAY_CALLBACKS"));
    map.insert("ArrayCallbacks_RBV".into(), ParamInfo::int32(base.array_callbacks, "ARRAY_CALLBACKS"));
    map.insert("ArraySizeX_RBV".into(), ParamInfo::int32(base.array_size_x, "ARRAY_SIZE_X"));
    map.insert("ArraySizeY_RBV".into(), ParamInfo::int32(base.array_size_y, "ARRAY_SIZE_Y"));
    map.insert("ArraySizeZ_RBV".into(), ParamInfo::int32(base.array_size_z, "ARRAY_SIZE_Z"));
    map.insert("ArraySize_RBV".into(), ParamInfo::int32(base.array_size, "ARRAY_SIZE"));
    map.insert("NDimensions".into(), ParamInfo::int32(base.n_dimensions, "NDIMENSIONS"));
    map.insert("NDimensions_RBV".into(), ParamInfo::int32(base.n_dimensions, "NDIMENSIONS"));
    map.insert("DataType".into(), ParamInfo::int32(base.data_type, "DATA_TYPE"));
    map.insert("DataType_RBV".into(), ParamInfo::int32(base.data_type, "DATA_TYPE"));
    map.insert("ColorMode".into(), ParamInfo::int32(base.color_mode, "COLOR_MODE"));
    map.insert("ColorMode_RBV".into(), ParamInfo::int32(base.color_mode, "COLOR_MODE"));
    map.insert("UniqueId_RBV".into(), ParamInfo::int32(base.unique_id, "UNIQUE_ID"));
    map.insert("BayerPattern_RBV".into(), ParamInfo::int32(base.bayer_pattern, "BAYER_PATTERN"));
    map.insert("Codec_RBV".into(), ParamInfo::string(base.codec, "CODEC"));
    map.insert("CompressedSize_RBV".into(), ParamInfo::int32(base.compressed_size, "COMPRESSED_SIZE"));
    map.insert("TimeStamp_RBV".into(), ParamInfo::float64(base.timestamp_rbv, "TIMESTAMP"));
    map.insert("EpicsTSSec_RBV".into(), ParamInfo::int32(base.epics_ts_sec, "EPICS_TS_SEC"));
    map.insert("EpicsTSNsec_RBV".into(), ParamInfo::int32(base.epics_ts_nsec, "EPICS_TS_NSEC"));

    // Pool stats (Float64 for memory, Int32 for counts)
    map.insert("PoolMaxMem".into(), ParamInfo::float64(base.pool_max_memory, "POOL_MAX_MEMORY"));
    map.insert("PoolUsedMem".into(), ParamInfo::float64(base.pool_used_memory, "POOL_USED_MEMORY"));
    map.insert("PoolAllocBuffers".into(), ParamInfo::int32(base.pool_alloc_buffers, "POOL_ALLOC_BUFFERS"));
    map.insert("PoolAllocBuffers_RBV".into(), ParamInfo::int32(base.pool_alloc_buffers, "POOL_ALLOC_BUFFERS"));
    map.insert("PoolFreeBuffers".into(), ParamInfo::int32(base.pool_free_buffers, "POOL_FREE_BUFFERS"));
    map.insert("PoolFreeBuffers_RBV".into(), ParamInfo::int32(base.pool_free_buffers, "POOL_FREE_BUFFERS"));
    map.insert("PoolMaxBuffers_RBV".into(), ParamInfo::int32(base.pool_max_buffers, "POOL_MAX_BUFFERS"));
    map.insert("PoolPollStats".into(), ParamInfo::int32(base.pool_poll_stats, "POOL_POLL_STATS"));
    map.insert("NumQueuedArrays".into(), ParamInfo::int32(base.num_queued_arrays, "NUM_QUEUED_ARRAYS"));
    map.insert("NumQueuedArrays_RBV".into(), ParamInfo::int32(base.num_queued_arrays, "NUM_QUEUED_ARRAYS"));

    // PluginBaseParams
    map.insert("EnableCallbacks".into(), ParamInfo::int32(plug.enable_callbacks, "PLUGIN_ENABLE_CALLBACKS"));
    map.insert("EnableCallbacks_RBV".into(), ParamInfo::int32(plug.enable_callbacks, "PLUGIN_ENABLE_CALLBACKS"));
    map.insert("BlockingCallbacks".into(), ParamInfo::int32(plug.blocking_callbacks, "PLUGIN_BLOCKING_CALLBACKS"));
    map.insert("BlockingCallbacks_RBV".into(), ParamInfo::int32(plug.blocking_callbacks, "PLUGIN_BLOCKING_CALLBACKS"));
    map.insert("QueueSize".into(), ParamInfo::int32(plug.queue_size, "PLUGIN_QUEUE_SIZE"));
    map.insert("QueueFree".into(), ParamInfo::int32(plug.queue_use, "PLUGIN_QUEUE_USE"));
    map.insert("DroppedArrays".into(), ParamInfo::int32(plug.dropped_arrays, "PLUGIN_DROPPED_ARRAYS"));
    map.insert("DroppedArrays_RBV".into(), ParamInfo::int32(plug.dropped_arrays, "PLUGIN_DROPPED_ARRAYS"));
    map.insert("NDArrayPort".into(), ParamInfo::string(plug.nd_array_port, "PLUGIN_NDARRAY_PORT"));
    map.insert("NDArrayPort_RBV".into(), ParamInfo::string(plug.nd_array_port, "PLUGIN_NDARRAY_PORT"));
    map.insert("NDArrayAddress".into(), ParamInfo::int32(plug.nd_array_addr, "PLUGIN_NDARRAY_ADDR"));
    map.insert("NDArrayAddress_RBV".into(), ParamInfo::int32(plug.nd_array_addr, "PLUGIN_NDARRAY_ADDR"));
    map.insert("PluginType_RBV".into(), ParamInfo::string(plug.plugin_type, "PLUGIN_TYPE"));

    map
}

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
            inner: AsynDeviceSupport::from_handle(handle, link, "asynInt32"),
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
                ParamType::Int32 => "asynInt32",
                ParamType::Float64 => "asynFloat64",
                ParamType::OctetString => "asynOctet",
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
