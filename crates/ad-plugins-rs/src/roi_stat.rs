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
    // Time series waveform arrays (per-ROI, differentiated by addr)
    pub ts_total: usize,
    pub ts_net: usize,
    pub ts_mean_value: usize,
    pub ts_min_value: usize,
    pub ts_max_value: usize,
    pub ts_timestamp: usize,
}

/// Processor that computes ROI statistics on 2D arrays.
pub struct ROIStatProcessor {
    rois: Vec<ROIStatROI>,
    results: Vec<ROIStatResult>,
    /// Time series buffers: `[roi_index][stat_index][time_point]`.
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

    /// Clamp a ROI's geometry to the array dimensions, mirroring the C
    /// NDPluginROIStat clamp loop (NDPluginROIStat.cpp:241-247): for each
    /// dimension present in the array, `offset ∈ [0, dim-1]` and
    /// `size ∈ [1, dim-offset]`. Offsets are already non-negative (the
    /// param-change handler clamps at 0), so only the upper bounds apply.
    /// `dims` holds the array's [X, Y] sizes; `ndims` selects how many of
    /// them are real (1 or 2). A clamped size is always ≥ 1, so an
    /// out-of-range or zero-size ROI collapses to a single edge pixel
    /// rather than yielding zero stats. Returns `None` only for a
    /// degenerate zero-length dimension (empty array).
    fn clamp_roi_geometry(
        roi: &ROIStatROI,
        ndims: usize,
        dims: [usize; 2],
    ) -> Option<([usize; 2], [usize; 2])> {
        let mut offset = roi.offset;
        let mut size = roi.size;
        for d in 0..ndims.min(2) {
            let dim = dims[d];
            if dim == 0 {
                return None;
            }
            offset[d] = offset[d].min(dim - 1);
            size[d] = size[d].max(1).min(dim - offset[d]);
        }
        Some((offset, size))
    }

    /// Compute statistics for one already-clamped ROI, mirroring the C
    /// `doComputeStatistics` (NDPluginROIStat.cpp:30-139). `array_size_x`
    /// is the array's X dimension (the row stride, = C `arraySize[0]`).
    /// `ndims` selects the 1-D (single X strip) or 2-D (rectangle) layout.
    /// Background pixels are summed exactly as C does: for 1-D the two
    /// X-end strips, for 2-D the four-edge border. Both can double-count in
    /// the degenerate thick-border case (`2*bgd_width > size`), matching C.
    pub fn compute_roi_stats(
        data: &NDDataBuffer,
        ndims: usize,
        array_size_x: usize,
        offset: [usize; 2],
        size: [usize; 2],
        bgd_width: usize,
    ) -> ROIStatResult {
        let offset_x = offset[0];
        let size_x = size[0];

        let mut min = f64::MAX;
        let mut max = f64::MIN;
        let mut total = 0.0f64;
        let mut bgd = 0.0f64;
        let mut n_bgd = 0usize;
        let n_elements;

        if ndims == 1 {
            if size_x == 0 {
                return ROIStatResult::default();
            }
            n_elements = size_x;
            for x in offset_x..offset_x + size_x {
                let v = data.get_as_f64(x).unwrap_or(0.0);
                min = min.min(v);
                max = max.max(v);
                total += v;
            }
            if bgd_width > 0 {
                let bw_x = bgd_width.min(size_x);
                for x in offset_x..offset_x + bw_x {
                    n_bgd += 1;
                    bgd += data.get_as_f64(x).unwrap_or(0.0);
                }
                for x in (offset_x + size_x - bw_x)..(offset_x + size_x) {
                    n_bgd += 1;
                    bgd += data.get_as_f64(x).unwrap_or(0.0);
                }
            }
        } else if ndims == 2 {
            let offset_y = offset[1];
            let size_y = size[1];
            if size_x == 0 || size_y == 0 {
                return ROIStatResult::default();
            }
            n_elements = size_x * size_y;
            for y in offset_y..offset_y + size_y {
                let row = y * array_size_x;
                for x in offset_x..offset_x + size_x {
                    let v = data.get_as_f64(row + x).unwrap_or(0.0);
                    min = min.min(v);
                    max = max.max(v);
                    total += v;
                }
            }
            if bgd_width > 0 {
                let bw_x = bgd_width.min(size_x);
                let bw_y = bgd_width.min(size_y);
                // Top and bottom bw_y rows (full ROI width).
                for y in offset_y..offset_y + bw_y {
                    let row = y * array_size_x;
                    for x in offset_x..offset_x + size_x {
                        n_bgd += 1;
                        bgd += data.get_as_f64(row + x).unwrap_or(0.0);
                    }
                }
                for y in (offset_y + size_y - bw_y)..(offset_y + size_y) {
                    let row = y * array_size_x;
                    for x in offset_x..offset_x + size_x {
                        n_bgd += 1;
                        bgd += data.get_as_f64(row + x).unwrap_or(0.0);
                    }
                }
                // Left and right bw_x columns of the middle rows.
                for y in (offset_y + bw_y)..(offset_y + size_y - bw_y) {
                    let row = y * array_size_x;
                    for x in offset_x..offset_x + bw_x {
                        n_bgd += 1;
                        bgd += data.get_as_f64(row + x).unwrap_or(0.0);
                    }
                    for x in (offset_x + size_x - bw_x)..(offset_x + size_x) {
                        n_bgd += 1;
                        bgd += data.get_as_f64(row + x).unwrap_or(0.0);
                    }
                }
            }
        } else {
            return ROIStatResult::default();
        }

        if n_elements == 0 {
            return ROIStatResult::default();
        }

        // C (NDPluginROIStat.cpp:128-135):
        //   if (nBgd > 0) bgd = bgd/nBgd * nElements;
        //   net  = total - bgd;          (bgd stays 0 when bgdWidth == 0)
        //   mean = total / nElements;
        let bgd_scaled = if n_bgd > 0 {
            bgd / n_bgd as f64 * n_elements as f64
        } else {
            0.0
        };
        ROIStatResult {
            min,
            max,
            mean: total / n_elements as f64,
            total,
            net: total - bgd_scaled,
        }
    }
}

