use rand::Rng;
use rand::rngs::StdRng;
use std::f64::consts::PI;

use ad_core_rs::ndarray::NDDataBuffer;

use crate::types::{SimMode, SineOperation};
use ad_core_rs::color_layout::ColorLayout;
use ad_core_rs::pixel_cast::{PixelCast, with_buffer, with_buffer_mut};

const MAX_PEAK_SIGMA: i32 = 4;

#[derive(Debug, Clone)]
pub struct Gains {
    pub gain: f64,
    pub gain_x: f64,
    pub gain_y: f64,
    pub gain_red: f64,
    pub gain_green: f64,
    pub gain_blue: f64,
}

#[derive(Debug, Clone)]
pub struct PeakParams {
    pub start_x: i32,
    pub start_y: i32,
    pub width_x: i32,
    pub width_y: i32,
    pub num_x: i32,
    pub num_y: i32,
    pub step_x: i32,
    pub step_y: i32,
    pub height_variation: f64,
}

#[derive(Debug, Clone)]
pub struct SineWave {
    pub amplitude: f64,
    pub frequency: f64,
    pub phase: f64,
}

#[derive(Debug, Clone)]
pub struct SineParams {
    pub x_sine1: SineWave,
    pub x_sine2: SineWave,
    pub y_sine1: SineWave,
    pub y_sine2: SineWave,
    pub x_op: SineOperation,
    pub y_op: SineOperation,
}

#[derive(Debug)]
pub struct SineState {
    pub x_sine1: Vec<f64>,
    pub x_sine2: Vec<f64>,
    pub y_sine1: Vec<f64>,
    pub y_sine2: Vec<f64>,
    pub x_counter: f64,
    pub y_counter: f64,
}

impl SineState {
    pub fn new() -> Self {
        Self {
            x_sine1: Vec::new(),
            x_sine2: Vec::new(),
            y_sine1: Vec::new(),
            y_sine2: Vec::new(),
            x_counter: 0.0,
            y_counter: 0.0,
        }
    }

    pub fn reset(&mut self, size_x: usize, size_y: usize) {
        self.x_sine1 = vec![0.0; size_x];
        self.x_sine2 = vec![0.0; size_x];
        self.y_sine1 = vec![0.0; size_y];
        self.y_sine2 = vec![0.0; size_y];
        self.x_counter = 0.0;
        self.y_counter = 0.0;
    }
}

/// Fill background buffer with offset + noise.
pub fn fill_background(buf: &mut NDDataBuffer, offset: f64, noise: f64, rng: &mut StdRng) {
    with_buffer_mut!(buf, |v| {
        if noise == 0.0 {
            for elem in v.iter_mut() {
                *elem = PixelCast::from_f64(offset);
            }
        } else {
            for elem in v.iter_mut() {
                let n: f64 = rng.random::<f64>() * noise;
                *elem = PixelCast::from_f64(n + offset);
            }
        }
    });
}

/// Apply background to raw buffer using circular copy from random start position.
pub fn apply_background(raw: &mut NDDataBuffer, background: &NDDataBuffer, rng: &mut StdRng) {
    // Read background as f64
    let bg_f64: Vec<f64> = with_buffer!(background, |v| {
        v.iter().map(|x| PixelCast::to_f64(*x)).collect()
    });
    let n = bg_f64.len();
    if n == 0 {
        return;
    }
    let start = (rng.random::<f64>() * n as f64) as usize % n;
    with_buffer_mut!(raw, |raw_v| {
        for (i, elem) in raw_v.iter_mut().enumerate() {
            let bg_idx = (start + i) % n;
            *elem = PixelCast::from_f64(bg_f64[bg_idx]);
        }
    });
}

/// Zero the raw buffer.
pub fn zero_buffer(buf: &mut NDDataBuffer) {
    with_buffer_mut!(buf, |v| {
        for elem in v.iter_mut() {
            *elem = PixelCast::from_f64(0.0);
        }
    });
}

