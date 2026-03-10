use std::sync::Arc;

use ad_core::ndarray::NDArray;
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::NDPluginProcess;

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
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        vec![Arc::new(array.clone())]
    }

    fn plugin_type(&self) -> &str {
        "NDPluginScatter"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core::ndarray::{NDDataType, NDDimension};

    #[test]
    fn test_scatter_processor_passthrough() {
        let mut proc = ScatterProcessor::new();
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = 42;

        let outputs = proc.process_array(&arr, &pool);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].unique_id, 42);
    }
}
