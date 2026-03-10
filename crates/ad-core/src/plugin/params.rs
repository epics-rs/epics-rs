use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;

/// Standard plugin base parameters registered on the PortDriverBase.
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
}

impl PluginBaseParams {
    /// Register all base plugin params on the given port.
    pub fn create(port_base: &mut PortDriverBase) -> AsynResult<Self> {
        Ok(Self {
            enable_callbacks: port_base.create_param("PLUGIN_ENABLE_CALLBACKS", ParamType::Int32)?,
            blocking_callbacks: port_base
                .create_param("PLUGIN_BLOCKING_CALLBACKS", ParamType::Int32)?,
            queue_size: port_base.create_param("PLUGIN_QUEUE_SIZE", ParamType::Int32)?,
            dropped_arrays: port_base.create_param("PLUGIN_DROPPED_ARRAYS", ParamType::Int32)?,
            queue_use: port_base.create_param("PLUGIN_QUEUE_USE", ParamType::Int32)?,
            nd_array_port: port_base.create_param("PLUGIN_NDARRAY_PORT", ParamType::Octet)?,
            nd_array_addr: port_base.create_param("PLUGIN_NDARRAY_ADDR", ParamType::Int32)?,
            plugin_type: port_base.create_param("PLUGIN_TYPE", ParamType::Octet)?,
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
        assert!(base.find_param("PLUGIN_ENABLE_CALLBACKS").is_some());
        assert!(base.find_param("PLUGIN_QUEUE_SIZE").is_some());
        assert!(base.find_param("PLUGIN_TYPE").is_some());
        assert_eq!(
            params.enable_callbacks,
            base.find_param("PLUGIN_ENABLE_CALLBACKS").unwrap()
        );
    }
}
