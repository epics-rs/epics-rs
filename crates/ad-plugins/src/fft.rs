use std::sync::Arc;

use ad_core::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::NDPluginProcess;

/// Compute 1D FFT magnitude for each row of a 2D array.
/// Returns a Float64 array with the same dimensions.
pub fn fft_1d_rows(src: &NDArray) -> Option<NDArray> {
    if src.dims.is_empty() {
        return None;
    }

    let width = src.dims[0].size;
    let height = if src.dims.len() >= 2 { src.dims[1].size } else { 1 };
    let n = width;

    // Simple DFT (not FFT) for correctness — production would use rustfft
    let mut magnitudes = vec![0.0f64; width * height];

    for row in 0..height {
        for k in 0..n {
            let mut re = 0.0f64;
            let mut im = 0.0f64;
            for t in 0..n {
                let val = src.data.get_as_f64(row * width + t).unwrap_or(0.0);
                let angle = -2.0 * std::f64::consts::PI * k as f64 * t as f64 / n as f64;
                re += val * angle.cos();
                im += val * angle.sin();
            }
            magnitudes[row * width + k] = (re * re + im * im).sqrt();
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

/// Compute 2D DFT magnitude.
pub fn fft_2d(src: &NDArray) -> Option<NDArray> {
    if src.dims.len() < 2 {
        return None;
    }

    let w = src.dims[0].size;
    let h = src.dims[1].size;

    let mut magnitudes = vec![0.0f64; w * h];

    for ky in 0..h {
        for kx in 0..w {
            let mut re = 0.0f64;
            let mut im = 0.0f64;
            for ty in 0..h {
                for tx in 0..w {
                    let val = src.data.get_as_f64(ty * w + tx).unwrap_or(0.0);
                    let angle = -2.0 * std::f64::consts::PI
                        * (kx as f64 * tx as f64 / w as f64
                           + ky as f64 * ty as f64 / h as f64);
                    re += val * angle.cos();
                    im += val * angle.sin();
                }
            }
            magnitudes[ky * w + kx] = (re * re + im * im).sqrt();
        }
    }

    let dims = vec![NDDimension::new(w), NDDimension::new(h)];
    let mut arr = NDArray::new(dims, NDDataType::Float64);
    arr.data = NDDataBuffer::F64(magnitudes);
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    Some(arr)
}

/// FFT mode selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FFTMode {
    Rows1D,
    Full2D,
}

/// Pure FFT processing logic.
pub struct FFTProcessor {
    mode: FFTMode,
}

impl FFTProcessor {
    pub fn new(mode: FFTMode) -> Self {
        Self { mode }
    }
}

impl NDPluginProcess for FFTProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        let result = match self.mode {
            FFTMode::Rows1D => fft_1d_rows(array),
            FFTMode::Full2D => fft_2d(array),
        };
        match result {
            Some(out) => vec![Arc::new(out)],
            None => vec![],
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginFFT"
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
            for i in 0..8 { v[i] = 1.0; }
        }

        let result = fft_1d_rows(&arr).unwrap();
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

        let result = fft_1d_rows(&arr).unwrap();
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
        let result = fft_2d(&arr).unwrap();
        assert_eq!(result.dims[0].size, 4);
        assert_eq!(result.dims[1].size, 4);
        assert_eq!(result.data.data_type(), NDDataType::Float64);
    }
}
