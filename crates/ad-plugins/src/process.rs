use std::sync::Arc;

#[cfg(feature = "parallel")]
use rayon::prelude::*;
#[cfg(feature = "parallel")]
use crate::par_util;

use ad_core::ndarray::{NDArray, NDDataBuffer, NDDataType};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::{NDPluginProcess, ProcessResult};

/// 4-tap recursive filter configuration.
///
/// Filter step:
///   F_n = FC1*I_n + FC2*F_{n-1} + FC3*(F_{n-2} - FOffset) + FC4*(F_{n-3} - FOffset)
///   F_n = FOffset + FScale * F_n
///
/// Output step:
///   O_n = OC1*F_n + OC2*F_{n-1} + OC3*(O_{n-1} - OOffset) + OC4*(O_{n-2} - OOffset)
///   O_n = OOffset + OScale * O_n
///
/// Reset (first frame or auto-reset):
///   F_0 = RC1 * I + RC2 * F_prev  (F_prev = 0 initially)
#[derive(Debug, Clone)]
pub struct FilterConfig {
    /// Number of frames to average before auto-reset (if enabled).
    pub num_filter: usize,
    /// Automatically reset the filter when num_filtered reaches num_filter.
    pub auto_reset: bool,
    /// Output every N frames (0 = every frame).
    pub filter_callbacks: usize,
    /// Output coefficients [OC1, OC2, OC3, OC4].
    pub oc: [f64; 4],
    /// Filter coefficients [FC1, FC2, FC3, FC4].
    pub fc: [f64; 4],
    /// Reset coefficients [RC1, RC2].
    pub rc: [f64; 2],
    /// Output offset.
    pub o_offset: f64,
    /// Output scale.
    pub o_scale: f64,
    /// Filter offset.
    pub f_offset: f64,
    /// Filter scale.
    pub f_scale: f64,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            num_filter: 1,
            auto_reset: false,
            filter_callbacks: 0,
            oc: [1.0, 0.0, 0.0, 0.0], // simple passthrough
            fc: [1.0, 0.0, 0.0, 0.0],
            rc: [1.0, 0.0],
            o_offset: 0.0,
            o_scale: 1.0,
            f_offset: 0.0,
            f_scale: 1.0,
        }
    }
}

/// Process plugin operations applied sequentially to an NDArray.
#[derive(Debug, Clone)]
pub struct ProcessConfig {
    pub enable_background: bool,
    pub enable_flat_field: bool,
    pub enable_offset_scale: bool,
    pub offset: f64,
    pub scale: f64,
    pub enable_low_clip: bool,
    pub low_clip: f64,
    pub enable_high_clip: bool,
    pub high_clip: f64,
    pub enable_filter: bool,
    pub filter: FilterConfig,
    pub output_type: Option<NDDataType>,
    /// One-shot flag: save current input as background on next process().
    pub save_background: bool,
    /// One-shot flag: save current input as flat field on next process().
    pub save_flat_field: bool,
    /// Read-only status: whether a valid background is loaded.
    pub valid_background: bool,
    /// Read-only status: whether a valid flat field is loaded.
    pub valid_flat_field: bool,
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            enable_background: false,
            enable_flat_field: false,
            enable_offset_scale: false,
            offset: 0.0,
            scale: 1.0,
            enable_low_clip: false,
            low_clip: 0.0,
            enable_high_clip: false,
            high_clip: 0.0,
            enable_filter: false,
            filter: FilterConfig::default(),
            output_type: None,
            save_background: false,
            save_flat_field: false,
            valid_background: false,
            valid_flat_field: false,
        }
    }
}

