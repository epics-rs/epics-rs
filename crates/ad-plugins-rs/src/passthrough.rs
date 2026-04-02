//! Generic passthrough plugin processor.
//!
//! Used as a stub for plugin types that are not yet fully implemented
//! but need to appear in the OPI with correct metadata.

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};

/// A no-op plugin processor that passes arrays through unchanged.
pub struct PassthroughProcessor {
    plugin_type: String,
}

impl PassthroughProcessor {
    pub fn new(plugin_type: &str) -> Self {
        Self {
            plugin_type: plugin_type.to_string(),
        }
    }
}

impl NDPluginProcess for PassthroughProcessor {
    fn plugin_type(&self) -> &str {
        &self.plugin_type
    }

    fn process_array(&mut self, _array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        ProcessResult::empty()
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        // Plugin-specific params based on plugin type
        if self.plugin_type.as_str() == "NDPvaConfigure" {
            base.create_param("PV_NAME", ParamType::Octet)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passthrough_plugin_type() {
        let p = PassthroughProcessor::new("NDPluginAttribute");
        assert_eq!(p.plugin_type(), "NDPluginAttribute");
    }
}
