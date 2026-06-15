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

    /// C `NDPluginStdArrays.cpp:343` sets `NDArrayCallbacks = 0`: this plugin
    /// serves pixel data via the StdArray waveforms, not downstream callbacks.
    fn does_array_callbacks(&self) -> bool {
        false
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

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(handle.array_sender().publish(make_array(42)));
        std::thread::sleep(std::time::Duration::from_millis(100));

        let latest = data.lock().clone();
        assert!(latest.is_some());
        assert_eq!(latest.unwrap().unique_id, 42);
    }

    #[test]
    fn test_std_arrays_initial_array_callbacks_off() {
        // C NDPluginStdArrays.cpp:343 sets NDArrayCallbacks=0 in the
        // constructor; the initial param a client reads must be 0, not 1.
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let wiring = Arc::new(WiringRegistry::new());
        let (handle, _data, _jh) = create_std_arrays_runtime("IMAGE1", pool, "", wiring);

        let val = handle
            .port_runtime()
            .port_handle()
            .read_int32_blocking(handle.ndarray_params.array_callbacks, 0)
            .unwrap();
        assert_eq!(val, 0, "StdArrays initial NDArrayCallbacks must be 0");
    }

    #[test]
    fn test_terminal_plugins_do_not_do_array_callbacks() {
        // C disables NDArrayCallbacks in the constructor of NDPluginStdArrays
        // (:343), NDPluginAttribute (:203), and NDPluginFile (:948, base of
        // every file writer). Each Rust counterpart overrides
        // does_array_callbacks() to false so the initial param reflects this.
        use crate::attribute::AttributeProcessor;
        use crate::file_hdf5::Hdf5FileProcessor;
        use crate::file_jpeg::JpegFileProcessor;
        use crate::file_magick::MagickFileProcessor;
        use crate::file_netcdf::NetcdfFileProcessor;
        use crate::file_nexus::NexusFileProcessor;
        use crate::file_tiff::TiffFileProcessor;
        use crate::passthrough::PassthroughProcessor;

        assert!(!StdArraysProcessor::new().does_array_callbacks());
        assert!(!AttributeProcessor::new("attr", 1).does_array_callbacks());
        assert!(!Hdf5FileProcessor::new().does_array_callbacks());
        assert!(!JpegFileProcessor::new(85).does_array_callbacks());
        assert!(!TiffFileProcessor::new().does_array_callbacks());
        assert!(!NetcdfFileProcessor::new().does_array_callbacks());
        assert!(!NexusFileProcessor::new().does_array_callbacks());
        assert!(!MagickFileProcessor::new().does_array_callbacks());

        // A non-terminal plugin keeps the default: it does deliver downstream.
        assert!(PassthroughProcessor::new("NDPluginProcess").does_array_callbacks());
    }
}
