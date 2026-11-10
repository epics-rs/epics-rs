use rand::Rng;
use rand::rngs::StdRng;
use std::f64::consts::PI;

use ad_core_rs::ndarray::NDDataViewMut;

use crate::types::{SimMode, SineOperation};
use ad_core_rs::color_layout::ColorLayout;
use ad_core_rs::pixel_cast::PixelCast;

const MAX_PEAK_SIGMA: i32 = 4;

/// C's arithmetic on a simDetector `epicsType` element.
pub trait SimPixel: PixelCast + PartialEq {
    /// `(epicsType)v`: truncates toward zero; an integer out of range keeps
    /// the low bits of its i64. C leaves that conversion undefined, and gcc's
    /// x86-64 code for the narrow types differs past ±2^31.
    fn cast(v: f64) -> Self;
    /// `a += b`: integers wrap.
    fn add(self, rhs: Self) -> Self;
    /// `a * b` stored back to epicsType: integers wrap.
    fn mul(self, rhs: Self) -> Self;
}

macro_rules! sim_pixel_int {
    ($($t:ty),+) => {$(
        impl SimPixel for $t {
            #[inline]
            fn cast(v: f64) -> Self {
                v as i64 as $t
            }
            #[inline]
            fn add(self, rhs: Self) -> Self {
                self.wrapping_add(rhs)
            }
            #[inline]
            fn mul(self, rhs: Self) -> Self {
                self.wrapping_mul(rhs)
            }
        }
    )+};
}

sim_pixel_int!(i8, u8, i16, u16, i32, u32, i64);

impl SimPixel for u64 {
    #[inline]
    fn cast(v: f64) -> Self {
        if v < 9_223_372_036_854_775_808.0 {
            v as i64 as u64
        } else {
            v as u64
        }
    }
    #[inline]
    fn add(self, rhs: Self) -> Self {
        self.wrapping_add(rhs)
    }
    #[inline]
    fn mul(self, rhs: Self) -> Self {
        self.wrapping_mul(rhs)
    }
}

macro_rules! sim_pixel_float {
    ($($t:ty),+) => {$(
        impl SimPixel for $t {
            #[inline]
            fn cast(v: f64) -> Self {
                v as $t
            }
            #[inline]
            fn add(self, rhs: Self) -> Self {
                self + rhs
            }
            #[inline]
            fn mul(self, rhs: Self) -> Self {
                self * rhs
            }
        }
    )+};
}

sim_pixel_float!(f32, f64);

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
pub fn fill_background<T: SimPixel>(v: &mut [T], offset: f64, noise: f64, rng: &mut StdRng) {
    let offset = T::cast(offset);
    if noise == 0.0 {
        v.fill(offset);
    } else {
        for elem in v.iter_mut() {
            let n: f64 = rng.random::<f64>() * noise;
            *elem = T::cast(n + offset.to_f64());
        }
    }
}

/// Apply background to raw buffer using circular copy from random start
/// position: C's two memcpy runs (simDetector.cpp:79-85).
pub fn apply_background<T: Copy>(raw: &mut [T], background: &[T], rng: &mut StdRng) {
    let n = background.len();
    if n == 0 {
        return;
    }
    let start = (rng.random::<f64>() * n as f64) as usize % n;
    let (head, tail) = raw.split_at_mut(n - start);
    head.copy_from_slice(&background[start..]);
    tail.copy_from_slice(&background[..start]);
}

/// Zero the raw buffer.
pub fn zero_buffer<T: SimPixel>(v: &mut [T]) {
    v.fill(T::default());
}