/// State for the process plugin (holds background, flat field, and filter history).
pub struct ProcessState {
    pub config: ProcessConfig,
    pub background: Option<Vec<f64>>,
    pub flat_field: Option<Vec<f64>>,
    /// F_{n-1}: most recent filter state.
    pub filter_state: Option<Vec<f64>>,
    /// [F_{n-2}, F_{n-3}]: older filter history.
    pub filter_state_prev: Option<Vec<Vec<f64>>>,
    /// O_{n-1}: most recent output state.
    pub output_state: Option<Vec<f64>>,
    /// O_{n-2}: previous output state.
    pub output_state_prev: Option<Vec<f64>>,
    /// Number of frames filtered since last reset.
    pub num_filtered: usize,
}

impl ProcessState {
    pub fn new(config: ProcessConfig) -> Self {
        Self {
            config,
            background: None,
            flat_field: None,
            filter_state: None,
            filter_state_prev: None,
            output_state: None,
            output_state_prev: None,
            num_filtered: 0,
        }
    }

    /// Save the current array as background.
    pub fn save_background(&mut self, array: &NDArray) {
        let n = array.data.len();
        let mut bg = vec![0.0f64; n];
        for i in 0..n {
            bg[i] = array.data.get_as_f64(i).unwrap_or(0.0);
        }
        self.background = Some(bg);
        self.config.valid_background = true;
    }

    /// Save the current array as flat field.
    pub fn save_flat_field(&mut self, array: &NDArray) {
        let n = array.data.len();
        let mut ff = vec![0.0f64; n];
        for i in 0..n {
            ff[i] = array.data.get_as_f64(i).unwrap_or(0.0);
        }
        self.flat_field = Some(ff);
        self.config.valid_flat_field = true;
    }

    /// Reset the filter state, clearing all history buffers.
    pub fn reset_filter(&mut self) {
        self.filter_state = None;
        self.filter_state_prev = None;
        self.output_state = None;
        self.output_state_prev = None;
        self.num_filtered = 0;
    }

