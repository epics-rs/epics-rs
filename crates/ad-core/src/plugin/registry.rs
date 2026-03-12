//! Parameter registry: maps EPICS record name suffixes to asyn param indices.
//!
//! This is the bridge between EPICS .template record naming conventions
//! (e.g. "ArraySizeX_RBV", "EnableCallbacks") and the asyn param system
//! (integer indices + drvInfo strings).

use std::collections::HashMap;

use super::params::PluginBaseParams;
use super::runtime::PluginRuntimeHandle;
use crate::params::ndarray_driver::NDArrayDriverParams;

/// Type classification for param registry entries.
#[derive(Clone, Copy)]
pub enum RegistryParamType {
    Int32,
    Float64,
    Float64Array,
    OctetString,
}

/// Maps a record name suffix to an asyn param index + type + drvInfo.
#[derive(Clone)]
pub struct ParamInfo {
    pub param_index: usize,
    pub param_type: RegistryParamType,
    pub drv_info: String,
}

impl ParamInfo {
    pub fn int32(index: usize, drv_info: &str) -> Self {
        Self { param_index: index, param_type: RegistryParamType::Int32, drv_info: drv_info.to_string() }
    }
    pub fn float64(index: usize, drv_info: &str) -> Self {
        Self { param_index: index, param_type: RegistryParamType::Float64, drv_info: drv_info.to_string() }
    }
    pub fn float64_array(index: usize, drv_info: &str) -> Self {
        Self { param_index: index, param_type: RegistryParamType::Float64Array, drv_info: drv_info.to_string() }
    }
    pub fn string(index: usize, drv_info: &str) -> Self {
        Self { param_index: index, param_type: RegistryParamType::OctetString, drv_info: drv_info.to_string() }
    }
}

/// Registry mapping record name suffixes to parameter info.
pub type ParamRegistry = HashMap<String, ParamInfo>;

/// Build the base parameter registry common to all plugins.
///
/// Covers NDArrayDriverParams (array info, pool stats) and PluginBaseParams
/// (enable callbacks, queue, plugin type, etc.).
pub fn build_plugin_base_registry(h: &PluginRuntimeHandle) -> ParamRegistry {
    let mut map = HashMap::new();
    let base = &h.ndarray_params;
    let plug = &h.plugin_params;

    insert_ndarray_driver_params(&mut map, base);
    insert_plugin_base_params(&mut map, plug);

    map
}

/// Insert NDArrayDriverParams mappings into a registry.
pub fn insert_ndarray_driver_params(map: &mut ParamRegistry, base: &NDArrayDriverParams) {
    map.insert("PortName_RBV".into(), ParamInfo::string(base.port_name_self, "PORT_NAME_SELF"));
    map.insert("ArrayCounter".into(), ParamInfo::int32(base.array_counter, "ARRAY_COUNTER"));
    map.insert("ArrayCounter_RBV".into(), ParamInfo::int32(base.array_counter, "ARRAY_COUNTER"));
    map.insert("ArrayCallbacks".into(), ParamInfo::int32(base.array_callbacks, "ARRAY_CALLBACKS"));
    map.insert("ArrayCallbacks_RBV".into(), ParamInfo::int32(base.array_callbacks, "ARRAY_CALLBACKS"));
    map.insert("ArraySizeX_RBV".into(), ParamInfo::int32(base.array_size_x, "ARRAY_SIZE_X"));
    map.insert("ArraySizeY_RBV".into(), ParamInfo::int32(base.array_size_y, "ARRAY_SIZE_Y"));
    map.insert("ArraySizeZ_RBV".into(), ParamInfo::int32(base.array_size_z, "ARRAY_SIZE_Z"));
    map.insert("ArraySize_RBV".into(), ParamInfo::int32(base.array_size, "ARRAY_SIZE"));
    // ArraySize0/1/2_RBV: used by NDPluginBase screens (bypass Dimensions waveform chain)
    map.insert("ArraySize0_RBV".into(), ParamInfo::int32(base.array_size_x, "ARRAY_SIZE_X"));
    map.insert("ArraySize1_RBV".into(), ParamInfo::int32(base.array_size_y, "ARRAY_SIZE_Y"));
    map.insert("ArraySize2_RBV".into(), ParamInfo::int32(base.array_size_z, "ARRAY_SIZE_Z"));
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

    // Pool stats
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
}

/// Insert PluginBaseParams mappings into a registry.
pub fn insert_plugin_base_params(map: &mut ParamRegistry, plug: &PluginBaseParams) {
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
    map.insert("ExecutionTime_RBV".into(), ParamInfo::float64(plug.execution_time, "EXECUTION_TIME"));
}
