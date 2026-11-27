// RTEMS-EXEC-MODEL-ALLOW(4): checked, not waived — all 4 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p ad-plugins-rs
// --all-features`, 590/590). ad-plugins-rs became a census subject when
// its `build.rs` began deriving `tokio_backend`; nothing here builds a
// CA server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.
use asyn_rs::port::InterfaceMask;
use std::sync::Arc;

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{
    ArraySource, NDPluginProcess, PluginRuntimeHandle, ProcessResult,
};
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
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_NONE;
    // NDPluginStdArrays.cpp:324-328, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::INT8_ARRAY
        .or(InterfaceMask::INT16_ARRAY)
        .or(InterfaceMask::INT32_ARRAY)
        .or(InterfaceMask::INT64_ARRAY)
        .or(InterfaceMask::FLOAT32_ARRAY)
        .or(InterfaceMask::FLOAT64_ARRAY);
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        // The frame goes on as it is; the runtime files it in `latest_data`.
        ProcessResult::forward(array, vec![])
    }

    fn plugin_type(&self) -> &str {
        "NDPluginStdArrays"
    }

    /// C `NDPluginStdArrays.cpp:343` sets `NDArrayCallbacks = 0`: this plugin
    /// serves pixel data via the StdArray waveforms, not downstream callbacks.
    fn array_callbacks_seed(&self) -> Option<bool> {
        Some(false)
    }

    /// C `NDPluginStdArrays` passes `compressionAware=true`
    /// (`NDPluginStdArrays.cpp:332`), so the waveform carries whatever bytes
    /// the frame holds. Without this the runtime drop gate discards every
    /// compressed array and `ArrayData` freezes on the last uncompressed
    /// frame.
    fn compression_aware(&self) -> bool {
        true
    }

    fn array_data_handle(&self) -> Option<Arc<Mutex<Option<Arc<NDArray>>>>> {
        Some(self.latest_data.clone())
    }
}

