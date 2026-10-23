//! NDPluginROIStat: computes basic statistics for multiple ROI regions on each array.
//!
//! Each ROI is a rectangular sub-region of a 2D image. For each enabled ROI,
//! the plugin computes min, max, mean, total, and net (background-subtracted total).
//! Optionally accumulates time series data in circular buffers.

use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{
    ArraySource, NDPluginProcess, ParamUpdate, PluginParamSnapshot, PluginRuntimeHandle,
    ProcessResult,
};
use ad_core_rs::plugin::wiring::WiringRegistry;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::PortDriverBase;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};

#[cfg(feature = "parallel")]
use crate::par_util;
use crate::stats::{StatsElem, merge_range};
#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// Configuration for a single ROI region. The default is NDPluginROIStat.cpp's
/// starting ROI: Use 0 (`:557`) and zero geometry.
#[derive(Debug, Clone, Default)]
pub struct ROIStatROI {
    pub enabled: bool,
    /// Offset in pixels: [x, y].
    pub offset: [usize; 2],
    /// Size in pixels: [x, y].
    pub size: [usize; 2],
    /// Width of the background border (pixels). 0 = no background subtraction.
    pub bgd_width: usize,
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

/// Series kept per ROI: min, max, mean, total, net and the frame's
/// timestamp (C's MAX_TIME_SERIES_TYPES).
const TS_TYPES: usize = 6;

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
    /// The base `NDArraySizeX/Y/Z`; ROIStat sets X and Y from its input.
    pub array_size: [usize; 3],
}

/// Processor that computes ROI statistics on 2D arrays.
/// The ROI definitions, their latest stats and the time-series accumulator.
/// One frame recomputes the results and appends them to the buffers, so the
/// whole set moves under one lock.
struct ROIStatState {
    rois: Vec<ROIStatROI>,
    results: Vec<ROIStatResult>,
    /// C's TSAcquiring.
    ts_mode: TSMode,
    /// Time series buffers: `[roi_index][stat_index][time_point]`, each
    /// `ts_num_points` long as C's `timeSeries_`; `ts_current` are valid.
    ts_buffers: Vec<Vec<Vec<f64>>>,
    ts_num_points: usize,
    ts_current: usize,
}

impl ROIStatState {
    /// NDPluginROIStat::clearTimeSeries, less the CurrentPoint post.
    fn clear_time_series(&mut self) {
        self.ts_current = 0;
        self.size_time_series();
    }

    /// Every buffer zeroed at `ts_num_points`, as C's calloc.
    fn size_time_series(&mut self) {
        let n = self.ts_num_points;
        for buf in self.ts_buffers.iter_mut().flatten() {
            buf.clear();
            buf.resize(n, 0.0);
        }
    }

    /// NDPluginROIStat::doTimeSeriesCallbacks: every ROI, used or not.
    fn time_series_callbacks(&self, p: &ROIStatParams, updates: &mut Vec<ParamUpdate>) {
        let reasons = [
            p.ts_min_value,
            p.ts_max_value,
            p.ts_mean_value,
            p.ts_total,
            p.ts_net,
            p.ts_timestamp,
        ];
        for (roi, bufs) in self.ts_buffers.iter().enumerate() {
            for (&reason, buf) in reasons.iter().zip(bufs) {
                // C passes currentTSPoint_ even past a shrunk buffer.
                let points = buf[..self.ts_current.min(buf.len())].to_vec();
                updates.push(ParamUpdate::float64_array_addr(reason, roi as i32, points));
            }
        }
    }
}

pub struct ROIStatProcessor {
    state: Mutex<ROIStatState>,
    /// Registered asyn param indices.
    params: ROIStatParams,
    /// Shared cell to export params after register_params is called.
    params_out: Arc<Mutex<ROIStatParams>>,
}

impl ROIStatProcessor {
    /// Create a new processor of `rois.len()` ROIs, C's maxROIs: one per
    /// address (NDPluginROIStat.cpp:495), and at least one (:502-505). `rois`
    /// seeds their geometry.
    pub fn new(mut rois: Vec<ROIStatROI>, ts_num_points: usize) -> Self {
        if rois.is_empty() {
            rois.push(ROIStatROI::default());
        }
        let n = rois.len();
        let results = vec![ROIStatResult::default(); n];
        let ts_buffers = vec![vec![vec![0.0; ts_num_points]; TS_TYPES]; n];
        Self {
            state: Mutex::new(ROIStatState {
                rois,
                results,
                ts_mode: TSMode::Idle,
                ts_buffers,
                ts_num_points,
                ts_current: 0,
            }),
            params: ROIStatParams::default(),
            params_out: Arc::new(Mutex::new(ROIStatParams::default())),
        }
    }

    /// Get a shared handle to the params (populated after register_params is called).
    pub fn params_handle(&self) -> Arc<Mutex<ROIStatParams>> {
        self.params_out.clone()
    }

    /// Get the current results for all ROIs.
    pub fn results(&self) -> Vec<ROIStatResult> {
        self.state.lock().results.clone()
    }

    /// Get the ROI definitions.
    pub fn rois(&self) -> Vec<ROIStatROI> {
        self.state.lock().rois.clone()
    }

