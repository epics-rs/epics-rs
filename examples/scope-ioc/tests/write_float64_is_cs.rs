//! testAsynPortDriver::writeFloat64 (testAsynPortDriver.cpp:290-326): an
//! UpdateTime below MIN_UPDATE_TIME is raised with an ASYN_TRACE_WARNING
//! line, and the write ends with callParamCallbacks, which posts every
//! changed parameter, not only the one written. readFloat64Array (:338-357)
//! cuts its reads to MaxPoints.

use std::sync::Arc;
use std::time::Duration;

use asyn_rs::param::ParamValue;
use asyn_rs::request::ParamSetValue;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
use asyn_rs::runtime::sync::Notify;
use asyn_rs::trace::{TraceFile, TraceMask};
use scope_ioc::driver::ScopeSimulator;

#[epics_base_rs::epics_test]
async fn a_short_update_time_is_raised_with_cs_warning() {
    let driver = ScopeSimulator::new("scopeUpdWarn", Arc::new(Notify::new()));
    let idx = driver.param_indices();
    let claim = asyn_rs::registry::claim_port("scopeUpdWarn").unwrap();
    let (runtime, _jh) = create_port_runtime(&claim, driver, RuntimeConfig::default()).unwrap();
    let handle = runtime.port_handle().clone();
    claim
        .publish(handle.clone(), &asyn_rs::registry::open_owner())
        .unwrap();
    let path = std::env::temp_dir().join(format!("scope-upd-warn-{}.log", std::process::id()));
    let file = Arc::new(std::sync::Mutex::new(std::fs::File::create(&path).unwrap()));
    handle
        .trace()
        .set_trace_file(Some("scopeUpdWarn"), TraceFile::File(file));
    handle
        .trace()
        .set_trace_mask(Some("scopeUpdWarn"), TraceMask::WARNING);

    handle
        .write_float64(idx.p_update_time, 0, 0.001)
        .await
        .unwrap();

    assert_eq!(
        handle.read_float64(idx.p_update_time, 0).await.unwrap(),
        0.02
    );
    let log = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(
        log.contains(
            "testAsynPortDriver:writeFloat64: warning, update time too small, \
             changed from 0.001000 to 0.020000\n"
        ),
        "{log:?}"
    );
}

#[epics_base_rs::epics_test]
async fn a_float64_write_posts_every_changed_parameter() {
    let driver = ScopeSimulator::new("scopeWriteFlush", Arc::new(Notify::new()));
    let idx = driver.param_indices();
    let claim = asyn_rs::registry::claim_port("scopeWriteFlush").unwrap();
    let (runtime, _jh) = create_port_runtime(&claim, driver, RuntimeConfig::default()).unwrap();
    let handle = runtime.port_handle().clone();
    claim
        .publish(handle.clone(), &asyn_rs::registry::open_owner())
        .unwrap();
    // A round trip lets the registration's one-shot flush go first.
    handle.read_float64(idx.p_volt_offset, 0).await.unwrap();
    let mut rx = handle.interrupts().subscribe_async(256, 1 << 16);

    // Changed but not yet posted, as a C setDoubleParam alone leaves it.
    handle
        .set_params_blocking(
            0,
            vec![ParamSetValue::new(
                idx.p_volt_offset,
                0,
                ParamValue::Float64(0.25),
            )],
        )
        .unwrap();
    handle
        .write_float64(idx.p_trigger_delay, 0, 0.5)
        .await
        .unwrap();

    let mut seen = Vec::new();
    let _ = epics_base_rs::runtime::task::timeout(Duration::from_secs(2), async {
        while !(seen.contains(&idx.p_volt_offset) && seen.contains(&idx.p_trigger_delay)) {
            let iv = rx.recv().await.expect("the port is alive");
            seen.push(iv.reason);
        }
    })
    .await;
    assert!(seen.contains(&idx.p_trigger_delay), "{seen:?}");
    assert!(seen.contains(&idx.p_volt_offset), "{seen:?}");
}

/// readFloat64Array copies min(MaxPoints, nElements) of either array
/// (testAsynPortDriver.cpp:346-357), the trace as well as the time base.
#[epics_base_rs::epics_test]
async fn the_waveform_read_is_cut_to_max_points() {
    let driver = ScopeSimulator::new("scopeMaxPts", Arc::new(Notify::new()));
    let idx = driver.param_indices();
    let claim = asyn_rs::registry::claim_port("scopeMaxPts").unwrap();
    let (runtime, _jh) = create_port_runtime(&claim, driver, RuntimeConfig::default()).unwrap();
    let handle = runtime.port_handle().clone();
    claim
        .publish(handle.clone(), &asyn_rs::registry::open_owner())
        .unwrap();

    handle.write_int32(idx.p_max_points, 0, 10).await.unwrap();
    for reason in [idx.p_waveform, idx.p_time_base] {
        let read = handle.read_float64_array(reason, 0, 1 << 14).await.unwrap();
        assert_eq!(read.len(), 10, "reason {reason}");
    }
}