impl NDPluginProcess for ROIStatProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        // NDPluginROIStat operates on the raw array dimensions like the C
        // plugin (NDPluginROIStat.cpp): dims[0] = X, dims[1] = Y. Only 1-D
        // or 2-D arrays are supported; C errors and yields zero stats for
        // any other rank.
        let ndims = array.dims.len();
        let dims = [
            array.dims.first().map(|d| d.size).unwrap_or(0),
            array.dims.get(1).map(|d| d.size).unwrap_or(0),
        ];
        let array_size_x = dims[0];
        let supported = ndims == 1 || ndims == 2;

        // Ensure results vec matches rois
        self.results
            .resize(self.rois.len(), ROIStatResult::default());

        // Clamp each enabled ROI's geometry to the array bounds (C clamp
        // loop). `None` for disabled ROIs or unsupported ranks — those keep
        // zero stats and skip the geometry write-back.
        let clamped: Vec<Option<([usize; 2], [usize; 2])>> = self
            .rois
            .iter()
            .map(|roi| {
                if roi.enabled && supported {
                    Self::clamp_roi_geometry(roi, ndims, dims)
                } else {
                    None
                }
            })
            .collect();

        #[cfg(feature = "parallel")]
        {
            let total_elements: usize = clamped
                .iter()
                .flatten()
                .map(|(_, size)| {
                    if ndims == 1 {
                        size[0]
                    } else {
                        size[0] * size[1]
                    }
                })
                .sum();

            if par_util::should_parallelize(total_elements) {
                let data = &array.data;
                let rois = &self.rois;
                let new_results: Vec<ROIStatResult> = par_util::thread_pool().install(|| {
                    rois.par_iter()
                        .zip(clamped.par_iter())
                        .map(|(roi, clamp)| match clamp {
                            Some((offset, size)) => Self::compute_roi_stats(
                                data,
                                ndims,
                                array_size_x,
                                *offset,
                                *size,
                                roi.bgd_width,
                            ),
                            None => ROIStatResult::default(),
                        })
                        .collect()
                });
                self.results = new_results;
            } else {
                for (i, (roi, clamp)) in self.rois.iter().zip(clamped.iter()).enumerate() {
                    self.results[i] = match clamp {
                        Some((offset, size)) => Self::compute_roi_stats(
                            &array.data,
                            ndims,
                            array_size_x,
                            *offset,
                            *size,
                            roi.bgd_width,
                        ),
                        None => ROIStatResult::default(),
                    };
                }
            }
        }

        #[cfg(not(feature = "parallel"))]
        for (i, (roi, clamp)) in self.rois.iter().zip(clamped.iter()).enumerate() {
            self.results[i] = match clamp {
                Some((offset, size)) => Self::compute_roi_stats(
                    &array.data,
                    ndims,
                    array_size_x,
                    *offset,
                    *size,
                    roi.bgd_width,
                ),
                None => ROIStatResult::default(),
            };
        }

        // Accumulate time series (fixed-length: stop when full)
        if self.ts_mode == TSMode::Acquiring {
            if self.ts_num_points > 0 && self.ts_current >= self.ts_num_points {
                // Buffer full — stop acquiring
                self.ts_mode = TSMode::Idle;
            } else {
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
                        buf.push(val);
                    }
                }
                self.ts_current += 1;
            }
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

        // Build per-ROI param updates (only for enabled ROIs)
        let p = &self.params;
        let mut updates = Vec::new();
        for (i, roi) in self.rois.iter().enumerate() {
            if !roi.enabled {
                continue;
            }
            let result = &self.results[i];
            let addr = i as i32;
            updates.push(ParamUpdate::float64_addr(p.min_value, addr, result.min));
            updates.push(ParamUpdate::float64_addr(p.max_value, addr, result.max));
            updates.push(ParamUpdate::float64_addr(p.mean_value, addr, result.mean));
            updates.push(ParamUpdate::float64_addr(p.total, addr, result.total));
            updates.push(ParamUpdate::float64_addr(p.net, addr, result.net));

            // Write back array sizes and the clamped geometry, matching the
            // C clamp loop's setIntegerParam calls (NDPluginROIStat.cpp:250-261).
            // MaxSize is 0 for an absent dimension; Dim*Min/Size readbacks
            // reflect the clamped values for supported ranks.
            updates.push(ParamUpdate::int32_addr(
                p.dim0_max_size,
                addr,
                if ndims >= 1 { dims[0] as i32 } else { 0 },
            ));
            updates.push(ParamUpdate::int32_addr(
                p.dim1_max_size,
                addr,
                if ndims >= 2 { dims[1] as i32 } else { 0 },
            ));
            if let Some((offset, size)) = clamped[i] {
                if ndims >= 1 {
                    updates.push(ParamUpdate::int32_addr(p.dim0_min, addr, offset[0] as i32));
                    updates.push(ParamUpdate::int32_addr(p.dim0_size, addr, size[0] as i32));
                }
                if ndims >= 2 {
                    updates.push(ParamUpdate::int32_addr(p.dim1_min, addr, offset[1] as i32));
                    updates.push(ParamUpdate::int32_addr(p.dim1_size, addr, size[1] as i32));
                }
            }
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

        // Write time series buffers to params for waveform readback
        for (i, roi) in self.rois.iter().enumerate() {
            if !roi.enabled || i >= self.ts_buffers.len() {
                continue;
            }
            let addr = i as i32;
            let bufs = &self.ts_buffers[i];
            // stat order: min=0, max=1, mean=2, total=3, net=4
            if !bufs.is_empty() {
                updates.push(ParamUpdate::float64_array_addr(
                    p.ts_min_value,
                    addr,
                    bufs[0].clone(),
                ));
            }
            if bufs.len() > 1 {
                updates.push(ParamUpdate::float64_array_addr(
                    p.ts_max_value,
                    addr,
                    bufs[1].clone(),
                ));
            }
            if bufs.len() > 2 {
                updates.push(ParamUpdate::float64_array_addr(
                    p.ts_mean_value,
                    addr,
                    bufs[2].clone(),
                ));
            }
            if bufs.len() > 3 {
                updates.push(ParamUpdate::float64_array_addr(
                    p.ts_total,
                    addr,
                    bufs[3].clone(),
                ));
            }
            if bufs.len() > 4 {
                updates.push(ParamUpdate::float64_array_addr(
                    p.ts_net,
                    addr,
                    bufs[4].clone(),
                ));
            }
        }

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

        // Time series waveform arrays (per-ROI)
        self.params.ts_total = base.create_param("ROISTAT_TS_TOTAL", ParamType::Float64Array)?;
        self.params.ts_net = base.create_param("ROISTAT_TS_NET", ParamType::Float64Array)?;
        self.params.ts_mean_value =
            base.create_param("ROISTAT_TS_MEAN_VALUE", ParamType::Float64Array)?;
        self.params.ts_min_value =
            base.create_param("ROISTAT_TS_MIN_VALUE", ParamType::Float64Array)?;
        self.params.ts_max_value =
            base.create_param("ROISTAT_TS_MAX_VALUE", ParamType::Float64Array)?;
        self.params.ts_timestamp =
            base.create_param("ROISTAT_TS_TIMESTAMP", ParamType::Float64Array)?;

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
            // 0=EraseStart (clear+start), 1=Start (resume), 2=Stop, 3=Read, 4=Erase
            match snapshot.value.as_i32() {
                0 => {
                    // EraseStart: clear buffers then start
                    for roi_bufs in &mut self.ts_buffers {
                        for stat_buf in roi_bufs.iter_mut() {
                            stat_buf.clear();
                        }
                    }
                    self.ts_current = 0;
                    self.ts_mode = TSMode::Acquiring;
                }
                1 => {
                    // Start: resume without clearing
                    self.ts_mode = TSMode::Acquiring;
                }
                2 => {
                    // Stop
                    self.ts_mode = TSMode::Idle;
                }
                3 => {
                    // Read: callback without stopping (no-op here, param update triggers read)
                }
                4 => {
                    // Erase: clear buffers
                    for roi_bufs in &mut self.ts_buffers {
                        for stat_buf in roi_bufs.iter_mut() {
                            stat_buf.clear();
                        }
                    }
                    self.ts_current = 0;
                }
                _ => {}
            }
        } else if reason == p.ts_num_points {
            self.ts_num_points = snapshot.value.as_i32().max(0) as usize;
        }
        ad_core_rs::plugin::runtime::ParamChangeResult::empty()
    }
}

