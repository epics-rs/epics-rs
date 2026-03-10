use std::sync::Arc;

use ad_core::ndarray::NDArray;
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::NDPluginProcess;

/// Pure gather processing logic (passthrough — gathers from multiple senders into one stream).
pub struct GatherProcessor {
    count: u64,
}

impl GatherProcessor {
    pub fn new() -> Self {
        Self { count: 0 }
    }

    pub fn total_received(&self) -> u64 {
        self.count
    }
}

impl Default for GatherProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for GatherProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        self.count += 1;
        vec![Arc::new(array.clone())]
    }

    fn plugin_type(&self) -> &str {
        "NDPluginGather"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core::ndarray::{NDDataType, NDDimension};

    #[test]
    fn test_gather_processor() {
        let mut proc = GatherProcessor::new();
        let pool = NDArrayPool::new(1_000_000);

        let arr1 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        let arr2 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);

        let out1 = proc.process_array(&arr1, &pool);
        let out2 = proc.process_array(&arr2, &pool);

        assert_eq!(out1.len(), 1);
        assert_eq!(out2.len(), 1);
        assert_eq!(proc.total_received(), 2);
    }
}