    /// Mutable access to ROI definitions; their count is the port's.
    pub fn rois_mut(&self) -> MappedMutexGuard<'_, [ROIStatROI]> {
        MutexGuard::map(self.state.lock(), |s| s.rois.as_mut_slice())
    }

    /// Set the time series mode.
    pub fn set_ts_mode(&self, mode: TSMode) {
        let mut state = self.state.lock();
        if mode == TSMode::Acquiring && state.ts_mode != TSMode::Acquiring {
            state.clear_time_series();
        }
        state.ts_mode = mode;
    }

    /// Get time series buffer for a specific ROI and stat index.
    /// stat_index: 0=min, 1=max, 2=mean, 3=total, 4=net, 5=timestamp
    pub fn ts_buffer(&self, roi_index: usize, stat_index: usize) -> Vec<f64> {
        let state = self.state.lock();
        if roi_index < state.ts_buffers.len() && stat_index < TS_TYPES {
            let buf = &state.ts_buffers[roi_index][stat_index];
            buf[..state.ts_current.min(buf.len())].to_vec()
        } else {
            Vec::new()
        }
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
    /// `doComputeStatistics` (NDPluginROIStat.cpp:33-139). `array_size_x`
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
        ad_core_rs::with_buffer!(data, |v| roi_stats_of(
            v,
            ndims,
            array_size_x,
            offset,
            size,
            bgd_width
        ))
    }
}

/// One ROI reduced over the typed frame. Each row of the rectangle goes
/// through [`StatsElem::range`], the kernel `compute_stats` runs, so the
/// extremes are taken in the element type under C's strict compare and the
/// total in its exact accumulator; a background strip only needs the total.
/// A rectangle that does not fit the buffer yields the zero result.
fn roi_stats_of<T: StatsElem>(
    v: &[T],
    ndims: usize,
    array_size_x: usize,
    offset: [usize; 2],
    size: [usize; 2],
    bgd_width: usize,
) -> ROIStatResult {
    let (offset_x, size_x) = (offset[0], size[0]);
    let mut bgd = 0.0f64;
    let mut n_bgd = 0usize;
    let n_elements;

    let range = if ndims == 1 {
        if size_x == 0 || offset_x + size_x > v.len() {
            return ROIStatResult::default();
        }
        n_elements = size_x;
        let strip = &v[offset_x..offset_x + size_x];
        if bgd_width > 0 {
            let bw_x = bgd_width.min(size_x);
            n_bgd += 2 * bw_x;
            bgd += T::range(&strip[..bw_x]).total;
            bgd += T::range(&strip[size_x - bw_x..]).total;
        }
        T::range(strip)
    } else if ndims == 2 {
        let (offset_y, size_y) = (offset[1], size[1]);
        if size_x == 0 || size_y == 0 {
            return ROIStatResult::default();
        }
        let row = |y: usize| -> Option<&[T]> {
            let start = y * array_size_x + offset_x;
            v.get(start..start + size_x)
        };
        let Some(rows) = (offset_y..offset_y + size_y)
            .map(row)
            .collect::<Option<Vec<_>>>()
        else {
            return ROIStatResult::default();
        };
        n_elements = size_x * size_y;
        if bgd_width > 0 {
            let bw_x = bgd_width.min(size_x);
            let bw_y = bgd_width.min(size_y);
            // Top and bottom bw_y rows (full ROI width).
            for &r in rows[..bw_y].iter().chain(&rows[size_y - bw_y..]) {
                n_bgd += size_x;
                bgd += T::range(r).total;
            }
            // Left and right bw_x columns of the middle rows: none when the
            // top and bottom borders already overlap (C's loop bounds cross).
            for &r in rows.get(bw_y..size_y - bw_y).unwrap_or(&[]) {
                n_bgd += 2 * bw_x;
                bgd += T::range(&r[..bw_x]).total;
                bgd += T::range(&r[size_x - bw_x..]).total;
            }
        }
        rows.iter()
            .map(|&r| T::range(r))
            .reduce(merge_range)
            .expect("size_y > 0")
    } else {
        return ROIStatResult::default();
    };

    // C (NDPluginROIStat.cpp:128-135):
    //   if (nBgd > 0) bgd = bgd/nBgd * nElements;
    //   net  = total - bgd;          (bgd stays 0 when bgdWidth == 0)
    //   mean = total / nElements;
    let bgd_scaled = if n_bgd > 0 {
        bgd / n_bgd as f64 * n_elements as f64
    } else {
        0.0
    };
    let total = range.total;
    ROIStatResult {
        min: range.min.to_f64(),
        max: range.max.to_f64(),
        mean: total / n_elements as f64,
        total,
        net: total - bgd_scaled,
    }
}