/// Accumulate linear ramp into raw/ramp buffer.
/// If reset is true, recompute the base ramp pattern; otherwise, increment it.
/// After computation, if use_background is true, add ramp_buf values to raw_buf.
pub fn accumulate_linear_ramp(
    raw: &mut NDDataBuffer,
    ramp_buf: &mut NDDataBuffer,
    layout: &ColorLayout,
    gains: &Gains,
    reset: bool,
    use_background: bool,
) {
    let size_x = layout.size_x;
    let size_y = layout.size_y;
    let gain = gains.gain;
    let gain_x = gains.gain_x;
    let gain_y = gains.gain_y;

    let inc_mono = gain;
    let inc_red = gains.gain_red * gain;
    let inc_green = gains.gain_green * gain;
    let inc_blue = gains.gain_blue * gain;

    // Choose destination: if using background, write to ramp_buf, then add to raw.
    // Otherwise write directly to raw.
    // Helper: fill ramp pattern into a buffer
    fn fill_ramp<T: PixelCast>(
        data: &mut [T],
        layout: &ColorLayout,
        size_x: usize,
        size_y: usize,
        inc_mono: f64,
        inc_red: f64,
        inc_green: f64,
        inc_blue: f64,
        gain_x: f64,
        gain_y: f64,
        reset: bool,
    ) {
        if reset {
            for y in 0..size_y {
                match layout.color_mode {
                    ad_core_rs::driver::ColorMode::Mono => {
                        for x in 0..size_x {
                            data[layout.index(x, y, 0)] =
                                T::from_f64(inc_mono * (gain_x * x as f64 + gain_y * y as f64));
                        }
                    }
                    _ => {
                        for x in 0..size_x {
                            let base_val = gain_x * x as f64 + gain_y * y as f64;
                            data[layout.index(x, y, 0)] = T::from_f64(inc_red * base_val);
                            data[layout.index(x, y, 1)] = T::from_f64(inc_green * base_val);
                            data[layout.index(x, y, 2)] = T::from_f64(inc_blue * base_val);
                        }
                    }
                }
            }
        } else {
            match layout.color_mode {
                ad_core_rs::driver::ColorMode::Mono => {
                    for elem in data.iter_mut() {
                        *elem = T::from_f64(T::to_f64(*elem) + inc_mono);
                    }
                }
                _ => {
                    for y in 0..size_y {
                        for x in 0..size_x {
                            let ri = layout.index(x, y, 0);
                            let gi = layout.index(x, y, 1);
                            let bi = layout.index(x, y, 2);
                            data[ri] = T::from_f64(T::to_f64(data[ri]) + inc_red);
                            data[gi] = T::from_f64(T::to_f64(data[gi]) + inc_green);
                            data[bi] = T::from_f64(T::to_f64(data[bi]) + inc_blue);
                        }
                    }
                }
            }
        }
    }

    if use_background {
        with_buffer_mut!(ramp_buf, |data| {
            fill_ramp(
                data, layout, size_x, size_y, inc_mono, inc_red, inc_green, inc_blue, gain_x,
                gain_y, reset,
            );
        });
        // Add ramp to raw: read ramp as f64 vec, then add to raw
        let ramp_f64: Vec<f64> = with_buffer!(&*ramp_buf, |v| {
            v.iter().map(|x| PixelCast::to_f64(*x)).collect()
        });
        with_buffer_mut!(raw, |data| {
            for (elem, &rv) in data.iter_mut().zip(ramp_f64.iter()) {
                *elem = PixelCast::from_f64(PixelCast::to_f64(*elem) + rv);
            }
        });
    } else {
        with_buffer_mut!(raw, |data| {
            fill_ramp(
                data, layout, size_x, size_y, inc_mono, inc_red, inc_green, inc_blue, gain_x,
                gain_y, reset,
            );
        });
    }
}

