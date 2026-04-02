//! NDPluginROIStat: computes basic statistics for multiple ROI regions on each array.
//!
//! Each ROI is a rectangular sub-region of a 2D image. For each enabled ROI,
//! the plugin computes min, max, mean, total, and net (background-subtracted total).
//! Optionally accumulates time series data in circular buffers.

use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamUpdate, PluginParamSnapshot, PluginRuntimeHandle, ProcessResult,
};
use ad_core_rs::plugin::wiring::WiringRegistry;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;
use parking_lot::Mutex;

#[cfg(feature = "parallel")]
use crate::par_util;
use crate::time_series::{TimeSeriesData, TimeSeriesSender};
#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// Configuration for a single ROI region.
#[derive(Debug, Clone)]
pub struct ROIStatROI {
    pub enabled: bool,
    /// Offset in pixels: [x, y].
    pub offset: [usize; 2],
    /// Size in pixels: [x, y].
    pub size: [usize; 2],
    /// Width of the background border (pixels). 0 = no background subtraction.
    pub bgd_width: usize,
}

impl Default for ROIStatROI {
    fn default() -> Self {
        Self {
            enabled: true,
            offset: [0, 0],
            size: [0, 0],
            bgd_width: 0,
        }
    }
}

/// Statistics computed for a single ROI.
#[derive(Debug, Clone, Default)]
pub struct ROIStatResult {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub total: f64,
    /// Net = total - background_average * roi_elements. Zero if bgd_width is 0.
    pub net: f64,
}

/// Time-series acquisition mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TSMode {
    Idle,
    Acquiring,
}

/// Number of statistics tracked per ROI (min, max, mean, total, net).
const NUM_STATS: usize = 5;

/// Per-ROI stat names used for time series channel naming.
const ROI_STAT_NAMES: [&str; NUM_STATS] = ["MinValue", "MaxValue", "MeanValue", "Total", "Net"];

/// Generate time series channel names for ROIStat with the given number of ROIs.
/// Produces names like "TS1:MinValue", "TS1:MaxValue", ..., "TS2:MinValue", etc.
pub fn roi_stat_ts_channel_names(num_rois: usize) -> Vec<String> {
    let mut names = Vec::with_capacity(num_rois * NUM_STATS);
    for roi_idx in 0..num_rois {
        for stat_name in &ROI_STAT_NAMES {
            names.push(format!("TS{}:{}", roi_idx + 1, stat_name));
        }
    }
    names
}

/// Parameter indices for NDROIStat plugin-specific params.
///
/// Per-ROI params use a single index and are differentiated by asyn addr (0..N).
#[derive(Clone, Copy, Default)]
pub struct ROIStatParams {
    // Global (addr 0)
    pub reset_all: usize,
    pub ts_control: usize,
    pub ts_num_points: usize,
    pub ts_current_point: usize,
    pub ts_acquiring: usize,
    // Per-ROI (same index, different addr)
    pub use_: usize,
    pub name: usize,
    pub reset: usize,
    pub bgd_width: usize,
    pub dim0_min: usize,
    pub dim1_min: usize,
    pub dim0_size: usize,
    pub dim1_size: usize,
    pub dim0_max_size: usize,
    pub dim1_max_size: usize,
    pub min_value: usize,
    pub max_value: usize,
    pub mean_value: usize,
    pub total: usize,
    pub net: usize,
}

/// Processor that computes ROI statistics on 2D arrays.
pub struct ROIStatProcessor {
    rois: Vec<ROIStatROI>,
    results: Vec<ROIStatResult>,
    /// Time series buffers: [roi_index][stat_index][time_point].
    ts_mode: TSMode,
    ts_buffers: Vec<Vec<Vec<f64>>>,
    ts_num_points: usize,
    ts_current: usize,
    /// Optional sender to push flattened stats to a TimeSeriesPortDriver.
    ts_sender: Option<TimeSeriesSender>,
    /// Registered asyn param indices.
    params: ROIStatParams,
    /// Shared cell to export params after register_params is called.
    params_out: Arc<Mutex<ROIStatParams>>,
}