impl NDPluginProcess for ROIStatProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_MULTIDEVICE;
    // NDPluginROIStat.cpp:496-497, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::INT32_ARRAY
        .or(InterfaceMask::FLOAT64)
        .or(InterfaceMask::FLOAT64_ARRAY)
        .or(InterfaceMask::GENERIC_POINTER);
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;
    // NDPluginROIStat.cpp:389-390; no writeFloat64 override.
    const WRITE_INT32_EPILOGUE: ad_core_rs::plugin::runtime::WriteEpilogue =
        ad_core_rs::plugin::runtime::WriteEpilogue::AddrAndZero;

    /// maxROIs (NDPluginROIStat.cpp:495).
    fn max_addr(&self) -> usize {
        self.state.lock().rois.len()
    }

    /// C `NDPluginROIStat::writeInt32` keeps the base class clamp but replaces its
    /// errorMessage with its own, carrying `getAddress`'s status
    /// (`"%s: status=%d, function=%d, value=%d"`, NDPluginROIStat.cpp:393).
    fn validate_int32(
        &self,
        reason: usize,
        _addr: i32,
        value: i32,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<i32>,
        _params: &mut ad_core_rs::plugin::runtime::WritePort<'_>,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<i32> {
        let verdict = match verdict {
            ad_core_rs::plugin::runtime::WriteVerdict::Refuse { keep, .. } => {
                ad_core_rs::plugin::runtime::WriteVerdict::Refuse {
                    keep,
                    message: format!(
                        "NDPluginROIStat::writeInt32: status=0, function={reason}, value={value}"
                    ),
                }
            }
            accepted => accepted,
        };
        let p = &self.params;
        if reason != p.ts_control && reason != p.ts_num_points {
            return verdict.into();
        }
        let value = verdict.kept();
        let mut state = self.state.lock();
        let mut updates = Vec::new();
        if reason == p.ts_num_points {
            // C reallocates the series zeroed and keeps currentTSPoint_.
            state.ts_num_points = value.max(0) as usize;
            state.size_time_series();
        } else {
            // NDPluginROIStat.cpp:361-382, inside the write.
            let callbacks = match value {
                // EraseStart
                0 => {
                    state.clear_time_series();
                    updates.push(ParamUpdate::int32(p.ts_current_point, 0));
                    state.ts_mode = TSMode::Acquiring;
                    updates.push(ParamUpdate::int32(p.ts_acquiring, 1));
                    false
                }
                // Start
                1 => {
                    if state.ts_current < state.ts_num_points {
                        state.ts_mode = TSMode::Acquiring;
                        updates.push(ParamUpdate::int32(p.ts_acquiring, 1));
                    }
                    false
                }
                // Stop
                2 => {
                    state.ts_mode = TSMode::Idle;
                    updates.push(ParamUpdate::int32(p.ts_acquiring, 0));
                    true
                }
                // Read
                3 => true,
                // Erase
                4 => {
                    state.clear_time_series();
                    updates.push(ParamUpdate::int32(p.ts_current_point, 0));
                    true
                }
                _ => false,
            };
            if callbacks {
                state.time_series_callbacks(p, &mut updates);
            }
        }
        ad_core_rs::plugin::runtime::WriteOutcome::from(verdict)
            .set_updates(updates)
            .settled()
    }
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
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

        // C clamps the geometry under the port lock, releases it
        // (NDPluginROIStat.cpp:267) and only then runs the per-ROI statistics.
        // Those per-pixel loops are the whole reason the callback pool exists,
        // so the guard covers reading `rois` and writing the results back, and
        // nothing in between.
        let rois: Vec<ROIStatROI> = self.state.lock().rois.clone();

        // Clamp each enabled ROI's geometry to the array bounds (C clamp
        // loop). `None` for disabled ROIs or unsupported ranks — those keep
        // zero stats and skip the geometry write-back.
        let clamped: Vec<Option<([usize; 2], [usize; 2])>> = rois
            .iter()
            .map(|roi| {
                if roi.enabled && supported {
                    Self::clamp_roi_geometry(roi, ndims, dims)
                } else {
                    None
                }
            })
            .collect();

        let serial_results = || -> Vec<ROIStatResult> {
            rois.iter()
                .zip(clamped.iter())
                .map(|(roi, clamp)| match clamp {
                    Some((offset, size)) => Self::compute_roi_stats(
                        &array.data,
                        ndims,
                        array_size_x,
                        *offset,
                        *size,
                        roi.bgd_width,
                    ),
                    None => ROIStatResult::default(),
                })
                .collect()
        };

        #[cfg(feature = "parallel")]
        let new_results: Vec<ROIStatResult> = {
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

            // 15 ps + 38 ps per element byte serially; the split measured
            // faster from 100 us (u16 1024x1024 1.07x serial, 1214x1214 0.59x).
            if par_util::pool_wins(
                total_elements,
                15 + 38 * array.data.data_type().element_size(),
                100,
                clamped.iter().flatten().count(),
            ) {
                let data = &array.data;
                par_util::thread_pool().install(|| {
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
                })
            } else {
                serial_results()
            }
        };

        #[cfg(not(feature = "parallel"))]
        let new_results: Vec<ROIStatResult> = serial_results();

        let mut guard = self.state.lock();
        let state = &mut *guard;
        state.results = new_results;

        // Build per-ROI param updates (only for enabled ROIs)
        let p = &self.params;
        let mut updates = Vec::new();
        // NDPluginROIStat.cpp:222-224: an axis the input lacks keeps its
        // previous value.
        for (reason, dim) in p.array_size.into_iter().zip(&array.dims).take(2) {
            updates.push(ParamUpdate::int32(reason, dim.size as i32));
        }
        for (i, roi) in rois.iter().enumerate() {
            if !roi.enabled {
                continue;
            }
            let result = &state.results[i];
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
            // NDPluginROIStat.cpp:311, ahead of the series posts.
            updates.push(ParamUpdate::callbacks(addr));
        }
        // NDPluginROIStat.cpp:290-321: each used ROI stores this frame's
        // point; a full series stops and posts.
        if state.ts_mode == TSMode::Acquiring {
            let n = state.ts_num_points;
            let k = state.ts_current;
            for (i, roi) in state.rois.iter().enumerate() {
                if !roi.enabled {
                    continue;
                }
                let r = &state.results[i];
                for (buf, v) in state.ts_buffers[i].iter_mut().zip([
                    r.min,
                    r.max,
                    r.mean,
                    r.total,
                    r.net,
                    array.time_stamp,
                ]) {
                    // C writes past a buffer TSNumPoints shrank under it.
                    if let Some(slot) = buf.get_mut(k) {
                        *slot = v;
                    }
                }
            }
            state.ts_current += 1;
            updates.push(ParamUpdate::int32(
                p.ts_current_point,
                state.ts_current as i32,
            ));
            if state.ts_current >= n {
                state.ts_mode = TSMode::Idle;
                updates.push(ParamUpdate::int32(p.ts_acquiring, 0));
                state.time_series_callbacks(p, &mut updates);
            }
        }

        // C `endProcessCallbacks(pArray, true, true)` (NDPluginROIStat.cpp:323).
        ProcessResult::forward(array, updates)
    }

    fn plugin_type(&self) -> &str {
        "NDPluginROIStat"
    }

    fn register_params(
        &mut self,
        base: &mut PortDriverBase,
    ) -> Result<(), asyn_rs::error::AsynError> {
        let state = self.state.get_mut();

        // Global params
        self.params.reset_all = base.create_param("ROISTAT_RESETALL", ParamType::Int32)?;
        self.params.ts_control = base.create_param("ROISTAT_TS_CONTROL", ParamType::Int32)?;
        self.params.ts_num_points = base.create_param("ROISTAT_TS_NUM_POINTS", ParamType::Int32)?;
        base.set_int32_param(self.params.ts_num_points, 0, state.ts_num_points as i32)?;
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

        // NDPluginROIStat.cpp:552-579; BgdWidth is left to the database.
        for (i, roi) in state.rois.iter().enumerate() {
            let addr = i as i32;
            base.set_string_param(self.params.name, addr, "")?;
            base.set_int32_param(self.params.use_, addr, roi.enabled as i32)?;
            base.set_int32_param(self.params.dim0_min, addr, roi.offset[0] as i32)?;
            base.set_int32_param(self.params.dim0_size, addr, roi.size[0] as i32)?;
            base.set_int32_param(self.params.dim0_max_size, addr, 0)?;
            base.set_int32_param(self.params.dim1_min, addr, roi.offset[1] as i32)?;
            base.set_int32_param(self.params.dim1_size, addr, roi.size[1] as i32)?;
            base.set_int32_param(self.params.dim1_max_size, addr, 0)?;
            for stat in [
                self.params.min_value,
                self.params.max_value,
                self.params.mean_value,
                self.params.total,
                self.params.net,
            ] {
                base.set_float64_param(stat, addr, 0.0)?;
            }
        }

        // Export params
        self.params.array_size =
            ad_core_rs::params::ndarray_driver::NDArrayDriverParams::array_size_reasons(base)?;
        *self.params_out.lock() = self.params;

        Ok(())
    }

    fn on_param_change(
        &self,
        reason: usize,
        snapshot: &PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        let addr = snapshot.addr as usize;
        let p = &self.params;
        let mut guard = self.state.lock();
        let state = &mut *guard;

        if reason == p.use_ && addr < state.rois.len() {
            state.rois[addr].enabled = snapshot.value.as_i32() != 0;
        } else if reason == p.dim0_min && addr < state.rois.len() {
            state.rois[addr].offset[0] = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.dim1_min && addr < state.rois.len() {
            state.rois[addr].offset[1] = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.dim0_size && addr < state.rois.len() {
            state.rois[addr].size[0] = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.dim1_size && addr < state.rois.len() {
            state.rois[addr].size[1] = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.bgd_width && addr < state.rois.len() {
            state.rois[addr].bgd_width = snapshot.value.as_i32().max(0) as usize;
        } else if reason == p.reset && addr < state.rois.len() {
            state.results[addr] = ROIStatResult::default();
        } else if reason == p.reset_all {
            for r in &mut state.results {
                *r = ROIStatResult::default();
            }
        }
        ad_core_rs::plugin::runtime::ParamChangeResult::empty()
    }
}

/// Create a ROIStat plugin runtime. `num_rois` is C's maxROIs: the port's
/// addresses and, at least one, its ROIs.
pub fn create_roi_stat_runtime(
    claim: &asyn_rs::registry::PortClaim,
    max_memory: usize,
    queue_size: i32,
    blocking_callbacks: i32,
    source: impl Into<ArraySource>,
    wiring: Arc<WiringRegistry>,
    num_rois: usize,
    threads: ad_core_rs::plugin::runtime::PluginThreads,
) -> (
    PluginRuntimeHandle,
    ROIStatParams,
    std::thread::JoinHandle<()>,
) {
    let processor = ROIStatProcessor::new(vec![ROIStatROI::default(); num_rois], 2048);
    let params_handle = processor.params_handle();

    let (handle, data_jh) = ad_core_rs::plugin::runtime::create_plugin_runtime(
        claim,
        processor,
        max_memory,
        queue_size,
        blocking_callbacks,
        source,
        wiring,
        threads,
    );

    let roi_stat_params = *params_handle.lock();

    (handle, roi_stat_params, data_jh)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `"%s: status=%d, function=%d, value=%d"` (NDPluginROIStat.cpp:393).
    #[test]
    fn a_refused_base_write_carries_c_error_message() {
        use ad_core_rs::plugin::runtime::WriteVerdict;
        let proc = ROIStatProcessor::new(vec![], 0);
        let mut base = asyn_rs::port::PortDriverBase::new(
            "T",
            1,
            Default::default(),
            asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
            asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let clamp = WriteVerdict::Refuse {
            keep: 1,
            message: String::new(),
        };
        assert_eq!(
            proc.validate_int32(
                71,
                0,
                99,
                clamp,
                &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base)
            )
            .verdict,
            WriteVerdict::Refuse {
                keep: 1,
                message: "NDPluginROIStat::writeInt32: status=0, function=71, value=99".into(),
            }
        );
        assert_eq!(
            proc.validate_int32(
                71,
                0,
                1,
                WriteVerdict::Accept(1),
                &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base)
            )
            .verdict,
            WriteVerdict::Accept(1)
        );
    }

    #[test]
    fn ts_control_sets_ts_acquiring_from_the_write() {
        // NDPluginROIStat.cpp:361-382, with no frame in between: the TSRead
        // record is disabled until TSAcquiring reads 1.
        let (handle, p, _jh) = create_roi_stat_runtime(
            &asyn_rs::asyn_record::claim_port("RS_TSCTL").unwrap(),
            1_000_000,
            1,
            0,
            "",
            Arc::new(WiringRegistry::new()),
            1,
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let acquiring = || port.read_int32_blocking(p.ts_acquiring, 0).unwrap();
        port.write_int32_blocking(p.ts_control, 0, 1).unwrap();
        assert_eq!(acquiring(), 1, "Start");
        port.write_int32_blocking(p.ts_control, 0, 3).unwrap();
        assert_eq!(acquiring(), 1, "Read");
        port.write_int32_blocking(p.ts_control, 0, 2).unwrap();
        assert_eq!(acquiring(), 0, "Stop");
        port.write_int32_blocking(p.ts_control, 0, 0).unwrap();
        assert_eq!(acquiring(), 1, "EraseStart");
        port.write_int32_blocking(p.ts_control, 0, 4).unwrap();
        assert_eq!(acquiring(), 1, "Erase keeps acquiring");
        port.write_int32_blocking(p.ts_control, 0, 2).unwrap();
        // Start is refused once the series is full.
        port.write_int32_blocking(p.ts_num_points, 0, 0).unwrap();
        port.write_int32_blocking(p.ts_control, 0, 1).unwrap();
        assert_eq!(acquiring(), 0, "Start on a full series");
    }

    #[test]
    fn time_series_arrays_post_only_from_do_time_series_callbacks() {
        // NDPluginROIStat.cpp:312-320, 361-382, 451-464: a frame stores its
        // point, Stop/Read/Erase and a full series post currentTSPoint_ of it.
        use ad_core_rs::plugin::runtime::{SideParam, WritePort, WriteVerdict};
        use asyn_rs::port::{InterfaceMask, PortDriverBase, PortFlags};
        let mut base = PortDriverBase::new(
            "RS_TSCB",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let roi = ROIStatROI {
            enabled: true,
            offset: [0, 0],
            size: [2, 2],
            bgd_width: 0,
        };
        let mut proc = ROIStatProcessor::new(vec![roi], 3);
        ad_core_rs::params::ndarray_driver::NDArrayDriverParams::create(&mut base).unwrap();
        proc.register_params(&mut base).unwrap();
        let p = proc.params;
        let mut control = |v| -> Vec<ParamUpdate> {
            let mut port = WritePort::new(&mut base);
            let out = proc.validate_int32(p.ts_control, 0, v, WriteVerdict::Accept(v), &mut port);
            out.side
                .into_iter()
                .filter_map(|s| match s {
                    SideParam::Float64Array {
                        reason,
                        addr,
                        value,
                    } => Some(ParamUpdate::float64_array_addr(reason, addr, value)),
                    _ => None,
                })
                .collect()
        };
        let means = |updates: &[ParamUpdate]| -> Vec<Vec<f64>> {
            updates
                .iter()
                .filter_map(|u| match u {
                    ParamUpdate::Float64Array { reason, value, .. }
                        if *reason == p.ts_mean_value =>
                    {
                        Some(value.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        let pool = NDArrayPool::new(1_000_000);
        let frame = |v: f64| {
            proc.process_array(&Arc::new(make_2d_array(2, 2, |_, _| v)), &pool)
                .param_updates
        };

        assert_eq!(means(&control(0)), Vec::<Vec<f64>>::new(), "EraseStart");
        assert_eq!(means(&frame(1.0)), Vec::<Vec<f64>>::new());
        assert_eq!(means(&frame(2.0)), Vec::<Vec<f64>>::new());
        assert_eq!(means(&control(3)), vec![vec![1.0, 2.0]], "Read");
        assert_eq!(means(&frame(3.0)), vec![vec![1.0, 2.0, 3.0]], "full");
        assert_eq!(means(&frame(4.0)), Vec::<Vec<f64>>::new(), "stopped");
        assert_eq!(means(&control(2)), vec![vec![1.0, 2.0, 3.0]], "Stop");
        assert_eq!(means(&control(4)), vec![Vec::<f64>::new()], "Erase");
    }

    #[test]
    fn time_series_records_each_frame_time_stamp() {
        // NDPluginROIStat.cpp:299, posted at :462.
        use ad_core_rs::plugin::runtime::{SideParam, WritePort, WriteVerdict};
        use asyn_rs::port::{InterfaceMask, PortDriverBase, PortFlags};
        let mut base = PortDriverBase::new(
            "RS_TSTS",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let roi = ROIStatROI {
            enabled: true,
            offset: [0, 0],
            size: [1, 1],
            bgd_width: 0,
        };
        let mut proc = ROIStatProcessor::new(vec![roi], 4);
        ad_core_rs::params::ndarray_driver::NDArrayDriverParams::create(&mut base).unwrap();
        proc.register_params(&mut base).unwrap();
        let p = proc.params;
        let mut control = |v| {
            let mut port = WritePort::new(&mut base);
            proc.validate_int32(p.ts_control, 0, v, WriteVerdict::Accept(v), &mut port)
                .side
        };
        control(0);
        let pool = NDArrayPool::new(1_000_000);
        for t in [10.5, 11.25] {
            let mut arr = make_2d_array(1, 1, |_, _| 0.0);
            arr.time_stamp = t;
            proc.process_array(&Arc::new(arr), &pool);
        }
        let stamps: Vec<Vec<f64>> = control(3)
            .into_iter()
            .filter_map(|s| match s {
                SideParam::Float64Array { reason, value, .. } if reason == p.ts_timestamp => {
                    Some(value)
                }
                _ => None,
            })
            .collect();
        assert_eq!(stamps, vec![vec![10.5, 11.25]]);
    }

    /// C clamps maxROIs below 1 to one ROI (NDPluginROIStat.cpp:502-505) on
    /// the one address asynPortDriver gives it (asynPortDriver.cpp:4006).
    #[test]
    fn a_max_rois_below_1_still_seeds_roi_0() {
        let (handle, p, _jh) = create_roi_stat_runtime(
            &asyn_rs::asyn_record::claim_port("RS_ZERO").unwrap(),
            1_000_000,
            1,
            0,
            "",
            Arc::new(WiringRegistry::new()),
            0,
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        assert_eq!(port.read_int32_blocking(p.dim0_max_size, 0).unwrap(), 0);
        assert_eq!(port.read_float64_blocking(p.total, 0).unwrap(), 0.0);
    }

    /// C's one maxROIs is both counts (NDPluginROIStat.cpp:495, :502-505),
    /// whoever builds the plugin.
    #[test]
    fn four_rois_build_a_four_address_port() {
        let (handle, _jh) = ad_core_rs::plugin::runtime::create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("RS_FOUR").unwrap(),
            ROIStatProcessor::new(vec![ROIStatROI::default(); 4], 16),
            1_000_000,
            1,
            0,
            "",
            Arc::new(WiringRegistry::new()),
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );
        assert_eq!(handle.port_runtime().port_handle().max_addr(), 4);
    }

    // RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — it ran and passed on
    // the exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run
    // -p ad-plugins-rs --all-features`, 729/729).
    /// pool_throughput's ROIStat stage as NDROIStatConfigure builds it: four
    /// quadrants set through the port, each posting its own total.
    #[test]
    fn four_quadrant_rois_each_post_their_own_total() {
        let (handle, p, _jh) = create_roi_stat_runtime(
            &asyn_rs::asyn_record::claim_port("RS_QUAD").unwrap(),
            1_000_000,
            4,
            0,
            "",
            Arc::new(WiringRegistry::new()),
            4,
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        for (addr, (x, y)) in [(0, 0), (2, 0), (0, 2), (2, 2)].into_iter().enumerate() {
            for (reason, value) in [
                (p.use_, 1),
                (p.dim0_min, x),
                (p.dim1_min, y),
                (p.dim0_size, 2),
                (p.dim1_size, 2),
            ] {
                port.write_int32_blocking(reason, addr as i32, value)
                    .unwrap();
            }
        }
        port.write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        let frame = make_2d_array(4, 4, |x, y| (1 + x / 2 + 2 * (y / 2)) as f64);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(
            handle.array_sender().publish(
                ad_core_rs::ndarray_pool::DriverPool::standalone(0)
                    .alloc_copy(&frame)
                    .unwrap(),
            ),
        );
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        let totals: Vec<f64> = (0..4)
            .map(|addr| port.read_float64_blocking(p.total, addr).unwrap())
            .collect();
        assert_eq!(totals, [4.0, 8.0, 12.0, 16.0]);
    }

    #[test]
    fn test_runtime_seeds_each_roi_as_the_c_constructor() {
        // Every value NDPluginROIStat.cpp:552-579 sets, at every address;
        // the controls stay undefined.
        let (handle, p, _jh) = create_roi_stat_runtime(
            &asyn_rs::asyn_record::claim_port("RS_INIT").unwrap(),
            1_000_000,
            1,
            0,
            "",
            Arc::new(WiringRegistry::new()),
            2,
            ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        for addr in 0..2 {
            for reason in [
                p.use_,
                p.dim0_min,
                p.dim0_size,
                p.dim0_max_size,
                p.dim1_min,
                p.dim1_size,
                p.dim1_max_size,
            ] {
                assert_eq!(port.read_int32_blocking(reason, addr).unwrap(), 0);
            }
            for reason in [p.min_value, p.max_value, p.mean_value, p.total, p.net] {
                assert_eq!(port.read_float64_blocking(reason, addr).unwrap(), 0.0);
            }
            for reason in [p.bgd_width, p.reset] {
                assert!(port.read_int32_blocking(reason, addr).is_err());
            }
        }
        assert_eq!(port.read_int32_blocking(p.ts_num_points, 0).unwrap(), 2048);
        for reason in [
            p.reset_all,
            p.ts_control,
            p.ts_current_point,
            p.ts_acquiring,
        ] {
            assert!(port.read_int32_blocking(reason, 0).is_err());
        }
        // The port handle has no blocking octet read; Name goes through the base.
        use asyn_rs::port::InterfaceMask;
        use asyn_rs::port::{PortDriverBase, PortFlags};
        let mut base = PortDriverBase::new(
            "RS_NAME",
            2,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let mut proc = ROIStatProcessor::new(vec![ROIStatROI::default(); 2], 2048);
        ad_core_rs::params::ndarray_driver::NDArrayDriverParams::create(&mut base).unwrap();
        proc.register_params(&mut base).unwrap();
        for addr in 0..2 {
            assert_eq!(base.get_string_param(proc.params.name, addr).unwrap(), b"");
        }
    }
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

    /// C hands every frame on, whatever its rank (NDPluginROIStat.cpp:323).
    #[test]
    fn every_frame_is_handed_on() {
        let pool = NDArrayPool::new(1_000_000);
        let rois = vec![ROIStatROI {
            enabled: true,
            offset: [0, 0],
            size: [2, 2],
            bgd_width: 0,
        }];
        let proc = ROIStatProcessor::new(rois, 0);
        let three_d = NDArray::new(
            vec![
                NDDimension::new(2),
                NDDimension::new(2),
                NDDimension::new(2),
            ],
            NDDataType::Float64,
        );
        for arr in [make_2d_array(2, 2, |_, _| 1.0), three_d] {
            let arr = Arc::new(arr);
            let result = proc.process_array(&arr, &pool);
            assert_eq!(result.output_arrays.len(), 1);
            assert!(Arc::ptr_eq(&result.output_arrays[0], &arr));
        }
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

        let proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&Arc::new(arr), &pool);

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

        let proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&Arc::new(arr), &pool);

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

        let proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&Arc::new(arr), &pool);

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

        let proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&Arc::new(arr), &pool);

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

        let proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&Arc::new(arr), &pool);

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

        let proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&Arc::new(arr), &pool);

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

        let proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&Arc::new(arr), &pool);

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

        let proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&Arc::new(arr), &pool);

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

        let proc = ROIStatProcessor::new(rois, 100);
        let pool = NDArrayPool::new(1_000_000);
        proc.set_ts_mode(TSMode::Acquiring);

        for i in 0..5 {
            let arr = make_2d_array(4, 4, |_, _| (i + 1) as f64);
            proc.process_array(&Arc::new(arr), &pool);
        }

        // Check mean time series (stat index 2)
        let ts = proc.ts_buffer(0, 2);
        assert_eq!(ts.len(), 5);
        assert!((ts[0] - 1.0).abs() < 1e-10);
        assert!((ts[4] - 5.0).abs() < 1e-10);
    }

    /// C flushes each used ROI's list (NDPluginROIStat.cpp:311) before a
    /// full series posts its arrays (:319).
    #[test]
    fn each_roi_flushes_before_the_time_series_posts() {
        let roi = ROIStatROI {
            enabled: true,
            offset: [0, 0],
            size: [2, 2],
            bgd_width: 0,
        };
        let proc = ROIStatProcessor::new(vec![roi.clone(), roi], 1);
        proc.set_ts_mode(TSMode::Acquiring);
        let r = proc.process_array(
            &Arc::new(make_2d_array(2, 2, |_, _| 1.0)),
            &NDArrayPool::new(1_000_000),
        );
        let mut seq: Vec<(&str, i32)> = r
            .param_updates
            .iter()
            .map(|u| match u {
                ParamUpdate::Int32 { addr, .. }
                | ParamUpdate::Float64 { addr, .. }
                | ParamUpdate::Octet { addr, .. } => ("set", *addr),
                ParamUpdate::Float64Array { addr, .. } => ("array", *addr),
                ParamUpdate::Callbacks { addr } => ("flush", *addr),
            })
            .collect();
        seq.dedup();
        assert_eq!(
            seq,
            [
                ("set", 0),
                ("flush", 0),
                ("set", 1),
                ("flush", 1),
                ("set", 0),
                ("array", 0),
                ("array", 1),
            ]
        );
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

        let proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&Arc::new(arr), &pool);

        let r = &proc.results()[0];
        assert!((r.min - 1.0).abs() < 1e-10);
        assert!((r.max - 16.0).abs() < 1e-10);
    }

    /// NDPluginROIStat.cpp:222-224 sets X and Y from the input, leaves an
    /// axis a 1-D frame lacks as it was, and never touches Z or ArraySize.
    #[test]
    fn the_array_sizes_describe_the_input() {
        let sizes = crate::test_util::array_sizes(
            "RS_SIZES",
            ROIStatProcessor::new(vec![], 0),
            &[make_2d_array(8, 6, |_, _| 0.0), make_1d_array(5, |_| 0.0)],
        );
        assert_eq!(sizes, [[8, 6, 0, 0], [5, 6, 0, 0]]);
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
        use asyn_rs::port::InterfaceMask;
        use asyn_rs::port::{PortDriverBase, PortFlags};

        let arr = make_2d_array(4, 4, |_, _| 10.0);
        let rois = vec![ROIStatROI {
            enabled: true,
            offset: [10, 1],
            size: [4, 9],
            bgd_width: 0,
        }];
        let mut proc = ROIStatProcessor::new(rois, 0);
        let mut base = PortDriverBase::new(
            "roistat_adp16",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        ad_core_rs::params::ndarray_driver::NDArrayDriverParams::create(&mut base).unwrap();
        proc.register_params(&mut base).unwrap();
        let p = *proc.params_handle().lock();

        let pool = NDArrayPool::new(1_000_000);
        let res = proc.process_array(&Arc::new(arr), &pool);

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
        let proc = ROIStatProcessor::new(rois, 0);
        let pool = NDArrayPool::new(1_000_000);
        proc.process_array(&Arc::new(arr), &pool);

        let r = &proc.results()[0];
        assert!((r.total - 20.0).abs() < 1e-10, "total={}", r.total);
        assert!((r.min - 0.0).abs() < 1e-10);
        assert!((r.max - 10.0).abs() < 1e-10);
        assert!((r.net + 40.0).abs() < 1e-10, "net={}", r.net);
    }

    /// C `doComputeStatisticsT` (NDPluginROIStat.cpp:33-139) loop for loop,
    /// in f64, over an f64 copy of the frame.
    fn reference_roi_stats(
        vals: &[f64],
        ndims: usize,
        asx: usize,
        offset: [usize; 2],
        size: [usize; 2],
        bgd_width: usize,
    ) -> ROIStatResult {
        let (ox, sx, oy, sy) = (offset[0], size[0], offset[1], size[1]);
        let (mut min, mut max, mut total, mut bgd, mut n_bgd) = (0.0, 0.0, 0.0, 0.0, 0usize);
        let mut initial = true;
        let mut see = |v: f64| {
            if initial {
                min = v;
                max = v;
                initial = false;
            }
            if v < min {
                min = v;
            }
            if v > max {
                max = v;
            }
            total += v;
        };
        let n_elements;
        if ndims == 1 {
            n_elements = sx;
            for x in ox..ox + sx {
                see(vals[x]);
            }
            let bw = bgd_width.min(sx);
            if bgd_width > 0 {
                for x in (ox..ox + bw).chain(ox + sx - bw..ox + sx) {
                    n_bgd += 1;
                    bgd += vals[x];
                }
            }
        } else {
            n_elements = sx * sy;
            for y in oy..oy + sy {
                for x in ox..ox + sx {
                    see(vals[y * asx + x]);
                }
            }
            let (bwx, bwy) = (bgd_width.min(sx), bgd_width.min(sy));
            if bgd_width > 0 {
                for y in (oy..oy + bwy).chain(oy + sy - bwy..oy + sy) {
                    for x in ox..ox + sx {
                        n_bgd += 1;
                        bgd += vals[y * asx + x];
                    }
                }
                for y in oy + bwy..oy + sy - bwy {
                    for x in (ox..ox + bwx).chain(ox + sx - bwx..ox + sx) {
                        n_bgd += 1;
                        bgd += vals[y * asx + x];
                    }
                }
            }
        }
        if n_bgd > 0 {
            bgd = bgd / n_bgd as f64 * n_elements as f64;
        }
        ROIStatResult {
            min,
            max,
            total,
            mean: total / n_elements as f64,
            net: total - bgd,
        }
    }

    fn check_roi_stats_against_reference<T: Copy>(
        name: &str,
        make: impl Fn(usize) -> T,
        wrap: fn(Vec<T>) -> NDDataBuffer,
        to_f64: fn(T) -> f64,
    ) {
        let (asx, asy) = (37usize, 23usize);
        let raw: Vec<T> = (0..asx * asy).map(&make).collect();
        let vals: Vec<f64> = raw.iter().map(|&e| to_f64(e)).collect();
        let data = wrap(raw);
        // `net` is a difference of two sums, so its error scales with the
        // total, not with itself: a border covering the whole ROI leaves an
        // exact 0 in C's single accumulator and an ulp of `total` here.
        let close = |what: String, got: f64, want: f64, scale: f64| {
            let tol = 1e-9 * want.abs().max(scale.abs()).max(1.0);
            assert!((got - want).abs() <= tol, "{what}: got {got}, want {want}");
        };
        // Widths on both sides of a LANES multiple, borders thinner than,
        // equal to and thicker than half the size (the overlapping case).
        for (ox, sx) in [(0, 1), (3, 5), (0, 16), (2, 17), (1, 33), (0, 37)] {
            for bgd_width in [0, 1, 2, 3, 9, 40] {
                let what = format!("{name} 1-D x={ox}+{sx} bgd={bgd_width}");
                let got =
                    ROIStatProcessor::compute_roi_stats(&data, 1, asx, [ox, 0], [sx, 1], bgd_width);
                let want = reference_roi_stats(&vals, 1, asx, [ox, 0], [sx, 1], bgd_width);
                for (f, g, w) in [
                    ("min", got.min, want.min),
                    ("max", got.max, want.max),
                    ("total", got.total, want.total),
                    ("mean", got.mean, want.mean),
                    ("net", got.net, want.net),
                ] {
                    close(format!("{what} {f}"), g, w, want.total);
                }
                for (oy, sy) in [(0, 1), (4, 3), (0, 23), (7, 16)] {
                    let what = format!("{name} 2-D x={ox}+{sx} y={oy}+{sy} bgd={bgd_width}");
                    let got = ROIStatProcessor::compute_roi_stats(
                        &data,
                        2,
                        asx,
                        [ox, oy],
                        [sx, sy],
                        bgd_width,
                    );
                    let want = reference_roi_stats(&vals, 2, asx, [ox, oy], [sx, sy], bgd_width);
                    for (f, g, w) in [
                        ("min", got.min, want.min),
                        ("max", got.max, want.max),
                        ("total", got.total, want.total),
                        ("mean", got.mean, want.mean),
                        ("net", got.net, want.net),
                    ] {
                        close(format!("{what} {f}"), g, w, want.total);
                    }
                }
            }
        }
    }

    fn seq(i: usize) -> u32 {
        (i as u32).wrapping_mul(2_654_435_761) >> 12
    }

    #[test]
    fn roi_stats_match_the_c_loops_for_every_element_type() {
        check_roi_stats_against_reference(
            "i8",
            |i| (seq(i) % 256) as u8 as i8,
            NDDataBuffer::I8,
            |e| e as f64,
        );
        check_roi_stats_against_reference(
            "u8",
            |i| (seq(i) % 256) as u8,
            NDDataBuffer::U8,
            |e| e as f64,
        );
        check_roi_stats_against_reference(
            "i16",
            |i| (seq(i) % 65536) as u16 as i16,
            NDDataBuffer::I16,
            |e| e as f64,
        );
        check_roi_stats_against_reference(
            "u16",
            |i| (seq(i) % 65536) as u16,
            NDDataBuffer::U16,
            |e| e as f64,
        );
        check_roi_stats_against_reference(
            "i32",
            |i| seq(i) as i32 - 500_000,
            NDDataBuffer::I32,
            |e| e as f64,
        );
        check_roi_stats_against_reference("u32", seq, NDDataBuffer::U32, |e| e as f64);
        check_roi_stats_against_reference(
            "i64",
            |i| seq(i) as i64 - 500_000,
            NDDataBuffer::I64,
            |e| e as f64,
        );
        check_roi_stats_against_reference(
            "u64",
            |i| seq(i) as u64,
            NDDataBuffer::U64,
            |e| e as f64,
        );
        check_roi_stats_against_reference(
            "f32",
            |i| seq(i) as f32 * 0.37 - 100.0,
            NDDataBuffer::F32,
            |e| e as f64,
        );
        check_roi_stats_against_reference(
            "f64",
            |i| seq(i) as f64 * 0.37 - 100.0,
            NDDataBuffer::F64,
            |e| e,
        );
    }

    /// A rectangle the buffer cannot hold yields the zero result rather than
    /// reading past it.
    #[test]
    fn roi_stats_of_a_rectangle_past_the_buffer_are_zero() {
        let data = NDDataBuffer::U8((0..12).collect());
        let r = ROIStatProcessor::compute_roi_stats(&data, 2, 4, [2, 1], [3, 2], 0);
        assert_eq!(r.total, 0.0);
        assert_eq!(r.max, 0.0);
        let r = ROIStatProcessor::compute_roi_stats(&data, 1, 4, [10, 0], [3, 1], 0);
        assert_eq!(r.total, 0.0);
    }
}