/// Accumulate Gaussian peaks into raw buffer.
/// peak_buf is used as a cache for the 2D Gaussian template.
pub fn accumulate_peaks(
    raw: &mut NDDataBuffer,
    peak_buf: &mut NDDataBuffer,
    layout: &ColorLayout,
    peak: &PeakParams,
    gains: &Gains,
    reset: bool,
    rng: &mut StdRng,
) {
    let size_x = layout.size_x as i32;
    let size_y = layout.size_y as i32;

    let peak_full_width_x = ((2 * MAX_PEAK_SIGMA * peak.width_x + 1).min(size_x - 1)).max(0);
    let peak_full_width_y = ((2 * MAX_PEAK_SIGMA * peak.width_y + 1).min(size_y - 1)).max(0);

    if reset {
        // Compute 2D Gaussian template in peak_buf (stored in first rows of maxSize buffer)
        with_buffer_mut!(peak_buf, |data| {
            for i in 0..peak_full_width_y {
                for j in 0..peak_full_width_x {
                    let idx = (i * size_x + j) as usize;
                    let gauss_y =
                        (-((i - peak_full_width_y / 2) as f64 / peak.width_y as f64).powi(2) / 2.0)
                            .exp();
                    let gauss_x =
                        (-((j - peak_full_width_x / 2) as f64 / peak.width_x as f64).powi(2) / 2.0)
                            .exp();
                    if idx < data.len() {
                        data[idx] = PixelCast::from_f64(gains.gain * gauss_x * gauss_y);
                    }
                }
            }
        });
    }

    // Read peak template as f64 for stamping
    let peak_f64: Vec<f64> = with_buffer!(&*peak_buf, |v| {
        v.iter().map(|x| PixelCast::to_f64(*x)).collect()
    });

    // Stamp peaks into raw
    with_buffer_mut!(raw, |raw_v| {
        for i in 0..peak.num_y {
            for j in 0..peak.num_x {
                let gain_variation = if peak.height_variation != 0.0 {
                    1.0 + (peak.height_variation / 100.0) * (rng.random::<f64>() - 0.5)
                } else {
                    1.0
                };

                let offset_y = i * peak.step_y + peak.start_y;
                let offset_x = j * peak.step_x + peak.start_x;

                for k in 0..peak_full_width_y {
                    let y_out = offset_y + k - peak_full_width_y / 2;
                    if y_out < 0 || y_out >= size_y {
                        continue;
                    }
                    for l in 0..peak_full_width_x {
                        let x_out = offset_x + l - peak_full_width_x / 2;
                        if x_out < 0 || x_out >= size_x {
                            continue;
                        }
                        let peak_idx = (k * size_x + l) as usize;
                        let pv = peak_f64[peak_idx];

                        match layout.color_mode {
                            ad_core_rs::driver::ColorMode::Mono => {
                                let raw_idx = layout.index(x_out as usize, y_out as usize, 0);
                                raw_v[raw_idx] = PixelCast::from_f64(
                                    PixelCast::to_f64(raw_v[raw_idx]) + gain_variation * pv,
                                );
                            }
                            _ => {
                                let ri = layout.index(x_out as usize, y_out as usize, 0);
                                let gi = layout.index(x_out as usize, y_out as usize, 1);
                                let bi = layout.index(x_out as usize, y_out as usize, 2);
                                raw_v[ri] = PixelCast::from_f64(
                                    PixelCast::to_f64(raw_v[ri])
                                        + gains.gain_red * gain_variation * pv,
                                );
                                raw_v[gi] = PixelCast::from_f64(
                                    PixelCast::to_f64(raw_v[gi])
                                        + gains.gain_green * gain_variation * pv,
                                );
                                raw_v[bi] = PixelCast::from_f64(
                                    PixelCast::to_f64(raw_v[bi])
                                        + gains.gain_blue * gain_variation * pv,
                                );
                            }
                        }
                    }
                }
            }
        }
    });
}

