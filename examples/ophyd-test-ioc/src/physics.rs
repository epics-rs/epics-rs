use rand::Rng;
use rand::rngs::StdRng;

const N_PER_I_PER_S: f64 = 200.0;

/// Configuration for beam current simulation.
#[derive(Debug, Clone)]
pub struct BeamCurrentConfig {
    pub offset: f64,
    pub amplitude: f64,
    pub period: f64,
}

impl Default for BeamCurrentConfig {
    fn default() -> Self {
        Self {
            offset: 500.0,
            amplitude: 25.0,
            period: 4.0,
        }
    }
}

/// Configuration for moving dot image generation.
#[derive(Debug, Clone)]
pub struct MovingDotImageConfig {
    pub sigma_x: f64,
    pub sigma_y: f64,
    pub background: f64,
    pub n_per_i_per_s: f64,
}

impl Default for MovingDotImageConfig {
    fn default() -> Self {
        Self {
            sigma_x: 50.0,
            sigma_y: 25.0,
            background: 1000.0,
            n_per_i_per_s: N_PER_I_PER_S,
        }
    }
}

/// Detector operating mode for point detectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectorMode {
    PinHole,
    Edge,
    Slit,
}

/// Compute the simulated beam current at time `t` seconds using the given config.
pub fn beam_current_with_config(t: f64, config: &BeamCurrentConfig) -> f64 {
    config.offset + config.amplitude * (2.0 * core::f64::consts::PI * t / config.period).sin()
}

/// Compute the simulated beam current at time `t` seconds.
/// Produces a sinusoidal oscillation: 500 + 25 * sin(2*pi*t/4) mA.
pub fn beam_current(t: f64) -> f64 {
    beam_current_with_config(t, &BeamCurrentConfig::default())
}

/// Compute a point detector reading based on mode, motor position, beam current, and exposure.
pub fn point_reading(
    mode: DetectorMode,
    mtr: f64,
    current: f64,
    exposure: f64,
    sigma: f64,
    center: f64,
) -> f64 {
    let base = N_PER_I_PER_S * current * exposure;
    match mode {
        DetectorMode::PinHole => {
            // Gaussian peak: N * I * exp * e^(-1/(2*sigma^2) * (mtr - center)^2)
            let arg = -1.0 / (2.0 * sigma * sigma) * (mtr - center) * (mtr - center);
            base * arg.exp()
        }
        DetectorMode::Edge => {
            // Error function edge: N * I * exp * erfc(1/sigma * (-mtr + center)) / 2
            let arg = (1.0 / sigma) * (-mtr + center);
            base * libm::erfc(arg) / 2.0
        }
        DetectorMode::Slit => {
            // Slit: N * I * exp * (erfc(1/sigma*(mtr-center)) - erfc(1/sigma*(mtr+center))) / 2
            let arg1 = (1.0 / sigma) * (mtr - center);
            let arg2 = (1.0 / sigma) * (mtr + center);
            base * (libm::erfc(arg1) - libm::erfc(arg2)) / 2.0
        }
    }
}

/// Default sigma for each detector mode (from caproto mini_beamline).
pub fn default_sigma(mode: DetectorMode) -> f64 {
    match mode {
        DetectorMode::PinHole => 5.0,
        DetectorMode::Edge => 2.5,
        DetectorMode::Slit => 2.5,
    }
}

/// Default center for each detector mode.
pub fn default_center(mode: DetectorMode) -> f64 {
    match mode {
        DetectorMode::PinHole => 0.0,
        DetectorMode::Edge => 5.0,
        DetectorMode::Slit => 7.5,
    }
}

/// Poisson sampling using Knuth's algorithm for small lambda,
/// or normal approximation for large lambda.
pub fn poisson_sample(rng: &mut StdRng, lambda: f64) -> f64 {
    if lambda <= 0.0 {
        return 0.0;
    }
    if lambda < 30.0 {
        // Knuth algorithm
        let l = (-lambda).exp();
        let mut k = 0i64;
        let mut p = 1.0;
        loop {
            k += 1;
            p *= rng.random::<f64>();
            if p <= l {
                return (k - 1) as f64;
            }
        }
    } else {
        // Normal approximation for large lambda
        let normal: f64 = rng.random::<f64>()
            + rng.random::<f64>()
            + rng.random::<f64>()
            + rng.random::<f64>()
            + rng.random::<f64>()
            + rng.random::<f64>()
            + rng.random::<f64>()
            + rng.random::<f64>()
            + rng.random::<f64>()
            + rng.random::<f64>()
            + rng.random::<f64>()
            + rng.random::<f64>()
            - 6.0;
        (lambda + lambda.sqrt() * normal).max(0.0)
    }
}

/// Generate a 2D moving dot image with configurable parameters.
pub fn moving_dot_image_with_config(
    width: usize,
    height: usize,
    mtrx: f64,
    mtry: f64,
    current: f64,
    exposure: f64,
    shutter_open: bool,
    rng: &mut StdRng,
    config: &MovingDotImageConfig,
) -> Vec<f64> {
    let n = height * width;
    let mut img = Vec::with_capacity(n);

    if !shutter_open {
        for _ in 0..n {
            img.push(poisson_sample(rng, config.background));
        }
        return img;
    }

    let sigmax = config.sigma_x;
    let sigmay = config.sigma_y;
    let intensity = config.n_per_i_per_s * current * exposure;

    let cx = width as f64 / 2.0 + mtrx;
    let cy = height as f64 / 2.0 + mtry;

    for row in 0..height {
        let dy = row as f64 - cy;
        let gy = (-dy * dy / (2.0 * sigmay * sigmay)).exp();
        for col in 0..width {
            let dx = col as f64 - cx;
            let gx = (-dx * dx / (2.0 * sigmax * sigmax)).exp();
            let signal = intensity * gx * gy;
            let bg = poisson_sample(rng, config.background);
            img.push(signal + bg);
        }
    }

    img
}

