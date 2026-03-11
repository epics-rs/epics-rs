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
