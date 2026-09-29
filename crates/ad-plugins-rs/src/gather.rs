use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};

/// Pure gather processing logic: every array from every source goes out as
/// it came in.
///
/// The sources are the `(NDArrayPort, NDArrayAddr)` pairs at addresses
/// `0..maxPorts`, which NDGatherN.template writes one per address; the
/// runtime wires one input per pair (`num_array_sources`), as
/// `NDPluginGather::connectToArrayPort` connects one asynUser per
/// `maxPorts_` (NDPluginGather.cpp:134-186).
pub struct GatherProcessor {
    max_ports: usize,
    /// Total arrays received across all sources.
    count: AtomicU64,
}

impl GatherProcessor {
    /// `max_ports` is the C constructor's `maxPorts`, floored to 1 as
    /// NDPluginGather.cpp:58 does; the port that serves the sources has the
    /// same number of addresses (`:45`).
    pub fn new(max_ports: usize) -> Self {
        Self {
            max_ports: max_ports.max(1),
            count: AtomicU64::new(0),
        }
    }

    pub fn max_ports(&self) -> usize {
        self.max_ports
    }

    pub fn total_received(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

impl NDPluginProcess for GatherProcessor {
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        self.count.fetch_add(1, Ordering::Relaxed);
        ProcessResult::forward(array, vec![])
    }

    fn plugin_type(&self) -> &str {
        "NDPluginGather"
    }

    fn num_array_sources(&self) -> usize {
        self.max_ports
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    #[test]
    fn test_gather_processor_passthrough() {
        let proc = GatherProcessor::new(8);
        let pool = NDArrayPool::new(1_000_000);

        let arr1 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        let arr2 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);

        let arr1 = Arc::new(arr1);
        let result1 = proc.process_array(&arr1, &pool);
        let arr2 = Arc::new(arr2);
        let result2 = proc.process_array(&arr2, &pool);

        assert!(Arc::ptr_eq(&result1.output_arrays[0], &arr1) && result1.output_arrays.len() == 1);
        assert!(Arc::ptr_eq(&result2.output_arrays[0], &arr2) && result2.output_arrays.len() == 1);
        assert_eq!(proc.total_received(), 2);
    }

    #[test]
    fn one_array_source_per_port_and_at_least_one() {
        assert_eq!(GatherProcessor::new(8).num_array_sources(), 8);
        assert_eq!(GatherProcessor::new(0).num_array_sources(), 1);
        assert_eq!(GatherProcessor::new(0).max_ports(), 1);
    }
}