/// Generate a 2D moving dot image.
///
/// Returns a row-major `Vec<f64>` of size `height * width`.
/// The dot is a 2D Gaussian centered at (mtrx, mtry) with background noise.
pub fn moving_dot_image(
    width: usize,
    height: usize,
    mtrx: f64,
    mtry: f64,
    current: f64,
    exposure: f64,
    shutter_open: bool,
    rng: &mut StdRng,
) -> Vec<f64> {
    moving_dot_image_with_config(
        width,
        height,
        mtrx,
        mtry,
        current,
        exposure,
        shutter_open,
        rng,
        &MovingDotImageConfig::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn test_beam_current_range() {
        // Should oscillate between 475 and 525
        for i in 0..100 {
            let t = i as f64 * 0.1;
            let c = beam_current(t);
            assert!(c >= 474.0 && c <= 526.0, "current={c} at t={t}");
        }
    }

    #[test]
    fn test_beam_current_period() {
        // Period is 4s, so beam_current(0) ~= beam_current(4)
        let c0 = beam_current(0.0);
        let c4 = beam_current(4.0);
        assert!((c0 - c4).abs() < 1e-10);
    }

    #[test]
    fn test_pinhole_peak_at_center() {
        let val_center = point_reading(DetectorMode::PinHole, 0.0, 500.0, 1.0, 5.0, 0.0);
        let val_offset = point_reading(DetectorMode::PinHole, 20.0, 500.0, 1.0, 5.0, 0.0);
        assert!(
            val_center > val_offset * 10.0,
            "peak should be much larger at center"
        );
    }

    #[test]
    fn test_pinhole_gaussian_shape() {
        let sigma = 5.0;
        let v0 = point_reading(DetectorMode::PinHole, 0.0, 500.0, 1.0, sigma, 0.0);
        let v_sigma = point_reading(DetectorMode::PinHole, sigma, 500.0, 1.0, sigma, 0.0);
        let expected_ratio = (-0.5_f64).exp(); // e^(-1/2)
        let ratio = v_sigma / v0;
        assert!(
            (ratio - expected_ratio).abs() < 1e-10,
            "ratio={ratio}, expected={expected_ratio}"
        );
    }

    #[test]
    fn test_edge_monotonic() {
        // Edge should be monotonically increasing
        let mut prev = 0.0;
        for i in 0..20 {
            let mtr = i as f64;
            let val = point_reading(DetectorMode::Edge, mtr, 500.0, 1.0, 2.5, 5.0);
            assert!(
                val >= prev,
                "edge not monotonic at mtr={mtr}: {val} < {prev}"
            );
            prev = val;
        }
    }

    #[test]
    fn test_slit_symmetric_around_zero() {
        // The slit formula erfc(1/σ*(mtr-center)) - erfc(1/σ*(mtr+center))
        // creates a slit from -center to +center, symmetric around mtr=0
        let center = 7.5;
        let val_pos = point_reading(DetectorMode::Slit, 2.0, 500.0, 1.0, 2.5, center);
        let val_neg = point_reading(DetectorMode::Slit, -2.0, 500.0, 1.0, 2.5, center);
        assert!(
            (val_pos - val_neg).abs() / val_pos.max(1e-10) < 0.01,
            "pos={val_pos}, neg={val_neg}"
        );
    }

    #[test]
    fn test_slit_peak_near_zero() {
        let center = 7.5;
        let val_zero = point_reading(DetectorMode::Slit, 0.0, 500.0, 1.0, 2.5, center);
        let val_far = point_reading(DetectorMode::Slit, 20.0, 500.0, 1.0, 2.5, center);
        assert!(val_zero > val_far * 5.0, "zero={val_zero}, far={val_far}");
    }

    #[test]
    fn test_poisson_mean() {
        let mut rng = StdRng::seed_from_u64(42);
        let lambda = 100.0;
        let n = 10_000;
        let sum: f64 = (0..n).map(|_| poisson_sample(&mut rng, lambda)).sum();
        let mean = sum / n as f64;
        assert!(
            (mean - lambda).abs() < 5.0,
            "poisson mean={mean}, expected ~{lambda}"
        );
    }

    #[test]
    fn test_moving_dot_image_size() {
        let mut rng = StdRng::seed_from_u64(42);
        let img = moving_dot_image(640, 480, 0.0, 0.0, 500.0, 0.1, true, &mut rng);
        assert_eq!(img.len(), 640 * 480);
    }

    #[test]
    fn test_moving_dot_dark_frame() {
        let mut rng = StdRng::seed_from_u64(42);
        let img = moving_dot_image(64, 48, 0.0, 0.0, 500.0, 1.0, false, &mut rng);
        // Dark frame should have only background noise (~1000)
        let mean: f64 = img.iter().sum::<f64>() / img.len() as f64;
        assert!((mean - 1000.0).abs() < 100.0, "dark mean={mean}");
    }

    #[test]
    fn test_moving_dot_has_peak() {
        let mut rng = StdRng::seed_from_u64(42);
        let img = moving_dot_image(640, 480, 0.0, 0.0, 500.0, 0.1, true, &mut rng);
        // Center pixel should be brighter than corner
        let center = img[240 * 640 + 320];
        let corner = img[0];
        assert!(center > corner * 2.0, "center={center}, corner={corner}");
    }
}
