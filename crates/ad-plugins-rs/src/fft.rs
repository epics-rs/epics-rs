use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// FFT mode selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FFTMode {
    Rows1D,
    Full2D,
}

/// FFT direction (forward or inverse transform).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FFTDirection {
    Forward,
    Inverse,
}

/// Configuration for FFT processing.
pub struct FFTConfig {
    pub mode: FFTMode,
    pub direction: FFTDirection,
    /// Zero out DC component (k=0) in the output magnitudes.
    pub suppress_dc: bool,
    /// Average N frames of magnitude. 0 or 1 means no averaging.
    pub num_average: usize,
}

impl Default for FFTConfig {
    fn default() -> Self {
        Self {
            mode: FFTMode::Rows1D,
            direction: FFTDirection::Forward,
            suppress_dc: false,
            num_average: 0,
        }
    }
}

/// Compute 1D FFT magnitude for each row of a 2D array using rustfft.
/// Returns a Float64 array with the same dimensions.
pub fn fft_1d_rows(src: &NDArray, suppress_dc: bool) -> Option<NDArray> {
    if src.dims.is_empty() {
        return None;
    }

    let width = src.dims[0].size;
    let height = if src.dims.len() >= 2 {
        src.dims[1].size
    } else {
        1
    };

    if width == 0 {
        return None;
    }

    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(width);

    let mut magnitudes = vec![0.0f64; width * height];
    let mut row_buf = vec![Complex::new(0.0, 0.0); width];

    for row in 0..height {
        // Fill complex buffer: real = pixel value, imag = 0
        for i in 0..width {
            row_buf[i] = Complex::new(src.data.get_as_f64(row * width + i).unwrap_or(0.0), 0.0);
        }

        fft.process(&mut row_buf);

        // Compute magnitudes
        for (i, c) in row_buf.iter().enumerate() {
            magnitudes[row * width + i] = c.norm();
        }

        if suppress_dc {
            magnitudes[row * width] = 0.0;
        }
    }

    let dims = src.dims.clone();
    let mut arr = NDArray::new(dims, NDDataType::Float64);
    arr.data = NDDataBuffer::F64(magnitudes);
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    Some(arr)
}

/// Compute 2D FFT magnitude using separable row-then-column FFT via rustfft.
pub fn fft_2d(src: &NDArray, suppress_dc: bool) -> Option<NDArray> {
    if src.dims.len() < 2 {
        return None;
    }

    let w = src.dims[0].size;
    let h = src.dims[1].size;

    if w == 0 || h == 0 {
        return None;
    }

    let mut planner = FftPlanner::<f64>::new();
    let fft_row = planner.plan_fft_forward(w);
    let fft_col = planner.plan_fft_forward(h);

    // Step 1: Row FFTs — build a w×h complex buffer
    let mut data = vec![Complex::new(0.0, 0.0); w * h];
    let mut row_buf = vec![Complex::new(0.0, 0.0); w];

    for row in 0..h {
        for i in 0..w {
            row_buf[i] = Complex::new(src.data.get_as_f64(row * w + i).unwrap_or(0.0), 0.0);
        }
        fft_row.process(&mut row_buf);
        data[row * w..(row * w + w)].copy_from_slice(&row_buf);
    }

    // Step 2: Column FFTs
    let mut col_buf = vec![Complex::new(0.0, 0.0); h];

    for col in 0..w {
        // Extract column
        for row in 0..h {
            col_buf[row] = data[row * w + col];
        }
        fft_col.process(&mut col_buf);
        // Write back
        for row in 0..h {
            data[row * w + col] = col_buf[row];
        }
    }

    // Step 3: Compute magnitudes
    let mut magnitudes: Vec<f64> = data.iter().map(|c| c.norm()).collect();

    if suppress_dc {
        magnitudes[0] = 0.0;
    }

    let dims = vec![NDDimension::new(w), NDDimension::new(h)];
    let mut arr = NDArray::new(dims, NDDataType::Float64);
    arr.data = NDDataBuffer::F64(magnitudes);
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    Some(arr)
}