impl ROIStatProcessor {
    /// Create a new processor with the given ROI definitions.
    pub fn new(rois: Vec<ROIStatROI>, ts_num_points: usize) -> Self {
        let n = rois.len();
        let results = vec![ROIStatResult::default(); n];
        let ts_buffers = vec![vec![Vec::new(); NUM_STATS]; n];
        Self {
            rois,
            results,
            ts_mode: TSMode::Idle,
            ts_buffers,
            ts_num_points,
            ts_current: 0,
            ts_sender: None,
            params: ROIStatParams::default(),
            params_out: Arc::new(Mutex::new(ROIStatParams::default())),
        }
    }

    /// Get a shared handle to the params (populated after register_params is called).
    pub fn params_handle(&self) -> Arc<Mutex<ROIStatParams>> {
        self.params_out.clone()
    }

    /// Get the current results for all ROIs.
    pub fn results(&self) -> &[ROIStatResult] {
        &self.results
    }

    /// Get the ROI definitions.
    pub fn rois(&self) -> &[ROIStatROI] {
        &self.rois
    }

    /// Mutable access to ROI definitions.
    pub fn rois_mut(&mut self) -> &mut Vec<ROIStatROI> {
        &mut self.rois
    }

    /// Set the time series mode.
    pub fn set_ts_mode(&mut self, mode: TSMode) {
        if mode == TSMode::Acquiring && self.ts_mode != TSMode::Acquiring {
            // Reset time series on start
            for roi_bufs in &mut self.ts_buffers {
                for stat_buf in roi_bufs.iter_mut() {
                    stat_buf.clear();
                }
            }
            self.ts_current = 0;
        }
        self.ts_mode = mode;
    }

    /// Get time series buffer for a specific ROI and stat index.
    /// stat_index: 0=min, 1=max, 2=mean, 3=total, 4=net
    pub fn ts_buffer(&self, roi_index: usize, stat_index: usize) -> &[f64] {
        if roi_index < self.ts_buffers.len() && stat_index < NUM_STATS {
            &self.ts_buffers[roi_index][stat_index]
        } else {
            &[]
        }
    }

    /// Set the sender for pushing time series data to a TimeSeriesPortDriver.
    pub fn set_ts_sender(&mut self, sender: TimeSeriesSender) {
        self.ts_sender = Some(sender);
    }

    /// Compute statistics for a single ROI on a 2D data buffer.
    pub fn compute_roi_stats(
        data: &NDDataBuffer,
        x_size: usize,
        y_size: usize,
        roi: &ROIStatROI,
    ) -> ROIStatResult {
        let roi_x = roi.offset[0];
        let roi_y = roi.offset[1];
        let roi_w = roi.size[0];
        let roi_h = roi.size[1];

        // Clamp ROI to image bounds
        if roi_x >= x_size || roi_y >= y_size || roi_w == 0 || roi_h == 0 {
            return ROIStatResult::default();
        }
        let roi_w = roi_w.min(x_size - roi_x);
        let roi_h = roi_h.min(y_size - roi_y);

        let mut min = f64::MAX;
        let mut max = f64::MIN;
        let mut total = 0.0f64;
        let mut count = 0usize;

        for iy in roi_y..(roi_y + roi_h) {
            for ix in roi_x..(roi_x + roi_w) {
                let idx = iy * x_size + ix;
                if let Some(val) = data.get_as_f64(idx) {
                    if val < min {
                        min = val;
                    }
                    if val > max {
                        max = val;
                    }
                    total += val;
                    count += 1;
                }
            }
        }

        if count == 0 {
            return ROIStatResult::default();
        }

        let mean = total / count as f64;

        // Background subtraction
        let net = if roi.bgd_width > 0 {
            let bgd = Self::compute_background(data, x_size, y_size, roi);
            total - bgd * count as f64
        } else {
            total
        };

        ROIStatResult {
            min,
            max,
            mean,
            total,
            net,
        }
    }

