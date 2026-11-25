//! testAsynPortDriver's constructor values (testAsynPortDriver.cpp:105-118):
//! gain 10, noise 0.1, zero statistics, the calloc'd trace, and the time and
//! volts selects never set.

use std::sync::Arc;

use asyn_rs::runtime::sync::Notify;
use scope_ioc::driver::ScopeSimulator;

#[test]
fn the_constructor_sets_c_s_values() {
    let driver = ScopeSimulator::new("scopeInitial", Arc::new(Notify::new()));
    let base = &driver.base;
    assert_eq!(
        base.get_int32_param(driver.p_vert_gain_select, 0).unwrap(),
        10
    );
    assert_eq!(base.get_float64_param(driver.p_vert_gain, 0).unwrap(), 10.0);
    assert_eq!(
        base.get_float64_param(driver.p_noise_amplitude, 0).unwrap(),
        0.1
    );
    for p in [driver.p_min_value, driver.p_max_value, driver.p_mean_value] {
        assert_eq!(base.get_float64_param(p, 0).unwrap(), 0.0);
    }
    let trace = base.params.get_float64_array(driver.p_waveform, 0).unwrap();
    assert_eq!(trace.len(), 1000);
    assert!(trace.iter().all(|&v| v == 0.0));
    assert!(
        base.get_int32_param(driver.p_time_per_div_select, 0)
            .is_err()
    );
    assert!(
        base.get_int32_param(driver.p_volts_per_div_select, 0)
            .is_err()
    );
}
