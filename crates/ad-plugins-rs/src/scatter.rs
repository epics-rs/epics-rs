use std::sync::Arc;

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};

/// Scatter processor: passes through arrays. Round-robin distribution is handled
/// by wiring multiple NDArraySender instances downstream.
pub struct ScatterProcessor;

impl ScatterProcessor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ScatterProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for ScatterProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        ProcessResult::arrays(vec![Arc::new(array.clone())])
    }

    fn plugin_type(&self) -> &str {
        "NDPluginScatter"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        base.create_param("SCATTER_METHOD", ParamType::Int32)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    #[test]
    fn test_scatter_processor_passthrough() {
        let mut proc = ScatterProcessor::new();
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = 42;

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(result.output_arrays[0].unique_id, 42);
    }
}
