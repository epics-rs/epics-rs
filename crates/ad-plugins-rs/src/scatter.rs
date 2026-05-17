use std::sync::Arc;

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};

/// Scatter method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScatterMethod {
    RoundRobin = 0,
}

impl ScatterMethod {
    pub fn from_i32(_v: i32) -> Self {
        Self::RoundRobin
    }
}

/// Scatter processor: distributes arrays to downstream plugins in round-robin
/// order.
///
/// The actual number of downstream consumers is discovered by the plugin
/// runtime, which publishes a scatter result to `scatter_index % senders.len()`.
/// `num_outputs == 0` (the default) means "use the live downstream count": the
/// processor emits a raw incrementing index and the runtime maps it onto the
/// connected consumers — equivalent to C++ NDPluginScatter advancing
/// `nextScatter` over its registered clients. A non-zero `num_outputs` caps the
/// round-robin span for tests / fixed fan-out.
pub struct ScatterProcessor {
    method: ScatterMethod,
    current_index: usize,
    num_outputs: usize,
    method_idx: Option<usize>,
}

impl ScatterProcessor {
    pub fn new() -> Self {
        Self {
            method: ScatterMethod::RoundRobin,
            current_index: 0,
            num_outputs: 0,
            method_idx: None,
        }
    }

    /// Override the round-robin span. `0` means "use the live downstream
    /// consumer count" (the default).
    pub fn set_num_outputs(&mut self, n: usize) {
        self.num_outputs = n;
    }
}

impl Default for ScatterProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for ScatterProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let idx = if self.num_outputs > 0 {
            self.current_index % self.num_outputs
        } else {
            self.current_index
        };
        self.current_index = self.current_index.wrapping_add(1);
        ProcessResult::scatter(vec![Arc::new(array.clone())], idx)
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
        self.method_idx = base.find_param("SCATTER_METHOD");
        Ok(())
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        if Some(reason) == self.method_idx {
            self.method = ScatterMethod::from_i32(params.value.as_i32());
        }
        ad_core_rs::plugin::runtime::ParamChangeResult::updates(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    #[test]
    fn test_scatter_processor_round_robin() {
        let mut proc = ScatterProcessor::new();
        proc.set_num_outputs(3);
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = 42;

        let r0 = proc.process_array(&arr, &pool);
        assert_eq!(r0.scatter_index, Some(0));
        assert_eq!(r0.output_arrays.len(), 1);

        let r1 = proc.process_array(&arr, &pool);
        assert_eq!(r1.scatter_index, Some(1));

        let r2 = proc.process_array(&arr, &pool);
        assert_eq!(r2.scatter_index, Some(2));

        // Should wrap around
        let r3 = proc.process_array(&arr, &pool);
        assert_eq!(r3.scatter_index, Some(0));
    }

    #[test]
    fn test_scatter_default_emits_raw_index() {
        // With the default num_outputs == 0 the processor emits a raw
        // incrementing index; the runtime maps it onto the actual downstream
        // consumer count (idx % senders.len()), so scatter does NOT degenerate
        // to a passthrough on consumer 0.
        let mut proc = ScatterProcessor::new();
        let pool = NDArrayPool::new(1_000_000);
        let arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);

        let indices: Vec<_> = (0..4)
            .map(|_| proc.process_array(&arr, &pool).scatter_index.unwrap())
            .collect();
        // Raw monotonically increasing indices — not all 0.
        assert_eq!(indices, vec![0, 1, 2, 3]);
    }
}
