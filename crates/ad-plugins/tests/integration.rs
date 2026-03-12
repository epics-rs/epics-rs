use std::collections::HashMap;
use std::sync::Arc;

use ad_core::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
use ad_core::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::driver::ad_driver::ADDriverBase;
use ad_core::plugin::runtime::NDPluginProcess;
use ad_core::plugin::wiring::WiringRegistry;
use ad_plugins::stats::create_stats_runtime;
use ad_plugins::std_arrays::create_std_arrays_runtime;

#[test]
fn test_driver_to_stats_pipeline() {
    let pool = Arc::new(ad_core::ndarray_pool::NDArrayPool::new(10_000_000));
    let wiring = Arc::new(WiringRegistry::new());
    let (stats_handle, stats_data, _params, _ts_runtime, _ts_params, _jh, _ts_actor_jh, _ts_data_jh) =
        create_stats_runtime("STATS1", pool.clone(), 10, "SIM1", wiring);

    // Plugins default to disabled — enable for test
    stats_handle.port_runtime().port_handle()
        .write_int32_blocking(stats_handle.plugin_params.enable_callbacks, 0, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));

    let mut driver = ADDriverBase::new("SIM1", 64, 64, 10_000_000).unwrap();
    driver.connect_downstream(stats_handle.array_sender().clone());

    let mut arr = driver.pool.alloc(
        vec![NDDimension::new(64), NDDimension::new(64)],
        NDDataType::UInt8,
    ).unwrap();

    if let NDDataBuffer::U8(ref mut v) = arr.data {
        for i in 0..v.len() {
            v[i] = (i % 256) as u8;
        }
    }

    driver.publish_array(Arc::new(arr)).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(100));

    let result = stats_data.lock().clone();
    assert_eq!(result.num_elements, 64 * 64);
    assert!(result.max > 0.0);
}

#[test]
fn test_driver_to_std_arrays_pipeline() {
    let pool = Arc::new(ad_core::ndarray_pool::NDArrayPool::new(10_000_000));
    let wiring = Arc::new(WiringRegistry::new());
    let (image_handle, image_data, _jh) = create_std_arrays_runtime("IMAGE1", pool.clone(), "SIM1", wiring);

    // Plugins default to disabled — enable for test
    image_handle.port_runtime().port_handle()
        .write_int32_blocking(image_handle.plugin_params.enable_callbacks, 0, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));

    let mut driver = ADDriverBase::new("SIM1", 32, 32, 10_000_000).unwrap();
    driver.connect_downstream(image_handle.array_sender().clone());

    let arr = driver.pool.alloc(
        vec![NDDimension::new(32), NDDimension::new(32)],
        NDDataType::UInt16,
    ).unwrap();

    let id = arr.unique_id;
    driver.publish_array(Arc::new(arr)).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(100));

    let latest = image_data.lock().clone().unwrap();
    assert_eq!(latest.unique_id, id);
}

#[test]
fn test_pool_reuse_in_pipeline() {
    let pool = Arc::new(ad_core::ndarray_pool::NDArrayPool::new(10_000_000));

    // Allocate, use, release, reallocate
    let arr1 = pool.alloc(vec![NDDimension::new(1000)], NDDataType::UInt8).unwrap();
    let bytes_after_first = pool.allocated_bytes();
    pool.release(arr1);
    assert_eq!(pool.num_free_buffers(), 1);

    let _arr2 = pool.alloc(vec![NDDimension::new(500)], NDDataType::UInt8).unwrap();
    assert_eq!(pool.num_free_buffers(), 0);
    // Should have reused buffer, allocated_bytes unchanged
    assert_eq!(pool.allocated_bytes(), bytes_after_first);
}

// --- Rewire integration test ---

