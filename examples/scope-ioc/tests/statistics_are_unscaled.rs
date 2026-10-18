//! C simTask takes the statistics before the offset and scale, and its noise
//! is `amplitude * (rand()/RAND_MAX - 0.5)` (testAsynPortDriver.cpp:204-212).

use scope_ioc::driver::{Rng, SimConfig, compute_waveform};

fn config(volts_per_div: f64, volt_offset: f64, noise_amplitude: f64) -> SimConfig {
    SimConfig {
        run: true,
        max_points: 1000,
        time_per_div: 0.001,
        volts_per_div,
        volt_offset,
        trigger_delay: 0.0,
        noise_amplitude,
        update_time: 0.5,
    }
}

#[test]
fn the_statistics_are_of_the_unscaled_volts() {
    let r = compute_waveform(&config(0.5, 2.0, 0.0), &mut Rng::new(1));
    assert!((r.max_val - 1.0).abs() < 1e-3, "{}", r.max_val);
    assert!((r.min_val + 1.0).abs() < 1e-3, "{}", r.min_val);
    assert!(r.mean_val.abs() < 1e-3, "{}", r.mean_val);
}

#[test]
fn the_noise_spans_half_the_amplitude() {
    let clean = compute_waveform(&config(1.0, 0.0, 0.0), &mut Rng::new(1));
    let noisy = compute_waveform(&config(1.0, 0.0, 4.0), &mut Rng::new(1));
    let worst = clean
        .waveform
        .iter()
        .zip(&noisy.waveform)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max);
    assert!(worst <= 2.0 && worst > 1.5, "{worst}");
}

/// simTask starts min/max at 1e6/-1e6 and divides the sum by maxPoints
/// (testAsynPortDriver.cpp:209-211, :221), so an empty trace reports those.
#[test]
fn an_empty_trace_reports_sim_tasks_start_values() {
    let mut empty = config(1.0, 0.0, 0.0);
    empty.max_points = 0;
    let r = compute_waveform(&empty, &mut Rng::new(1));
    assert_eq!((r.min_val, r.max_val), (1e6, -1e6));
    assert!(r.mean_val.is_nan(), "{}", r.mean_val);
}

/// simTask's loop runs `i < maxPoints` on the int it read (:201-213), so a
/// negative MaxPoints is an empty trace: min/max stay 1e6/-1e6 and the mean
/// is 0/maxPoints.
#[test]
fn a_negative_max_points_is_an_empty_trace() {
    let sim = scope_ioc::driver::ScopeSimulator::new(
        "scopeNegPts",
        std::sync::Arc::new(asyn_rs::runtime::sync::Notify::new()),
    );
    let idx = sim.param_indices();
    let mut base = sim.base;
    base.set_int32_param(idx.p_max_points, 0, -1).unwrap();
    let config = scope_ioc::driver::read_config(&base, &idx);
    let r = compute_waveform(&config, &mut Rng::new(1));
    assert!(r.waveform.is_empty());
    assert_eq!((r.min_val, r.max_val), (1e6, -1e6));
    assert_eq!(r.mean_val, 0.0);
    assert!(r.mean_val.is_sign_negative(), "0/-1 is -0");
}
