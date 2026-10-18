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