/// Create a StdArrays plugin runtime.
pub fn create_std_arrays_runtime(
    claim: &asyn_rs::registry::PortClaim,
    max_memory: usize,
    queue_size: i32,
    blocking_callbacks: i32,
    source: impl Into<ArraySource>,
    wiring: Arc<WiringRegistry>,
    threads: ad_core_rs::plugin::runtime::PluginThreads,
) -> (
    PluginRuntimeHandle,
    Arc<Mutex<Option<Arc<NDArray>>>>,
    std::thread::JoinHandle<()>,
) {
    let processor = StdArraysProcessor::new();
    let data_handle = processor.data_handle();

    let (handle, data_jh) = ad_core_rs::plugin::runtime::create_plugin_runtime(
        claim,
        processor,
        max_memory,
        queue_size,
        blocking_callbacks,
        source,
        wiring,
        threads,
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
        let deadline =
            epics_libcom_rs::runtime::time::deadline_from_now(std::time::Duration::from_secs(10));
        while !cond() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn test_processor_passes_the_input_through() {
        let proc = StdArraysProcessor::new();
        let pool = NDArrayPool::new(1_000_000);

        let arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        let arr = Arc::new(arr);
        let result = proc.process_array(&arr, &pool);
        assert!(Arc::ptr_eq(&result.output_arrays[0], &arr));
        assert_eq!(result.output_arrays.len(), 1);
    }

    #[test]
    fn test_std_arrays_runtime() {
        let pool = NDArrayPool::new(1_000_000);
        let wiring = Arc::new(WiringRegistry::new());
        let (handle, data, _jh) = create_std_arrays_runtime(
            &asyn_rs::asyn_record::claim_port("IMAGE1").unwrap(),
            pool.max_memory(),
            1,
            0,
            "",
            wiring,
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );

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
        rt.block_on(
            handle.array_sender().publish(
                ad_core_rs::ndarray_pool::DriverPool::standalone(0)
                    .alloc_copy(&make_array(42))
                    .unwrap(),
            ),
        );
        wait_until("StdArrays to store the published array", || {
            data.lock().as_ref().is_some_and(|a| a.unique_id == 42)
        });
    }

    #[test]
    fn test_std_arrays_initial_array_callbacks_off() {
        // C NDPluginStdArrays.cpp:343 sets NDArrayCallbacks=0 in the
        // constructor; the initial param a client reads must be 0, not 1.
        let pool = NDArrayPool::new(1_000_000);
        let wiring = Arc::new(WiringRegistry::new());
        let (handle, _data, _jh) = create_std_arrays_runtime(
            &asyn_rs::asyn_record::claim_port("IMAGE1").unwrap(),
            pool.max_memory(),
            1,
            0,
            "",
            wiring,
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );

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
        // file writer). Each Rust counterpart seeds `array_callbacks_seed()` with false so
        // the initial param reflects this.
        use crate::attribute::AttributeProcessor;
        use crate::file_hdf5::Hdf5FileProcessor;
        use crate::file_jpeg::JpegFileProcessor;
        use crate::file_magick::MagickFileProcessor;
        use crate::file_netcdf::NetcdfFileProcessor;
        use crate::file_nexus::NexusFileProcessor;
        use crate::file_tiff::TiffFileProcessor;
        use crate::passthrough::PassthroughProcessor;

        assert_eq!(
            StdArraysProcessor::new().array_callbacks_seed(),
            Some(false)
        );
        assert_eq!(
            AttributeProcessor::new(1).array_callbacks_seed(),
            Some(false)
        );
        assert_eq!(Hdf5FileProcessor::new().array_callbacks_seed(), Some(false));
        assert_eq!(
            JpegFileProcessor::new(85).array_callbacks_seed(),
            Some(false)
        );
        assert_eq!(TiffFileProcessor::new().array_callbacks_seed(), Some(false));
        assert_eq!(
            NetcdfFileProcessor::new().array_callbacks_seed(),
            Some(false)
        );
        assert_eq!(
            NexusFileProcessor::new().array_callbacks_seed(),
            Some(false)
        );
        assert_eq!(
            MagickFileProcessor::new().array_callbacks_seed(),
            Some(false)
        );

        // A plugin whose C constructor does not set it leaves it to the template.
        assert_eq!(
            PassthroughProcessor::new("NDPluginProcess").array_callbacks_seed(),
            None
        );
    }

    #[test]
    fn test_std_arrays_compressed_frame_is_not_dropped() {
        // C NDPluginStdArrays passes compressionAware=true
        // (NDPluginStdArrays.cpp:332), so the runtime admission gate
        // (`plugin/channel.rs` `classify`: `codec.is_some() &&
        // !compression_aware` => DropCompressed) must let a compressed frame
        // through. Without the override every frame behind an NDCodec is
        // dropped and the ArrayData waveform freezes on the last uncompressed
        // one.
        use std::sync::atomic::Ordering;

        let mut raw = NDArray::new(vec![NDDimension::new(8)], NDDataType::UInt16);
        raw.unique_id = 7;
        let compressed = crate::codec::compress_lz4(&raw).unwrap();
        assert!(compressed.codec.is_some(), "input must carry a codec");

        let pool = NDArrayPool::new(1_000_000);
        let wiring = Arc::new(WiringRegistry::new());
        let (handle, data, _jh) = create_std_arrays_runtime(
            &asyn_rs::asyn_record::claim_port("IMAGE1").unwrap(),
            pool.max_memory(),
            1,
            0,
            "",
            wiring,
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );
        let dropped = handle.array_sender().dropped_arrays_counter().clone();
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
        rt.block_on(
            handle.array_sender().publish(
                ad_core_rs::ndarray_pool::DriverPool::standalone(0)
                    .alloc_copy(&compressed)
                    .unwrap(),
            ),
        );
        wait_until("StdArrays to store the compressed frame", || {
            data.lock().as_ref().is_some_and(|a| a.unique_id == 7)
        });
        assert_eq!(
            dropped.load(Ordering::Acquire),
            0,
            "compression-aware StdArrays must not count its input as dropped"
        );
    }

    #[test]
    fn test_compression_aware_plugins_match_c() {
        // The ADCore plugins constructed with compressionAware=true are
        // NDPluginCodec (`:870`), NDFileHDF5 (`:2268`), NDPluginPva (`:157`)
        // and NDPluginStdArrays (`:332`); NDPluginPvxs also does but postdates
        // R3-14 and has no Rust counterpart. Every other plugin passes false,
        // which is the trait default, so this is the whole family — the flag is
        // a defaulted trait method here where C++ makes it a constructor
        // argument, so a missing override is silent.
        use crate::codec::{CodecMode, CodecProcessor};
        use crate::file_hdf5::Hdf5FileProcessor;
        use crate::passthrough::PassthroughProcessor;

        assert!(StdArraysProcessor::new().compression_aware());
        #[cfg(feature = "pva")]
        assert!(
            crate::pva::PvaProcessor::new("TEST:Pva1:Image".into()).compression_aware(),
            "NDPluginPva.cpp:157 passes compressionAware=true"
        );
        assert!(CodecProcessor::new(CodecMode::Decompress).compression_aware());
        assert!(Hdf5FileProcessor::new().compression_aware());

        // A plugin that works on raw pixels keeps the default.
        assert!(!PassthroughProcessor::new("NDPluginProcess").compression_aware());
    }

    #[test]
    fn test_std_arrays_serves_waveform_with_callbacks_off() {
        // C NDPluginStdArrays fires its typed-array waveform callbacks
        // (NDPluginStdArrays.cpp:71-73) regardless of NDArrayCallbacks — and it
        // defaults NDArrayCallbacks=0. So the ArrayData waveform on I/O Intr
        // must still update with NDArrayCallbacks=0. This guards against the
        // STD_ARRAY_DATA interrupt being gated by the downstream-delivery flag.
        use asyn_rs::param::ParamValue;

        let pool = NDArrayPool::new(1_000_000);
        let wiring = Arc::new(WiringRegistry::new());
        let (handle, _data, _jh) = create_std_arrays_runtime(
            &asyn_rs::asyn_record::claim_port("IMAGE1").unwrap(),
            pool.max_memory(),
            1,
            0,
            "",
            wiring,
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );

        let port = handle.port_runtime().port_handle();
        port.write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        // Force NDArrayCallbacks=0 at runtime (StdArrays' C default) — the
        // waveform output must still fire.
        port.write_int32_blocking(handle.ndarray_params.array_callbacks, 0, 0)
            .unwrap();
        params_applied(&handle);

        // The ArrayData waveform's binding, an asynInt8ArrayIn I/O Intr.
        let std = port
            .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("STD_ARRAY_DATA", 0))
            .unwrap()
            .reason;
        let served = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = served.clone();
        let _binding = port
            .interrupts()
            .register_sync_callback(
                asyn_rs::interrupt::InterruptFilter {
                    reason: Some(std),
                    addr: Some(0),
                    iface: Some(asyn_rs::interfaces::InterfaceType::Int8Array),
                    ..Default::default()
                },
                move |iv| {
                    if matches!(iv.value, ParamValue::Int8Array(_)) {
                        seen.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                },
            )
            .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(
            handle.array_sender().publish(
                ad_core_rs::ndarray_pool::DriverPool::standalone(0)
                    .alloc_copy(&make_array(7))
                    .unwrap(),
            ),
        );
        // ArrayCounter==1 ⟹ the frame was processed and its param batch
        // (including the waveform interrupts) flushed.
        wait_until("StdArrays to process the frame", || {
            port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                .is_ok_and(|v| v == 1)
        });

        assert!(
            served.load(std::sync::atomic::Ordering::Relaxed),
            "StdArrays must serve STD_ARRAY_DATA with NDArrayCallbacks=0"
        );
    }

    #[test]
    fn test_std_arrays_throttled_frame_does_not_advance_array_counter() {
        // C NDPluginStdArrays.cpp:202-211 decrements NDArrayCounter when its
        // MaxByteRate throttle drops the waveform output, so a throttled frame
        // leaves ArrayCounter unchanged (ImageJ etc. see no new data) while
        // DroppedOutputArrays advances.
        // MaxByteRate = 4 bytes/sec, an Int8 client: the bucket starts full
        // at 4, so the first frame passes and the next (sent before a
        // meaningful refill) is throttled.
        let plugin = Waveforms::new(&[asyn_rs::interfaces::InterfaceType::Int8Array]);
        plugin.set_max_byte_rate(4.0);
        assert_eq!(plugin.send(1), (1, 0), "first frame is served and counted");
        assert_eq!(plugin.send(2), (1, 1), "throttled frame is not counted");
    }

    /// A StdArrays plugin whose STD_ARRAY_DATA has one waveform client per
    /// entry of `bound`, each counting the posts it receives.
    struct Waveforms {
        handle: PluginRuntimeHandle,
        posts: Vec<Arc<std::sync::atomic::AtomicUsize>>,
        _bindings: Vec<asyn_rs::interrupt::SyncCallbackSubscription>,
        _jh: std::thread::JoinHandle<()>,
    }

    impl Waveforms {
        fn new(bound: &[asyn_rs::interfaces::InterfaceType]) -> Self {
            let (handle, _data, jh) = create_std_arrays_runtime(
                &asyn_rs::asyn_record::claim_port("IMAGE1").unwrap(),
                1_000_000,
                1,
                0,
                "",
                Arc::new(WiringRegistry::new()),
                ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
            );
            let port = handle.port_runtime().port_handle().clone();
            port.write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
                .unwrap();
            params_applied(&handle);
            let std = port
                .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("STD_ARRAY_DATA", 0))
                .unwrap()
                .reason;
            let posts: Vec<Arc<std::sync::atomic::AtomicUsize>> =
                bound.iter().map(|_| Arc::default()).collect();
            let _bindings = bound
                .iter()
                .zip(&posts)
                .map(|(&iface, count)| {
                    let count = Arc::clone(count);
                    port.interrupts()
                        .register_sync_callback(
                            asyn_rs::interrupt::InterruptFilter {
                                reason: Some(std),
                                addr: Some(0),
                                iface: Some(iface),
                                ..Default::default()
                            },
                            move |_| {
                                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            },
                        )
                        .unwrap()
                })
                .collect();
            Self {
                handle,
                posts,
                _bindings,
                _jh: jh,
            }
        }

        /// MaxByteRate, which refills the bucket to `rate` bytes.
        fn set_max_byte_rate(&self, rate: f64) {
            self.handle
                .port_runtime()
                .port_handle()
                .write_float64_blocking(self.handle.plugin_params.max_byte_rate, 0, rate)
                .unwrap();
            params_applied(&self.handle);
        }

        /// Sends `make_array(id)`, a 4-byte UInt8 frame, and returns
        /// ArrayCounter and DroppedOutputArrays once its params are posted.
        fn send(&self, id: i32) -> (i32, i32) {
            let port = self.handle.port_runtime().port_handle();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(
                    self.handle.array_sender().publish(
                        ad_core_rs::ndarray_pool::DriverPool::standalone(0)
                            .alloc_copy(&make_array(id))
                            .unwrap(),
                    ),
                );
            let unique_id = self.handle.ndarray_params.unique_id;
            wait_until("the frame's params", || {
                port.read_int32_blocking(unique_id, 0)
                    .is_ok_and(|v| v == id)
            });
            params_applied(&self.handle);
            let read = |reason| port.read_int32_blocking(reason, 0).unwrap();
            (
                read(self.handle.ndarray_params.array_counter),
                read(self.handle.plugin_params.dropped_output_arrays),
            )
        }

        fn posts(&self) -> Vec<usize> {
            self.posts
                .iter()
                .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                .collect()
        }
    }

    /// A frame no waveform client is bound to posts nothing, so MaxByteRate
    /// has nothing to charge: it is counted and nothing is dropped.
    #[test]
    fn a_frame_without_waveform_clients_is_not_throttled() {
        let plugin = Waveforms::new(&[]);
        plugin.set_max_byte_rate(1.0);
        assert_eq!(plugin.send(1), (1, 0));
    }

    /// Each interface pays for the array converted into it: a 4-byte UInt8
    /// frame read as Int32 costs 16 bytes.
    #[test]
    fn a_waveform_is_charged_its_converted_size() {
        use asyn_rs::interfaces::InterfaceType::Int32Array;
        let plugin = Waveforms::new(&[Int32Array]);
        plugin.set_max_byte_rate(15.0);
        assert_eq!(plugin.send(1), (0, 1));
        assert_eq!(plugin.posts(), [0]);
        plugin.set_max_byte_rate(16.0);
        assert_eq!(plugin.send(2), (1, 1));
        assert_eq!(plugin.posts(), [1]);
    }

    /// Two records on one interface each pay for the converted array, one
    /// at a time as C walks its client list: a bucket that holds one post
    /// serves exactly one of them, and the other counts as dropped.
    #[test]
    fn a_bucket_that_holds_one_waveform_serves_exactly_one() {
        use asyn_rs::interfaces::InterfaceType::Int8Array;
        let plugin = Waveforms::new(&[Int8Array, Int8Array]);
        plugin.set_max_byte_rate(7.0);
        assert_eq!(plugin.send(1), (0, 1));
        assert_eq!(plugin.posts(), [1, 0]);
        plugin.set_max_byte_rate(8.0);
        assert_eq!(plugin.send(2), (1, 1));
        assert_eq!(plugin.posts(), [2, 1]);
    }

    /// A client bound to every reason of the interface receives the
    /// waveform as a record bound to STD_ARRAY_DATA does, and pays for it.
    #[test]
    fn a_client_of_every_reason_is_served_and_charged() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let plugin = Waveforms::new(&[]);
        let port = plugin.handle.port_runtime().port_handle().clone();
        let std = port
            .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("STD_ARRAY_DATA", 0))
            .unwrap()
            .reason;
        let posts = Arc::new(AtomicUsize::new(0));
        let _every_reason = port
            .interrupts()
            .register_sync_callback(
                asyn_rs::interrupt::InterruptFilter {
                    iface: Some(asyn_rs::interfaces::InterfaceType::Int8Array),
                    ..Default::default()
                },
                {
                    let posts = Arc::clone(&posts);
                    move |iv| {
                        if iv.reason == std {
                            posts.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                },
            )
            .unwrap();
        plugin.set_max_byte_rate(4.0);
        assert_eq!(plugin.send(1), (1, 0));
        assert_eq!(posts.load(Ordering::Relaxed), 1);
        assert_eq!(plugin.send(2), (1, 1));
        assert_eq!(posts.load(Ordering::Relaxed), 1);
    }

    /// A waveform client that reads the port while the frame is posted to it
    /// reads that frame's params, as C posts with the port unlocked after
    /// `beginProcessCallbacks` has set them (NDPluginStdArrays.cpp:161-166).
    #[test]
    fn a_waveform_client_reading_the_port_sees_the_frames_params() {
        use std::sync::Mutex;

        let plugin = Waveforms::new(&[]);
        let port = plugin.handle.port_runtime().port_handle().clone();
        let std = port
            .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("STD_ARRAY_DATA", 0))
            .unwrap()
            .reason;
        let unique_id = plugin.handle.ndarray_params.unique_id;
        let read: Arc<Mutex<Vec<asyn_rs::error::AsynResult<i32>>>> = Arc::default();
        let _reader = port
            .interrupts()
            .register_sync_callback(
                asyn_rs::interrupt::InterruptFilter {
                    reason: Some(std),
                    addr: Some(0),
                    iface: Some(asyn_rs::interfaces::InterfaceType::Int8Array),
                    ..Default::default()
                },
                {
                    let (read, port) = (Arc::clone(&read), port.clone());
                    move |_| {
                        let id = port.read_int32_blocking(unique_id, 0);
                        read.lock().unwrap().push(id);
                    }
                },
            )
            .unwrap();
        assert_eq!(plugin.send(7), (1, 0));
        let read = read.lock().unwrap();
        assert!(matches!(read.as_slice(), [Ok(7)]), "{:?}", read.as_slice());
    }

    /// A record that is gone by the time a frame is posted pays nothing for
    /// it. Here the first record's post unbinds the second, so a bucket that
    /// holds one post serves the first and drops nothing.
    #[test]
    fn a_waveform_that_leaves_before_the_post_is_not_charged() {
        use asyn_rs::interrupt::{InterruptFilter, SyncCallbackSubscription};
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let plugin = Waveforms::new(&[]);
        let port = plugin.handle.port_runtime().port_handle().clone();
        let std = port
            .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("STD_ARRAY_DATA", 0))
            .unwrap()
            .reason;
        let filter = InterruptFilter {
            reason: Some(std),
            addr: Some(0),
            iface: Some(asyn_rs::interfaces::InterfaceType::Int8Array),
            ..Default::default()
        };
        let leaving: Arc<Mutex<Option<SyncCallbackSubscription>>> = Arc::default();
        let posts: [Arc<AtomicUsize>; 2] = Default::default();
        let _staying = port
            .interrupts()
            .register_sync_callback(filter.clone(), {
                let (posts, leaving) = (Arc::clone(&posts[0]), Arc::clone(&leaving));
                move |_| {
                    posts.fetch_add(1, Ordering::Relaxed);
                    leaving.lock().unwrap().take();
                }
            })
            .unwrap();
        *leaving.lock().unwrap() = Some(
            port.interrupts()
                .register_sync_callback(filter, {
                    let posts = Arc::clone(&posts[1]);
                    move |_| {
                        posts.fetch_add(1, Ordering::Relaxed);
                    }
                })
                .unwrap(),
        );
        plugin.set_max_byte_rate(4.0);
        assert_eq!(plugin.send(1), (1, 0));
        assert_eq!(posts.each_ref().map(|n| n.load(Ordering::Relaxed)), [1, 0]);
    }
}
