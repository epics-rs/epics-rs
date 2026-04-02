use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;

/// Standard plugin base parameters registered on the PortDriverBase.
///
/// Param names match C ADCore NDPluginDriver.h string definitions.
#[derive(Clone, Copy)]
pub struct PluginBaseParams {
    pub enable_callbacks: usize,
    pub blocking_callbacks: usize,
    pub queue_size: usize,
    pub dropped_arrays: usize,
    pub queue_use: usize,
    pub nd_array_port: usize,
    pub nd_array_addr: usize,
    pub plugin_type: usize,
    pub execution_time: usize,
    pub max_threads: usize,
    pub num_threads: usize,
    pub sort_mode: usize,
    pub sort_time: usize,
    pub sort_size: usize,
    pub sort_free: usize,
    pub disordered_arrays: usize,
    pub dropped_output_arrays: usize,
    pub process_plugin: usize,
    pub min_callback_time: usize,
    pub max_byte_rate: usize,
}

impl PluginBaseParams {
    /// Register all base plugin params on the given port.
    pub fn create(port_base: &mut PortDriverBase) -> AsynResult<Self> {
        Ok(Self {
            enable_callbacks: port_base.create_param("ENABLE_CALLBACKS", ParamType::Int32)?,
            blocking_callbacks: port_base.create_param("BLOCKING_CALLBACKS", ParamType::Int32)?,
            queue_size: port_base.create_param("QUEUE_SIZE", ParamType::Int32)?,
            dropped_arrays: port_base.create_param("DROPPED_ARRAYS", ParamType::Int32)?,
            queue_use: port_base.create_param("QUEUE_FREE", ParamType::Int32)?,
            nd_array_port: port_base.create_param("NDARRAY_PORT", ParamType::Octet)?,
            nd_array_addr: port_base.create_param("NDARRAY_ADDR", ParamType::Int32)?,
            plugin_type: port_base.create_param("PLUGIN_TYPE", ParamType::Octet)?,
            execution_time: port_base.create_param("EXECUTION_TIME", ParamType::Float64)?,
            max_threads: port_base.create_param("MAX_THREADS", ParamType::Int32)?,
            num_threads: port_base.create_param("NUM_THREADS", ParamType::Int32)?,
            sort_mode: port_base.create_param("SORT_MODE", ParamType::Int32)?,
            sort_time: port_base.create_param("SORT_TIME", ParamType::Float64)?,
            sort_size: port_base.create_param("SORT_SIZE", ParamType::Int32)?,
            sort_free: port_base.create_param("SORT_FREE", ParamType::Int32)?,
            disordered_arrays: port_base.create_param("DISORDERED_ARRAYS", ParamType::Int32)?,
            dropped_output_arrays: port_base
                .create_param("DROPPED_OUTPUT_ARRAYS", ParamType::Int32)?,
            process_plugin: port_base.create_param("PROCESS_PLUGIN", ParamType::Int32)?,
            min_callback_time: port_base.create_param("MIN_CALLBACK_TIME", ParamType::Float64)?,
            max_byte_rate: port_base.create_param("MAX_BYTE_RATE", ParamType::Float64)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asyn_rs::port::PortFlags;

    #[test]
    fn test_create_plugin_base_params() {
        let mut base = PortDriverBase::new("test", 1, PortFlags::default());
        let params = PluginBaseParams::create(&mut base).unwrap();
        assert!(base.find_param("ENABLE_CALLBACKS").is_some());
        assert!(base.find_param("QUEUE_SIZE").is_some());
        assert!(base.find_param("PLUGIN_TYPE").is_some());
        assert!(base.find_param("SORT_MODE").is_some());
        assert!(base.find_param("MAX_THREADS").is_some());
        assert!(base.find_param("MIN_CALLBACK_TIME").is_some());
        assert_eq!(
            params.enable_callbacks,
            base.find_param("ENABLE_CALLBACKS").unwrap()
        );
    }
}