    /// Process an array through the configured pipeline.
    pub fn process(&mut self, src: &NDArray) -> NDArray {
        let n = src.data.len();
        let mut values = vec![0.0f64; n];
        for i in 0..n {
            values[i] = src.data.get_as_f64(i).unwrap_or(0.0);
        }

        // 0. Save background/flat field (one-shot flags)
        if self.config.save_background {
            self.save_background(src);
            self.config.save_background = false;
        }
        if self.config.save_flat_field {
            self.save_flat_field(src);
            self.config.save_flat_field = false;
        }

        // Stages 1-4: element-wise operations (background, flat field, offset+scale, clipping)
        // These can be combined into a single pass and parallelized.
        let needs_element_ops = self.config.enable_background
            || self.config.enable_flat_field
            || self.config.enable_offset_scale
            || self.config.enable_low_clip
            || self.config.enable_high_clip;

        if needs_element_ops {
            let bg = if self.config.enable_background { self.background.as_ref() } else { None };
            let (ff, ff_mean) = if self.config.enable_flat_field {
                if let Some(ref ff) = self.flat_field {
                    let mean = ff.iter().sum::<f64>() / ff.len().max(1) as f64;
                    (Some(ff.as_slice()), mean)
                } else {
                    (None, 0.0)
                }
            } else {
                (None, 0.0)
            };
            let do_offset_scale = self.config.enable_offset_scale;
            let scale = self.config.scale;
            let offset = self.config.offset;
            let do_low_clip = self.config.enable_low_clip;
            let low_clip = self.config.low_clip;
            let do_high_clip = self.config.enable_high_clip;
            let high_clip = self.config.high_clip;

            let apply_stages = |i: usize, v: &mut f64| {
                // Stage 1: Background subtraction
                if let Some(bg) = bg {
                    if i < bg.len() {
                        *v -= bg[i];
                    }
                }
                // Stage 2: Flat field normalization
                if let Some(ff) = ff {
                    if i < ff.len() && ff[i] != 0.0 {
                        *v = *v * ff_mean / ff[i];
                    }
                }
                // Stage 3: Offset + scale
                if do_offset_scale {
                    *v = *v * scale + offset;
                }
                // Stage 4: Clipping
                if do_low_clip && *v < low_clip {
                    *v = low_clip;
                }
                if do_high_clip && *v > high_clip {
                    *v = high_clip;
                }
            };

            #[cfg(feature = "parallel")]
            let use_parallel = par_util::should_parallelize(n);
            #[cfg(not(feature = "parallel"))]
            let use_parallel = false;

            if use_parallel {
                #[cfg(feature = "parallel")]
                par_util::thread_pool().install(|| {
                    values.par_iter_mut().enumerate().for_each(|(i, v)| {
                        apply_stages(i, v);
                    });
                });
            } else {
                for (i, v) in values.iter_mut().enumerate() {
                    apply_stages(i, v);
                }
            }
        }

        // 5. 4-tap recursive filter
        if self.config.enable_filter {
            let fc = &self.config.filter;
            let is_first_frame = self.filter_state.is_none();

            if is_first_frame {
                // Reset mode: F_0 = RC1 * I + RC2 * F_prev (F_prev = 0 initially)
                let rc1 = fc.rc[0];
                let rc2 = fc.rc[1];

                let mut f_new = vec![0.0f64; n];
                // On very first frame, F_prev is all zeros
                let f_prev = self.filter_state.as_ref();
                for i in 0..n {
                    let fp = f_prev.map_or(0.0, |p| p[i]);
                    f_new[i] = rc1 * values[i] + rc2 * fp;
                }

                // Output on reset: O = OC1*F_new (no history for OC2/OC3/OC4 terms)
                let mut o_new = vec![0.0f64; n];
                for i in 0..n {
                    o_new[i] = fc.oc[0] * f_new[i];
                    o_new[i] = fc.o_offset + fc.o_scale * o_new[i];
                }

                // Store history
                self.filter_state_prev = Some(vec![vec![0.0; n], vec![0.0; n]]);
                self.output_state_prev = Some(vec![0.0; n]);
                self.output_state = Some(o_new.clone());
                self.filter_state = Some(f_new);
                self.num_filtered = 1;

                values = o_new;
            } else {
                // Normal 4-tap recursive filter
                let f_prev = self.filter_state.as_ref().unwrap(); // F_{n-1}
                let f_prev_history = self.filter_state_prev.as_ref().unwrap();
                let f_prev2 = &f_prev_history[0]; // F_{n-2}
                let f_prev3 = &f_prev_history[1]; // F_{n-3}

                let o_prev = self.output_state.as_ref().unwrap(); // O_{n-1}
                let o_prev2 = self.output_state_prev.as_ref().unwrap(); // O_{n-2}

                let f_offset = fc.f_offset;
                let f_scale = fc.f_scale;
                let o_offset = fc.o_offset;
                let o_scale = fc.o_scale;
                let fc_coeffs = fc.fc;
                let oc_coeffs = fc.oc;

                let mut f_new = vec![0.0f64; n];
                let mut o_new = vec![0.0f64; n];

                for i in 0..n {
                    // Filter: F_n = FC1*I + FC2*F_{n-1} + FC3*(F_{n-2}-FOffset) + FC4*(F_{n-3}-FOffset)
                    f_new[i] = fc_coeffs[0] * values[i]
                        + fc_coeffs[1] * f_prev[i]
                        + fc_coeffs[2] * (f_prev2[i] - f_offset)
                        + fc_coeffs[3] * (f_prev3[i] - f_offset);
                    // Apply filter scale
                    f_new[i] = f_offset + f_scale * f_new[i];

                    // Output: O_n = OC1*F_n + OC2*F_{n-1} + OC3*(O_{n-1}-OOffset) + OC4*(O_{n-2}-OOffset)
                    o_new[i] = oc_coeffs[0] * f_new[i]
                        + oc_coeffs[1] * f_prev[i]
                        + oc_coeffs[2] * (o_prev[i] - o_offset)
                        + oc_coeffs[3] * (o_prev2[i] - o_offset);
                    // Apply output scale
                    o_new[i] = o_offset + o_scale * o_new[i];
                }

                // Shift filter history: [n-2, n-3] <- [n-1, old n-2]
                let old_f_prev = f_prev.clone();
                self.filter_state_prev = Some(vec![old_f_prev, f_prev2.clone()]);
                // Shift output history: O_{n-2} <- O_{n-1}
                self.output_state_prev = Some(o_prev.clone());
                self.output_state = Some(o_new.clone());
                self.filter_state = Some(f_new);

                self.num_filtered += 1;

                // Auto-reset: reset filter state when num_filtered reaches num_filter
                if fc.auto_reset && fc.num_filter > 0 && self.num_filtered >= fc.num_filter {
                    self.reset_filter();
                }

                values = o_new;
            }
        }

        // Build output
        let out_type = self.config.output_type.unwrap_or(src.data.data_type());
        let mut out_data = NDDataBuffer::zeros(out_type, n);
        for i in 0..n {
            out_data.set_from_f64(i, values[i]);
        }

        let mut arr = NDArray::new(src.dims.clone(), out_type);
        arr.data = out_data;
        arr.unique_id = src.unique_id;
        arr.timestamp = src.timestamp;
        arr.attributes = src.attributes.clone();
        arr
    }
}

