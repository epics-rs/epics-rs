//! C NDPluginTimeSeries holds its circular buffer as an NDArray from
//! `pNDArrayPool` (NDPluginTimeSeries.cpp:132), allocated by the constructor
//! and again on every NumPoints write, so the plugin's PoolPollStats
//! readbacks count it from boot.

use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDimension};
use ad_core_rs::ndarray_pool::DriverPool;
use ad_core_rs::plugin::runtime::{DEFAULT_PLUGIN_THREADS, create_plugin_runtime_multi_addr};
use ad_core_rs::plugin::wiring::WiringRegistry;
use ad_plugins_rs::time_series_plugin::TimeSeriesProcessor;
use asyn_rs::port::DrvUserRequest;

#[test]
fn the_circular_buffer_is_counted_on_the_plugin_pool() {
    let wiring = Arc::new(WiringRegistry::new());
    let (ts, _jh) = create_plugin_runtime_multi_addr(
        &asyn_rs::asyn_record::claim_port("TSPOOL").unwrap(),
        TimeSeriesProcessor::new(3),
        0,
        10,
        "",
        wiring,
        4,
        DEFAULT_PLUGIN_THREADS,
    );
    let port = ts.port_runtime().port_handle();
    let p = ts.ndarray_params;
    let applied = || assert!(ts.wait_params_applied(std::time::Duration::from_secs(10)));
    let poll = || {
        port.write_int32_blocking(p.pool_poll_stats, 0, 1).unwrap();
        applied();
        (
            port.read_int32_blocking(p.pool_alloc_buffers, 0).unwrap(),
            port.read_int32_blocking(p.pool_free_buffers, 0).unwrap(),
            port.read_float64_blocking(p.pool_used_memory, 0).unwrap(),
        )
    };
    const MB: f64 = 1024.0 * 1024.0;

    // The constructor's [2048 points, 3 signals] Float64 buffer.
    assert_eq!(poll(), (1, 0, (2048 * 3 * 8) as f64 / MB));

    // NumPoints releases it first; the free buffer is over 1.5x the new
    // size, so alloc frees it and allocates afresh (NDArrayPool.cpp:161-183).
    let num_points = port
        .drv_user_create_blocking(&DrvUserRequest::new("TS_NUM_POINTS", 0))
        .unwrap()
        .reason;
    port.write_int32_blocking(num_points, 0, 10).unwrap();
    applied();
    assert_eq!(poll(), (1, 0, (10 * 3 * 8) as f64 / MB));
}

/// A frame of another type or width reallocates the buffer (:414-422) from
/// the pool the frame brought (NDPluginDriver.cpp:419); the old one goes back
/// to the plugin's own pool.
// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// ad-plugins-rs --all-features`); it builds no CA server, and its reactor is
// the sender thread's own current-thread runtime.
#[test]
fn a_new_frame_shape_reallocates_from_the_frame_s_pool() {
    let wiring = Arc::new(WiringRegistry::new());
    let (ts, _jh) = create_plugin_runtime_multi_addr(
        &asyn_rs::asyn_record::claim_port("TSPOOLF").unwrap(),
        TimeSeriesProcessor::new(3),
        0,
        10,
        "",
        wiring,
        4,
        DEFAULT_PLUGIN_THREADS,
    );
    let port = ts.port_runtime().port_handle();
    port.write_int32_blocking(ts.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    assert!(ts.wait_params_applied(std::time::Duration::from_secs(10)));

    let driver = DriverPool::standalone(0);
    let frame = NDArray::with_data(vec![NDDimension::new(2)], NDDataBuffer::I16(vec![1, 2]));
    let sender = ts.array_sender().clone();
    let published = driver.alloc_copy(&frame).unwrap();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(sender.publish(published));
    })
    .join()
    .unwrap();
    assert!(ts.wait_params_applied(std::time::Duration::from_secs(10)));

    let own = ts.own_pool().pool();
    assert_eq!((own.num_alloc_buffers(), own.num_free_buffers()), (1, 1));
    // The frame's buffer and the [2048, 2] Int16 series.
    let drv = driver.pool();
    assert_eq!(drv.num_alloc_buffers(), 2);
    assert_eq!(drv.allocated_bytes(), 2 * 2 + 2048 * 2 * 2);
}