    /// Compute average background from the border of the ROI.
    fn compute_background(
        data: &NDDataBuffer,
        x_size: usize,
        y_size: usize,
        roi: &ROIStatROI,
    ) -> f64 {
        let roi_x = roi.offset[0];
        let roi_y = roi.offset[1];
        let roi_w = roi.size[0].min(x_size.saturating_sub(roi_x));
        let roi_h = roi.size[1].min(y_size.saturating_sub(roi_y));
        let bw = roi.bgd_width;

        if bw == 0 || roi_w == 0 || roi_h == 0 {
            return 0.0;
        }

        let mut bgd_total = 0.0f64;
        let mut bgd_count = 0usize;

        for iy in roi_y..(roi_y + roi_h) {
            for ix in roi_x..(roi_x + roi_w) {
                // Check if this pixel is in the border region
                let dx_from_left = ix - roi_x;
                let dx_from_right = (roi_x + roi_w - 1) - ix;
                let dy_from_top = iy - roi_y;
                let dy_from_bottom = (roi_y + roi_h - 1) - iy;

                let in_border = dx_from_left < bw
                    || dx_from_right < bw
                    || dy_from_top < bw
                    || dy_from_bottom < bw;

                if in_border {
                    let idx = iy * x_size + ix;
                    if let Some(val) = data.get_as_f64(idx) {
                        bgd_total += val;
                        bgd_count += 1;
                    }
                }
            }
        }

        if bgd_count == 0 {
            0.0
        } else {
            bgd_total / bgd_count as f64
        }
    }
}

