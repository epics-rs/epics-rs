use std::sync::Arc;

use ad_core::ndarray::{NDArray, NDDataBuffer};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::{NDPluginProcess, PluginRuntimeHandle};
use parking_lot::Mutex;

/// Statistics computed from an NDArray.
#[derive(Debug, Clone, Default)]
pub struct StatsResult {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub sigma: f64,
    pub total: f64,
    pub num_elements: usize,
    pub centroid_x: f64,
    pub centroid_y: f64,
    pub sigma_x: f64,
    pub sigma_y: f64,
}

/// Compute min/max/mean/sigma/total from an NDDataBuffer.
pub fn compute_stats(data: &NDDataBuffer) -> StatsResult {
    macro_rules! stats_for {
        ($vec:expr) => {{
            let v = $vec;
            if v.is_empty() {
                return StatsResult::default();
            }
            let mut min = v[0] as f64;
            let mut max = v[0] as f64;
            let mut total = 0.0f64;
            for &elem in v.iter() {
                let f = elem as f64;
                if f < min { min = f; }
                if f > max { max = f; }
                total += f;
            }
            let mean = total / v.len() as f64;
            let mut variance = 0.0f64;
            for &elem in v.iter() {
                let diff = elem as f64 - mean;
                variance += diff * diff;
            }
            let sigma = (variance / v.len() as f64).sqrt();
            StatsResult {
                min,
                max,
                mean,
                sigma,
                total,
                num_elements: v.len(),
                centroid_x: 0.0,
                centroid_y: 0.0,
                sigma_x: 0.0,
                sigma_y: 0.0,
            }
        }};
    }

    match data {
        NDDataBuffer::I8(v) => stats_for!(v),
        NDDataBuffer::U8(v) => stats_for!(v),
        NDDataBuffer::I16(v) => stats_for!(v),
        NDDataBuffer::U16(v) => stats_for!(v),
        NDDataBuffer::I32(v) => stats_for!(v),
        NDDataBuffer::U32(v) => stats_for!(v),
        NDDataBuffer::I64(v) => stats_for!(v),
        NDDataBuffer::U64(v) => stats_for!(v),
        NDDataBuffer::F32(v) => stats_for!(v),
        NDDataBuffer::F64(v) => stats_for!(v),
    }
}

/// Compute centroid for a 2D array.
pub fn compute_centroid(data: &NDDataBuffer, x_size: usize, y_size: usize) -> (f64, f64, f64, f64) {
    let n = x_size * y_size;
    if n == 0 || data.len() < n {
        return (0.0, 0.0, 0.0, 0.0);
    }

    let mut total = 0.0f64;
    let mut cx = 0.0f64;
    let mut cy = 0.0f64;

    for iy in 0..y_size {
        for ix in 0..x_size {
            let val = data.get_as_f64(iy * x_size + ix).unwrap_or(0.0);
            total += val;
            cx += val * ix as f64;
            cy += val * iy as f64;
        }
    }

    if total == 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }

    cx /= total;
    cy /= total;

    let mut sx = 0.0f64;
    let mut sy = 0.0f64;
    for iy in 0..y_size {
        for ix in 0..x_size {
            let val = data.get_as_f64(iy * x_size + ix).unwrap_or(0.0);
            sx += val * (ix as f64 - cx).powi(2);
            sy += val * (iy as f64 - cy).powi(2);
        }
    }
    sx = (sx / total).sqrt();
    sy = (sy / total).sqrt();

    (cx, cy, sx, sy)
}

/// Pure processing logic for statistics computation.
pub struct StatsProcessor {
    latest_stats: Arc<Mutex<StatsResult>>,
    compute_centroid: bool,
}

impl StatsProcessor {
    pub fn new() -> Self {
        Self {
            latest_stats: Arc::new(Mutex::new(StatsResult::default())),
            compute_centroid: true,
        }
    }

    /// Get a cloneable handle to the latest stats.
    pub fn stats_handle(&self) -> Arc<Mutex<StatsResult>> {
        self.latest_stats.clone()
    }
}

impl Default for StatsProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for StatsProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        let mut result = compute_stats(&array.data);
        if self.compute_centroid {
            let info = array.info();
            if info.color_size == 1 && array.dims.len() >= 2 {
                let (cx, cy, sx, sy) = compute_centroid(
                    &array.data, info.x_size, info.y_size,
                );
                result.centroid_x = cx;
                result.centroid_y = cy;
                result.sigma_x = sx;
                result.sigma_y = sy;
            }
        }
        *self.latest_stats.lock() = result;
        vec![] // sink: no output arrays
    }

    fn plugin_type(&self) -> &str {
        "NDPluginStats"
    }
}

