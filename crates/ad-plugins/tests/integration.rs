use std::sync::Arc;

use ad_core::ndarray::{NDDataBuffer, NDDataType, NDDimension};
use ad_core::driver::ad_driver::ADDriverBase;
use ad_plugins::stats::create_stats_runtime;
use ad_plugins::std_arrays::create_std_arrays_runtime;

#[test]
fn test_driver_to_stats_pipeline() {
    let pool = Arc::new(ad_core::ndarray_pool::NDArrayPool::new(10_000_000));
    let (stats_handle, stats_data, _jh) = create_stats_runtime("STATS1", pool.clone(), 10);

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
    let (image_handle, image_data, _jh) = create_std_arrays_runtime("IMAGE1", pool.clone());

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