#[test]
fn test_rewire_ndarray_port_at_runtime() {
    use ad_core::plugin::channel::NDArrayOutput;
    use ad_core::plugin::runtime::{create_plugin_runtime, ProcessResult};
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;

    let pool = Arc::new(NDArrayPool::new(1_000_000));
    let wiring = Arc::new(WiringRegistry::new());

    // Create two "upstream" outputs: SIM1 and ROI1
    let sim_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    wiring.register_output("SIM1", sim_output.clone());

    let roi_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    wiring.register_output("ROI1", roi_output.clone());

    // Tracking processor — records unique_id of last processed array
    struct TrackingProcessor {
        last_id: Arc<parking_lot::Mutex<i32>>,
    }
    impl NDPluginProcess for TrackingProcessor {
        fn process_array(&mut self, array: &ad_core::ndarray::NDArray, _pool: &NDArrayPool) -> ProcessResult {
            *self.last_id.lock() = array.unique_id;
            ProcessResult::empty()
        }
        fn plugin_type(&self) -> &str { "Tracking" }
    }

    let last_id = Arc::new(parking_lot::Mutex::new(-1i32));
    let proc = TrackingProcessor { last_id: last_id.clone() };

    let (handle, _jh) = create_plugin_runtime(
        "STATS1", proc, pool, 10, "SIM1", wiring.clone(),
    );

    // Wire STATS1 → SIM1
    wiring.rewire(handle.array_sender(), "", "SIM1").unwrap();

    // Enable callbacks
    handle.port_runtime().port_handle()
        .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Send array from SIM1
    let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
    arr.unique_id = 100;
    sim_output.lock().publish(Arc::new(arr));
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(*last_id.lock(), 100, "STATS1 should receive array from SIM1");

    // Now rewire STATS1 from SIM1 → ROI1 via OctetWrite
    handle.port_runtime().port_handle()
        .submit_blocking(
            RequestOp::OctetWrite { data: b"ROI1".to_vec() },
            AsynUser::new(handle.plugin_params.nd_array_port),
        ).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Send array from SIM1 — should NOT reach STATS1 anymore
    let mut arr2 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
    arr2.unique_id = 200;
    sim_output.lock().publish(Arc::new(arr2));
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(*last_id.lock(), 100, "STATS1 should no longer receive from SIM1 after rewire");

    // Send array from ROI1 — should reach STATS1
    let mut arr3 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
    arr3.unique_id = 300;
    roi_output.lock().publish(Arc::new(arr3));
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(*last_id.lock(), 300, "STATS1 should receive from ROI1 after rewire");
}