/// Accumulate linear ramp into raw/ramp buffer.
/// If reset is true, recompute the base ramp pattern; otherwise, increment it.
/// After computation, if use_background is true, add ramp_buf values to raw_buf.
pub fn accumulate_linear_ramp<T: SimPixel>(
    raw: &mut [T],
    ramp_buf: &mut [T],
    layout: &ColorLayout,
    gains: &Gains,
    reset: bool,
    use_background: bool,
) {
    let size_x = layout.size_x;
    let size_y = layout.size_y;
    let gain_x = gains.gain_x;
    let gain_y = gains.gain_y;

    let inc_mono = T::cast(gains.gain);
    let inc_red = T::cast(gains.gain_red).mul(inc_mono);
    let inc_green = T::cast(gains.gain_green).mul(inc_mono);
    let inc_blue = T::cast(gains.gain_blue).mul(inc_mono);

    // Choose destination: if using background, write to ramp_buf, then add to raw.
    // Otherwise write directly to raw.
    // Helper: fill ramp pattern into a buffer
    fn fill_ramp<T: SimPixel>(
        data: &mut [T],
        layout: &ColorLayout,
        size_x: usize,
        size_y: usize,
        inc_mono: T,
        inc_red: T,
        inc_green: T,
        inc_blue: T,
        gain_x: f64,
        gain_y: f64,
        reset: bool,
    ) {
        if reset {
            for y in 0..size_y {
                match layout.color_mode {
                    ad_core_rs::driver::ColorMode::Mono => {
                        for x in 0..size_x {
                            data[layout.index(x, y, 0)] = T::cast(
                                inc_mono.to_f64() * (gain_x * x as f64 + gain_y * y as f64),
                            );
                        }
                    }
                    _ => {
                        for x in 0..size_x {
                            let base_val = gain_x * x as f64 + gain_y * y as f64;
                            data[layout.index(x, y, 0)] = T::cast(inc_red.to_f64() * base_val);
                            data[layout.index(x, y, 1)] = T::cast(inc_green.to_f64() * base_val);
                            data[layout.index(x, y, 2)] = T::cast(inc_blue.to_f64() * base_val);
                        }
                    }
                }
            }
        } else {
            match layout.color_mode {
                ad_core_rs::driver::ColorMode::Mono => {
                    for elem in data.iter_mut() {
                        *elem = elem.add(inc_mono);
                    }
                }
                _ => {
                    for y in 0..size_y {
                        for x in 0..size_x {
                            let ri = layout.index(x, y, 0);
                            let gi = layout.index(x, y, 1);
                            let bi = layout.index(x, y, 2);
                            data[ri] = data[ri].add(inc_red);
                            data[gi] = data[gi].add(inc_green);
                            data[bi] = data[bi].add(inc_blue);
                        }
                    }
                }
            }
        }
    }

    if use_background {
        fill_ramp(
            ramp_buf, layout, size_x, size_y, inc_mono, inc_red, inc_green, inc_blue, gain_x,
            gain_y, reset,
        );
        for (elem, rv) in raw.iter_mut().zip(ramp_buf.iter()) {
            *elem = elem.add(*rv);
        }
    } else {
        fill_ramp(
            raw, layout, size_x, size_y, inc_mono, inc_red, inc_green, inc_blue, gain_x, gain_y,
            reset,
        );
    }
}

/// Accumulate Gaussian peaks into raw buffer.
/// peak_buf is used as a cache for the 2D Gaussian template.
pub fn accumulate_peaks<T: SimPixel>(
    raw: &mut [T],
    peak_buf: &mut [T],
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
        for i in 0..peak_full_width_y {
            for j in 0..peak_full_width_x {
                let idx = (i * size_x + j) as usize;
                let gauss_y = (-((i - peak_full_width_y / 2) as f64 / peak.width_y as f64).powi(2)
                    / 2.0)
                    .exp();
                let gauss_x = (-((j - peak_full_width_x / 2) as f64 / peak.width_x as f64).powi(2)
                    / 2.0)
                    .exp();
                if idx < peak_buf.len() {
                    peak_buf[idx] = T::cast(gains.gain * gauss_x * gauss_y);
                }
            }
        }
    }

    // Stamp peaks into raw
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
                    let pv = peak_buf[peak_idx].to_f64();

                    match layout.color_mode {
                        ad_core_rs::driver::ColorMode::Mono => {
                            let raw_idx = layout.index(x_out as usize, y_out as usize, 0);
                            raw[raw_idx] = T::cast(raw[raw_idx].to_f64() + gain_variation * pv);
                        }
                        _ => {
                            let ri = layout.index(x_out as usize, y_out as usize, 0);
                            let gi = layout.index(x_out as usize, y_out as usize, 1);
                            let bi = layout.index(x_out as usize, y_out as usize, 2);
                            raw[ri] = raw[ri].add(T::cast(gains.gain_red * gain_variation * pv));
                            raw[gi] = raw[gi].add(T::cast(gains.gain_green * gain_variation * pv));
                            raw[bi] = raw[bi].add(T::cast(gains.gain_blue * gain_variation * pv));
                        }
                    }
                }
            }
        }
    }
}

