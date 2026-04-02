use std::sync::Arc;

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, PluginRuntimeHandle, ProcessResult};
use ad_core_rs::plugin::wiring::WiringRegistry;
use parking_lot::Mutex;

/// Pure processing logic: stores the latest array and passes it through.
pub struct StdArraysProcessor {
    latest_data: Arc<Mutex<Option<Arc<NDArray>>>>,
}

impl StdArraysProcessor {
    pub fn new() -> Self {
        Self {
            latest_data: Arc::new(Mutex::new(None)),
        }
    }

    /// Get a cloneable handle to the latest array.
    pub fn data_handle(&self) -> Arc<Mutex<Option<Arc<NDArray>>>> {
        self.latest_data.clone()
    }
}

impl Default for StdArraysProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for StdArraysProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let out = Arc::new(array.clone());
        *self.latest_data.lock() = Some(out.clone());
        ProcessResult::arrays(vec![out])
    }

    fn plugin_type(&self) -> &str {
        "NDPluginStdArrays"
    }

    fn array_data_handle(&self) -> Option<Arc<Mutex<Option<Arc<NDArray>>>>> {
        Some(self.latest_data.clone())
    }
}

/// Create a StdArrays plugin runtime.
pub fn create_std_arrays_runtime(
    port_name: &str,
    pool: Arc<NDArrayPool>,
    ndarray_port: &str,
    wiring: Arc<WiringRegistry>,
) -> (
    PluginRuntimeHandle,
    Arc<Mutex<Option<Arc<NDArray>>>>,
    std::thread::JoinHandle<()>,
) {
    let processor = StdArraysProcessor::new();
    let data_handle = processor.data_handle();

    let (handle, data_jh) = ad_core_rs::plugin::runtime::create_plugin_runtime(
        port_name,
        processor,
        pool,
        1, // LatestOnly semantics
        ndarray_port,
        wiring,
    );

    (handle, data_handle, data_jh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    fn make_array(id: i32) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        Arc::new(arr)
    }

    #[test]
    fn test_processor_stores_and_passes_through() {
        let mut proc = StdArraysProcessor::new();
        let pool = NDArrayPool::new(1_000_000);

        let arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);

        let latest = proc.data_handle().lock().clone();
        assert!(latest.is_some());
    }

    #[test]
    fn test_std_arrays_runtime() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let wiring = Arc::new(WiringRegistry::new());
        let (handle, data, _jh) = create_std_arrays_runtime("IMAGE1", pool, "", wiring);

        // Plugins default to disabled — enable for test
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        handle.array_sender().send(make_array(42));
        std::thread::sleep(std::time::Duration::from_millis(100));

        let latest = data.lock().clone();
        assert!(latest.is_some());
        assert_eq!(latest.unwrap().unique_id, 42);
    }
}