impl NDPluginProcess for ROIStatProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let info = array.info();
        let x_size = info.x_size;
        let y_size = info.y_size;

        // Ensure results vec matches rois
        self.results
            .resize(self.rois.len(), ROIStatResult::default());

        #[cfg(feature = "parallel")]
        {
            let total_elements: usize = self
                .rois
                .iter()
                .filter(|r| r.enabled)
                .map(|r| r.size[0] * r.size[1])
                .sum();

            if par_util::should_parallelize(total_elements) {
                let data = &array.data;
                let rois = &self.rois;
                let new_results: Vec<ROIStatResult> = par_util::thread_pool().install(|| {
                    rois.par_iter()
                        .map(|roi| {
                            if roi.enabled {
                                Self::compute_roi_stats(data, x_size, y_size, roi)
                            } else {
                                ROIStatResult::default()
                            }
                        })
                        .collect()
                });
                self.results = new_results;
            } else {
                for (i, roi) in self.rois.iter().enumerate() {
                    if !roi.enabled {
                        self.results[i] = ROIStatResult::default();
                        continue;
                    }
                    self.results[i] = Self::compute_roi_stats(&array.data, x_size, y_size, roi);
                }
            }
        }

        #[cfg(not(feature = "parallel"))]
        for (i, roi) in self.rois.iter().enumerate() {
            if !roi.enabled {
                self.results[i] = ROIStatResult::default();
                continue;
            }
            self.results[i] = Self::compute_roi_stats(&array.data, x_size, y_size, roi);
        }

        // Accumulate time series
        if self.ts_mode == TSMode::Acquiring {
            // Ensure ts_buffers match roi count
            while self.ts_buffers.len() < self.rois.len() {
                self.ts_buffers.push(vec![Vec::new(); NUM_STATS]);
            }

            for (i, result) in self.results.iter().enumerate() {
                if i >= self.ts_buffers.len() {
                    break;
                }
                let stats = [
                    result.min,
                    result.max,
                    result.mean,
                    result.total,
                    result.net,
                ];
                for (s, &val) in stats.iter().enumerate() {
                    let buf = &mut self.ts_buffers[i][s];
                    if buf.len() >= self.ts_num_points && self.ts_num_points > 0 {
                        // Circular: overwrite oldest
                        let idx = self.ts_current % self.ts_num_points;
                        if idx < buf.len() {
                            buf[idx] = val;
                        }
                    } else {
                        buf.push(val);
                    }
                }
            }
            self.ts_current += 1;
        }

        // Send flattened stats to TimeSeriesPortDriver if connected
        if let Some(ref sender) = self.ts_sender {
            let mut values = Vec::with_capacity(self.results.len() * NUM_STATS);
            for result in &self.results {
                values.push(result.min);
                values.push(result.max);
                values.push(result.mean);
                values.push(result.total);
                values.push(result.net);
            }
            let _ = sender.try_send(TimeSeriesData { values });
        }

        // Build per-ROI param updates
        let p = &self.params;
        let mut updates = Vec::new();
        for (i, result) in self.results.iter().enumerate() {
            let addr = i as i32;
            updates.push(ParamUpdate::float64_addr(p.min_value, addr, result.min));
            updates.push(ParamUpdate::float64_addr(p.max_value, addr, result.max));
            updates.push(ParamUpdate::float64_addr(p.mean_value, addr, result.mean));
            updates.push(ParamUpdate::float64_addr(p.total, addr, result.total));
            updates.push(ParamUpdate::float64_addr(p.net, addr, result.net));
            updates.push(ParamUpdate::int32_addr(
                p.dim0_max_size,
                addr,
                x_size as i32,
            ));
            updates.push(ParamUpdate::int32_addr(
                p.dim1_max_size,
                addr,
                y_size as i32,
            ));
        }
        updates.push(ParamUpdate::int32(
            p.ts_current_point,
            self.ts_current as i32,
        ));
        updates.push(ParamUpdate::int32(
            p.ts_acquiring,
            if self.ts_mode == TSMode::Acquiring {
                1
            } else {
                0
            },
        ));

        ProcessResult::sink(updates)
    }

    fn plugin_type(&self) -> &str {
        "NDPluginROIStat"
    }

    fn register_params(
        &mut self,
        base: &mut PortDriverBase,
    ) -> Result<(), asyn_rs::error::AsynError> {
        // Global params
        self.params.reset_all = base.create_param("ROISTAT_RESETALL", ParamType::Int32)?;
        self.params.ts_control = base.create_param("ROISTAT_TS_CONTROL", ParamType::Int32)?;
        self.params.ts_num_points = base.create_param("ROISTAT_TS_NUM_POINTS", ParamType::Int32)?;
        base.set_int32_param(self.params.ts_num_points, 0, self.ts_num_points as i32)?;
        self.params.ts_current_point =
            base.create_param("ROISTAT_TS_CURRENT_POINT", ParamType::Int32)?;
        self.params.ts_acquiring = base.create_param("ROISTAT_TS_ACQUIRING", ParamType::Int32)?;

        // Per-ROI params (single index, differentiated by addr)
        self.params.use_ = base.create_param("ROISTAT_USE", ParamType::Int32)?;
        self.params.name = base.create_param("ROISTAT_NAME", ParamType::Octet)?;
        self.params.reset = base.create_param("ROISTAT_RESET", ParamType::Int32)?;
        self.params.bgd_width = base.create_param("ROISTAT_BGD_WIDTH", ParamType::Int32)?;
        self.params.dim0_min = base.create_param("ROISTAT_DIM0_MIN", ParamType::Int32)?;
        self.params.dim1_min = base.create_param("ROISTAT_DIM1_MIN", ParamType::Int32)?;
        self.params.dim0_size = base.create_param("ROISTAT_DIM0_SIZE", ParamType::Int32)?;
        self.params.dim1_size = base.create_param("ROISTAT_DIM1_SIZE", ParamType::Int32)?;
        self.params.dim0_max_size = base.create_param("ROISTAT_DIM0_MAX_SIZE", ParamType::Int32)?;
        self.params.dim1_max_size = base.create_param("ROISTAT_DIM1_MAX_SIZE", ParamType::Int32)?;
        self.params.min_value = base.create_param("ROISTAT_MIN_VALUE", ParamType::Float64)?;
        self.params.max_value = base.create_param("ROISTAT_MAX_VALUE", ParamType::Float64)?;
        self.params.mean_value = base.create_param("ROISTAT_MEAN_VALUE", ParamType::Float64)?;
        self.params.total = base.create_param("ROISTAT_TOTAL", ParamType::Float64)?;
        self.params.net = base.create_param("ROISTAT_NET", ParamType::Float64)?;

        // Set initial per-ROI values
        for (i, roi) in self.rois.iter().enumerate() {
            let addr = i as i32;
            base.set_int32_param(self.params.use_, addr, roi.enabled as i32)?;
            base.set_int32_param(self.params.bgd_width, addr, roi.bgd_width as i32)?;
            base.set_int32_param(self.params.dim0_min, addr, roi.offset[0] as i32)?;
            base.set_int32_param(self.params.dim1_min, addr, roi.offset[1] as i32)?;
            base.set_int32_param(self.params.dim0_size, addr, roi.size[0] as i32)?;
            base.set_int32_param(self.params.dim1_size, addr, roi.size[1] as i32)?;
        }

        // Export params
        *self.params_out.lock() = self.params;

        Ok(())
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        snapshot: &PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        let addr = snapshot.addr as usize;
        let p = &self.params;

        if reason == p.use_ && addr < self.rois.len() {
            self.rois[addr].enabled = snapshot.value.as_i32() != 0;
        } else if reason == p.dim0_min && addr < self.rois.len() {
            self.rois[addr].offset[0] = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.dim1_min && addr < self.rois.len() {
            self.rois[addr].offset[1] = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.dim0_size && addr < self.rois.len() {
            self.rois[addr].size[0] = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.dim1_size && addr < self.rois.len() {
            self.rois[addr].size[1] = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.bgd_width && addr < self.rois.len() {
            self.rois[addr].bgd_width = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.reset && addr < self.rois.len() {
            self.results[addr] = ROIStatResult::default();
        } else if reason == p.reset_all {
            for r in &mut self.results {
                *r = ROIStatResult::default();
            }
        } else if reason == p.ts_control {
            let mode = if snapshot.value.as_i32() != 0 {
                TSMode::Acquiring
            } else {
                TSMode::Idle
            };
            self.set_ts_mode(mode);
        } else if reason == p.ts_num_points {
            self.ts_num_points = snapshot.value.as_i32().max(0) as usize;
        }
        ad_core_rs::plugin::runtime::ParamChangeResult::empty()
    }
}

/// Create a ROIStat plugin runtime with multi-addr support.
///
/// Returns:
/// - Plugin runtime handle
/// - ROIStatParams (for building registry)
/// - Thread join handle
pub fn create_roi_stat_runtime(
    port_name: &str,
    pool: Arc<NDArrayPool>,
    queue_size: usize,
    ndarray_port: &str,
    wiring: Arc<WiringRegistry>,
    num_rois: usize,
) -> (
    PluginRuntimeHandle,
    ROIStatParams,
    std::thread::JoinHandle<()>,
) {
    let rois: Vec<ROIStatROI> = (0..num_rois).map(|_| ROIStatROI::default()).collect();
    let processor = ROIStatProcessor::new(rois, 2048);
    let params_handle = processor.params_handle();

    let (handle, jh) = ad_core_rs::plugin::runtime::create_plugin_runtime_multi_addr(
        port_name,
        processor,
        pool,
        queue_size,
        ndarray_port,
        wiring,
        num_rois,
    );

    // Params were populated by register_params and exported via the shared handle.
    let roi_stat_params = *params_handle.lock();

    (handle, roi_stat_params, jh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    fn make_2d_array(x: usize, y: usize, fill: impl Fn(usize, usize) -> f64) -> NDArray {
        let mut arr = NDArray::new(
            vec![NDDimension::new(x), NDDimension::new(y)],
            NDDataType::Float64,
        );
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for iy in 0..y {
                for ix in 0..x {
                    v[iy * x + ix] = fill(ix, iy);
                }
            }
        }
        arr
    }

    #[test]
    fn test_single_roi_full_image() {
        let arr = make_2d_array(4, 4, |_x, _y| 10.0);
        let rois = vec![ROIStatROI {
            enabled: true,
            offset: [0, 0],
            size: [4, 4],
            bgd_width: 0,
        }];

        let mut proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&arr, &pool);

        let r = &proc.results()[0];
        assert!((r.min - 10.0).abs() < 1e-10);
        assert!((r.max - 10.0).abs() < 1e-10);
        assert!((r.mean - 10.0).abs() < 1e-10);
        assert!((r.total - 160.0).abs() < 1e-10);
    }

    #[test]
    fn test_single_roi_subregion() {
        // 8x8 image, values = x + y * 8
        let arr = make_2d_array(8, 8, |x, y| (x + y * 8) as f64);

        let rois = vec![ROIStatROI {
            enabled: true,
            offset: [2, 2],
            size: [3, 3],
            bgd_width: 0,
        }];

        let mut proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&arr, &pool);

        let r = &proc.results()[0];
        // ROI pixels: (2,2)=18, (3,2)=19, (4,2)=20, (2,3)=26, (3,3)=27, (4,3)=28, (2,4)=34, (3,4)=35, (4,4)=36
        assert!((r.min - 18.0).abs() < 1e-10);
        assert!((r.max - 36.0).abs() < 1e-10);
        let expected_total = 18.0 + 19.0 + 20.0 + 26.0 + 27.0 + 28.0 + 34.0 + 35.0 + 36.0;
        assert!((r.total - expected_total).abs() < 1e-10);
        assert!((r.mean - expected_total / 9.0).abs() < 1e-10);
    }

    #[test]
    fn test_multiple_rois() {
        let arr = make_2d_array(8, 8, |x, _y| x as f64);

        let rois = vec![
            ROIStatROI {
                enabled: true,
                offset: [0, 0],
                size: [4, 4],
                bgd_width: 0,
            },
            ROIStatROI {
                enabled: true,
                offset: [4, 0],
                size: [4, 4],
                bgd_width: 0,
            },
        ];

        let mut proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&arr, &pool);

        let r0 = &proc.results()[0];
        assert!((r0.min - 0.0).abs() < 1e-10);
        assert!((r0.max - 3.0).abs() < 1e-10);

        let r1 = &proc.results()[1];
        assert!((r1.min - 4.0).abs() < 1e-10);
        assert!((r1.max - 7.0).abs() < 1e-10);
    }

    #[test]
    fn test_bgd_width() {
        // 6x6 image, center 2x2 has value 100, border has value 10
        let arr = make_2d_array(6, 6, |x, y| {
            if x >= 2 && x < 4 && y >= 2 && y < 4 {
                100.0
            } else {
                10.0
            }
        });

        let rois = vec![ROIStatROI {
            enabled: true,
            offset: [1, 1],
            size: [4, 4],
            bgd_width: 1,
        }];

        let mut proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&arr, &pool);

        let r = &proc.results()[0];
        // ROI is 4x4 at (1,1): border pixels = 12 (all with value 10), center = 4 (value 100)
        // bgd average = (12*10 + ... well, border includes some 100s)
        // Actually border pixels at bgd_width=1: the outer ring of the 4x4 ROI
        // That outer ring occupies 12 of 16 pixels
        assert!(
            r.net < r.total,
            "net should be less than total with bgd subtraction"
        );
    }

    #[test]
    fn test_empty_roi() {
        let arr = make_2d_array(4, 4, |_, _| 10.0);
        let rois = vec![ROIStatROI {
            enabled: true,
            offset: [0, 0],
            size: [0, 0],
            bgd_width: 0,
        }];

        let mut proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&arr, &pool);

        let r = &proc.results()[0];
        assert!((r.total - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_disabled_roi() {
        let arr = make_2d_array(4, 4, |_, _| 10.0);
        let rois = vec![ROIStatROI {
            enabled: false,
            offset: [0, 0],
            size: [4, 4],
            bgd_width: 0,
        }];

        let mut proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&arr, &pool);

        let r = &proc.results()[0];
        assert!(
            (r.total - 0.0).abs() < 1e-10,
            "disabled ROI should have zero stats"
        );
    }

    #[test]
    fn test_roi_out_of_bounds() {
        let arr = make_2d_array(4, 4, |_, _| 10.0);
        let rois = vec![ROIStatROI {
            enabled: true,
            offset: [10, 10],
            size: [4, 4],
            bgd_width: 0,
        }];

        let mut proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&arr, &pool);

        let r = &proc.results()[0];
        assert!(
            (r.total - 0.0).abs() < 1e-10,
            "out-of-bounds ROI should produce zero stats"
        );
    }

    #[test]
    fn test_roi_partially_out_of_bounds() {
        let arr = make_2d_array(4, 4, |_, _| 5.0);
        let rois = vec![ROIStatROI {
            enabled: true,
            offset: [2, 2],
            size: [10, 10], // extends beyond image
            bgd_width: 0,
        }];

        let mut proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&arr, &pool);

        let r = &proc.results()[0];
        // Should be clamped to 2x2 region
        assert!((r.total - 20.0).abs() < 1e-10);
        assert!((r.mean - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_time_series() {
        let rois = vec![ROIStatROI {
            enabled: true,
            offset: [0, 0],
            size: [4, 4],
            bgd_width: 0,
        }];

        let mut proc = ROIStatProcessor::new(rois, 100);
        let pool = NDArrayPool::new(1_000_000);
        proc.set_ts_mode(TSMode::Acquiring);

        for i in 0..5 {
            let arr = make_2d_array(4, 4, |_, _| (i + 1) as f64);
            proc.process_array(&arr, &pool);
        }

        // Check mean time series (stat index 2)
        let ts = proc.ts_buffer(0, 2);
        assert_eq!(ts.len(), 5);
        assert!((ts[0] - 1.0).abs() < 1e-10);
        assert!((ts[4] - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_u8_data() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for (i, val) in v.iter_mut().enumerate() {
                *val = (i + 1) as u8;
            }
        }

        let rois = vec![ROIStatROI {
            enabled: true,
            offset: [0, 0],
            size: [4, 4],
            bgd_width: 0,
        }];

        let mut proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&arr, &pool);

        let r = &proc.results()[0];
        assert!((r.min - 1.0).abs() < 1e-10);
        assert!((r.max - 16.0).abs() < 1e-10);
    }

    #[test]
    fn test_ts_channel_names() {
        let names = roi_stat_ts_channel_names(2);
        assert_eq!(names.len(), 10); // 2 ROIs * 5 stats
        assert_eq!(names[0], "TS1:MinValue");
        assert_eq!(names[1], "TS1:MaxValue");
        assert_eq!(names[4], "TS1:Net");
        assert_eq!(names[5], "TS2:MinValue");
        assert_eq!(names[9], "TS2:Net");
    }

    #[test]
    fn test_ts_sender_integration() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TimeSeriesData>(16);

        let rois = vec![
            ROIStatROI {
                enabled: true,
                offset: [0, 0],
                size: [4, 4],
                bgd_width: 0,
            },
            ROIStatROI {
                enabled: true,
                offset: [0, 0],
                size: [2, 2],
                bgd_width: 0,
            },
        ];

        let mut proc = ROIStatProcessor::new(rois, 0);
        proc.set_ts_sender(tx);

        let pool = NDArrayPool::new(1_000_000);
        let arr = make_2d_array(4, 4, |_, _| 7.0);
        proc.process_array(&arr, &pool);

        let data = rx.try_recv().unwrap();
        // 2 ROIs * 5 stats = 10 values
        assert_eq!(data.values.len(), 10);
        // ROI1: min=7, max=7, mean=7, total=112 (4*4*7), net=112
        assert!((data.values[0] - 7.0).abs() < 1e-10); // min
        assert!((data.values[1] - 7.0).abs() < 1e-10); // max
        assert!((data.values[2] - 7.0).abs() < 1e-10); // mean
        assert!((data.values[3] - 112.0).abs() < 1e-10); // total
        // ROI2: 2x2 region, total=28 (2*2*7)
        assert!((data.values[8] - 28.0).abs() < 1e-10); // total
    }
}