/// Accumulate sine wave pattern into raw buffer.
pub fn accumulate_sine<T: SimPixel>(
    raw: &mut [T],
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
    for y in 0..size_y {
        match layout.color_mode {
            ad_core_rs::driver::ColorMode::Mono => {
                for x in 0..size_x {
                    let idx = layout.index(x, y, 0);
                    let val = gains.gain * (state.y_sine1[y] + state.x_sine1[x]);
                    raw[idx] = raw[idx].add(T::cast(val));
                }
            }
            _ => {
                for x in 0..size_x {
                    let ri = layout.index(x, y, 0);
                    let gi = layout.index(x, y, 1);
                    let bi = layout.index(x, y, 2);
                    raw[ri] = raw[ri].add(T::cast(gains.gain * gains.gain_red * state.x_sine1[x]));
                    raw[gi] =
                        raw[gi].add(T::cast(gains.gain * gains.gain_green * state.y_sine1[y]));
                    raw[bi] = raw[bi].add(T::cast(
                        gains.gain * gains.gain_blue * (state.x_sine2[x] + state.y_sine2[y]) / 2.0,
                    ));
                }
            }
        }
    }
}

/// Compute a single frame based on the current config and sim mode.
/// This is the main pipeline function called from the acquisition loop.
///
/// Like C's `computeArray<epicsType>` (simDetector.cpp:649-680) it picks the
/// element type once per frame; the four buffers are the one data type and
/// length the task allocates them with.
pub fn compute_frame(
    raw: NDDataViewMut<'_>,
    background: NDDataViewMut<'_>,
    ramp_buf: NDDataViewMut<'_>,
    peak_buf: NDDataViewMut<'_>,
    sine_state: &mut SineState,
    layout: &ColorLayout,
    sim_mode: SimMode,
    gains: &Gains,
    peak: &PeakParams,
    sine: &SineParams,
    offset: f64,
    noise: f64,
    use_background: &mut bool,
    reset: bool,
    rng: &mut StdRng,
) {
    macro_rules! typed {
        ($($v:ident),+) => {
            match (raw, background, ramp_buf, peak_buf) {
                $((
                    NDDataViewMut::$v(raw),
                    NDDataViewMut::$v(background),
                    NDDataViewMut::$v(ramp_buf),
                    NDDataViewMut::$v(peak_buf),
                ) => compute_typed(
                    raw, background, ramp_buf, peak_buf, sine_state, layout, sim_mode, gains,
                    peak, sine, offset, noise, use_background, reset, rng,
                ),)+
                _ => unreachable!("sim buffers of different data types"),
            }
        };
    }
    typed!(I8, U8, I16, U16, I32, U32, I64, U64, F32, F64)
}