/// Accumulate sine wave pattern into raw buffer.
pub fn accumulate_sine(
    raw: &mut NDDataBuffer,
    layout: &ColorLayout,
    state: &mut SineState,
    sine: &SineParams,
    gains: &Gains,
    reset: bool,
) {
    let size_x = layout.size_x;
    let size_y = layout.size_y;

    if reset {
        state.reset(size_x, size_y);
    }

    // Ensure buffers are the right size (may have been reset or first use)
    if state.x_sine1.len() != size_x {
        state.reset(size_x, size_y);
    }

    // Compute sine tables
    for i in 0..size_x {
        let x_time = state.x_counter * gains.gain_x / size_x as f64;
        state.x_counter += 1.0;
        state.x_sine1[i] = sine.x_sine1.amplitude
            * (((x_time * sine.x_sine1.frequency + sine.x_sine1.phase / 360.0) * 2.0 * PI).sin());
        state.x_sine2[i] = sine.x_sine2.amplitude
            * (((x_time * sine.x_sine2.frequency + sine.x_sine2.phase / 360.0) * 2.0 * PI).sin());
    }

    for i in 0..size_y {
        let y_time = state.y_counter * gains.gain_y / size_y as f64;
        state.y_counter += 1.0;
        state.y_sine1[i] = sine.y_sine1.amplitude
            * (((y_time * sine.y_sine1.frequency + sine.y_sine1.phase / 360.0) * 2.0 * PI).sin());
        state.y_sine2[i] = sine.y_sine2.amplitude
            * (((y_time * sine.y_sine2.frequency + sine.y_sine2.phase / 360.0) * 2.0 * PI).sin());
    }

    // Combine sine waves for Mono mode
    match layout.color_mode {
        ad_core_rs::driver::ColorMode::Mono => {
            // Combine x sines
            match sine.x_op {
                SineOperation::Add => {
                    for i in 0..size_x {
                        state.x_sine1[i] += state.x_sine2[i];
                    }
                }
                SineOperation::Multiply => {
                    for i in 0..size_x {
                        state.x_sine1[i] *= state.x_sine2[i];
                    }
                }
            }
            // Combine y sines
            match sine.y_op {
                SineOperation::Add => {
                    for i in 0..size_y {
                        state.y_sine1[i] += state.y_sine2[i];
                    }
                }
                SineOperation::Multiply => {
                    for i in 0..size_y {
                        state.y_sine1[i] *= state.y_sine2[i];
                    }
                }
            }
        }
        _ => {} // RGB: use individual sines separately
    }

    // Apply to image
    with_buffer_mut!(raw, |data| {
        for y in 0..size_y {
            match layout.color_mode {
                ad_core_rs::driver::ColorMode::Mono => {
                    for x in 0..size_x {
                        let idx = layout.index(x, y, 0);
                        let val = gains.gain * (state.y_sine1[y] + state.x_sine1[x]);
                        data[idx] = PixelCast::from_f64(PixelCast::to_f64(data[idx]) + val);
                    }
                }
                _ => {
                    for x in 0..size_x {
                        let ri = layout.index(x, y, 0);
                        let gi = layout.index(x, y, 1);
                        let bi = layout.index(x, y, 2);
                        data[ri] = PixelCast::from_f64(
                            PixelCast::to_f64(data[ri])
                                + gains.gain * gains.gain_red * state.x_sine1[x],
                        );
                        data[gi] = PixelCast::from_f64(
                            PixelCast::to_f64(data[gi])
                                + gains.gain * gains.gain_green * state.y_sine1[y],
                        );
                        data[bi] = PixelCast::from_f64(
                            PixelCast::to_f64(data[bi])
                                + gains.gain
                                    * gains.gain_blue
                                    * (state.x_sine2[x] + state.y_sine2[y])
                                    / 2.0,
                        );
                    }
                }
            }
        }
    });
}