/// Create a ROIStat plugin runtime. The TS receiver is stored in the registry
/// for later pickup by `NDTimeSeriesConfigure`.
pub fn create_roi_stat_runtime(
    port_name: &str,
    pool: Arc<NDArrayPool>,
    queue_size: usize,
    ndarray_port: &str,
    wiring: Arc<WiringRegistry>,
    num_rois: usize,
    ts_registry: &crate::time_series::TsReceiverRegistry,
) -> (
    PluginRuntimeHandle,
    ROIStatParams,
    std::thread::JoinHandle<()>,
) {
    let (ts_tx, ts_rx) = tokio::sync::mpsc::channel(256);

    let rois: Vec<ROIStatROI> = (0..num_rois).map(|_| ROIStatROI::default()).collect();
    let mut processor = ROIStatProcessor::new(rois, 2048);
    processor.set_ts_sender(ts_tx);
    let params_handle = processor.params_handle();

    let (handle, data_jh) = ad_core_rs::plugin::runtime::create_plugin_runtime_multi_addr(
        port_name,
        processor,
        pool,
        queue_size,
        ndarray_port,
        wiring,
        num_rois,
    );

    let roi_stat_params = *params_handle.lock();

    // Store the TS receiver for NDTimeSeriesConfigure to pick up
    let channel_names = roi_stat_ts_channel_names(num_rois);
    ts_registry.store(port_name, ts_rx, channel_names);

    (handle, roi_stat_params, data_jh)
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

        // C clamps a zero-size ROI to a single pixel (size >= 1) at the
        // clamped offset (0,0), so stats reflect that one pixel, not zero.
        let r = &proc.results()[0];
        assert!((r.total - 10.0).abs() < 1e-10);
        assert!((r.mean - 10.0).abs() < 1e-10);
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

        // C clamps offset to dim-1 (3,3) and size to 1, so the ROI is the
        // single corner pixel — stats reflect it, not zero.
        let r = &proc.results()[0];
        assert!(
            (r.total - 10.0).abs() < 1e-10,
            "out-of-bounds ROI clamps to one edge pixel, not zero"
        );
        assert!((r.mean - 10.0).abs() < 1e-10);
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

    fn make_1d_array(n: usize, fill: impl Fn(usize) -> f64) -> NDArray {
        let mut arr = NDArray::new(vec![NDDimension::new(n)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for (i, slot) in v.iter_mut().enumerate() {
                *slot = fill(i);
            }
        }
        arr
    }

    #[test]
    fn test_adp16_clamp_out_of_range_offset_to_one_pixel() {
        // 4x4 array. offset (10,1) is out of range in X; size (4,9) overflows
        // Y. C clamps offset to [0,dim-1] and size to [1,dim-offset].
        let roi = ROIStatROI {
            enabled: true,
            offset: [10, 1],
            size: [4, 9],
            bgd_width: 0,
        };
        let (offset, size) = ROIStatProcessor::clamp_roi_geometry(&roi, 2, [4, 4]).unwrap();
        assert_eq!(offset, [3, 1]); // X: min(10,3); Y: min(1,3)
        assert_eq!(size, [1, 3]); // X: min(4,4-3)=1; Y: min(9,4-1)=3
    }

    #[test]
    fn test_adp16_clamp_zero_size_to_one_pixel() {
        let roi = ROIStatROI {
            enabled: true,
            offset: [0, 0],
            size: [0, 0],
            bgd_width: 0,
        };
        let (offset, size) = ROIStatProcessor::clamp_roi_geometry(&roi, 2, [4, 4]).unwrap();
        assert_eq!(offset, [0, 0]);
        assert_eq!(size, [1, 1]); // zero size clamps up to 1 in each dim
    }

    #[test]
    fn test_adp16_geometry_writeback_uses_clamped_values() {
        use asyn_rs::port::{PortDriverBase, PortFlags};

        let arr = make_2d_array(4, 4, |_, _| 10.0);
        let rois = vec![ROIStatROI {
            enabled: true,
            offset: [10, 1],
            size: [4, 9],
            bgd_width: 0,
        }];
        let mut proc = ROIStatProcessor::new(rois, 0);
        let mut base = PortDriverBase::new("roistat_adp16", 1, PortFlags::default());
        proc.register_params(&mut base).unwrap();
        let p = *proc.params_handle().lock();

        let pool = NDArrayPool::new(1_000_000);
        let res = proc.process_array(&arr, &pool);

        let find = |reason: usize, addr: i32| {
            res.param_updates.iter().find_map(|u| match u {
                ParamUpdate::Int32 {
                    reason: r,
                    addr: a,
                    value,
                } if *r == reason && *a == addr => Some(*value),
                _ => None,
            })
        };
        // offset (10,1) -> (3,1); size (4,9) -> (1,3); MaxSize = dims (4,4).
        assert_eq!(find(p.dim0_min, 0), Some(3));
        assert_eq!(find(p.dim0_size, 0), Some(1));
        assert_eq!(find(p.dim1_min, 0), Some(1));
        assert_eq!(find(p.dim1_size, 0), Some(3));
        assert_eq!(find(p.dim0_max_size, 0), Some(4));
        assert_eq!(find(p.dim1_max_size, 0), Some(4));
    }

    #[test]
    fn test_adp17_1d_background_uses_x_strips_only() {
        // Genuine 1-D array [10,0,0,0,0,10]. With bgd_width=1 the C 1-D path
        // averages only the two X-end pixels (both 10): bgd = 10, scaled over
        // 6 elements = 60, so net = total - 60 = 20 - 60 = -40. A 2-D
        // border-ring would treat the single row as all-border (net = 0), and
        // the old early-return would zero every stat for a 1-D array.
        let arr = make_1d_array(6, |x| if x == 0 || x == 5 { 10.0 } else { 0.0 });
        let rois = vec![ROIStatROI {
            enabled: true,
            offset: [0, 0],
            size: [6, 0],
            bgd_width: 1,
        }];
        let mut proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&arr, &pool);

        let r = &proc.results()[0];
        assert!((r.total - 20.0).abs() < 1e-10, "total={}", r.total);
        assert!((r.min - 0.0).abs() < 1e-10);
        assert!((r.max - 10.0).abs() < 1e-10);
        assert!((r.net + 40.0).abs() < 1e-10, "net={}", r.net);
    }
}
