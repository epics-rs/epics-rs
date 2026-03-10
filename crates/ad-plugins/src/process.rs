use std::sync::Arc;

use ad_core::ndarray::{NDArray, NDDataBuffer, NDDataType};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::NDPluginProcess;

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
    pub filter_coeff: f64,
    pub output_type: Option<NDDataType>,
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
            filter_coeff: 1.0,
            output_type: None,
        }
    }
}

/// State for the process plugin (holds background and flat field arrays).
pub struct ProcessState {
    pub config: ProcessConfig,
    pub background: Option<Vec<f64>>,
    pub flat_field: Option<Vec<f64>>,
    pub filter_state: Option<Vec<f64>>,
}

impl ProcessState {
    pub fn new(config: ProcessConfig) -> Self {
        Self {
            config,
            background: None,
            flat_field: None,
            filter_state: None,
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
    }

    /// Save the current array as flat field.
    pub fn save_flat_field(&mut self, array: &NDArray) {
        let n = array.data.len();
        let mut ff = vec![0.0f64; n];
        for i in 0..n {
            ff[i] = array.data.get_as_f64(i).unwrap_or(0.0);
        }
        self.flat_field = Some(ff);
    }

    /// Process an array through the configured pipeline.
    pub fn process(&mut self, src: &NDArray) -> NDArray {
        let n = src.data.len();
        let mut values = vec![0.0f64; n];
        for i in 0..n {
            values[i] = src.data.get_as_f64(i).unwrap_or(0.0);
        }

        // 1. Background subtraction
        if self.config.enable_background {
            if let Some(ref bg) = self.background {
                for i in 0..n.min(bg.len()) {
                    values[i] -= bg[i];
                }
            }
        }

        // 2. Flat field normalization
        if self.config.enable_flat_field {
            if let Some(ref ff) = self.flat_field {
                let ff_mean: f64 = ff.iter().sum::<f64>() / ff.len().max(1) as f64;
                for i in 0..n.min(ff.len()) {
                    if ff[i] != 0.0 {
                        values[i] = values[i] * ff_mean / ff[i];
                    }
                }
            }
        }

        // 3. Offset + scale
        if self.config.enable_offset_scale {
            for v in values.iter_mut() {
                *v = *v * self.config.scale + self.config.offset;
            }
        }

        // 4. Clipping
        if self.config.enable_low_clip {
            for v in values.iter_mut() {
                if *v < self.config.low_clip {
                    *v = self.config.low_clip;
                }
            }
        }
        if self.config.enable_high_clip {
            for v in values.iter_mut() {
                if *v > self.config.high_clip {
                    *v = self.config.high_clip;
                }
            }
        }

        // 5. Recursive filter (IIR: output = coeff * input + (1-coeff) * prev)
        if self.config.enable_filter {
            let coeff = self.config.filter_coeff.clamp(0.0, 1.0);
            if let Some(ref mut prev) = self.filter_state {
                if prev.len() == n {
                    for i in 0..n {
                        values[i] = coeff * values[i] + (1.0 - coeff) * prev[i];
                    }
                }
            }
            self.filter_state = Some(values.clone());
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

// --- New ProcessProcessor (NDPluginProcess-based) ---

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
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        let out = self.state.process(array);
        vec![Arc::new(out)]
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
            // ff_mean ≈ 116.67, so all values should be ≈ 116
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
        let input1 = make_array(&[100, 100, 100]);
        let input2 = make_array(&[0, 0, 0]);

        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter_coeff: 0.5,
            ..Default::default()
        });

        let _ = state.process(&input1); // sets filter state to [100,100,100]
        let result = state.process(&input2); // 0.5*0 + 0.5*100 = 50
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

    // --- New ProcessProcessor tests ---

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
        let outputs = proc.process_array(&input, &pool);
        assert_eq!(outputs.len(), 1);
        if let NDDataBuffer::U8(ref v) = outputs[0].data {
            assert_eq!(v[0], 21); // 10*2+1
        }
    }
}