/// Compute a single frame based on the current config and sim mode.
/// This is the main pipeline function called from the acquisition loop.
pub fn compute_frame(
    raw: &mut NDDataBuffer,
    background: &mut NDDataBuffer,
    ramp_buf: &mut NDDataBuffer,
    peak_buf: &mut NDDataBuffer,
    sine_state: &mut SineState,
    layout: &ColorLayout,
    sim_mode: SimMode,
    gains: &Gains,
    peak: &PeakParams,
    sine: &SineParams,
    offset: f64,
    noise: f64,
    use_background: bool,
    reset: bool,
    rng: &mut StdRng,
) {
    // Step 1: Rebuild background if needed
    if reset && (noise != 0.0 || offset != 0.0) {
        fill_background(background, offset, noise, rng);
    }

    // Step 2: Apply background or zero
    if use_background {
        apply_background(raw, background, rng);
    } else if sim_mode != SimMode::LinearRamp {
        zero_buffer(raw);
    }

    // Step 3: Apply sim mode
    match sim_mode {
        SimMode::LinearRamp => {
            accumulate_linear_ramp(raw, ramp_buf, layout, gains, reset, use_background);
        }
        SimMode::Peaks => {
            accumulate_peaks(raw, peak_buf, layout, peak, gains, reset, rng);
        }
        SimMode::Sine => {
            accumulate_sine(raw, layout, sine_state, sine, gains, reset);
        }
        SimMode::OffsetNoise => {
            // Background only, no additional signal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::driver::ColorMode;
    use ad_core_rs::ndarray::NDDataBuffer;
    use rand::SeedableRng;

    fn default_gains() -> Gains {
        Gains {
            gain: 1.0,
            gain_x: 1.0,
            gain_y: 1.0,
            gain_red: 1.0,
            gain_green: 1.0,
            gain_blue: 1.0,
        }
    }

    fn mono_layout(sx: usize, sy: usize) -> ColorLayout {
        ColorLayout {
            color_mode: ColorMode::Mono,
            size_x: sx,
            size_y: sy,
        }
    }

    #[test]
    fn test_fill_background_offset_only() {
        let mut buf = NDDataBuffer::F64(vec![0.0; 10]);
        let mut rng = StdRng::seed_from_u64(42);
        fill_background(&mut buf, 5.0, 0.0, &mut rng);
        if let NDDataBuffer::F64(v) = &buf {
            for &x in v {
                assert_eq!(x, 5.0);
            }
        }
    }

    #[test]
    fn test_fill_background_with_noise() {
        let mut buf = NDDataBuffer::F64(vec![0.0; 100]);
        let mut rng = StdRng::seed_from_u64(42);
        fill_background(&mut buf, 10.0, 5.0, &mut rng);
        if let NDDataBuffer::F64(v) = &buf {
            let min = v.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            assert!(min >= 10.0);
            assert!(max <= 15.0);
        }
    }

    #[test]
    fn test_linear_ramp_mono_u8_reset() {
        let layout = mono_layout(4, 3);
        let n = layout.num_elements();
        let mut raw = NDDataBuffer::U8(vec![0; n]);
        let mut ramp = NDDataBuffer::U8(vec![0; n]);
        let gains = default_gains();
        accumulate_linear_ramp(&mut raw, &mut ramp, &layout, &gains, true, false);
        if let NDDataBuffer::U8(v) = &raw {
            // pixel(x,y) = gain * (gainX*x + gainY*y) = 1*(1*x+1*y) = x+y
            assert_eq!(v[0], 0); // (0,0)
            assert_eq!(v[1], 1); // (1,0)
            assert_eq!(v[4], 1); // (0,1)
            assert_eq!(v[5], 2); // (1,1)
        }
    }

    #[test]
    fn test_linear_ramp_mono_f64_increment() {
        let layout = mono_layout(2, 2);
        let n = layout.num_elements();
        let mut raw = NDDataBuffer::F64(vec![0.0; n]);
        let mut ramp = NDDataBuffer::F64(vec![0.0; n]);
        let gains = Gains {
            gain: 3.0,
            gain_x: 1.0,
            gain_y: 1.0,
            ..default_gains()
        };
        // Reset: base pattern
        accumulate_linear_ramp(&mut raw, &mut ramp, &layout, &gains, true, false);
        // Frame 2: increment
        accumulate_linear_ramp(&mut raw, &mut ramp, &layout, &gains, false, false);
        if let NDDataBuffer::F64(v) = &raw {
            // (0,0): reset=0, +3 = 3
            assert_eq!(v[0], 3.0);
            // (1,0): reset=3, +3 = 6
            assert_eq!(v[1], 6.0);
        }
    }

    #[test]
    fn test_peaks_gaussian_symmetry() {
        let layout = mono_layout(64, 64);
        let n = layout.num_elements();
        let mut raw = NDDataBuffer::F64(vec![0.0; n]);
        let mut peak_buf = NDDataBuffer::F64(vec![0.0; n]);
        let mut rng = StdRng::seed_from_u64(42);
        let gains = default_gains();
        let peak = PeakParams {
            start_x: 32,
            start_y: 32,
            width_x: 5,
            width_y: 5,
            num_x: 1,
            num_y: 1,
            step_x: 0,
            step_y: 0,
            height_variation: 0.0,
        };
        accumulate_peaks(
            &mut raw,
            &mut peak_buf,
            &layout,
            &peak,
            &gains,
            true,
            &mut rng,
        );
        if let NDDataBuffer::F64(v) = &raw {
            // Center should have max value
            let center = layout.index(32, 32, 0);
            let left = layout.index(31, 32, 0);
            let right = layout.index(33, 32, 0);
            assert!(v[center] > 0.0);
            // Should be symmetric
            assert!((v[left] - v[right]).abs() < 1e-10);
        }
    }

    #[test]
    fn test_sine_mono_f64() {
        let layout = mono_layout(32, 32);
        let n = layout.num_elements();
        let mut raw = NDDataBuffer::F64(vec![0.0; n]);
        let mut state = SineState::new();
        let gains = default_gains();
        let sine = SineParams {
            x_sine1: SineWave {
                amplitude: 100.0,
                frequency: 1.0,
                phase: 0.0,
            },
            x_sine2: SineWave {
                amplitude: 0.0,
                frequency: 0.0,
                phase: 0.0,
            },
            y_sine1: SineWave {
                amplitude: 50.0,
                frequency: 1.0,
                phase: 0.0,
            },
            y_sine2: SineWave {
                amplitude: 0.0,
                frequency: 0.0,
                phase: 0.0,
            },
            x_op: SineOperation::Add,
            y_op: SineOperation::Add,
        };
        accumulate_sine(&mut raw, &layout, &mut state, &sine, &gains, true);
        if let NDDataBuffer::F64(v) = &raw {
            // Should have non-zero values
            let max = v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            assert!(max > 0.0);
        }
    }

    #[test]
    fn test_sine_phase_continuity() {
        let layout = mono_layout(16, 16);
        let n = layout.num_elements();
        let mut raw1 = NDDataBuffer::F64(vec![0.0; n]);
        let mut raw2 = NDDataBuffer::F64(vec![0.0; n]);
        let mut state = SineState::new();
        let gains = default_gains();
        let sine = SineParams {
            x_sine1: SineWave {
                amplitude: 100.0,
                frequency: 1.0,
                phase: 0.0,
            },
            x_sine2: SineWave {
                amplitude: 0.0,
                frequency: 0.0,
                phase: 0.0,
            },
            y_sine1: SineWave {
                amplitude: 0.0,
                frequency: 0.0,
                phase: 0.0,
            },
            y_sine2: SineWave {
                amplitude: 0.0,
                frequency: 0.0,
                phase: 0.0,
            },
            x_op: SineOperation::Add,
            y_op: SineOperation::Add,
        };
        // Frame 1
        accumulate_sine(&mut raw1, &layout, &mut state, &sine, &gains, true);
        // Frame 2: counter continues from where it left off
        let counter_after_1 = state.x_counter;
        accumulate_sine(&mut raw2, &layout, &mut state, &sine, &gains, false);
        let counter_after_2 = state.x_counter;
        // Counter should have advanced by size_x for each frame
        assert_eq!(counter_after_1, 16.0);
        assert_eq!(counter_after_2, 32.0);
        // Frames should be different (phase shifted)
        if let (NDDataBuffer::F64(v1), NDDataBuffer::F64(v2)) = (&raw1, &raw2) {
            assert_ne!(v1, v2);
        }
    }

    #[test]
    fn test_offset_noise_mode() {
        let layout = mono_layout(8, 8);
        let n = layout.num_elements();
        let mut raw = NDDataBuffer::F64(vec![0.0; n]);
        let mut bg = NDDataBuffer::F64(vec![0.0; n]);
        let mut ramp = NDDataBuffer::F64(vec![0.0; n]);
        let mut peak = NDDataBuffer::F64(vec![0.0; n]);
        let mut sine_state = SineState::new();
        let mut rng = StdRng::seed_from_u64(42);
        let gains = default_gains();
        let peak_p = PeakParams {
            start_x: 0,
            start_y: 0,
            width_x: 1,
            width_y: 1,
            num_x: 0,
            num_y: 0,
            step_x: 0,
            step_y: 0,
            height_variation: 0.0,
        };
        let sine_p = SineParams {
            x_sine1: SineWave {
                amplitude: 0.0,
                frequency: 0.0,
                phase: 0.0,
            },
            x_sine2: SineWave {
                amplitude: 0.0,
                frequency: 0.0,
                phase: 0.0,
            },
            y_sine1: SineWave {
                amplitude: 0.0,
                frequency: 0.0,
                phase: 0.0,
            },
            y_sine2: SineWave {
                amplitude: 0.0,
                frequency: 0.0,
                phase: 0.0,
            },
            x_op: SineOperation::Add,
            y_op: SineOperation::Add,
        };

        compute_frame(
            &mut raw,
            &mut bg,
            &mut ramp,
            &mut peak,
            &mut sine_state,
            &layout,
            SimMode::OffsetNoise,
            &gains,
            &peak_p,
            &sine_p,
            10.0,
            5.0,
            true,
            true,
            &mut rng,
        );

        if let NDDataBuffer::F64(v) = &raw {
            // All values should be between offset and offset+noise
            for &x in v.iter() {
                assert!(x >= 10.0 && x <= 15.0, "value {} out of range", x);
            }
        }
    }
}