/// Create a stats plugin runtime. Returns the handle and a cloneable stats accessor.
pub fn create_stats_runtime(
    port_name: &str,
    pool: Arc<NDArrayPool>,
    queue_size: usize,
) -> (PluginRuntimeHandle, Arc<Mutex<StatsResult>>, std::thread::JoinHandle<()>) {
    let processor = StatsProcessor::new();
    let stats_handle = processor.stats_handle();

    let (plugin_handle, data_jh) = ad_core::plugin::runtime::create_plugin_runtime(
        port_name,
        processor,
        pool,
        queue_size,
    );

    (plugin_handle, stats_handle, data_jh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core::ndarray::{NDDataType, NDDimension};

    #[test]
    fn test_compute_stats_u8() {
        let data = NDDataBuffer::U8(vec![10, 20, 30, 40, 50]);
        let stats = compute_stats(&data);
        assert_eq!(stats.min, 10.0);
        assert_eq!(stats.max, 50.0);
        assert_eq!(stats.mean, 30.0);
        assert_eq!(stats.total, 150.0);
        assert_eq!(stats.num_elements, 5);
    }

    #[test]
    fn test_compute_stats_sigma() {
        let data = NDDataBuffer::F64(vec![2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]);
        let stats = compute_stats(&data);
        assert!((stats.mean - 5.0).abs() < 1e-10);
        assert!((stats.sigma - 2.0).abs() < 1e-10);
    }

    #[test]
    fn test_compute_stats_u16() {
        let data = NDDataBuffer::U16(vec![100, 200, 300]);
        let stats = compute_stats(&data);
        assert_eq!(stats.min, 100.0);
        assert_eq!(stats.max, 300.0);
        assert_eq!(stats.mean, 200.0);
    }

    #[test]
    fn test_compute_stats_f64() {
        let data = NDDataBuffer::F64(vec![1.5, 2.5, 3.5]);
        let stats = compute_stats(&data);
        assert!((stats.min - 1.5).abs() < 1e-10);
        assert!((stats.max - 3.5).abs() < 1e-10);
        assert!((stats.mean - 2.5).abs() < 1e-10);
    }

    #[test]
    fn test_compute_stats_single_element() {
        let data = NDDataBuffer::I32(vec![42]);
        let stats = compute_stats(&data);
        assert_eq!(stats.min, 42.0);
        assert_eq!(stats.max, 42.0);
        assert_eq!(stats.mean, 42.0);
        assert_eq!(stats.sigma, 0.0);
        assert_eq!(stats.num_elements, 1);
    }

    #[test]
    fn test_compute_stats_empty() {
        let data = NDDataBuffer::U8(vec![]);
        let stats = compute_stats(&data);
        assert_eq!(stats.num_elements, 0);
    }

    #[test]
    fn test_centroid_uniform() {
        let data = NDDataBuffer::U8(vec![1; 16]);
        let (cx, cy, _, _) = compute_centroid(&data, 4, 4);
        assert!((cx - 1.5).abs() < 1e-10);
        assert!((cy - 1.5).abs() < 1e-10);
    }

    #[test]
    fn test_centroid_corner() {
        let mut d = vec![0u8; 16];
        d[0] = 255;
        let data = NDDataBuffer::U8(d);
        let (cx, cy, _, _) = compute_centroid(&data, 4, 4);
        assert!((cx - 0.0).abs() < 1e-10);
        assert!((cy - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_stats_processor_direct() {
        let mut proc = StatsProcessor::new();
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(5)], NDDataType::UInt8);
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v[0] = 10; v[1] = 20; v[2] = 30; v[3] = 40; v[4] = 50;
        }

        let outputs = proc.process_array(&arr, &pool);
        assert!(outputs.is_empty(), "stats is a sink");

        let stats = proc.stats_handle().lock().clone();
        assert_eq!(stats.min, 10.0);
        assert_eq!(stats.max, 50.0);
        assert_eq!(stats.mean, 30.0);
    }

    #[test]
    fn test_stats_runtime_end_to_end() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (handle, stats, _jh) = create_stats_runtime("STATS_RT", pool, 10);

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for (i, val) in v.iter_mut().enumerate() {
                *val = (i + 1) as u8;
            }
        }

        handle.array_sender().send(Arc::new(arr));
        std::thread::sleep(std::time::Duration::from_millis(100));

        let result = stats.lock().clone();
        assert_eq!(result.min, 1.0);
        assert_eq!(result.max, 16.0);
        assert_eq!(result.num_elements, 16);
        assert!(result.centroid_x > 0.0, "centroid should be computed for 2D");
    }
}
