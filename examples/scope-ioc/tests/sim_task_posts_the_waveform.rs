//! C's simTask posts the trace with doCallbacksFloat64Array
//! (testAsynPortDriver.cpp:230); the handle-driven task must reach a
//! waveform subscriber too. The constructor's seed trace is posted at
//! registration, so only a noisy trace proves the task posted.

use std::sync::Arc;
use std::time::Duration;

use asyn_rs::param::ParamValue;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
use asyn_rs::runtime::sync::Notify;
use scope_ioc::driver::{ScopeSimulator, sim_task_handle};

#[epics_base_rs::epics_test]
async fn a_running_simulation_posts_the_waveform() {
    let notify = Arc::new(Notify::new());
    let driver = ScopeSimulator::new("scopePost", notify.clone());
    let idx = driver.param_indices();
    let claim = asyn_rs::registry::claim_port("scopePost").unwrap();
    let (runtime, _jh) = create_port_runtime(&claim, driver, RuntimeConfig::default()).unwrap();
    let handle = runtime.port_handle().clone();
    let mut rx = handle.interrupts().subscribe_async(256, 1 << 16);
    claim
        .publish(handle.clone(), &asyn_rs::registry::open_owner())
        .unwrap();

    let seed = handle
        .read_float64_array(idx.p_waveform, 0, 1 << 14)
        .await
        .unwrap()
        .to_vec();

    let sim = handle.clone();
    asyn_rs::runtime::task::spawn(async move { sim_task_handle(sim, notify, idx).await });
    handle
        .write_float64(idx.p_update_time, 0, 0.05)
        .await
        .unwrap();
    handle
        .write_float64(idx.p_noise_amplitude, 0, 0.5)
        .await
        .unwrap();
    handle.write_int32(idx.p_run, 0, 1).await.unwrap();

    let started = std::time::SystemTime::now();
    let (posted, stamp) = epics_base_rs::runtime::task::timeout(Duration::from_secs(5), async {
        loop {
            let iv = rx.recv().await.expect("the port is alive");
            if iv.reason == idx.p_waveform
                && let ParamValue::Float64Array(trace) = iv.value
                && !trace.is_empty()
                && trace[..] != seed[..]
            {
                return (trace, iv.timestamp);
            }
        }
    })
    .await
    .expect("a running simulation posts its waveform");
    assert!(posted.iter().all(|v| v.is_finite()));
    // simTask's updateTimeStamp (testAsynPortDriver.cpp:220) stamps the post.
    assert!(
        stamp >= started,
        "the post carries the port's updated stamp"
    );
}

/// testAsynPortDriver fixes the time base at construction (:71) and
/// no callback ever posts it, whatever the time-per-division.
#[epics_base_rs::epics_test]
async fn the_time_base_is_fixed_at_construction() {
    let notify = Arc::new(Notify::new());
    let driver = ScopeSimulator::new("scopeTimeBase", notify.clone());
    let idx = driver.param_indices();
    let claim = asyn_rs::registry::claim_port("scopeTimeBase").unwrap();
    let (runtime, _jh) = create_port_runtime(&claim, driver, RuntimeConfig::default()).unwrap();
    let handle = runtime.port_handle().clone();
    let mut rx = handle.interrupts().subscribe_async(256, 1 << 16);
    claim
        .publish(handle.clone(), &asyn_rs::registry::open_owner())
        .unwrap();

    let sim = handle.clone();
    asyn_rs::runtime::task::spawn(async move { sim_task_handle(sim, notify, idx).await });
    handle
        .write_float64(idx.p_update_time, 0, 0.05)
        .await
        .unwrap();
    handle
        .write_float64(idx.p_time_per_div, 0, 0.5)
        .await
        .unwrap();
    handle.write_int32(idx.p_run, 0, 1).await.unwrap();

    let mut waveforms = 0;
    epics_base_rs::runtime::task::timeout(Duration::from_secs(5), async {
        while waveforms < 3 {
            let iv = rx.recv().await.expect("the port is alive");
            assert_ne!(
                iv.reason, idx.p_time_base,
                "no callback posts the time base"
            );
            if iv.reason == idx.p_waveform {
                waveforms += 1;
            }
        }
    })
    .await
    .expect("a running simulation posts its waveform");

    let time_base = handle
        .read_float64_array(idx.p_time_base, 0, 1 << 14)
        .await
        .unwrap();
    assert_eq!(time_base.len(), 1000);
    for (i, t) in time_base.iter().enumerate() {
        assert_eq!(*t, i as f64 / 999.0 * 10.0);
    }
}
