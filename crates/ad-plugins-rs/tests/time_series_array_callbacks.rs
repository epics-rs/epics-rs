// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — it ran and passed on
// the exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run
// -p ad-plugins-rs --all-features`, 568/568); the runtime only drives one
// publish.
//! C NDPluginTimeSeries copies, allocates and takes `uniqueId_++` only inside
//! `if (arrayCallbacks)` (NDPluginTimeSeries.cpp:341-367), so a TSRead with
//! NDArrayCallbacks 0 leaves the next post's uniqueId where it was.

use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::channel::ndarray_channel;
use ad_core_rs::plugin::runtime::{DEFAULT_PLUGIN_THREADS, create_plugin_runtime_multi_addr};
use ad_core_rs::plugin::wiring::WiringRegistry;
use ad_plugins_rs::time_series_plugin::TimeSeriesProcessor;
use asyn_rs::port::DrvUserRequest;

#[test]
fn a_tsread_without_array_callbacks_takes_no_unique_id() {
    let pool = NDArrayPool::new(1_000_000);
    let wiring = Arc::new(WiringRegistry::new());
    let (ts, _jh) = create_plugin_runtime_multi_addr(
        &asyn_rs::asyn_record::claim_port("TSAC").unwrap(),
        TimeSeriesProcessor::new(1),
        pool.max_memory(),
        10,
        0,
        "",
        wiring.clone(),
        2,
        DEFAULT_PLUGIN_THREADS,
    );
    let (down, mut rx) = ndarray_channel("TSAC_DOWN", 10);
    // numSignals_ is 1, so the 2-D series posts at address 1.
    wiring.subscribe(&down, "TSAC", 1).unwrap();
    let port = ts.port_runtime().port_handle();
    let reason = |name: &str| {
        port.drv_user_create_blocking(&DrvUserRequest::new(name, 0))
            .unwrap()
            .reason
    };
    let applied = || assert!(ts.wait_params_applied(std::time::Duration::from_secs(10)));
    let write = |reason: usize, v: i32| {
        port.write_int32_blocking(reason, 0, v).unwrap();
        applied();
    };
    write(ts.plugin_params.enable_callbacks, 1);
    write(reason("TS_ACQUIRE"), 1);

    let sender = ts.array_sender().clone();
    let publish = |v: f64| {
        let sender = sender.clone();
        let frame = NDArray::with_data(vec![NDDimension::new(1)], NDDataBuffer::F64(vec![v]));
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(
                    sender.publish(
                        ad_core_rs::ndarray_pool::DriverPool::standalone(0)
                            .alloc_copy(&frame)
                            .unwrap(),
                    ),
                );
        })
        .join()
        .unwrap();
        applied();
    };
    publish(7.0);

    let ts_read = reason("TS_READ");
    write(ts.ndarray_params.array_callbacks, 0);
    write(ts_read, 1);
    // A second point, so the first array to arrive tells which read made it.
    publish(9.0);
    write(ts.ndarray_params.array_callbacks, 1);
    write(ts_read, 1);
    let posted = rx.blocking_recv().unwrap();
    let NDDataBuffer::F64(series) = &posted.data else {
        panic!("not Float64");
    };
    assert_eq!(series[..2], [7.0, 9.0], "the withheld TSRead posted");
    assert_eq!(posted.unique_id, 0, "the withheld TSRead took a uniqueId");
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — it ran and passed on
// the exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run
// -p ad-plugins-rs --all-features`, 603/603).
/// C's control-write posts come from `pNDArrayPool` (NDPluginTimeSeries.cpp:347,
/// :374): the plugin's own pool until a frame arrives, then that frame's
/// (NDPluginDriver.cpp:419).
#[test]
fn a_tsread_allocates_from_the_pool_the_last_frame_brought() {
    let wiring = Arc::new(WiringRegistry::new());
    let (ts, _jh) = create_plugin_runtime_multi_addr(
        &asyn_rs::asyn_record::claim_port("TSPOOL").unwrap(),
        TimeSeriesProcessor::new(1),
        1_000_000,
        10,
        0,
        "",
        wiring.clone(),
        2,
        DEFAULT_PLUGIN_THREADS,
    );
    let (down, mut rx) = ndarray_channel("TSPOOL_DOWN", 10);
    wiring.subscribe(&down, "TSPOOL", 1).unwrap();
    let port = ts.port_runtime().port_handle();
    let applied = || assert!(ts.wait_params_applied(std::time::Duration::from_secs(10)));
    let ts_read = port
        .drv_user_create_blocking(&DrvUserRequest::new("TS_READ", 0))
        .unwrap()
        .reason;
    let mut read = || {
        port.write_int32_blocking(ts_read, 0, 1).unwrap();
        applied();
        rx.blocking_recv().unwrap().pool().unwrap()
    };
    port.write_int32_blocking(ts.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    applied();

    assert!(Arc::ptr_eq(&read(), ts.own_pool().pool()));

    let camera = ad_core_rs::ndarray_pool::DriverPool::standalone(0);
    let frame = NDArray::with_data(vec![NDDimension::new(1)], NDDataBuffer::F64(vec![1.0]));
    let frame = camera.alloc_copy(&frame).unwrap();
    let sender = ts.array_sender().clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(sender.publish(frame))
    })
    .join()
    .unwrap();
    applied();

    assert!(Arc::ptr_eq(&read(), camera.pool()));
}
