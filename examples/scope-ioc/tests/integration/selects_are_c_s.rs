//! testAsynPortDriver's three selects are asynInt32 parameters
//! (testAsynPortDriver.cpp:83-89): their raw values are the setting, and
//! only `VoltsPerDivSelect` has a driver table (`readEnum` :376-394), which
//! `setVertGain` rebuilds in mV (:398-409). The rs driver once made them
//! Enum parameters whose index tables replaced the db's strings and values.

use std::sync::Arc;
use std::time::Duration;

use asyn_rs::param::{EnumEntry, ParamValue};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::request::RequestOp;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
use asyn_rs::runtime::sync::Notify;
use asyn_rs::user::AsynUser;
use scope_ioc::driver::ScopeSimulator;

async fn enum_table(handle: &PortHandle, reason: usize) -> Option<Vec<(String, i32)>> {
    let result = handle
        .submit_async(RequestOp::EnumRead, AsynUser::new(reason))
        .await
        .ok()?;
    Some(table(&result.enum_entries.unwrap()))
}

fn table(entries: &[EnumEntry]) -> Vec<(String, i32)> {
    entries
        .iter()
        .map(|e| (e.string.clone(), e.value))
        .collect()
}

fn expected(strings: [&str; 4], values: [i32; 4]) -> Vec<(String, i32)> {
    strings.map(String::from).into_iter().zip(values).collect()
}

#[epics_base_rs::epics_test]
async fn the_selects_are_c_s_int32_parameters() {
    let driver = ScopeSimulator::new("scopeSelects", Arc::new(Notify::new()));
    let p = (
        driver.p_time_per_div_select,
        driver.p_time_per_div,
        driver.p_vert_gain_select,
        driver.p_vert_gain,
        driver.p_volts_per_div_select,
        driver.p_volts_per_div,
    );
    let claim = asyn_rs::registry::claim_port("scopeSelects").unwrap();
    let (runtime, _jh) = create_port_runtime(&claim, driver, RuntimeConfig::default()).unwrap();
    let handle = runtime.port_handle().clone();
    let mut rx = handle.interrupts().subscribe_async(256, 1 << 16);
    claim
        .publish(handle.clone(), &asyn_rs::registry::open_owner())
        .unwrap();
    let (tpd_select, tpd, gain_select, gain, vpd_select, vpd) = p;

    // The time and gain selects keep the db's strings.
    assert_eq!(enum_table(&handle, tpd_select).await, None);
    assert_eq!(enum_table(&handle, gain_select).await, None);

    // TimePerDivSelect values are microseconds.
    handle.write_int32(tpd_select, 0, 200).await.unwrap();
    assert_eq!(handle.read_float64(tpd, 0).await.unwrap(), 200e-6);

    // VertGainSelect values are the gain; the volts table follows it.
    handle.write_int32(gain_select, 0, 10).await.unwrap();
    assert_eq!(handle.read_float64(gain, 0).await.unwrap(), 10.0);
    let c_table = expected(["0.10", "0.20", "0.50", "1.00"], [100, 200, 500, 1000]);
    assert_eq!(enum_table(&handle, vpd_select).await, Some(c_table.clone()));
    let posted = epics_base_rs::runtime::task::timeout(Duration::from_secs(5), async {
        loop {
            let iv = rx.recv().await.expect("the port is alive");
            if iv.reason == vpd_select
                && let ParamValue::Enum { choices, .. } = iv.value
            {
                return table(&choices);
            }
        }
    })
    .await
    .expect("setVertGain posts the volts table");
    assert_eq!(posted, c_table);

    // VoltsPerDivSelect values are mV.
    handle.write_int32(vpd_select, 0, 500).await.unwrap();
    assert_eq!(handle.read_float64(vpd, 0).await.unwrap(), 0.5);
}