#[test]
fn test_rewire_through_real_roi_plugin() {
    use ad_core::plugin::channel::NDArrayOutput;
    use ad_core::plugin::runtime::{create_plugin_runtime, ProcessResult};
    use asyn_rs::request::RequestOp;
    use asyn_rs::user::AsynUser;
    use ad_plugins::roi::{ROIConfig, ROIDimConfig, ROIProcessor};

    let pool = Arc::new(NDArrayPool::new(1_000_000));
    let wiring = Arc::new(WiringRegistry::new());

    // SIM1 driver output
    let sim_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    wiring.register_output("SIM1", sim_output.clone());

    // Create ROI1 plugin wired to SIM1 (full-frame passthrough ROI)
    let mut roi_config = ROIConfig::default();
    roi_config.dims[0] = ROIDimConfig { min: 0, size: 8, bin: 1, reverse: false, enable: true, auto_size: false };
    roi_config.dims[1] = ROIDimConfig { min: 0, size: 8, bin: 1, reverse: false, enable: true, auto_size: false };
    let (roi_handle, _roi_jh) = create_plugin_runtime(
        "ROI1", ROIProcessor::new(roi_config),
        pool.clone(), 10, "SIM1", wiring.clone(),
    );
    wiring.register_output("ROI1", roi_handle.array_output().clone());
    wiring.rewire(roi_handle.array_sender(), "", "SIM1").unwrap();

    // Enable ROI1
    roi_handle.port_runtime().port_handle()
        .write_int32_blocking(roi_handle.plugin_params.enable_callbacks, 0, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Create STATS1 (tracking processor) initially wired to SIM1
    struct TrackingProcessor {
        last_id: Arc<parking_lot::Mutex<i32>>,
    }
    impl NDPluginProcess for TrackingProcessor {
        fn process_array(&mut self, array: &ad_core::ndarray::NDArray, _pool: &NDArrayPool) -> ProcessResult {
            *self.last_id.lock() = array.unique_id;
            ProcessResult::empty()
        }
        fn plugin_type(&self) -> &str { "Tracking" }
    }

    let last_id = Arc::new(parking_lot::Mutex::new(-1i32));
    let proc = TrackingProcessor { last_id: last_id.clone() };

    let (stats_handle, _stats_jh) = create_plugin_runtime(
        "STATS1", proc, pool.clone(), 10, "SIM1", wiring.clone(),
    );
    wiring.register_output("STATS1", stats_handle.array_output().clone());
    wiring.rewire(stats_handle.array_sender(), "", "SIM1").unwrap();

    // Enable STATS1
    stats_handle.port_runtime().port_handle()
        .write_int32_blocking(stats_handle.plugin_params.enable_callbacks, 0, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Send array from SIM1 → both ROI1 and STATS1 receive directly
    let mut arr = NDArray::new(vec![NDDimension::new(8), NDDimension::new(8)], NDDataType::UInt8);
    arr.unique_id = 100;
    sim_output.lock().publish(Arc::new(arr));
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(*last_id.lock(), 100, "STATS1 gets array from SIM1");

    // Rewire STATS1 from SIM1 → ROI1
    stats_handle.port_runtime().port_handle()
        .submit_blocking(
            RequestOp::OctetWrite { data: b"ROI1".to_vec() },
            AsynUser::new(stats_handle.plugin_params.nd_array_port),
        ).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Send another array from SIM1
    // ROI1 gets it from SIM1, processes it, publishes to its output
    // STATS1 (now wired to ROI1) should receive ROI1's output
    let mut arr2 = NDArray::new(vec![NDDimension::new(8), NDDimension::new(8)], NDDataType::UInt8);
    arr2.unique_id = 200;
    sim_output.lock().publish(Arc::new(arr2));
    std::thread::sleep(std::time::Duration::from_millis(200));

    let received_id = *last_id.lock();
    assert!(received_id != 100, "STATS1 should receive new array through ROI1, got {received_id}");
}

#[test]
fn test_roi_param_change_enables_output() {
    use ad_core::plugin::channel::NDArrayOutput;
    use ad_core::plugin::runtime::{create_plugin_runtime, ProcessResult};

    let pool = Arc::new(NDArrayPool::new(1_000_000));
    let wiring = Arc::new(WiringRegistry::new());

    // SIM1 driver output
    let sim_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    wiring.register_output("SIM1", sim_output.clone());

    // Create ROI1 with DEFAULT config (size=0) — like the real IOC
    let (roi_handle, roi_params, _roi_jh) = ad_plugins::roi::create_roi_runtime(
        "ROI1", pool.clone(), 10, "SIM1", wiring.clone(),
    );
    wiring.register_output("ROI1", roi_handle.array_output().clone());
    wiring.rewire(roi_handle.array_sender(), "", "SIM1").unwrap();

    // Enable ROI1
    roi_handle.port_runtime().port_handle()
        .write_int32_blocking(roi_handle.plugin_params.enable_callbacks, 0, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Tracking processor downstream of ROI1
    struct TrackingProcessor { last_id: Arc<parking_lot::Mutex<i32>> }
    impl NDPluginProcess for TrackingProcessor {
        fn process_array(&mut self, array: &ad_core::ndarray::NDArray, _pool: &NDArrayPool) -> ProcessResult {
            *self.last_id.lock() = array.unique_id;
            ProcessResult::empty()
        }
        fn plugin_type(&self) -> &str { "Tracking" }
    }

    let last_id = Arc::new(parking_lot::Mutex::new(-1i32));
    let proc = TrackingProcessor { last_id: last_id.clone() };
    let (stats_handle, _jh) = create_plugin_runtime(
        "STATS1", proc, pool.clone(), 10, "ROI1", wiring.clone(),
    );
    wiring.register_output("STATS1", stats_handle.array_output().clone());
    wiring.rewire(stats_handle.array_sender(), "", "ROI1").unwrap();
    stats_handle.port_runtime().port_handle()
        .write_int32_blocking(stats_handle.plugin_params.enable_callbacks, 0, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Send array — ROI1 has default size=0, should produce NO output
    let mut arr = NDArray::new(vec![NDDimension::new(8), NDDimension::new(8)], NDDataType::UInt8);
    arr.unique_id = 100;
    sim_output.lock().publish(Arc::new(arr));
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(*last_id.lock(), -1, "STATS1 should NOT receive with ROI size=0");

    // Now set ROI size via param write (like PINI or user).
    let roi_ph = roi_handle.port_runtime().port_handle();
    roi_ph.write_int32_blocking(roi_params.dims[0].size, 0, 8).unwrap();
    roi_ph.write_int32_blocking(roi_params.dims[1].size, 0, 8).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Send another array — now ROI1 has size=8x8, should produce output
    let mut arr2 = NDArray::new(vec![NDDimension::new(8), NDDimension::new(8)], NDDataType::UInt8);
    arr2.unique_id = 200;
    sim_output.lock().publish(Arc::new(arr2));
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(*last_id.lock(), 200, "STATS1 should receive after ROI size set to 8x8");
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
    use ad_plugins::roi::{ROIConfig, ROIDimConfig, ROIProcessor};
    use ad_plugins::stats::StatsProcessor;

    let pool = NDArrayPool::new(1_000_000);
    let arr = make_2d_u8(16, 16);

    // ROI: extract 4x4 region from (2,2)
    let mut roi_config = ROIConfig::default();
    roi_config.dims[0] = ROIDimConfig { min: 2, size: 4, bin: 1, reverse: false, enable: true, auto_size: false };
    roi_config.dims[1] = ROIDimConfig { min: 2, size: 4, bin: 1, reverse: false, enable: true, auto_size: false };

    let mut roi_proc = ROIProcessor::new(roi_config);
    let roi_result = roi_proc.process_array(&arr, &pool);
    assert_eq!(roi_result.output_arrays.len(), 1);
    assert_eq!(roi_result.output_arrays[0].dims[0].size, 4);
    assert_eq!(roi_result.output_arrays[0].dims[1].size, 4);

    // Feed ROI output to Stats
    let mut stats_proc = StatsProcessor::new();
    let stats_result = stats_proc.process_array(&roi_result.output_arrays[0], &pool);

    // Stats should be computed on the 4x4 ROI, not the full 16x16
    let stats = stats_proc.stats_handle().lock().clone();
    assert_eq!(stats.num_elements, 16); // 4*4
    assert!(stats.min >= 0.0);
    assert!(stats.max <= 255.0);
    assert!(stats_result.output_arrays.is_empty()); // stats is a sink
}

#[test]
fn test_process_then_file_tiff_pipeline() {
    use ad_plugins::process::{ProcessConfig, ProcessProcessor};
    use ad_plugins::file_tiff::TiffFileProcessor;
    use ad_core::plugin::file_base::NDFileMode;

    let pool = NDArrayPool::new(1_000_000);
    let arr = make_2d_u8(8, 8);

    // Process: scale by 2
    let mut proc = ProcessProcessor::new(ProcessConfig {
        enable_offset_scale: true,
        scale: 2.0,
        offset: 0.0,
        ..Default::default()
    });
    let proc_result = proc.process_array(&arr, &pool);
    assert_eq!(proc_result.output_arrays.len(), 1);

    // Write to TIFF
    let path = std::env::temp_dir().join("integration_process_tiff.tif");
    let mut tiff_proc = TiffFileProcessor::new();
    tiff_proc.file_base_mut().file_path = path.parent().unwrap().to_str().unwrap().into();
    tiff_proc.file_base_mut().file_name = path.file_name().unwrap().to_str().unwrap().into();
    tiff_proc.file_base_mut().set_mode(NDFileMode::Single);

    // Use the writer directly for this test
    use ad_core::plugin::file_base::NDFileWriter;
    use ad_plugins::file_tiff::TiffWriter;
    let mut writer = TiffWriter::new();
    writer.open_file(&path, NDFileMode::Single, &proc_result.output_arrays[0]).unwrap();
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
    use ad_plugins::circular_buff::{CircularBuffProcessor, TriggerCondition};

    let pool = NDArrayPool::new(1_000_000);
    let mut proc = CircularBuffProcessor::new(
        2, // pre_count
        1, // post_count
        TriggerCondition::External,
    );

    // Fill pre-buffer
    let arr1 = make_2d_u8(4, 4);
    let mut arr1c = arr1.clone(); arr1c.unique_id = 1;
    proc.process_array(&arr1c, &pool);

    let mut arr2 = arr1.clone(); arr2.unique_id = 2;
    proc.process_array(&arr2, &pool);

    let mut arr3 = arr1.clone(); arr3.unique_id = 3;
    proc.process_array(&arr3, &pool);

    // Trigger
    proc.trigger();
    assert!(proc.buffer().is_triggered());

    // Post-trigger frame
    let mut arr4 = arr1.clone(); arr4.unique_id = 4;
    let result = proc.process_array(&arr4, &pool);

    // Should have captured: 2 pre + 1 post = 3 frames
    assert_eq!(result.output_arrays.len(), 3);
    assert_eq!(result.output_arrays[0].unique_id, 2);
    assert_eq!(result.output_arrays[1].unique_id, 3);
    assert_eq!(result.output_arrays[2].unique_id, 4);
}

#[test]
fn test_codec_compress_decompress_roundtrip() {
    use ad_plugins::codec::{compress_lz4, decompress_lz4};

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
    use ad_plugins::attribute::AttributeProcessor;

    let pool = NDArrayPool::new(1_000_000);
    let mut proc = AttributeProcessor::new("exposure");

    let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
    arr.attributes.add(NDAttribute {
        name: "exposure".into(),
        description: "".into(),
        source: NDAttrSource::Driver,
        value: NDAttrValue::Float64(0.5),
    });

    let result = proc.process_array(&arr, &pool);
    // AttributeProcessor is a sink (no output arrays)
    assert!(result.output_arrays.is_empty());
    // Check param updates contain the value
    assert!(!result.param_updates.is_empty());
}

#[test]
fn test_pos_plugin_position_attachment() {
    use ad_plugins::pos_plugin::{PosPluginProcessor, PosMode};

    let pool = NDArrayPool::new(1_000_000);
    let mut proc = PosPluginProcessor::new(PosMode::Discard);

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
    let mx = out.attributes.get("MotorX").unwrap().value.as_f64().unwrap();
    assert!((mx - 42.5).abs() < 1e-10);
    let my = out.attributes.get("MotorY").unwrap().value.as_f64().unwrap();
    assert!((my - 13.7).abs() < 1e-10);
}

#[test]
fn test_process_and_publish_writes_array_size_params() {
    // Verify that process_and_publish writes ArraySizeX/Y/Z params correctly.
    let pool = Arc::new(NDArrayPool::new(1_000_000));
    let wiring = Arc::new(WiringRegistry::new());
    let (image_handle, image_data, _jh) = create_std_arrays_runtime("IMG_SZ", pool.clone(), "DRV1", wiring);

    // Enable callbacks
    let ph = image_handle.port_runtime().port_handle();
    ph.write_int32_blocking(image_handle.plugin_params.enable_callbacks, 0, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Connect and send a 64x48 array
    let mut driver = ad_core::driver::ad_driver::ADDriverBase::new("DRV1", 64, 48, 1_000_000).unwrap();
    driver.connect_downstream(image_handle.array_sender().clone());

    let mut arr = NDArray::new(
        vec![NDDimension::new(64), NDDimension::new(48)],
        NDDataType::UInt8,
    );
    arr.unique_id = 42;
    driver.publish_array(Arc::new(arr)).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(200));

    // Verify the StdArrays data was stored
    let latest = image_data.lock().clone();
    assert!(latest.is_some(), "StdArrays should have latest data");

    // Read back params from the plugin's port handle
    let size_x = ph.read_int32_blocking(image_handle.ndarray_params.array_size_x, 0).unwrap();
    let size_y = ph.read_int32_blocking(image_handle.ndarray_params.array_size_y, 0).unwrap();
    let size_z = ph.read_int32_blocking(image_handle.ndarray_params.array_size_z, 0).unwrap();
    let array_size = ph.read_int32_blocking(image_handle.ndarray_params.array_size, 0).unwrap();
    let counter = ph.read_int32_blocking(image_handle.ndarray_params.array_counter, 0).unwrap();
    let unique_id = ph.read_int32_blocking(image_handle.ndarray_params.unique_id, 0).unwrap();

    assert_eq!(size_x, 64, "ArraySizeX should be 64");
    assert_eq!(size_y, 48, "ArraySizeY should be 48");
    assert_eq!(size_z, 1, "ArraySizeZ should be 1 for 2D mono");
    assert_eq!(array_size, 64 * 48, "ArraySize should be total bytes");
    assert_eq!(counter, 1, "ArrayCounter should be 1");
    assert_eq!(unique_id, 42, "UniqueId should be 42");
}