fn compute_typed<T: SimPixel>(
    raw: &mut [T],
    background: &mut [T],
    ramp_buf: &mut [T],
    peak_buf: &mut [T],
    sine_state: &mut SineState,
    layout: &ColorLayout,
    sim_mode: SimMode,
    gains: &Gains,
    peak: &PeakParams,
    sine: &SineParams,
    offset: f64,
    noise: f64,
    use_background: &mut bool,
    reset: bool,
    rng: &mut StdRng,
) {
    // Step 1: Rebuild background if needed. C compares the offset after
    // casting it to epicsType, so an offset that truncates to 0 uses none.
    if reset {
        *use_background = noise != 0.0 || T::cast(offset) != T::default();
        if *use_background {
            fill_background(background, offset, noise, rng);
        }
    }
    let use_background = *use_background;

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
    use ad_core_rs::ndarray::{NDDataBuffer, NDDataView};
    use ad_core_rs::pixel_cast::with_buffer;
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
        let mut buf = vec![0.0; 10];
        let mut rng = StdRng::seed_from_u64(42);
        fill_background(&mut buf, 5.0, 0.0, &mut rng);
        for &x in &buf {
            assert_eq!(x, 5.0);
        }
    }

    #[test]
    fn test_fill_background_with_noise() {
        let mut buf = vec![0.0; 100];
        let mut rng = StdRng::seed_from_u64(42);
        fill_background(&mut buf, 10.0, 5.0, &mut rng);
        let min = buf.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = buf.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(min >= 10.0);
        assert!(max <= 15.0);
    }

    #[test]
    fn test_linear_ramp_mono_u8_reset() {
        let layout = mono_layout(4, 3);
        let n = layout.num_elements();
        let mut raw = vec![0u8; n];
        let mut ramp = vec![0u8; n];
        let gains = default_gains();
        accumulate_linear_ramp(&mut raw, &mut ramp, &layout, &gains, true, false);
        // pixel(x,y) = gain * (gainX*x + gainY*y) = 1*(1*x+1*y) = x+y
        assert_eq!(raw[0], 0); // (0,0)
        assert_eq!(raw[1], 1); // (1,0)
        assert_eq!(raw[4], 1); // (0,1)
        assert_eq!(raw[5], 2); // (1,1)
    }

    #[test]
    fn test_linear_ramp_mono_f64_increment() {
        let layout = mono_layout(2, 2);
        let n = layout.num_elements();
        let mut raw = vec![0.0; n];
        let mut ramp = vec![0.0; n];
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
        // (0,0): reset=0, +3 = 3
        assert_eq!(raw[0], 3.0);
        // (1,0): reset=3, +3 = 6
        assert_eq!(raw[1], 6.0);
    }

    /// C adds `(epicsType)gain` in epicsType: a UInt8 ramp wraps past 255
    /// and a fractional gain truncates.
    #[test]
    fn a_u8_ramp_wraps_and_truncates_as_c_does() {
        let layout = mono_layout(4, 1);
        let mut raw = [0u8; 4];
        let mut ramp = [0u8; 4];
        let gains = Gains {
            gain: 100.0,
            ..default_gains()
        };
        accumulate_linear_ramp(&mut raw, &mut ramp, &layout, &gains, true, false);
        assert_eq!(raw, [0, 100, 200, 44]);
        accumulate_linear_ramp(&mut raw, &mut ramp, &layout, &gains, false, false);
        assert_eq!(raw, [100, 200, 44, 144]);

        let gains = Gains {
            gain: 2.5,
            ..default_gains()
        };
        accumulate_linear_ramp(&mut raw, &mut ramp, &layout, &gains, true, false);
        assert_eq!(raw, [0, 2, 4, 6]);
        accumulate_linear_ramp(&mut raw, &mut ramp, &layout, &gains, false, false);
        assert_eq!(raw, [2, 4, 6, 8]);
    }

    /// C casts the offset to epicsType before testing it: a UInt8 offset of
    /// 0.5 leaves the background off, and 100.7 fills it with 100.
    #[test]
    fn the_background_uses_the_offset_cast_to_the_pixel_type() {
        let layout = mono_layout(4, 1);
        let still = || SineWave {
            amplitude: 0.0,
            frequency: 0.0,
            phase: 0.0,
        };
        let sine = SineParams {
            x_sine1: still(),
            x_sine2: still(),
            y_sine1: still(),
            y_sine2: still(),
            x_op: SineOperation::Add,
            y_op: SineOperation::Add,
        };
        let peak = PeakParams {
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
        let frame = |offset: f64, use_background: &mut bool| {
            let mut raw = [7u8; 4];
            let [bg, ramp, peak_buf] = &mut [[0u8; 4]; 3];
            compute_typed(
                &mut raw,
                bg,
                ramp,
                peak_buf,
                &mut SineState::new(),
                &layout,
                SimMode::OffsetNoise,
                &default_gains(),
                &peak,
                &sine,
                offset,
                0.0,
                use_background,
                true,
                &mut StdRng::seed_from_u64(1),
            );
            raw
        };
        let mut use_background = true;
        assert_eq!(frame(0.5, &mut use_background), [0; 4]);
        assert!(!use_background);
        assert_eq!(frame(100.7, &mut use_background), [100; 4]);
        assert!(use_background);
    }

    #[test]
    fn test_peaks_gaussian_symmetry() {
        let layout = mono_layout(64, 64);
        let n = layout.num_elements();
        let mut raw = vec![0.0f64; n];
        let mut peak_buf = vec![0.0; n];
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
        // Center should have max value
        let center = layout.index(32, 32, 0);
        let left = layout.index(31, 32, 0);
        let right = layout.index(33, 32, 0);
        assert!(raw[center] > 0.0);
        // Should be symmetric
        assert!((raw[left] - raw[right]).abs() < 1e-10);
    }

    #[test]
    fn test_sine_mono_f64() {
        let layout = mono_layout(32, 32);
        let n = layout.num_elements();
        let mut raw = vec![0.0; n];
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
        // Should have non-zero values
        let max = raw.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(max > 0.0);
    }

    #[test]
    fn test_sine_phase_continuity() {
        let layout = mono_layout(16, 16);
        let n = layout.num_elements();
        let mut raw1 = vec![0.0; n];
        let mut raw2 = vec![0.0; n];
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
        assert_ne!(raw1, raw2);
    }

    #[test]
    fn test_offset_noise_mode() {
        let layout = mono_layout(8, 8);
        let n = layout.num_elements();
        let mut raw = NDDataBuffer::from_vec::<f64>(vec![0.0; n]);
        let mut bg = NDDataBuffer::from_vec::<f64>(vec![0.0; n]);
        let mut ramp = NDDataBuffer::from_vec::<f64>(vec![0.0; n]);
        let mut peak = NDDataBuffer::from_vec::<f64>(vec![0.0; n]);
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
            raw.view_mut(),
            bg.view_mut(),
            ramp.view_mut(),
            peak.view_mut(),
            &mut sine_state,
            &layout,
            SimMode::OffsetNoise,
            &gains,
            &peak_p,
            &sine_p,
            10.0,
            5.0,
            &mut false,
            true,
            &mut rng,
        );

        if let NDDataView::F64(v) = raw.view() {
            // All values should be between offset and offset+noise
            for &x in v.iter() {
                assert!(x >= 10.0 && x <= 15.0, "value {} out of range", x);
            }
        }
    }

    /// FNV-1a over the little-endian bytes of every frame of each data type,
    /// mode, colour layout and background setting, from a fixed seed, with
    /// float elements rounded to 1e-3; the constants were captured when the
    /// integer frames moved to C's epicsType arithmetic.
    #[test]
    fn frame_output_is_pinned_for_a_fixed_seed() {
        use ad_core_rs::ndarray::NDDataType;

        let gains = Gains {
            gain: 2.5,
            gain_x: 1.5,
            gain_y: 0.75,
            gain_red: 1.0,
            gain_green: 0.5,
            gain_blue: 0.25,
        };
        let peak = PeakParams {
            start_x: 6,
            start_y: 5,
            width_x: 2,
            width_y: 1,
            num_x: 3,
            num_y: 2,
            step_x: 11,
            step_y: 9,
            height_variation: 20.0,
        };
        let wave = |amplitude, frequency, phase| SineWave {
            amplitude,
            frequency,
            phase,
        };
        let sine = SineParams {
            x_sine1: wave(40.0, 2.0, 10.0),
            x_sine2: wave(15.0, 5.0, 90.0),
            y_sine1: wave(30.0, 1.0, 45.0),
            y_sine2: wave(10.0, 3.0, 0.0),
            x_op: SineOperation::Add,
            y_op: SineOperation::Multiply,
        };
        let mut got = Vec::new();
        for data_type in [
            NDDataType::Int8,
            NDDataType::UInt8,
            NDDataType::Int16,
            NDDataType::UInt16,
            NDDataType::Int32,
            NDDataType::UInt32,
            NDDataType::Int64,
            NDDataType::UInt64,
            NDDataType::Float32,
            NDDataType::Float64,
        ] {
            for sim_mode in [
                SimMode::LinearRamp,
                SimMode::Peaks,
                SimMode::Sine,
                SimMode::OffsetNoise,
            ] {
                let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
                for color_mode in [
                    ColorMode::Mono,
                    ColorMode::RGB1,
                    ColorMode::RGB2,
                    ColorMode::RGB3,
                ] {
                    for (offset, noise) in [(0.0, 0.0), (100.0, 0.0), (100.0, 50.0)] {
                        let layout = ColorLayout {
                            color_mode,
                            size_x: 37,
                            size_y: 23,
                        };
                        let n = layout.num_elements();
                        let mut raw = NDDataBuffer::zeros(data_type, n);
                        let mut bg = NDDataBuffer::zeros(data_type, n);
                        let mut ramp = NDDataBuffer::zeros(data_type, n);
                        let mut peak_buf = NDDataBuffer::zeros(data_type, n);
                        let mut sine_state = SineState::new();
                        let mut rng = StdRng::seed_from_u64(7);
                        let mut use_background = false;
                        for frame in 0..3 {
                            compute_frame(
                                raw.view_mut(),
                                bg.view_mut(),
                                ramp.view_mut(),
                                peak_buf.view_mut(),
                                &mut sine_state,
                                &layout,
                                sim_mode,
                                &gains,
                                &peak,
                                &sine,
                                offset,
                                noise,
                                &mut use_background,
                                frame == 0,
                                &mut rng,
                            );
                            let mut feed = |bytes: &[u8]| {
                                for &b in bytes {
                                    hash = (hash ^ b as u64).wrapping_mul(0x100_0000_01b3);
                                }
                            };
                            // Rounded to 1e-3, far above the ULPs by which
                            // libm sin differs between MSVC and glibc.
                            let rounded = |x: f64| ((x * 1e3).round() as i64).to_le_bytes();
                            match raw.view() {
                                NDDataView::F32(v) => {
                                    v.iter().for_each(|&x| feed(&rounded(x as f64)))
                                }
                                NDDataView::F64(v) => v.iter().for_each(|&x| feed(&rounded(x))),
                                _ => with_buffer!(raw, |v| {
                                    v.iter().for_each(|x| feed(&x.to_le_bytes()))
                                }),
                            }
                        }
                    }
                }
                got.push((data_type, sim_mode, hash));
            }
        }
        let want: [u64; 40] = [
            0x130bbcf89df32604,
            0x27529964c644de7f,
            0xb1b1fe7c82f8f665,
            0x7a6fb0931738ec8c,
            0x130bbcf89df32604,
            0xc7a4b9a72b1246ca,
            0xb1b1fe7c82f8f665,
            0x7a6fb0931738ec8c,
            0x6cdbc52b53c39f14,
            0xbdc2657b4b6f1518,
            0xded64194baedd7c1,
            0x12799a38a6501d74,
            0x6cdbc52b53c39f14,
            0xbdc2657b4b6f1518,
            0xded64194baedd7c1,
            0x12799a38a6501d74,
            0xdf55fea4ae3124bc,
            0x1146938fa107eca0,
            0xb6c3cd16f0270949,
            0xccdf05dd0a878664,
            0xdf55fea4ae3124bc,
            0x1146938fa107eca0,
            0xb6c3cd16f0270949,
            0xccdf05dd0a878664,
            0x0d2f0054ff6a38ac,
            0x9f2d3ff5911abb90,
            0x0e52c36864324051,
            0xb1be25cd8dd5c304,
            0x0d2f0054ff6a38ac,
            0x9f2d3ff5911abb90,
            0x0e52c36864324051,
            0xb1be25cd8dd5c304,
            0xca35e2f113a75a5f,
            0x5bf5ef9a8060787f,
            0x6819981b823ff7c9,
            0xac16ce73fb1655d0,
            0x2c32ffc0d2f1c153,
            0xeea53f08450e5a1c,
            0xeb6b3c1bc822339f,
            0x5c2fea1952e0f9bd,
        ];
        assert_eq!(got.len(), want.len());
        let changed: Vec<_> = got
            .iter()
            .zip(want)
            .filter(|((_, _, hash), want)| hash != want)
            .map(|(&(data_type, sim_mode, _), _)| (data_type, sim_mode))
            .collect();
        assert!(changed.is_empty(), "{changed:?}");
    }
}
