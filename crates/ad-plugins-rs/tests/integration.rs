// RTEMS-EXEC-MODEL-ALLOW(6): checked, not waived — all 6 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p ad-plugins-rs
// --all-features`, 556/556). ad-plugins-rs became a census subject when
// its `build.rs` began deriving `tokio_backend`; nothing here builds a
// CA server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.
#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;
use std::sync::Arc;

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
use ad_core_rs::driver::ad_driver::ADDriverBase;
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, PluginRuntimeHandle};
use ad_core_rs::plugin::wiring::WiringRegistry;
use ad_plugins_rs::stats::create_stats_runtime;
use ad_plugins_rs::std_arrays::create_std_arrays_runtime;

/// Fence: `write_*_blocking`/`submit_blocking` only queue param changes for
/// the plugin's data thread; the barrier ack proves they have been applied
/// (enable flips, ROI sizes, NDArrayPort rewires).
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
fn test_driver_to_stats_pipeline() {
    let pool = Arc::new(ad_core_rs::ndarray_pool::NDArrayPool::new(10_000_000));
    let wiring = Arc::new(WiringRegistry::new());
    let ts_registry = ad_plugins_rs::time_series::TsReceiverRegistry::new();
    let (stats_handle, stats_data, _params, _jh) =
        create_stats_runtime("STATS1", pool.clone(), 10, "SIM1", wiring, &ts_registry);

    // Plugins default to disabled — enable for test
    stats_handle
        .port_runtime()
        .port_handle()
        .write_int32_blocking(stats_handle.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    params_applied(&stats_handle);

    let mut driver = ADDriverBase::new("SIM1", 64, 64, 10_000_000).unwrap();
    driver.connect_downstream(stats_handle.array_sender().clone());

    let mut arr = driver
        .pool
        .alloc(
            vec![NDDimension::new(64), NDDimension::new(64)],
            NDDataType::UInt8,
        )
        .unwrap();

    if let NDDataBuffer::U8(ref mut v) = arr.data {
        for i in 0..v.len() {
            v[i] = (i % 256) as u8;
        }
    }

    let to_publish = driver.prepare_array(Arc::new(arr)).unwrap().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(driver.array_output.publish(to_publish));

    wait_until("stats to process the frame", || {
        stats_data.lock().num_elements == 64 * 64
    });

    let result = stats_data.lock().clone();
    assert_eq!(result.num_elements, 64 * 64);
    assert!(result.max > 0.0);
}

#[test]
fn test_driver_to_std_arrays_pipeline() {
    let pool = Arc::new(ad_core_rs::ndarray_pool::NDArrayPool::new(10_000_000));
    let wiring = Arc::new(WiringRegistry::new());
    let (image_handle, image_data, _jh) =
        create_std_arrays_runtime("IMAGE1", pool.clone(), "SIM1", wiring);

    // Plugins default to disabled — enable for test
    image_handle
        .port_runtime()
        .port_handle()
        .write_int32_blocking(image_handle.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    params_applied(&image_handle);

    let mut driver = ADDriverBase::new("SIM1", 32, 32, 10_000_000).unwrap();
    driver.connect_downstream(image_handle.array_sender().clone());

    let arr = driver
        .pool
        .alloc(
            vec![NDDimension::new(32), NDDimension::new(32)],
            NDDataType::UInt16,
        )
        .unwrap();

    let id = arr.unique_id;
    let to_publish = driver.prepare_array(Arc::new(arr)).unwrap().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(driver.array_output.publish(to_publish));

    wait_until("StdArrays to store the published array", || {
        image_data
            .lock()
            .as_ref()
            .is_some_and(|a| a.unique_id == id)
    });

    let latest = image_data.lock().clone().unwrap();
    assert_eq!(latest.unique_id, id);
}

#[test]
fn test_pool_reuse_in_pipeline() {
    let pool = Arc::new(ad_core_rs::ndarray_pool::NDArrayPool::new(10_000_000));

    // Allocate, use, release, reallocate
    // Use sizes within THRESHOLD_SIZE_RATIO (1.5) to ensure reuse
    let arr1 = pool
        .alloc(vec![NDDimension::new(1000)], NDDataType::UInt8)
        .unwrap();
    let bytes_after_first = pool.allocated_bytes();
    pool.release(arr1);
    assert_eq!(pool.num_free_buffers(), 1);

    // 1000/800 = 1.25 < 1.5 threshold, so buffer is reused
    let _arr2 = pool
        .alloc(vec![NDDimension::new(800)], NDDataType::UInt8)
        .unwrap();
    assert_eq!(pool.num_free_buffers(), 0);
    // Should have reused buffer, allocated_bytes unchanged
    assert_eq!(pool.allocated_bytes(), bytes_after_first);
}

// --- Rewire integration test ---

#[test]
fn test_rewire_ndarray_port_at_runtime() {
    use ad_core_rs::plugin::channel::{ArrayPublisher, NDArrayOutput};
    use ad_core_rs::plugin::runtime::{ProcessResult, create_plugin_runtime};
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let pool = Arc::new(NDArrayPool::new(1_000_000));
    let wiring = Arc::new(WiringRegistry::new());

    // Create two "upstream" outputs: SIM1 and ROI1
    let sim_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    wiring.register_output("SIM1", sim_output.clone());
    let sim_publisher = ArrayPublisher::new(sim_output.clone());

    let roi_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    wiring.register_output("ROI1", roi_output.clone());
    let roi_publisher = ArrayPublisher::new(roi_output.clone());

    // Tracking processor — records unique_id of last processed array
    struct TrackingProcessor {
        last_id: Arc<parking_lot::Mutex<i32>>,
    }
    impl NDPluginProcess for TrackingProcessor {
        fn process_array(
            &self,
            array: &ad_core_rs::ndarray::NDArray,
            _pool: &NDArrayPool,
        ) -> ProcessResult {
            *self.last_id.lock() = array.unique_id;
            ProcessResult::empty()
        }
        fn plugin_type(&self) -> &str {
            "Tracking"
        }
    }

    let last_id = Arc::new(parking_lot::Mutex::new(-1i32));
    let proc = TrackingProcessor {
        last_id: last_id.clone(),
    };

    let (handle, _jh) = create_plugin_runtime("STATS1", proc, pool, 10, "SIM1", wiring.clone());

    // Wire STATS1 → SIM1
    wiring.rewire(handle.array_sender(), "", "SIM1").unwrap();

    // Enable callbacks
    handle
        .port_runtime()
        .port_handle()
        .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    params_applied(&handle);

    // Send array from SIM1
    let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
    arr.unique_id = 100;
    rt.block_on(sim_publisher.publish(Arc::new(arr)));
    wait_until("STATS1 to receive array from SIM1", || {
        *last_id.lock() == 100
    });

    // Now rewire STATS1 from SIM1 → ROI1 via OctetWrite
    handle
        .port_runtime()
        .port_handle()
        .submit_blocking(
            RequestOp::OctetWrite {
                data: b"ROI1".to_vec(),
            },
            AsynUser::new(handle.plugin_params.nd_array_port),
        )
        .unwrap();
    params_applied(&handle);

    // Send array from SIM1 — should NOT reach STATS1 anymore. The rewire is
    // already applied (barrier above), so SIM1's output no longer carries
    // STATS1's sender and delivery is impossible — no wait needed.
    let mut arr2 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
    arr2.unique_id = 200;
    rt.block_on(sim_publisher.publish(Arc::new(arr2)));
    assert_eq!(
        *last_id.lock(),
        100,
        "STATS1 should no longer receive from SIM1 after rewire"
    );

    // Send array from ROI1 — should reach STATS1
    let mut arr3 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
    arr3.unique_id = 300;
    rt.block_on(roi_publisher.publish(Arc::new(arr3)));
    wait_until("STATS1 to receive from ROI1 after rewire", || {
        *last_id.lock() == 300
    });
}

#[test]
fn test_rewire_through_real_roi_plugin() {
    use ad_core_rs::plugin::channel::{ArrayPublisher, NDArrayOutput};
    use ad_core_rs::plugin::runtime::{ProcessResult, create_plugin_runtime};
    use ad_plugins_rs::roi::{ROIConfig, ROIDimConfig, ROIProcessor};
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let pool = Arc::new(NDArrayPool::new(1_000_000));
    let wiring = Arc::new(WiringRegistry::new());

    // SIM1 driver output
    let sim_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    wiring.register_output("SIM1", sim_output.clone());
    let sim_publisher = ArrayPublisher::new(sim_output.clone());

    // Create ROI1 plugin wired to SIM1 (full-frame passthrough ROI)
    let mut roi_config = ROIConfig::default();
    roi_config.dims[0] = ROIDimConfig {
        min: 0,
        size: 8,
        bin: 1,
        reverse: false,
        enable: true,
        auto_size: false,
    };
    roi_config.dims[1] = ROIDimConfig {
        min: 0,
        size: 8,
        bin: 1,
        reverse: false,
        enable: true,
        auto_size: false,
    };
    let (roi_handle, _roi_jh) = create_plugin_runtime(
        "ROI1",
        ROIProcessor::new(roi_config),
        pool.clone(),
        10,
        "SIM1",
        wiring.clone(),
    );
    wiring.register_output("ROI1", roi_handle.array_output().clone());
    wiring
        .rewire(roi_handle.array_sender(), "", "SIM1")
        .unwrap();

    // Enable ROI1
    roi_handle
        .port_runtime()
        .port_handle()
        .write_int32_blocking(roi_handle.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    params_applied(&roi_handle);

    // Create STATS1 (tracking processor) initially wired to SIM1
    struct TrackingProcessor {
        last_id: Arc<parking_lot::Mutex<i32>>,
    }
    impl NDPluginProcess for TrackingProcessor {
        fn process_array(
            &self,
            array: &ad_core_rs::ndarray::NDArray,
            _pool: &NDArrayPool,
        ) -> ProcessResult {
            *self.last_id.lock() = array.unique_id;
            ProcessResult::empty()
        }
        fn plugin_type(&self) -> &str {
            "Tracking"
        }
    }

    let last_id = Arc::new(parking_lot::Mutex::new(-1i32));
    let proc = TrackingProcessor {
        last_id: last_id.clone(),
    };

    let (stats_handle, _stats_jh) =
        create_plugin_runtime("STATS1", proc, pool.clone(), 10, "SIM1", wiring.clone());
    wiring.register_output("STATS1", stats_handle.array_output().clone());
    wiring
        .rewire(stats_handle.array_sender(), "", "SIM1")
        .unwrap();

    // Enable STATS1
    stats_handle
        .port_runtime()
        .port_handle()
        .write_int32_blocking(stats_handle.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    params_applied(&stats_handle);

    // Send array from SIM1 → both ROI1 and STATS1 receive directly
    let mut arr = NDArray::new(
        vec![NDDimension::new(8), NDDimension::new(8)],
        NDDataType::UInt8,
    );
    arr.unique_id = 100;
    rt.block_on(sim_publisher.publish(Arc::new(arr)));
    wait_until("STATS1 to get the array from SIM1", || {
        *last_id.lock() == 100
    });

    // Rewire STATS1 from SIM1 → ROI1
    stats_handle
        .port_runtime()
        .port_handle()
        .submit_blocking(
            RequestOp::OctetWrite {
                data: b"ROI1".to_vec(),
            },
            AsynUser::new(stats_handle.plugin_params.nd_array_port),
        )
        .unwrap();
    params_applied(&stats_handle);

    // Send another array from SIM1
    // ROI1 gets it from SIM1, processes it, publishes to its output
    // STATS1 (now wired to ROI1) should receive ROI1's output
    let mut arr2 = NDArray::new(
        vec![NDDimension::new(8), NDDimension::new(8)],
        NDDataType::UInt8,
    );
    arr2.unique_id = 200;
    rt.block_on(sim_publisher.publish(Arc::new(arr2)));
    wait_until("STATS1 to receive the new array through ROI1", || {
        *last_id.lock() != 100
    });
}

#[test]
fn test_roi_param_change_enables_output() {
    use ad_core_rs::plugin::channel::{ArrayPublisher, NDArrayOutput};
    use ad_core_rs::plugin::runtime::{ProcessResult, create_plugin_runtime};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let pool = Arc::new(NDArrayPool::new(1_000_000));
    let wiring = Arc::new(WiringRegistry::new());

    // SIM1 driver output
    let sim_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    wiring.register_output("SIM1", sim_output.clone());
    let sim_publisher = ArrayPublisher::new(sim_output.clone());

    // Create ROI1 with DEFAULT config (size=0) — like the real IOC
    let (roi_handle, roi_params, _roi_jh) =
        ad_plugins_rs::roi::create_roi_runtime("ROI1", pool.clone(), 10, "SIM1", wiring.clone());
    wiring.register_output("ROI1", roi_handle.array_output().clone());
    wiring
        .rewire(roi_handle.array_sender(), "", "SIM1")
        .unwrap();

    // Enable ROI1
    roi_handle
        .port_runtime()
        .port_handle()
        .write_int32_blocking(roi_handle.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    params_applied(&roi_handle);

    // Tracking processor downstream of ROI1
    struct TrackingProcessor {
        last_id: Arc<parking_lot::Mutex<i32>>,
    }
    impl NDPluginProcess for TrackingProcessor {
        fn process_array(
            &self,
            array: &ad_core_rs::ndarray::NDArray,
            _pool: &NDArrayPool,
        ) -> ProcessResult {
            *self.last_id.lock() = array.unique_id;
            ProcessResult::empty()
        }
        fn plugin_type(&self) -> &str {
            "Tracking"
        }
    }

    let last_id = Arc::new(parking_lot::Mutex::new(-1i32));
    let proc = TrackingProcessor {
        last_id: last_id.clone(),
    };
    let (stats_handle, _jh) =
        create_plugin_runtime("STATS1", proc, pool.clone(), 10, "ROI1", wiring.clone());
    wiring.register_output("STATS1", stats_handle.array_output().clone());
    wiring
        .rewire(stats_handle.array_sender(), "", "ROI1")
        .unwrap();
    stats_handle
        .port_runtime()
        .port_handle()
        .write_int32_blocking(stats_handle.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    params_applied(&stats_handle);

    // Send array — ROI1 has default size=0. Matching C++ NDPluginROI
    // (size = MAX(size, 1)), an enabled ROI dim with size 0 clamps to a
    // 1-pixel ROI and still produces an output, so STATS1 receives it.
    let mut arr = NDArray::new(
        vec![NDDimension::new(8), NDDimension::new(8)],
        NDDataType::UInt8,
    );
    arr.unique_id = 100;
    rt.block_on(sim_publisher.publish(Arc::new(arr)));
    // ROI size=0 clamps to a 1-pixel ROI (C++ MAX(size,1)) and still emits.
    wait_until("STATS1 to receive the 1-pixel ROI output", || {
        *last_id.lock() == 100
    });

    // Now set ROI size via param write (like PINI or user).
    let roi_ph = roi_handle.port_runtime().port_handle();
    roi_ph
        .write_int32_blocking(roi_params.dims[0].size, 0, 8)
        .unwrap();
    roi_ph
        .write_int32_blocking(roi_params.dims[1].size, 0, 8)
        .unwrap();
    params_applied(&roi_handle);

    // Send another array — now ROI1 has size=8x8, should produce output
    let mut arr2 = NDArray::new(
        vec![NDDimension::new(8), NDDimension::new(8)],
        NDDataType::UInt8,
    );
    arr2.unique_id = 200;
    rt.block_on(sim_publisher.publish(Arc::new(arr2)));
    wait_until("STATS1 to receive after ROI size set to 8x8", || {
        *last_id.lock() == 200
    });
}

// --- Phase 5: New integration tests ---

fn make_2d_u8(w: usize, h: usize) -> NDArray {
    let mut arr = NDArray::new(
        vec![NDDimension::new(w), NDDimension::new(h)],
        NDDataType::UInt8,
    );
    if let NDDataBuffer::U8(ref mut v) = arr.data {
        for i in 0..v.len() {
            v[i] = (i % 256) as u8;
        }
    }
    arr
}

#[test]
fn test_roi_then_stats_chain() {
    use ad_plugins_rs::roi::{ROIConfig, ROIDimConfig, ROIProcessor};
    use ad_plugins_rs::stats::StatsProcessor;

    let pool = NDArrayPool::new(1_000_000);
    let arr = make_2d_u8(16, 16);

    // ROI: extract 4x4 region from (2,2)
    let mut roi_config = ROIConfig::default();
    roi_config.dims[0] = ROIDimConfig {
        min: 2,
        size: 4,
        bin: 1,
        reverse: false,
        enable: true,
        auto_size: false,
    };
    roi_config.dims[1] = ROIDimConfig {
        min: 2,
        size: 4,
        bin: 1,
        reverse: false,
        enable: true,
        auto_size: false,
    };

    let roi_proc = ROIProcessor::new(roi_config);
    let roi_result = roi_proc.process_array(&arr, &pool);
    assert_eq!(roi_result.output_arrays.len(), 1);
    assert_eq!(roi_result.output_arrays[0].dims[0].size, 4);
    assert_eq!(roi_result.output_arrays[0].dims[1].size, 4);

    // Feed ROI output to Stats
    let stats_proc = StatsProcessor::new();
    let stats_result = stats_proc.process_array(&roi_result.output_arrays[0], &pool);

    // Stats should be computed on the 4x4 ROI, not the full 16x16
    let stats = stats_proc.stats_handle().lock().clone();
    assert_eq!(stats.num_elements, 16); // 4*4
    assert!(stats.min >= 0.0);
    assert!(stats.max <= 255.0);
    assert_eq!(stats_result.output_arrays.len(), 1); // stats forwards the array
}

#[test]
fn test_process_then_file_tiff_pipeline() {
    use ad_core_rs::plugin::file_base::NDFileMode;
    use ad_plugins_rs::file_tiff::TiffFileProcessor;
    use ad_plugins_rs::process::{ProcessConfig, ProcessProcessor};

    let pool = NDArrayPool::new(1_000_000);
    let arr = make_2d_u8(8, 8);

    // Process: scale by 2
    let proc = ProcessProcessor::new(ProcessConfig {
        enable_offset_scale: true,
        scale: 2.0,
        offset: 0.0,
        ..Default::default()
    });
    let proc_result = proc.process_array(&arr, &pool);
    assert_eq!(proc_result.output_arrays.len(), 1);

    // Write to TIFF
    let path = std::env::temp_dir().join(format!(
        "integration_process_tiff_{}.tif",
        std::process::id()
    ));
    let tiff_proc = TiffFileProcessor::new();
    tiff_proc.ctrl.lock().file_base.file_path = path.parent().unwrap().to_str().unwrap().into();
    tiff_proc.ctrl.lock().file_base.file_name = path.file_name().unwrap().to_str().unwrap().into();
    tiff_proc.ctrl.lock().file_base.set_mode(NDFileMode::Single);

    // Use the writer directly for this test
    use ad_core_rs::plugin::file_base::NDFileWriter;
    use ad_plugins_rs::file_tiff::TiffWriter;
    let mut writer = TiffWriter::new();
    writer
        .open_file(&path, NDFileMode::Single, &proc_result.output_arrays[0])
        .unwrap();
    writer.write_file(&proc_result.output_arrays[0]).unwrap();
    writer.close_file().unwrap();

    // Verify file exists
    assert!(path.exists());
    let meta = std::fs::metadata(&path).unwrap();
    assert!(meta.len() > 0);
    std::fs::remove_file(&path).ok();
}

#[test]
fn test_circular_buff_trigger_flow() {
    use ad_plugins_rs::circular_buff::{CircularBuffProcessor, TriggerCondition};

    let pool = NDArrayPool::new(1_000_000);
    let proc = CircularBuffProcessor::new(
        2, // pre_count
        1, // post_count
        TriggerCondition::External,
        100, // max_buffers (C maxBuffers_)
    );
    // C NDCircBuffControl = 1: the plugin records nothing until acquisition is
    // turned on (NDPluginCircularBuff.cpp:121, the `if (scopeControl)` gate).
    proc.start();

    // Fill pre-buffer
    let arr1 = make_2d_u8(4, 4);
    let mut arr1c = arr1.clone();
    arr1c.unique_id = 1;
    proc.process_array(&arr1c, &pool);

    let mut arr2 = arr1.clone();
    arr2.unique_id = 2;
    proc.process_array(&arr2, &pool);

    let mut arr3 = arr1.clone();
    arr3.unique_id = 3;
    proc.process_array(&arr3, &pool);

    // Trigger
    proc.trigger();
    assert!(proc.buffer().is_triggered());

    // Post-trigger frame
    let mut arr4 = arr1.clone();
    arr4.unique_id = 4;
    let result = proc.process_array(&arr4, &pool);

    // Should have captured: 2 pre + 1 post = 3 frames
    assert_eq!(result.output_arrays.len(), 3);
    assert_eq!(result.output_arrays[0].unique_id, 2);
    assert_eq!(result.output_arrays[1].unique_id, 3);
    assert_eq!(result.output_arrays[2].unique_id, 4);
}

#[test]
fn test_codec_compress_decompress_roundtrip() {
    use ad_plugins_rs::codec::{compress_lz4, decompress_lz4};

    // Create array with compressible data (all zeros)
    let arr = NDArray::new(
        vec![NDDimension::new(64), NDDimension::new(64)],
        NDDataType::UInt8,
    );
    // Data is already zeros from NDArray::new

    let compressed = compress_lz4(&arr);
    assert!(compressed.codec.is_some());

    let decompressed = decompress_lz4(&compressed).unwrap();
    assert!(decompressed.codec.is_none());
    assert_eq!(decompressed.data.as_u8_slice(), arr.data.as_u8_slice());
}

#[test]
fn test_attribute_plugin_value_extraction() {
    use ad_plugins_rs::attribute::AttributeProcessor;

    let pool = NDArrayPool::new(1_000_000);
    let proc = AttributeProcessor::new("exposure", 8);

    let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
    arr.attributes.add(NDAttribute::new_static(
        "exposure",
        "",
        NDAttrSource::Driver,
        NDAttrValue::Float64(0.5),
    ));

    let result = proc.process_array(&arr, &pool);
    // AttributeProcessor is a sink (no output arrays)
    assert!(result.output_arrays.is_empty());
    // Check param updates contain the value
    assert!(!result.param_updates.is_empty());
}

#[test]
fn test_pos_plugin_position_attachment() {
    use ad_plugins_rs::pos_plugin::{PosMode, PosPluginProcessor};

    let pool = NDArrayPool::new(1_000_000);
    let proc = PosPluginProcessor::new(PosMode::Discard);

    let mut pos = HashMap::new();
    pos.insert("MotorX".into(), 42.5);
    pos.insert("MotorY".into(), 13.7);
    proc.load_positions(vec![pos]);
    proc.start();

    let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
    arr.unique_id = 1;

    let result = proc.process_array(&arr, &pool);
    assert_eq!(result.output_arrays.len(), 1);

    let out = &result.output_arrays[0];
    let mx = out
        .attributes
        .get("MotorX")
        .unwrap()
        .value
        .as_f64()
        .unwrap();
    assert!((mx - 42.5).abs() < 1e-10);
    let my = out
        .attributes
        .get("MotorY")
        .unwrap()
        .value
        .as_f64()
        .unwrap();
    assert!((my - 13.7).abs() < 1e-10);
}

#[test]
fn test_process_and_publish_writes_array_size_params() {
    // Verify that process_and_publish writes ArraySizeX/Y/Z params correctly.
    let pool = Arc::new(NDArrayPool::new(1_000_000));
    let wiring = Arc::new(WiringRegistry::new());
    let (image_handle, image_data, _jh) =
        create_std_arrays_runtime("IMG_SZ", pool.clone(), "DRV1", wiring);

    // Enable callbacks
    let ph = image_handle.port_runtime().port_handle();
    ph.write_int32_blocking(image_handle.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    params_applied(&image_handle);

    // Connect and send a 64x48 array
    let mut driver =
        ad_core_rs::driver::ad_driver::ADDriverBase::new("DRV1", 64, 48, 1_000_000).unwrap();
    driver.connect_downstream(image_handle.array_sender().clone());

    let mut arr = NDArray::new(
        vec![NDDimension::new(64), NDDimension::new(48)],
        NDDataType::UInt8,
    );
    arr.unique_id = 42;
    let to_publish = driver.prepare_array(Arc::new(arr)).unwrap().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(driver.array_output.publish(to_publish));

    // The frame's params (sizes, counter, unique id) flush as one batch, so
    // ArrayCounter==1 implies every param below is already readable.
    wait_until("StdArrays to process the frame", || {
        ph.read_int32_blocking(image_handle.ndarray_params.array_counter, 0)
            .is_ok_and(|v| v == 1)
    });

    // Verify the StdArrays data was stored
    let latest = image_data.lock().clone();
    assert!(latest.is_some(), "StdArrays should have latest data");

    // Read back params from the plugin's port handle
    let size_x = ph
        .read_int32_blocking(image_handle.ndarray_params.array_size_x, 0)
        .unwrap();
    let size_y = ph
        .read_int32_blocking(image_handle.ndarray_params.array_size_y, 0)
        .unwrap();
    let size_z = ph
        .read_int32_blocking(image_handle.ndarray_params.array_size_z, 0)
        .unwrap();
    let array_size = ph
        .read_int32_blocking(image_handle.ndarray_params.array_size, 0)
        .unwrap();
    let counter = ph
        .read_int32_blocking(image_handle.ndarray_params.array_counter, 0)
        .unwrap();
    let unique_id = ph
        .read_int32_blocking(image_handle.ndarray_params.unique_id, 0)
        .unwrap();

    assert_eq!(size_x, 64, "ArraySizeX should be 64");
    assert_eq!(size_y, 48, "ArraySizeY should be 48");
    // C parity: NDArray::getInfo leaves colorSize at 0 for a 2-D array (no
    // color dimension), so ArraySizeZ_RBV is 0 for 2D mono.
    assert_eq!(
        size_z, 0,
        "ArraySizeZ should be 0 for 2D mono (no color dim)"
    );
    assert_eq!(array_size, 64 * 48, "ArraySize should be total bytes");
    assert_eq!(counter, 1, "ArrayCounter should be 1");
    assert_eq!(unique_id, 42, "UniqueId should be 42");
}
