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
use scope_ioc::driver::{ScopeSimulator, sim_task, sim_task_handle};

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

/// simTask posts the statistics with callParamCallbacks before the trace's
/// doCallbacksFloat64Array (testAsynPortDriver.cpp:224-230), so a client
/// sees a cycle's min/max/mean before its waveform.
#[epics_base_rs::epics_test]
async fn the_statistics_are_posted_before_the_waveform() {
    let notify = Arc::new(Notify::new());
    let driver = ScopeSimulator::new("scopeOrder", notify.clone());
    let idx = driver.param_indices();
    let claim = asyn_rs::registry::claim_port("scopeOrder").unwrap();
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
    handle.write_int32(idx.p_run, 0, 1).await.unwrap();

    // The constructor's statistics are 0; a cycle's min is about -1.
    let mut stats = Vec::new();
    epics_base_rs::runtime::task::timeout(Duration::from_secs(5), async {
        loop {
            let iv = rx.recv().await.expect("the port is alive");
            if let ParamValue::Float64(v) = iv.value
                && v != 0.0
                && [idx.p_min_value, idx.p_max_value, idx.p_mean_value].contains(&iv.reason)
            {
                stats.push(iv.reason);
            }
            if iv.reason == idx.p_waveform
                && let ParamValue::Float64Array(trace) = iv.value
                && trace.iter().any(|v| *v != 0.0)
            {
                return;
            }
        }
    })
    .await
    .expect("a running simulation posts its waveform");
    assert!(
        stats.contains(&idx.p_min_value) && stats.contains(&idx.p_max_value),
        "the first cycle's waveform came before its statistics: {stats:?}"
    );
}

/// The same order from the task that drives a hand-held driver.
#[epics_base_rs::epics_test]
async fn sim_task_posts_the_statistics_before_the_waveform() {
    let notify = Arc::new(Notify::new());
    let mut driver = ScopeSimulator::new("scopeOrderMutex", notify.clone());
    driver.base.interrupts =
        asyn_rs::interrupt::InterruptManager::owned_by(asyn_rs::registry::open_owner());
    let idx = driver.param_indices();
    let mut rx = driver.base.interrupts.subscribe_async(256, 1 << 16);
    driver
        .base
        .set_float64_param(idx.p_update_time, 0, 0.05)
        .unwrap();
    driver
        .base
        .set_float64_param(idx.p_noise_amplitude, 0, 0.5)
        .unwrap();
    driver.base.set_int32_param(idx.p_run, 0, 1).unwrap();
    let port = Arc::new(parking_lot::Mutex::new(driver));
    asyn_rs::runtime::task::spawn(async move { sim_task(port, notify, idx).await });

    // Each trace is 5 + y at unit volts/div and no offset, so the min posted
    // ahead of it is its own only if the statistics went first.
    let mut last_min = None;
    let mut checked = 0;
    epics_base_rs::runtime::task::timeout(Duration::from_secs(5), async {
        while checked < 3 {
            let iv = rx.recv().await.expect("the port is alive");
            match iv.value {
                ParamValue::Float64(v) if iv.reason == idx.p_min_value => last_min = Some(v),
                // The constructor's zero trace is not a cycle's.
                ParamValue::Float64Array(trace)
                    if iv.reason == idx.p_waveform && trace.iter().any(|v| *v != 0.0) =>
                {
                    let trace_min = trace.iter().copied().fold(f64::INFINITY, f64::min) - 5.0;
                    if let Some(min) = last_min {
                        assert!(
                            (trace_min - min).abs() < 1e-9,
                            "the waveform came before its statistics: {trace_min} vs {min}"
                        );
                        checked += 1;
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .expect("a running simulation posts");
}
