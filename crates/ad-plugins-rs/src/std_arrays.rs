// RTEMS-EXEC-MODEL-ALLOW(3): checked, not waived — all 3 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p ad-plugins-rs
// --all-features`, 556/556). ad-plugins-rs became a census subject when
// its `build.rs` began deriving `tokio_backend`; nothing here builds a
// CA server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.
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
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
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

    /// Fence: `write_*_blocking` only queues param changes for the data
    /// thread; the barrier ack proves they have been applied.
    fn params_applied(handle: &PluginRuntimeHandle) {
        assert!(
            handle.wait_params_applied(std::time::Duration::from_secs(10)),
            "data thread did not apply queued param changes"
        );
    }

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !cond() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn test_processor_stores_and_passes_through() {
        let proc = StdArraysProcessor::new();
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
        params_applied(&handle);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(handle.array_sender().publish(make_array(42)));
        wait_until("StdArrays to store the published array", || {
            data.lock().as_ref().is_some_and(|a| a.unique_id == 42)
        });
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
        // C disables NDArrayCallbacks in the constructor of `NDPluginStdArrays.cpp` (`:343`),
        // `NDPluginAttribute.cpp` (`:203`), and `NDPluginFile.cpp` (`:948`, base of every
        // file writer). Each Rust counterpart overrides `does_array_callbacks()` to false so
        // the initial param reflects this.
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

    #[test]
    fn test_std_arrays_serves_waveform_with_callbacks_off() {
        // C NDPluginStdArrays fires its typed-array waveform callbacks
        // (NDPluginStdArrays.cpp:71-73) regardless of NDArrayCallbacks — and it
        // defaults NDArrayCallbacks=0. So the ArrayData waveform on I/O Intr
        // must still update with NDArrayCallbacks=0. This guards against the
        // STD_ARRAY_DATA interrupt being gated by the downstream-delivery flag.
        use asyn_rs::param::ParamValue;

        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let wiring = Arc::new(WiringRegistry::new());
        let (handle, _data, _jh) = create_std_arrays_runtime("IMAGE1", pool, "", wiring);

        let port = handle.port_runtime().port_handle();
        port.write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        // Force NDArrayCallbacks=0 at runtime (StdArrays' C default) — the
        // waveform output must still fire.
        port.write_int32_blocking(handle.ndarray_params.array_callbacks, 0, 0)
            .unwrap();
        params_applied(&handle);

        let mut rx = port.interrupts().subscribe_async();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(handle.array_sender().publish(make_array(7)));
        // ArrayCounter==1 ⟹ the frame was processed and its param batch
        // (including the waveform interrupts) flushed.
        wait_until("StdArrays to process the frame", || {
            port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                .is_ok_and(|v| v == 1)
        });

        // make_array(7) is a 4-element UInt8 array → the STD_ARRAY_DATA
        // interrupt carries it as an Int8Array; the dimensions interrupt carries
        // an Int32Array. Finding an Int8Array interrupt proves the waveform was
        // served despite NDArrayCallbacks=0.
        let mut served = false;
        loop {
            match rx.try_recv() {
                Ok(v) => {
                    if matches!(v.value, ParamValue::Int8Array(_)) {
                        served = true;
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        assert!(
            served,
            "StdArrays must serve STD_ARRAY_DATA with NDArrayCallbacks=0"
        );
    }

    #[test]
    fn test_std_arrays_throttled_frame_does_not_advance_array_counter() {
        // C NDPluginStdArrays.cpp:202-211 decrements NDArrayCounter when its
        // MaxByteRate throttle drops the waveform output, so a throttled frame
        // leaves ArrayCounter unchanged (ImageJ etc. see no new data) while
        // DroppedArrays advances.

        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let wiring = Arc::new(WiringRegistry::new());
        let (handle, _data, _jh) = create_std_arrays_runtime("IMAGE1", pool, "", wiring);
        let port = handle.port_runtime().port_handle();
        port.write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        // MaxByteRate = 4 bytes/sec; make_array is a 4-byte UInt8 array. The
        // bucket starts full at 4, so the first frame passes and the next
        // (sent before a meaningful refill) is throttled.
        port.write_float64_blocking(handle.plugin_params.max_byte_rate, 0, 4.0)
            .unwrap();
        params_applied(&handle);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(handle.array_sender().publish(make_array(1)));
        wait_until("first frame to be served and counted", || {
            port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                .is_ok_and(|v| v == 1)
        });
        let counter_after_first = port
            .read_int32_blocking(handle.ndarray_params.array_counter, 0)
            .unwrap();

        rt.block_on(handle.array_sender().publish(make_array(2)));
        // The throttled frame leaves ArrayCounter unchanged, so wait on the
        // observable it DOES advance: DroppedArrays.
        wait_until("throttled frame to advance DroppedArrays", || {
            port.read_int32_blocking(handle.plugin_params.dropped_output_arrays, 0)
                .is_ok_and(|v| v == 1)
        });
        let counter_after_second = port
            .read_int32_blocking(handle.ndarray_params.array_counter, 0)
            .unwrap();
        let dropped = port
            .read_int32_blocking(handle.plugin_params.dropped_output_arrays, 0)
            .unwrap();

        assert_eq!(counter_after_first, 1, "first frame is served and counted");
        assert_eq!(
            counter_after_second, 1,
            "throttled frame must NOT advance ArrayCounter"
        );
        assert_eq!(dropped, 1, "throttled frame advances DroppedArrays");
    }
}