/// FFT processing engine with cached planner and optional magnitude averaging.
pub struct FFTProcessor {
    config: FFTConfig,
    planner: FftPlanner<f64>,
    /// Running average magnitude buffer.
    avg_buffer: Option<Vec<f64>>,
    /// Number of frames accumulated so far.
    avg_count: usize,
    /// Cached dimensions to detect changes.
    cached_dims: Vec<usize>,
}

impl FFTProcessor {
    pub fn new(mode: FFTMode) -> Self {
        Self {
            config: FFTConfig {
                mode,
                direction: FFTDirection::Forward,
                suppress_dc: false,
                num_average: 0,
            },
            planner: FftPlanner::new(),
            avg_buffer: None,
            avg_count: 0,
            cached_dims: Vec::new(),
        }
    }

    pub fn with_config(config: FFTConfig) -> Self {
        Self {
            config,
            planner: FftPlanner::new(),
            avg_buffer: None,
            avg_count: 0,
            cached_dims: Vec::new(),
        }
    }

    /// Check if dimensions changed and reset averaging state if so.
    fn check_dims_changed(&mut self, dims: &[NDDimension]) {
        let current: Vec<usize> = dims.iter().map(|d| d.size).collect();
        if current != self.cached_dims {
            self.cached_dims = current;
            self.avg_buffer = None;
            self.avg_count = 0;
        }
    }

    /// Compute FFT using cached planner for plan reuse across frames.
    fn compute_fft(&mut self, src: &NDArray) -> Option<NDArray> {
        let suppress_dc = self.config.suppress_dc;

        match (self.config.mode, self.config.direction) {
            (FFTMode::Rows1D, FFTDirection::Forward) => {
                self.compute_fft_1d_rows_forward(src, suppress_dc)
            }
            (FFTMode::Rows1D, FFTDirection::Inverse) => {
                self.compute_fft_1d_rows_inverse(src, suppress_dc)
            }
            (FFTMode::Full2D, FFTDirection::Forward) => {
                self.compute_fft_2d_forward(src, suppress_dc)
            }
            (FFTMode::Full2D, FFTDirection::Inverse) => {
                self.compute_fft_2d_inverse(src, suppress_dc)
            }
        }
    }

    fn compute_fft_1d_rows_forward(&mut self, src: &NDArray, suppress_dc: bool) -> Option<NDArray> {
        if src.dims.is_empty() {
            return None;
        }

        let width = src.dims[0].size;
        let height = if src.dims.len() >= 2 {
            src.dims[1].size
        } else {
            1
        };

        if width == 0 {
            return None;
        }

        let fft = self.planner.plan_fft_forward(width);

        let mut magnitudes = vec![0.0f64; width * height];
        let mut row_buf = vec![Complex::new(0.0, 0.0); width];

        for row in 0..height {
            for i in 0..width {
                row_buf[i] = Complex::new(src.data.get_as_f64(row * width + i).unwrap_or(0.0), 0.0);
            }
            fft.process(&mut row_buf);
            for (i, c) in row_buf.iter().enumerate() {
                magnitudes[row * width + i] = c.norm();
            }
            if suppress_dc {
                magnitudes[row * width] = 0.0;
            }
        }

        let dims = src.dims.clone();
        let mut arr = NDArray::new(dims, NDDataType::Float64);
        arr.data = NDDataBuffer::F64(magnitudes);
        arr.unique_id = src.unique_id;
        arr.timestamp = src.timestamp;
        arr.attributes = src.attributes.clone();
        Some(arr)
    }