// --- ProcessProcessor (NDPluginProcess-based) ---

/// ProcessProcessor wraps existing ProcessState.
pub struct ProcessProcessor {
    state: ProcessState,
}

impl ProcessProcessor {
    pub fn new(config: ProcessConfig) -> Self {
        Self {
            state: ProcessState::new(config),
        }
    }

    pub fn state(&self) -> &ProcessState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut ProcessState {
        &mut self.state
    }
}

impl NDPluginProcess for ProcessProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let out = self.state.process(array);
        ProcessResult::arrays(vec![Arc::new(out)])
    }

    fn plugin_type(&self) -> &str {
        "NDPluginProcess"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core::ndarray::{NDDimension, NDDataBuffer};

    fn make_array(vals: &[u8]) -> NDArray {
        let mut arr = NDArray::new(
            vec![NDDimension::new(vals.len())],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v.copy_from_slice(vals);
        }
        arr
    }

    fn make_f64_array(vals: &[f64]) -> NDArray {
        let mut arr = NDArray::new(
            vec![NDDimension::new(vals.len())],
            NDDataType::Float64,
        );
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v.copy_from_slice(vals);
        }
        arr
    }

    #[test]
    fn test_background_subtraction() {
        let bg_arr = make_array(&[10, 20, 30]);
        let input = make_array(&[15, 25, 35]);

        let mut state = ProcessState::new(ProcessConfig {
            enable_background: true,
            ..Default::default()
        });
        state.save_background(&bg_arr);

        let result = state.process(&input);
        if let NDDataBuffer::U8(ref v) = result.data {
            assert_eq!(v[0], 5);
            assert_eq!(v[1], 5);
            assert_eq!(v[2], 5);
        }
    }

    #[test]
    fn test_flat_field() {
        let ff_arr = make_array(&[100, 200, 50]);
        let input = make_array(&[100, 200, 50]);

        let mut state = ProcessState::new(ProcessConfig {
            enable_flat_field: true,
            ..Default::default()
        });
        state.save_flat_field(&ff_arr);

        let result = state.process(&input);
        // After flat field: all values should be normalized to the mean
        if let NDDataBuffer::U8(ref v) = result.data {
            // ff_mean ~= 116.67, so all values should be ~= 116
            assert!((v[0] as f64 - 116.67).abs() < 1.0);
            assert!((v[1] as f64 - 116.67).abs() < 1.0);
            assert!((v[2] as f64 - 116.67).abs() < 1.0);
        }
    }

    #[test]
    fn test_offset_scale() {
        let input = make_array(&[10, 20, 30]);
        let mut state = ProcessState::new(ProcessConfig {
            enable_offset_scale: true,
            scale: 2.0,
            offset: 5.0,
            ..Default::default()
        });

        let result = state.process(&input);
        if let NDDataBuffer::U8(ref v) = result.data {
            assert_eq!(v[0], 25);  // 10*2+5
            assert_eq!(v[1], 45);  // 20*2+5
            assert_eq!(v[2], 65);  // 30*2+5
        }
    }

    #[test]
    fn test_clipping() {
        let input = make_array(&[5, 50, 200]);
        let mut state = ProcessState::new(ProcessConfig {
            enable_low_clip: true,
            low_clip: 10.0,
            enable_high_clip: true,
            high_clip: 100.0,
            ..Default::default()
        });

        let result = state.process(&input);
        if let NDDataBuffer::U8(ref v) = result.data {
            assert_eq!(v[0], 10);   // clipped up
            assert_eq!(v[1], 50);   // unchanged
            assert_eq!(v[2], 100);  // clipped down
        }
    }

    #[test]
    fn test_recursive_filter() {
        // Reproduce old IIR behavior: filter_coeff=0.5 maps to
        // fc: [0.5, 0.5, 0.0, 0.0], oc: [1.0, 0.0, 0.0, 0.0]
        let input1 = make_array(&[100, 100, 100]);
        let input2 = make_array(&[0, 0, 0]);

        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                fc: [0.5, 0.5, 0.0, 0.0],
                oc: [1.0, 0.0, 0.0, 0.0],
                ..Default::default()
            },
            ..Default::default()
        });

        let _ = state.process(&input1); // first frame: reset => F_0 = RC1*I = 1.0*100 = 100
        let result = state.process(&input2); // F_1 = 0.5*0 + 0.5*100 = 50, O = 1.0*50 = 50
        if let NDDataBuffer::U8(ref v) = result.data {
            assert_eq!(v[0], 50);
            assert_eq!(v[1], 50);
        }
    }

    #[test]
    fn test_output_type_conversion() {
        let input = make_array(&[10, 20, 30]);
        let mut state = ProcessState::new(ProcessConfig {
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        let result = state.process(&input);
        assert_eq!(result.data.data_type(), NDDataType::Float64);
    }

    // --- ProcessProcessor tests ---

    #[test]
    fn test_process_processor() {
        let mut proc = ProcessProcessor::new(ProcessConfig {
            enable_offset_scale: true,
            scale: 2.0,
            offset: 1.0,
            ..Default::default()
        });
        let pool = NDArrayPool::new(1_000_000);

        let input = make_array(&[10, 20, 30]);
        let result = proc.process_array(&input, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        if let NDDataBuffer::U8(ref v) = result.output_arrays[0].data {
            assert_eq!(v[0], 21); // 10*2+1
        }
    }

    // --- New Phase 2-1 tests ---

    #[test]
    fn test_4tap_filter_averaging() {
        // Set up a running-average filter: FC=[1/N, (N-1)/N, 0, 0], OC=[1,0,0,0]
        // This is a simple exponential moving average with N=4.
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                fc: [0.25, 0.75, 0.0, 0.0],
                oc: [1.0, 0.0, 0.0, 0.0],
                ..Default::default()
            },
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        // Frame 1: constant 100 => reset: F_0 = 1.0*100 = 100
        let r1 = state.process(&make_f64_array(&[100.0]));
        let v1 = r1.data.get_as_f64(0).unwrap();
        assert!((v1 - 100.0).abs() < 1e-9, "frame 1: got {v1}");

        // Frame 2: constant 100 => F_1 = 0.25*100 + 0.75*100 = 100
        let r2 = state.process(&make_f64_array(&[100.0]));
        let v2 = r2.data.get_as_f64(0).unwrap();
        assert!((v2 - 100.0).abs() < 1e-9, "frame 2: got {v2}");

        // Frame 3: input 0 => F_2 = 0.25*0 + 0.75*100 = 75
        let r3 = state.process(&make_f64_array(&[0.0]));
        let v3 = r3.data.get_as_f64(0).unwrap();
        assert!((v3 - 75.0).abs() < 1e-9, "frame 3: got {v3}");

        // Frame 4: input 0 => F_3 = 0.25*0 + 0.75*75 = 56.25
        let r4 = state.process(&make_f64_array(&[0.0]));
        let v4 = r4.data.get_as_f64(0).unwrap();
        assert!((v4 - 56.25).abs() < 1e-9, "frame 4: got {v4}");
    }

    #[test]
    fn test_4tap_filter_all_taps() {
        // Use all 4 filter taps and 4 output taps to verify the full recurrence.
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                fc: [0.5, 0.3, 0.1, 0.1],
                oc: [0.7, 0.2, 0.05, 0.05],
                rc: [1.0, 0.0],
                f_offset: 0.0,
                f_scale: 1.0,
                o_offset: 0.0,
                o_scale: 1.0,
                ..Default::default()
            },
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        // Frame 0: reset => F_0 = RC1*I = 10.0, O_0 = OC1*F_0 = 7.0
        let _ = state.process(&make_f64_array(&[10.0]));

        // Frame 1: I=20
        // F_1 = 0.5*20 + 0.3*10 + 0.1*(0-0) + 0.1*(0-0) = 10+3 = 13
        // O_1 = 0.7*13 + 0.2*10 + 0.05*(7-0) + 0.05*(0-0) = 9.1+2+0.35 = 11.45
        let r1 = state.process(&make_f64_array(&[20.0]));
        let v1 = r1.data.get_as_f64(0).unwrap();
        assert!((v1 - 11.45).abs() < 1e-9, "frame 1: got {v1}");

        // Frame 2: I=30
        // F_2 = 0.5*30 + 0.3*13 + 0.1*(10-0) + 0.1*(0-0) = 15+3.9+1 = 19.9
        // O_2 = 0.7*19.9 + 0.2*13 + 0.05*(11.45-0) + 0.05*(7-0) = 13.93+2.6+0.5725+0.35 = 17.4525
        let r2 = state.process(&make_f64_array(&[30.0]));
        let v2 = r2.data.get_as_f64(0).unwrap();
        assert!((v2 - 17.4525).abs() < 1e-9, "frame 2: got {v2}");
    }

    #[test]
    fn test_save_background_one_shot() {
        let mut state = ProcessState::new(ProcessConfig {
            save_background: true,
            ..Default::default()
        });

        assert!(!state.config.valid_background);
        assert!(state.background.is_none());

        // Process with save_background=true: should capture and clear flag
        let input = make_array(&[10, 20, 30]);
        let _ = state.process(&input);

        assert!(!state.config.save_background, "save_background should be cleared");
        assert!(state.config.valid_background, "valid_background should be set");
        assert!(state.background.is_some());

        let bg = state.background.as_ref().unwrap();
        assert_eq!(bg.len(), 3);
        assert!((bg[0] - 10.0).abs() < 1e-9);
        assert!((bg[1] - 20.0).abs() < 1e-9);
        assert!((bg[2] - 30.0).abs() < 1e-9);

        // Process again: flag should remain cleared, background should persist
        let input2 = make_array(&[40, 50, 60]);
        let _ = state.process(&input2);

        assert!(!state.config.save_background, "save_background stays cleared");
        // Background unchanged
        let bg2 = state.background.as_ref().unwrap();
        assert!((bg2[0] - 10.0).abs() < 1e-9);
    }

    #[test]
    fn test_save_flat_field_one_shot() {
        let mut state = ProcessState::new(ProcessConfig {
            save_flat_field: true,
            ..Default::default()
        });

        assert!(!state.config.valid_flat_field);
        assert!(state.flat_field.is_none());

        let input = make_array(&[50, 100, 150]);
        let _ = state.process(&input);

        assert!(!state.config.save_flat_field, "save_flat_field should be cleared");
        assert!(state.config.valid_flat_field, "valid_flat_field should be set");
        assert!(state.flat_field.is_some());

        let ff = state.flat_field.as_ref().unwrap();
        assert_eq!(ff.len(), 3);
        assert!((ff[0] - 50.0).abs() < 1e-9);
        assert!((ff[1] - 100.0).abs() < 1e-9);
        assert!((ff[2] - 150.0).abs() < 1e-9);
    }

    #[test]
    fn test_auto_reset_when_num_filter_reached() {
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                num_filter: 3,
                auto_reset: true,
                fc: [0.5, 0.5, 0.0, 0.0],
                oc: [1.0, 0.0, 0.0, 0.0],
                ..Default::default()
            },
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        // Frame 0 (reset): num_filtered becomes 1
        let _ = state.process(&make_f64_array(&[100.0]));
        assert_eq!(state.num_filtered, 1);

        // Frame 1: num_filtered becomes 2
        let _ = state.process(&make_f64_array(&[100.0]));
        assert_eq!(state.num_filtered, 2);

        // Frame 2: num_filtered would become 3, triggers auto_reset => 0
        let _ = state.process(&make_f64_array(&[100.0]));
        assert_eq!(state.num_filtered, 0, "auto_reset should have fired");
        assert!(state.filter_state.is_none(), "filter state should be cleared");
        assert!(state.output_state.is_none(), "output state should be cleared");

        // Frame 3 (after reset): acts as a new first frame
        let _ = state.process(&make_f64_array(&[200.0]));
        assert_eq!(state.num_filtered, 1, "fresh start after reset");
    }

    #[test]
    fn test_filter_with_offset_scale() {
        // Test that f_offset/f_scale and o_offset/o_scale are applied correctly.
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                fc: [1.0, 0.0, 0.0, 0.0],
                oc: [1.0, 0.0, 0.0, 0.0],
                f_offset: 10.0,
                f_scale: 2.0,
                o_offset: 5.0,
                o_scale: 3.0,
                ..Default::default()
            },
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        // Frame 0 (reset): F_0 = RC1*I = 1.0*50 = 50
        // Output on reset: O = OC1*F = 1.0*50 = 50, then O = o_offset + o_scale*O = 5+3*50 = 155
        let r0 = state.process(&make_f64_array(&[50.0]));
        let v0 = r0.data.get_as_f64(0).unwrap();
        assert!((v0 - 155.0).abs() < 1e-9, "frame 0: got {v0}");

        // Frame 1: I=20
        // F_1 = FC1*20 = 20, then F_1 = f_offset + f_scale*F_1 = 10+2*20 = 50
        // O_1 = OC1*F_1 = 50, then O_1 = o_offset + o_scale*O_1 = 5+3*50 = 155
        let r1 = state.process(&make_f64_array(&[20.0]));
        let v1 = r1.data.get_as_f64(0).unwrap();
        assert!((v1 - 155.0).abs() < 1e-9, "frame 1: got {v1}");
    }

    #[test]
    fn test_reset_filter_manual() {
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                fc: [0.5, 0.5, 0.0, 0.0],
                oc: [1.0, 0.0, 0.0, 0.0],
                ..Default::default()
            },
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        // Build up filter state
        let _ = state.process(&make_f64_array(&[100.0]));
        let _ = state.process(&make_f64_array(&[100.0]));
        assert!(state.filter_state.is_some());
        assert_eq!(state.num_filtered, 2);

        // Manual reset
        state.reset_filter();
        assert!(state.filter_state.is_none());
        assert!(state.output_state.is_none());
        assert_eq!(state.num_filtered, 0);

        // Next frame should act as first frame (reset mode)
        let r = state.process(&make_f64_array(&[200.0]));
        let v = r.data.get_as_f64(0).unwrap();
        assert!((v - 200.0).abs() < 1e-9, "after reset, first frame: got {v}");
        assert_eq!(state.num_filtered, 1);
    }
}