    fn compute_fft_1d_rows_inverse(&mut self, src: &NDArray, suppress_dc: bool) -> Option<NDArray> {
        if src.dims.is_empty() {
            return None;
        }

        let width = src.dims[0].size;
        let height = if src.dims.len() >= 2 {
            src.dims[1].size
        } else {
            1
        };

        if width == 0 {
            return None;
        }

        let fft = self.planner.plan_fft_inverse(width);
        let scale = 1.0 / width as f64;

        let mut magnitudes = vec![0.0f64; width * height];
        let mut row_buf = vec![Complex::new(0.0, 0.0); width];

        for row in 0..height {
            for i in 0..width {
                row_buf[i] = Complex::new(src.data.get_as_f64(row * width + i).unwrap_or(0.0), 0.0);
            }
            if suppress_dc {
                row_buf[0] = Complex::new(0.0, 0.0);
            }
            fft.process(&mut row_buf);
            for (i, c) in row_buf.iter().enumerate() {
                magnitudes[row * width + i] = c.norm() * scale;
            }
        }

        let dims = src.dims.clone();
        let mut arr = NDArray::new(dims, NDDataType::Float64);
        arr.data = NDDataBuffer::F64(magnitudes);
        arr.unique_id = src.unique_id;
        arr.timestamp = src.timestamp;
        arr.attributes = src.attributes.clone();
        Some(arr)
    }

    fn compute_fft_2d_forward(&mut self, src: &NDArray, suppress_dc: bool) -> Option<NDArray> {
        if src.dims.len() < 2 {
            return None;
        }

        let w = src.dims[0].size;
        let h = src.dims[1].size;

        if w == 0 || h == 0 {
            return None;
        }

        let fft_row = self.planner.plan_fft_forward(w);
        let fft_col = self.planner.plan_fft_forward(h);

        let mut data = vec![Complex::new(0.0, 0.0); w * h];
        let mut row_buf = vec![Complex::new(0.0, 0.0); w];

        for row in 0..h {
            for i in 0..w {
                row_buf[i] = Complex::new(src.data.get_as_f64(row * w + i).unwrap_or(0.0), 0.0);
            }
            fft_row.process(&mut row_buf);
            data[row * w..(row * w + w)].copy_from_slice(&row_buf);
        }

        let mut col_buf = vec![Complex::new(0.0, 0.0); h];
        for col in 0..w {
            for row in 0..h {
                col_buf[row] = data[row * w + col];
            }
            fft_col.process(&mut col_buf);
            for row in 0..h {
                data[row * w + col] = col_buf[row];
            }
        }

        let mut magnitudes: Vec<f64> = data.iter().map(|c| c.norm()).collect();

        if suppress_dc {
            magnitudes[0] = 0.0;
        }

        let dims = vec![NDDimension::new(w), NDDimension::new(h)];
        let mut arr = NDArray::new(dims, NDDataType::Float64);
        arr.data = NDDataBuffer::F64(magnitudes);
        arr.unique_id = src.unique_id;
        arr.timestamp = src.timestamp;
        arr.attributes = src.attributes.clone();
        Some(arr)
    }

    fn compute_fft_2d_inverse(&mut self, src: &NDArray, suppress_dc: bool) -> Option<NDArray> {
        if src.dims.len() < 2 {
            return None;
        }

        let w = src.dims[0].size;
        let h = src.dims[1].size;

        if w == 0 || h == 0 {
            return None;
        }

        let fft_row = self.planner.plan_fft_inverse(w);
        let fft_col = self.planner.plan_fft_inverse(h);
        let scale = 1.0 / (w * h) as f64;

        let mut data = vec![Complex::new(0.0, 0.0); w * h];
        for i in 0..w * h {
            data[i] = Complex::new(src.data.get_as_f64(i).unwrap_or(0.0), 0.0);
        }

        if suppress_dc {
            data[0] = Complex::new(0.0, 0.0);
        }

        let mut col_buf = vec![Complex::new(0.0, 0.0); h];
        for col in 0..w {
            for row in 0..h {
                col_buf[row] = data[row * w + col];
            }
            fft_col.process(&mut col_buf);
            for row in 0..h {
                data[row * w + col] = col_buf[row];
            }
        }

        let mut row_buf = vec![Complex::new(0.0, 0.0); w];
        for row in 0..h {
            row_buf.copy_from_slice(&data[row * w..(row * w + w)]);
            fft_row.process(&mut row_buf);
            data[row * w..(row * w + w)].copy_from_slice(&row_buf);
        }

        let magnitudes: Vec<f64> = data.iter().map(|c| c.norm() * scale).collect();

        let dims = vec![NDDimension::new(w), NDDimension::new(h)];
        let mut arr = NDArray::new(dims, NDDataType::Float64);
        arr.data = NDDataBuffer::F64(magnitudes);
        arr.unique_id = src.unique_id;
        arr.timestamp = src.timestamp;
        arr.attributes = src.attributes.clone();
        Some(arr)
    }

    /// Apply magnitude averaging. Returns the averaged magnitudes if ready.
    fn apply_averaging(&mut self, magnitudes: &[f64]) -> Vec<f64> {
        let num_avg = self.config.num_average;
        if num_avg <= 1 {
            return magnitudes.to_vec();
        }

        let buf = self
            .avg_buffer
            .get_or_insert_with(|| vec![0.0; magnitudes.len()]);

        // Reset if buffer size changed (shouldn't happen after check_dims_changed, but guard)
        if buf.len() != magnitudes.len() {
            *buf = vec![0.0; magnitudes.len()];
            self.avg_count = 0;
        }

        // Accumulate
        for (b, &m) in buf.iter_mut().zip(magnitudes.iter()) {
            *b += m;
        }
        self.avg_count += 1;

        if self.avg_count >= num_avg {
            // Output averaged result and reset
            let result: Vec<f64> = buf.iter().map(|&v| v / self.avg_count as f64).collect();
            buf.iter_mut().for_each(|v| *v = 0.0);
            self.avg_count = 0;
            result
        } else {
            // Not enough frames yet — return current partial average
            buf.iter().map(|&v| v / self.avg_count as f64).collect()
        }
    }
}

impl NDPluginProcess for FFTProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        self.check_dims_changed(&array.dims);

        let result = self.compute_fft(array);
        match result {
            Some(mut out) => {
                if self.config.num_average > 1 {
                    if let NDDataBuffer::F64(ref mags) = out.data {
                        let averaged = self.apply_averaging(mags);
                        out.data = NDDataBuffer::F64(averaged);
                    }
                }
                ProcessResult::arrays(vec![Arc::new(out)])
            }
            None => ProcessResult::empty(),
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginFFT"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        base.create_param("FFT_TIME_PER_POINT", ParamType::Float64)?;
        base.create_param("FFT_TIME_AXIS", ParamType::Float64Array)?;
        base.create_param("FFT_FREQ_AXIS", ParamType::Float64Array)?;
        base.create_param("FFT_DIRECTION", ParamType::Int32)?;
        base.create_param("FFT_SUPPRESS_DC", ParamType::Int32)?;
        base.create_param("FFT_NUM_AVERAGE", ParamType::Int32)?;
        base.create_param("FFT_NUM_AVERAGED", ParamType::Int32)?;
        base.create_param("FFT_RESET_AVERAGE", ParamType::Int32)?;
        base.create_param("FFT_TIME_SERIES", ParamType::Float64Array)?;
        base.create_param("FFT_REAL", ParamType::Float64Array)?;
        base.create_param("FFT_IMAGINARY", ParamType::Float64Array)?;
        base.create_param("FFT_ABS_VALUE", ParamType::Float64Array)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fft_1d_dc() {
        // Constant signal: DC component should dominate
        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..8 {
                v[i] = 1.0;
            }
        }

        let result = fft_1d_rows(&arr, false).unwrap();
        if let NDDataBuffer::F64(ref v) = result.data {
            // DC component (k=0) should be 8.0
            assert!((v[0] - 8.0).abs() < 1e-10);
            // Other components should be ~0
            assert!(v[1].abs() < 1e-10);
        }
    }

    #[test]
    fn test_fft_1d_sine() {
        // Sine wave at frequency 1: peak at k=1 and k=N-1
        let n = 16;
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..n {
                v[i] = (2.0 * std::f64::consts::PI * i as f64 / n as f64).sin();
            }
        }

        let result = fft_1d_rows(&arr, false).unwrap();
        if let NDDataBuffer::F64(ref v) = result.data {
            // DC should be ~0
            assert!(v[0].abs() < 1e-10);
            // Peak at k=1
            assert!(v[1] > 7.0);
            // k=2 should be small
            assert!(v[2].abs() < 1e-10);
        }
    }

    #[test]
    fn test_fft_2d_dimensions() {
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        let result = fft_2d(&arr, false).unwrap();
        assert_eq!(result.dims[0].size, 4);
        assert_eq!(result.dims[1].size, 4);
        assert_eq!(result.data.data_type(), NDDataType::Float64);
    }

    #[test]
    fn test_fft_1d_suppress_dc() {
        // Constant signal: DC component should be suppressed
        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..8 {
                v[i] = 1.0;
            }
        }

        let result = fft_1d_rows(&arr, true).unwrap();
        if let NDDataBuffer::F64(ref v) = result.data {
            // DC component should be zeroed out
            assert!((v[0]).abs() < 1e-15);
            // Other components should still be ~0 for constant signal
            assert!(v[1].abs() < 1e-10);
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_fft_2d_suppress_dc() {
        // 4x4 constant array, suppress_dc should zero out [0,0]
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::Float64,
        );
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for val in v.iter_mut() {
                *val = 3.0;
            }
        }

        let result = fft_2d(&arr, true).unwrap();
        if let NDDataBuffer::F64(ref v) = result.data {
            // DC at [0,0] should be zeroed
            assert!((v[0]).abs() < 1e-15);
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_fft_2d_known_dc() {
        // 4x4 constant=2.0 => DC should be 4*4*2 = 32
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::Float64,
        );
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for val in v.iter_mut() {
                *val = 2.0;
            }
        }

        let result = fft_2d(&arr, false).unwrap();
        if let NDDataBuffer::F64(ref v) = result.data {
            assert!((v[0] - 32.0).abs() < 1e-10, "DC = {}, expected 32", v[0]);
            // All other bins should be ~0
            for i in 1..v.len() {
                assert!(v[i].abs() < 1e-10, "bin {} = {}, expected ~0", i, v[i]);
            }
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_fft_1d_known_cosine_peaks() {
        // Cosine at frequency 3 in N=16: peaks at k=3 and k=N-3=13
        let n = 16;
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..n {
                v[i] = (2.0 * std::f64::consts::PI * 3.0 * i as f64 / n as f64).cos();
            }
        }

        let result = fft_1d_rows(&arr, false).unwrap();
        if let NDDataBuffer::F64(ref v) = result.data {
            // DC should be ~0
            assert!(v[0].abs() < 1e-10);
            // k=3 and k=13 should have magnitude N/2 = 8
            assert!(
                (v[3] - 8.0).abs() < 1e-10,
                "k=3 magnitude = {}, expected 8",
                v[3]
            );
            assert!(
                (v[13] - 8.0).abs() < 1e-10,
                "k=13 magnitude = {}, expected 8",
                v[13]
            );
            // Other bins should be ~0
            for k in [0, 1, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 15] {
                assert!(
                    v[k].abs() < 1e-10,
                    "k={} magnitude = {}, expected ~0",
                    k,
                    v[k]
                );
            }
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_processor_with_config() {
        let config = FFTConfig {
            mode: FFTMode::Rows1D,
            direction: FFTDirection::Forward,
            suppress_dc: true,
            num_average: 0,
        };
        let mut proc = FFTProcessor::with_config(config);
        let pool = NDArrayPool::new(0);

        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..8 {
                v[i] = 5.0;
            }
        }

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        if let NDDataBuffer::F64(ref v) = result.output_arrays[0].data {
            // suppress_dc: DC should be 0
            assert!(v[0].abs() < 1e-15);
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_processor_averaging() {
        let config = FFTConfig {
            mode: FFTMode::Rows1D,
            direction: FFTDirection::Forward,
            suppress_dc: false,
            num_average: 2,
        };
        let mut proc = FFTProcessor::with_config(config);
        let pool = NDArrayPool::new(0);

        // Frame 1: constant = 2.0 => DC magnitude = 8*2 = 16
        let mut arr1 = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr1.data {
            for i in 0..8 {
                v[i] = 2.0;
            }
        }

        // Frame 2: constant = 4.0 => DC magnitude = 8*4 = 32
        let mut arr2 = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr2.data {
            for i in 0..8 {
                v[i] = 4.0;
            }
        }

        let r1 = proc.process_array(&arr1, &pool);
        assert_eq!(r1.output_arrays.len(), 1);
        // After 1 frame with num_average=2, partial average = 16/1 = 16
        if let NDDataBuffer::F64(ref v) = r1.output_arrays[0].data {
            assert!((v[0] - 16.0).abs() < 1e-10, "partial avg DC = {}", v[0]);
        }

        let r2 = proc.process_array(&arr2, &pool);
        assert_eq!(r2.output_arrays.len(), 1);
        // After 2 frames: average = (16+32)/2 = 24, then reset
        if let NDDataBuffer::F64(ref v) = r2.output_arrays[0].data {
            assert!((v[0] - 24.0).abs() < 1e-10, "averaged DC = {}", v[0]);
        }
    }

    #[test]
    fn test_processor_averaging_dimension_change_resets() {
        let config = FFTConfig {
            mode: FFTMode::Rows1D,
            direction: FFTDirection::Forward,
            suppress_dc: false,
            num_average: 3,
        };
        let mut proc = FFTProcessor::with_config(config);
        let pool = NDArrayPool::new(0);

        // Frame 1: width=8
        let mut arr1 = NDArray::new(vec![NDDimension::new(8)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr1.data {
            for i in 0..8 {
                v[i] = 1.0;
            }
        }
        let _ = proc.process_array(&arr1, &pool);
        assert_eq!(proc.avg_count, 1);

        // Frame 2: width=4 — dimension change should reset
        let mut arr2 = NDArray::new(vec![NDDimension::new(4)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr2.data {
            for i in 0..4 {
                v[i] = 1.0;
            }
        }
        let _ = proc.process_array(&arr2, &pool);
        // After dimension change, avg_count should be 1 (reset + one new frame)
        assert_eq!(proc.avg_count, 1);
    }

    #[test]
    fn test_fft_1d_multirow() {
        // 2 rows, each a different constant
        let w = 4;
        let h = 2;
        let mut arr = NDArray::new(
            vec![NDDimension::new(w), NDDimension::new(h)],
            NDDataType::Float64,
        );
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            // Row 0: all 1.0
            for i in 0..w {
                v[i] = 1.0;
            }
            // Row 1: all 3.0
            for i in w..2 * w {
                v[i] = 3.0;
            }
        }

        let result = fft_1d_rows(&arr, false).unwrap();
        if let NDDataBuffer::F64(ref v) = result.data {
            // Row 0 DC = 4*1 = 4
            assert!((v[0] - 4.0).abs() < 1e-10);
            // Row 1 DC = 4*3 = 12
            assert!((v[w] - 12.0).abs() < 1e-10);
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_inverse_fft_1d() {
        // IFFT of a known forward FFT should give back the original magnitudes
        // For a real constant signal, forward FFT gives [N, 0, 0, ...0]
        // IFFT of [N, 0, ...0] (real input) should give constant = 1.0 for each sample
        let n = 8;
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v[0] = 8.0; // DC = N
            // rest are 0
        }

        let config = FFTConfig {
            mode: FFTMode::Rows1D,
            direction: FFTDirection::Inverse,
            suppress_dc: false,
            num_average: 0,
        };
        let mut proc = FFTProcessor::with_config(config);
        let pool = NDArrayPool::new(0);

        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        if let NDDataBuffer::F64(ref v) = result.output_arrays[0].data {
            // Each sample should be magnitude 1.0 (8/8 = 1.0 after normalization)
            for i in 0..n {
                assert!(
                    (v[i] - 1.0).abs() < 1e-10,
                    "sample {} = {}, expected 1.0",
                    i,
                    v[i]
                );
            }
        } else {
            panic!("expected F64 data");
        }
    }

    #[test]
    fn test_fft_preserves_metadata() {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::Float64);
        arr.unique_id = 42;
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v[0] = 1.0;
        }

        let result = fft_1d_rows(&arr, false).unwrap();
        assert_eq!(result.unique_id, 42);
        assert_eq!(result.timestamp, arr.timestamp);
    }
}
