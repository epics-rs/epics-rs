use std::sync::Arc;

#[cfg(feature = "parallel")]
use crate::par_util;
#[cfg(feature = "parallel")]
use rayon::prelude::*;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};

/// Recursive filter configuration matching C++ NDPluginProcess.
///
/// The C++ filter uses a single filter buffer and numFiltered-dependent coefficients:
///
/// Reset:
///   filter[i] = rOffset + rc1*filter[i] + rc2*data[i]
///
/// Normal operation (after numFiltered is incremented):
///   O1 = oScale * (oc1 + oc2/numFiltered)
///   O2 = oScale * (oc3 + oc4/numFiltered)
///   F1 = fScale * (fc1 + fc2/numFiltered)
///   F2 = fScale * (fc3 + fc4/numFiltered)
///   data[i]   = oOffset + O1*filter[i] + O2*data[i]
///   filter[i] = fOffset + F1*filter[i] + F2*data[i]
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
    /// Reset offset (C++ rOffset).
    pub r_offset: f64,
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
            r_offset: 0.0,
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
    pub low_clip_thresh: f64,
    pub low_clip_value: f64,
    pub enable_high_clip: bool,
    pub high_clip_thresh: f64,
    pub high_clip_value: f64,
    pub scale_flat_field: f64,
    pub enable_filter: bool,
    pub filter: FilterConfig,
    pub output_type: Option<NDDataType>,
    /// One-shot flag: save current input as background on next process().
    pub save_background: bool,
    /// One-shot flag: save current input as flat field on next process().
    pub save_flat_field: bool,
    /// One-shot flag: compute offset/scale automatically from the next input
    /// array (C++ `NDPluginProcessAutoOffsetScale`). Cleared after it runs.
    pub auto_offset_scale_pending: bool,
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
            low_clip_thresh: 0.0,
            low_clip_value: 0.0,
            enable_high_clip: false,
            high_clip_thresh: 100.0,
            high_clip_value: 100.0,
            scale_flat_field: 255.0,
            enable_filter: false,
            filter: FilterConfig::default(),
            output_type: None,
            save_background: false,
            save_flat_field: false,
            auto_offset_scale_pending: false,
            valid_background: false,
            valid_flat_field: false,
        }
    }
}

/// State for the process plugin (holds background, flat field, and filter state).
///
/// Matches the C++ NDPluginProcess which uses a single `pFilter` array.
pub struct ProcessState {
    pub config: ProcessConfig,
    pub background: Option<Vec<f64>>,
    pub flat_field: Option<Vec<f64>>,
    /// Single filter buffer (equivalent to C++ `pFilter`).
    pub filter_state: Option<Vec<f64>>,
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

    /// Auto-calculate offset and scale matching C++ NDPluginProcess.
    ///
    /// C++: scale = maxScale / (maxValue - minValue); offset = -minValue;
    /// Also enables offset/scale processing and clipping (matching C++ lines 238-249).
    pub fn auto_offset_scale(&mut self, array: &NDArray) {
        let n = array.data.len();
        if n == 0 {
            return;
        }
        let mut min_val = f64::MAX;
        let mut max_val = f64::MIN;
        for i in 0..n {
            let v = array.data.get_as_f64(i).unwrap_or(0.0);
            if v < min_val {
                min_val = v;
            }
            if v > max_val {
                max_val = v;
            }
        }
        let range = max_val - min_val;
        if range > 0.0 {
            // C++: maxScale = pow(2, bytesPerElement*8) - 1
            let bytes_per_elem = match self.config.output_type.unwrap_or(array.data.data_type()) {
                NDDataType::Int8 | NDDataType::UInt8 => 1,
                NDDataType::Int16 | NDDataType::UInt16 => 2,
                NDDataType::Int32 | NDDataType::UInt32 => 4,
                NDDataType::Int64 | NDDataType::UInt64 => 8,
                NDDataType::Float32 => 4,
                NDDataType::Float64 => 8,
            };
            let max_scale = 2.0f64.powi(bytes_per_elem * 8) - 1.0;
            // C++: scale = maxScale/(maxValue-minValue); offset = -minValue;
            self.config.scale = max_scale / range;
            self.config.offset = -min_val;
            // C++ also enables offset/scale and clipping
            self.config.enable_offset_scale = true;
            self.config.enable_low_clip = true;
            self.config.low_clip_thresh = 0.0;
            self.config.enable_high_clip = true;
            self.config.high_clip_thresh = max_scale;
        }
    }

    /// Apply a named filter type preset, setting the FC/OC/RC coefficients.
    ///
    /// Uses the C++ coefficient scheme where:
    ///   O1 = oScale * (oc[0] + oc[1]/N), O2 = oScale * (oc[2] + oc[3]/N)
    ///   F1 = fScale * (fc[0] + fc[1]/N), F2 = fScale * (fc[2] + fc[3]/N)
    ///   data[i]   = oOffset + O1*filter[i] + O2*data[i]
    ///   filter[i] = fOffset + F1*filter[i] + F2*data[i]
    pub fn apply_filter_type(&mut self, filter_type: i32) {
        let fc = &mut self.config.filter;
        match filter_type {
            0 => {
                // RecursiveAve: running average
                // F1=fScale*(0 + 1/N)=1/N (old filter weight decreases)
                // F2=fScale*(1 + -1/N)=(N-1)/N (new data weight increases)
                // Actually: F[n]=(1-1/N)*F[n-1] + (1/N)*data[n]
                //   fc1=0, fc2=1 → F1=fScale*(0+1/N)=1/N ← weight on filter
                // Wait, the formula is: F2=fScale*(fc3+fc4/N)
                // For recursive avg: filter = ((N-1)*filter + data)/N
                //   F1 applied to filter: want (N-1)/N → fc1=1, fc2=-1
                //     F1 = fScale*(1 + (-1)/N) = (N-1)/N ✓
                //   F2 applied to data: want 1/N → fc3=0, fc4=1
                //     F2 = fScale*(0 + 1/N) = 1/N ✓
                // O1 applied to filter: want 1 → oc1=1, oc2=0
                // O2 applied to data: want 0 → oc3=0, oc4=0
                fc.fc = [1.0, -1.0, 0.0, 1.0];
                fc.oc = [1.0, 0.0, 0.0, 0.0];
                fc.rc = [0.0, 1.0]; // reset: filter = data
                fc.r_offset = 0.0;
                fc.f_offset = 0.0;
                fc.f_scale = 1.0;
                fc.o_offset = 0.0;
                fc.o_scale = 1.0;
            }
            1 => {
                // Average: accumulate sum in filter, output = filter/N
                // filter = filter + data → F1=1*filter, F2=1*data
                //   fc1=1,fc2=0 → F1=fScale*(1+0/N)=1; fc3=1,fc4=0 → F2=fScale*(1+0/N)=1
                // output = filter/N → O1=1/N*filter
                //   oc1=0,oc2=1 → O1=oScale*(0+1/N)=1/N; oc3=0,oc4=0 → O2=0
                fc.fc = [1.0, 0.0, 1.0, 0.0];
                fc.oc = [0.0, 1.0, 0.0, 0.0];
                fc.rc = [0.0, 1.0]; // reset: filter = data
                fc.r_offset = 0.0;
                fc.f_offset = 0.0;
                fc.f_scale = 1.0;
                fc.o_offset = 0.0;
                fc.o_scale = 1.0;
            }
            2 => {
                // Sum: filter = filter + data, output = filter
                fc.fc = [1.0, 0.0, 1.0, 0.0];
                fc.oc = [1.0, 0.0, 0.0, 0.0];
                fc.rc = [0.0, 1.0];
                fc.r_offset = 0.0;
                fc.f_offset = 0.0;
                fc.f_scale = 1.0;
                fc.o_offset = 0.0;
                fc.o_scale = 1.0;
            }
            3 => {
                // Difference: output = data - filter, filter = data
                // O1=-1*filter, O2=1*data → oc1=-1,oc2=0,oc3=1,oc4=0
                // F1=0, F2=1*data → fc1=0,fc2=0,fc3=1,fc4=0
                fc.fc = [0.0, 0.0, 1.0, 0.0];
                fc.oc = [-1.0, 0.0, 1.0, 0.0];
                fc.rc = [0.0, 1.0];
                fc.r_offset = 0.0;
                fc.f_offset = 0.0;
                fc.f_scale = 1.0;
                fc.o_offset = 0.0;
                fc.o_scale = 1.0;
            }
            4 => {
                // RecursiveAveDiff: output = data - running_avg
                // Same filter as RecursiveAve but output = data - filter
                fc.fc = [1.0, -1.0, 0.0, 1.0];
                fc.oc = [-1.0, 0.0, 1.0, 0.0];
                fc.rc = [0.0, 1.0];
                fc.r_offset = 0.0;
                fc.f_offset = 0.0;
                fc.f_scale = 1.0;
                fc.o_offset = 0.0;
                fc.o_scale = 1.0;
            }
            5 => {
                // CopyToFilter: filter = data, output = filter
                fc.fc = [0.0, 0.0, 1.0, 0.0];
                fc.oc = [1.0, 0.0, 0.0, 0.0];
                fc.rc = [0.0, 1.0];
                fc.r_offset = 0.0;
                fc.f_offset = 0.0;
                fc.f_scale = 1.0;
                fc.o_offset = 0.0;
                fc.o_scale = 1.0;
            }
            _ => {} // Unknown type — leave coefficients unchanged
        }
    }

    /// Reset the filter state, clearing the filter buffer.
    pub fn reset_filter(&mut self) {
        self.filter_state = None;
        self.num_filtered = 0;
    }

    /// Process an array through the configured pipeline.
    /// Process one input array.
    ///
    /// Returns `Some(output)` for a normal frame, or `None` when the frame is
    /// suppressed by the recursive-filter `filter_callbacks` setting (C++ sets
    /// `doCallbacks = 0` and the frame is dropped — nothing goes downstream).
    pub fn process(&mut self, src: &NDArray) -> Option<NDArray> {
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
        // 0b. Auto offset/scale (one-shot): C MEASURES this frame's min/max and
        // ARMS scale/offset + clipping for the NEXT frame — the trigger frame
        // itself is emitted with the pre-existing config, NOT the derived scale
        // (NDPluginProcess.cpp:164-178 only updates min/max; 238-250 arms the
        // params after the output array is built). Consume the one-shot here and
        // defer the arming until after this frame's output is produced.
        let auto_offset_scale_now = self.config.auto_offset_scale_pending;
        self.config.auto_offset_scale_pending = false;

        // Recompute valid background / flat field each frame from the element
        // count (C NDPluginProcess.cpp:120-125): a saved buffer is usable only
        // when its length matches the current frame. A size mismatch
        // invalidates it — the buffer is dropped entirely, never applied to a
        // matching prefix.
        self.config.valid_background = self.background.as_ref().is_some_and(|b| b.len() == n);
        self.config.valid_flat_field = self.flat_field.as_ref().is_some_and(|f| f.len() == n);

        // Stages 1-4: element-wise operations (background, flat field, offset+scale, clipping)
        // These can be combined into a single pass and parallelized.
        let needs_element_ops = self.config.enable_background
            || self.config.enable_flat_field
            || self.config.enable_offset_scale
            || self.config.enable_low_clip
            || self.config.enable_high_clip;

        if needs_element_ops {
            // C only takes the background/flat-field pointer when the buffer is
            // BOTH enabled AND valid for this frame (NDPluginProcess.cpp:127-130).
            let bg = if self.config.enable_background && self.config.valid_background {
                self.background.as_ref()
            } else {
                None
            };
            let (ff, ff_scale) = if self.config.enable_flat_field && self.config.valid_flat_field {
                if let Some(ref ff) = self.flat_field {
                    // C++: value *= scaleFlatField / flatField[i]
                    // (NDPluginProcess.cpp:172). scaleFlatField is used directly
                    // — there is no mean substitution when it is <= 0.
                    (Some(ff.as_slice()), self.config.scale_flat_field)
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
            let low_clip_thresh = self.config.low_clip_thresh;
            let low_clip_value = self.config.low_clip_value;
            let do_high_clip = self.config.enable_high_clip;
            let high_clip_thresh = self.config.high_clip_thresh;
            let high_clip_value = self.config.high_clip_value;

            let apply_stages = |i: usize, v: &mut f64| {
                // Stage 1: Background subtraction. bg.len() == n is guaranteed by
                // the validity gate above, so index directly (C subtracts
                // background[i] unconditionally for every element).
                if let Some(bg) = bg {
                    *v -= bg[i];
                }
                // Stage 2: Flat field normalization
                if let Some(ff) = ff {
                    if ff[i] != 0.0 {
                        *v = *v * ff_scale / ff[i];
                    }
                }
                // Stage 3: Offset + scale (C++: value = (value + offset) * scale)
                if do_offset_scale {
                    *v = (*v + offset) * scale;
                }
                // Stage 4: Clipping — C applies high-clip THEN low-clip
                // (NDPluginProcess.cpp:175-176). When the two thresholds cross
                // (high < low) the order changes the result, so it must match.
                if do_high_clip && *v > high_clip_thresh {
                    *v = high_clip_value;
                }
                if do_low_clip && *v < low_clip_thresh {
                    *v = low_clip_value;
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

        // 5. Recursive filter (matching C++ NDPluginProcess algorithm)
        if self.config.enable_filter {
            let fc = &self.config.filter;

            // Ensure filter buffer exists and matches element count
            if let Some(ref f) = self.filter_state {
                if f.len() != n {
                    self.filter_state = None;
                }
            }

            let mut reset_filter = self.filter_state.is_none();
            if self.num_filtered >= fc.num_filter && fc.auto_reset {
                reset_filter = true;
            }

            // Initialize filter buffer from current data if needed
            if self.filter_state.is_none() {
                self.filter_state = Some(values.clone());
            }

            let filter = self.filter_state.as_mut().unwrap();

            if reset_filter {
                // C++: filter[i] = rOffset + rc1*filter[i] + rc2*data[i]
                let r_offset = fc.r_offset;
                let rc1 = fc.rc[0];
                let rc2 = fc.rc[1];
                for i in 0..n {
                    let new_filter = r_offset + rc1 * filter[i] + rc2 * values[i];
                    filter[i] = new_filter;
                }
                self.num_filtered = 0;
            }

            // Increment filtered count (C++: if (numFiltered < numFilter) numFiltered++)
            if self.num_filtered < fc.num_filter {
                self.num_filtered += 1;
            }

            // Compute effective coefficients (depend on numFiltered)
            let nf = self.num_filtered as f64;
            let o1 = fc.o_scale * (fc.oc[0] + fc.oc[1] / nf);
            let o2 = fc.o_scale * (fc.oc[2] + fc.oc[3] / nf);
            let f1 = fc.f_scale * (fc.fc[0] + fc.fc[1] / nf);
            let f2 = fc.f_scale * (fc.fc[2] + fc.fc[3] / nf);
            let o_offset = fc.o_offset;
            let f_offset = fc.f_offset;

            // C++ NDPluginProcess.cpp:220-227 doProcess:
            //   newData   = oOffset + O1*filter[i] + O2*data[i];
            //   newFilter = fOffset + F1*filter[i] + F2*data[i];
            //   data[i]   = newData;
            //   filter[i] = newFilter;
            // Both newData AND newFilter are computed from the ORIGINAL
            // data[i]; data[i] = newData is assigned only afterward. So the
            // filter-state update must use the original input, not new_data.
            for i in 0..n {
                let new_data = o_offset + o1 * filter[i] + o2 * values[i];
                let new_filter = f_offset + f1 * filter[i] + f2 * values[i];
                values[i] = new_data;
                filter[i] = new_filter;
            }

            // Suppress output if filterCallbacks is set and we haven't reached
            // numFilter. C++ sets doCallbacks = 0 and does NOT call
            // endProcessCallbacks — the frame is dropped, nothing goes
            // downstream (the unprocessed input is NOT forwarded).
            if fc.filter_callbacks > 0 && self.num_filtered != fc.num_filter {
                return None;
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

        // Arm auto offset/scale from THIS frame's data for the NEXT frame
        // (C NDPluginProcess.cpp:238-250 runs after the output array is built).
        // Only on the emitted-output path: a suppressed frame produces no output
        // array, so C (pArrayOut == NULL) does not arm either.
        if auto_offset_scale_now {
            self.auto_offset_scale(src);
        }

        Some(arr)
    }
}

// --- ProcessProcessor (NDPluginProcess-based) ---

/// Param indices for the process plugin.
#[derive(Default)]
struct ProcParamIndices {
    data_type: Option<usize>,
    save_background: Option<usize>,
    enable_background: Option<usize>,
    valid_background: Option<usize>,
    save_flat_field: Option<usize>,
    enable_flat_field: Option<usize>,
    valid_flat_field: Option<usize>,
    scale_flat_field: Option<usize>,
    enable_offset_scale: Option<usize>,
    auto_offset_scale: Option<usize>,
    offset: Option<usize>,
    scale: Option<usize>,
    enable_low_clip: Option<usize>,
    low_clip_thresh: Option<usize>,
    low_clip_value: Option<usize>,
    enable_high_clip: Option<usize>,
    high_clip_thresh: Option<usize>,
    high_clip_value: Option<usize>,
    enable_filter: Option<usize>,
    filter_type: Option<usize>,
    reset_filter: Option<usize>,
    auto_reset_filter: Option<usize>,
    filter_callbacks: Option<usize>,
    num_filter: Option<usize>,
    num_filtered: Option<usize>,
    o_offset: Option<usize>,
    o_scale: Option<usize>,
    oc: [Option<usize>; 4],
    f_offset: Option<usize>,
    f_scale: Option<usize>,
    fc: [Option<usize>; 4],
    r_offset: Option<usize>,
    rc: [Option<usize>; 2],
}

/// ProcessProcessor wraps existing ProcessState.
pub struct ProcessProcessor {
    state: ProcessState,
    params: ProcParamIndices,
}

impl ProcessProcessor {
    pub fn new(config: ProcessConfig) -> Self {
        Self {
            state: ProcessState::new(config),
            params: ProcParamIndices::default(),
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
        use ad_core_rs::plugin::runtime::ParamUpdate;

        let out = self.state.process(array);
        // A suppressed frame (filter_callbacks) produces no output array but
        // still publishes readback params.
        let mut result = match out {
            Some(arr) => ProcessResult::arrays(vec![Arc::new(arr)]),
            None => ProcessResult::sink(vec![]),
        };

        // Push readback params
        if let Some(idx) = self.params.valid_background {
            result.param_updates.push(ParamUpdate::int32(
                idx,
                if self.state.config.valid_background {
                    1
                } else {
                    0
                },
            ));
        }
        if let Some(idx) = self.params.valid_flat_field {
            result.param_updates.push(ParamUpdate::int32(
                idx,
                if self.state.config.valid_flat_field {
                    1
                } else {
                    0
                },
            ));
        }
        if let Some(idx) = self.params.num_filtered {
            result
                .param_updates
                .push(ParamUpdate::int32(idx, self.state.num_filtered as i32));
        }
        // Reset save_background/save_flat_field readback to 0 (one-shot)
        if let Some(idx) = self.params.save_background {
            result.param_updates.push(ParamUpdate::int32(idx, 0));
        }
        if let Some(idx) = self.params.save_flat_field {
            result.param_updates.push(ParamUpdate::int32(idx, 0));
        }

        result
    }

    fn plugin_type(&self) -> &str {
        "NDPluginProcess"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        base.create_param("PROCESS_DATA_TYPE", ParamType::Int32)?;
        base.create_param("SAVE_BACKGROUND", ParamType::Int32)?;
        base.create_param("ENABLE_BACKGROUND", ParamType::Int32)?;
        base.create_param("VALID_BACKGROUND", ParamType::Int32)?;
        base.create_param("SAVE_FLAT_FIELD", ParamType::Int32)?;
        base.create_param("ENABLE_FLAT_FIELD", ParamType::Int32)?;
        base.create_param("VALID_FLAT_FIELD", ParamType::Int32)?;
        base.create_param("SCALE_FLAT_FIELD", ParamType::Float64)?;
        base.create_param("ENABLE_OFFSET_SCALE", ParamType::Int32)?;
        base.create_param("AUTO_OFFSET_SCALE", ParamType::Int32)?;
        base.create_param("OFFSET", ParamType::Float64)?;
        base.create_param("SCALE", ParamType::Float64)?;
        base.create_param("ENABLE_LOW_CLIP", ParamType::Int32)?;
        base.create_param("LOW_CLIP_THRESH", ParamType::Float64)?;
        base.create_param("LOW_CLIP_VALUE", ParamType::Float64)?;
        base.create_param("ENABLE_HIGH_CLIP", ParamType::Int32)?;
        base.create_param("HIGH_CLIP_THRESH", ParamType::Float64)?;
        base.create_param("HIGH_CLIP_VALUE", ParamType::Float64)?;
        base.create_param("ENABLE_FILTER", ParamType::Int32)?;
        base.create_param("FILTER_TYPE", ParamType::Int32)?;
        base.create_param("RESET_FILTER", ParamType::Int32)?;
        base.create_param("AUTO_RESET_FILTER", ParamType::Int32)?;
        base.create_param("FILTER_CALLBACKS", ParamType::Int32)?;
        base.create_param("NUM_FILTER", ParamType::Int32)?;
        base.create_param("NUM_FILTERED", ParamType::Int32)?;
        base.create_param("FILTER_OOFFSET", ParamType::Float64)?;
        base.create_param("FILTER_OSCALE", ParamType::Float64)?;
        base.create_param("FILTER_OC1", ParamType::Float64)?;
        base.create_param("FILTER_OC2", ParamType::Float64)?;
        base.create_param("FILTER_OC3", ParamType::Float64)?;
        base.create_param("FILTER_OC4", ParamType::Float64)?;
        base.create_param("FILTER_FOFFSET", ParamType::Float64)?;
        base.create_param("FILTER_FSCALE", ParamType::Float64)?;
        base.create_param("FILTER_FC1", ParamType::Float64)?;
        base.create_param("FILTER_FC2", ParamType::Float64)?;
        base.create_param("FILTER_FC3", ParamType::Float64)?;
        base.create_param("FILTER_FC4", ParamType::Float64)?;
        base.create_param("FILTER_ROFFSET", ParamType::Float64)?;
        base.create_param("FILTER_RC1", ParamType::Float64)?;
        base.create_param("FILTER_RC2", ParamType::Float64)?;

        // Look up param indices
        self.params.data_type = base.find_param("PROCESS_DATA_TYPE");
        self.params.save_background = base.find_param("SAVE_BACKGROUND");
        self.params.enable_background = base.find_param("ENABLE_BACKGROUND");
        self.params.valid_background = base.find_param("VALID_BACKGROUND");
        self.params.save_flat_field = base.find_param("SAVE_FLAT_FIELD");
        self.params.enable_flat_field = base.find_param("ENABLE_FLAT_FIELD");
        self.params.valid_flat_field = base.find_param("VALID_FLAT_FIELD");
        self.params.scale_flat_field = base.find_param("SCALE_FLAT_FIELD");
        self.params.enable_offset_scale = base.find_param("ENABLE_OFFSET_SCALE");
        self.params.auto_offset_scale = base.find_param("AUTO_OFFSET_SCALE");
        self.params.offset = base.find_param("OFFSET");
        self.params.scale = base.find_param("SCALE");
        self.params.enable_low_clip = base.find_param("ENABLE_LOW_CLIP");
        self.params.low_clip_thresh = base.find_param("LOW_CLIP_THRESH");
        self.params.low_clip_value = base.find_param("LOW_CLIP_VALUE");
        self.params.enable_high_clip = base.find_param("ENABLE_HIGH_CLIP");
        self.params.high_clip_thresh = base.find_param("HIGH_CLIP_THRESH");
        self.params.high_clip_value = base.find_param("HIGH_CLIP_VALUE");
        self.params.enable_filter = base.find_param("ENABLE_FILTER");
        self.params.filter_type = base.find_param("FILTER_TYPE");
        self.params.reset_filter = base.find_param("RESET_FILTER");
        self.params.auto_reset_filter = base.find_param("AUTO_RESET_FILTER");
        self.params.filter_callbacks = base.find_param("FILTER_CALLBACKS");
        self.params.num_filter = base.find_param("NUM_FILTER");
        self.params.num_filtered = base.find_param("NUM_FILTERED");
        self.params.o_offset = base.find_param("FILTER_OOFFSET");
        self.params.o_scale = base.find_param("FILTER_OSCALE");
        self.params.oc[0] = base.find_param("FILTER_OC1");
        self.params.oc[1] = base.find_param("FILTER_OC2");
        self.params.oc[2] = base.find_param("FILTER_OC3");
        self.params.oc[3] = base.find_param("FILTER_OC4");
        self.params.f_offset = base.find_param("FILTER_FOFFSET");
        self.params.f_scale = base.find_param("FILTER_FSCALE");
        self.params.fc[0] = base.find_param("FILTER_FC1");
        self.params.fc[1] = base.find_param("FILTER_FC2");
        self.params.fc[2] = base.find_param("FILTER_FC3");
        self.params.fc[3] = base.find_param("FILTER_FC4");
        self.params.r_offset = base.find_param("FILTER_ROFFSET");
        self.params.rc[0] = base.find_param("FILTER_RC1");
        self.params.rc[1] = base.find_param("FILTER_RC2");
        Ok(())
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        use ad_core_rs::plugin::runtime::{ParamChangeResult, ParamUpdate};

        let s = &mut self.state;
        let p = &self.params;
        let mut updates = Vec::new();

        if Some(reason) == p.data_type {
            let v = params.value.as_i32();
            s.config.output_type = if v < 0 {
                None // Automatic
            } else {
                NDDataType::from_ordinal(v as u8)
            };
        } else if Some(reason) == p.save_background {
            if params.value.as_i32() != 0 {
                s.config.save_background = true;
            }
        } else if Some(reason) == p.enable_background {
            s.config.enable_background = params.value.as_i32() != 0;
        } else if Some(reason) == p.save_flat_field {
            if params.value.as_i32() != 0 {
                s.config.save_flat_field = true;
            }
        } else if Some(reason) == p.enable_flat_field {
            s.config.enable_flat_field = params.value.as_i32() != 0;
        } else if Some(reason) == p.scale_flat_field {
            s.config.scale_flat_field = params.value.as_f64();
        } else if Some(reason) == p.enable_offset_scale {
            s.config.enable_offset_scale = params.value.as_i32() != 0;
        } else if Some(reason) == p.auto_offset_scale {
            if params.value.as_i32() != 0 {
                // Arm the one-shot: auto_offset_scale() runs on the next
                // process() call (it needs an NDArray to read the data
                // range). C++ resets NDPluginProcessAutoOffsetScale to 0
                // after handling, so echo a 0 readback here.
                s.config.auto_offset_scale_pending = true;
                if let Some(idx) = p.auto_offset_scale {
                    updates.push(ParamUpdate::int32(idx, 0));
                }
            }
        } else if Some(reason) == p.offset {
            s.config.offset = params.value.as_f64();
        } else if Some(reason) == p.scale {
            s.config.scale = params.value.as_f64();
        } else if Some(reason) == p.enable_low_clip {
            s.config.enable_low_clip = params.value.as_i32() != 0;
        } else if Some(reason) == p.low_clip_thresh {
            s.config.low_clip_thresh = params.value.as_f64();
        } else if Some(reason) == p.low_clip_value {
            s.config.low_clip_value = params.value.as_f64();
        } else if Some(reason) == p.enable_high_clip {
            s.config.enable_high_clip = params.value.as_i32() != 0;
        } else if Some(reason) == p.high_clip_thresh {
            s.config.high_clip_thresh = params.value.as_f64();
        } else if Some(reason) == p.high_clip_value {
            s.config.high_clip_value = params.value.as_f64();
        } else if Some(reason) == p.enable_filter {
            s.config.enable_filter = params.value.as_i32() != 0;
        } else if Some(reason) == p.filter_type {
            s.apply_filter_type(params.value.as_i32());
            s.reset_filter();
            // Push updated coefficients back
            let fc = &s.config.filter;
            for (i, idx) in p.fc.iter().enumerate() {
                if let Some(idx) = *idx {
                    updates.push(ParamUpdate::float64(idx, fc.fc[i]));
                }
            }
            for (i, idx) in p.oc.iter().enumerate() {
                if let Some(idx) = *idx {
                    updates.push(ParamUpdate::float64(idx, fc.oc[i]));
                }
            }
            for (i, idx) in p.rc.iter().enumerate() {
                if let Some(idx) = *idx {
                    updates.push(ParamUpdate::float64(idx, fc.rc[i]));
                }
            }
            if let Some(idx) = p.f_offset {
                updates.push(ParamUpdate::float64(idx, fc.f_offset));
            }
            if let Some(idx) = p.f_scale {
                updates.push(ParamUpdate::float64(idx, fc.f_scale));
            }
            if let Some(idx) = p.o_offset {
                updates.push(ParamUpdate::float64(idx, fc.o_offset));
            }
            if let Some(idx) = p.o_scale {
                updates.push(ParamUpdate::float64(idx, fc.o_scale));
            }
        } else if Some(reason) == p.reset_filter {
            if params.value.as_i32() != 0 {
                s.reset_filter();
                if let Some(idx) = p.num_filtered {
                    updates.push(ParamUpdate::int32(idx, 0));
                }
            }
        } else if Some(reason) == p.auto_reset_filter {
            s.config.filter.auto_reset = params.value.as_i32() != 0;
        } else if Some(reason) == p.filter_callbacks {
            s.config.filter.filter_callbacks = params.value.as_i32().max(0) as usize;
        } else if Some(reason) == p.num_filter {
            s.config.filter.num_filter = params.value.as_i32().max(1) as usize;
        } else if Some(reason) == p.o_offset {
            s.config.filter.o_offset = params.value.as_f64();
        } else if Some(reason) == p.o_scale {
            s.config.filter.o_scale = params.value.as_f64();
        } else if Some(reason) == p.f_offset {
            s.config.filter.f_offset = params.value.as_f64();
        } else if Some(reason) == p.f_scale {
            s.config.filter.f_scale = params.value.as_f64();
        } else if Some(reason) == p.r_offset {
            s.config.filter.r_offset = params.value.as_f64();
        } else {
            // Check individual OC/FC/RC params
            for i in 0..4 {
                if Some(reason) == p.oc[i] {
                    s.config.filter.oc[i] = params.value.as_f64();
                    return ParamChangeResult::updates(vec![]);
                }
                if Some(reason) == p.fc[i] {
                    s.config.filter.fc[i] = params.value.as_f64();
                    return ParamChangeResult::updates(vec![]);
                }
            }
            for i in 0..2 {
                if Some(reason) == p.rc[i] {
                    s.config.filter.rc[i] = params.value.as_f64();
                    return ParamChangeResult::updates(vec![]);
                }
            }
        }

        ParamChangeResult::updates(updates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataBuffer, NDDimension};

    fn make_array(vals: &[u8]) -> NDArray {
        let mut arr = NDArray::new(vec![NDDimension::new(vals.len())], NDDataType::UInt8);
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v.copy_from_slice(vals);
        }
        arr
    }

    fn make_f64_array(vals: &[f64]) -> NDArray {
        let mut arr = NDArray::new(vec![NDDimension::new(vals.len())], NDDataType::Float64);
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

        let result = state.process(&input).unwrap();
        if let NDDataBuffer::U8(ref v) = result.data {
            assert_eq!(v[0], 5);
            assert_eq!(v[1], 5);
            assert_eq!(v[2], 5);
        }
    }

    #[test]
    fn test_adp7_size_mismatched_background_invalidated_not_partial() {
        // C recomputes validBackground each frame as (pBackground && nElements ==
        // nBackgroundElements) (NDPluginProcess.cpp:121). A size mismatch
        // invalidates the whole buffer — it is NOT applied to the matching
        // prefix.
        let bg_arr = make_array(&[10, 20]); // 2 elements
        let input = make_array(&[15, 25, 35]); // 3 elements
        let mut state = ProcessState::new(ProcessConfig {
            enable_background: true,
            ..Default::default()
        });
        state.save_background(&bg_arr);
        assert!(state.config.valid_background); // set at save time (C writeInt32)

        let result = state.process(&input).unwrap();
        // Size mismatch → background ignored → output unchanged; valid recomputed
        // false at process time.
        assert!(!state.config.valid_background);
        if let NDDataBuffer::U8(ref v) = result.data {
            assert_eq!(v, &[15, 25, 35]);
        } else {
            panic!("expected U8 output");
        }
    }

    #[test]
    fn test_flat_field() {
        // C++: value *= scaleFlatField / flatField[i] (NDPluginProcess.cpp:172).
        // scaleFlatField is used directly (no mean substitution).
        let ff_arr = make_array(&[100, 200, 50]);
        let input = make_array(&[100, 100, 100]);

        let mut state = ProcessState::new(ProcessConfig {
            enable_flat_field: true,
            scale_flat_field: 100.0,
            ..Default::default()
        });
        state.save_flat_field(&ff_arr);

        let result = state.process(&input).unwrap();
        if let NDDataBuffer::U8(ref v) = result.data {
            assert_eq!(v[0], 100); // 100*100/100
            assert_eq!(v[1], 50); //  100*100/200
            assert_eq!(v[2], 200); // 100*100/50
        } else {
            panic!("expected U8 output");
        }
    }

    #[test]
    fn test_adp24_scale_flat_field_zero_zeroes_output() {
        // C uses scaleFlatField directly: value *= scaleFlatField/flatField[i].
        // With scaleFlatField == 0 every pixel (whose flatField != 0) becomes 0
        // — there is NO mean substitution (NDPluginProcess.cpp:171-172).
        let ff_arr = make_array(&[100, 200, 50]);
        let input = make_array(&[100, 100, 100]);
        let mut state = ProcessState::new(ProcessConfig {
            enable_flat_field: true,
            scale_flat_field: 0.0,
            ..Default::default()
        });
        state.save_flat_field(&ff_arr);
        let result = state.process(&input).unwrap();
        if let NDDataBuffer::U8(ref v) = result.data {
            assert_eq!(v, &[0, 0, 0]);
        } else {
            panic!("expected U8 output");
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

        let result = state.process(&input).unwrap();
        if let NDDataBuffer::U8(ref v) = result.data {
            // C++: value = (value + offset) * scale
            assert_eq!(v[0], 30); // (10+5)*2
            assert_eq!(v[1], 50); // (20+5)*2
            assert_eq!(v[2], 70); // (30+5)*2
        }
    }

    #[test]
    fn test_clipping() {
        let input = make_array(&[5, 50, 200]);
        let mut state = ProcessState::new(ProcessConfig {
            enable_low_clip: true,
            low_clip_thresh: 10.0,
            low_clip_value: 10.0,
            enable_high_clip: true,
            high_clip_thresh: 100.0,
            high_clip_value: 100.0,
            ..Default::default()
        });

        let result = state.process(&input).unwrap();
        if let NDDataBuffer::U8(ref v) = result.data {
            assert_eq!(v[0], 10); // clipped up
            assert_eq!(v[1], 50); // unchanged
            assert_eq!(v[2], 100); // clipped down
        }
    }

    #[test]
    fn test_adp5_clip_order_high_before_low() {
        // C applies high-clip THEN low-clip (NDPluginProcess.cpp:175-176). With
        // crossing thresholds (high < low) the order is observable:
        //   v=200 → high(>100 ⇒ 10) → low(<50 ⇒ 999) ⇒ 999
        // Low-then-high would instead give 200 → (not <50) → high(>100 ⇒ 10) ⇒ 10.
        let input = make_f64_array(&[200.0]);
        let mut state = ProcessState::new(ProcessConfig {
            enable_high_clip: true,
            high_clip_thresh: 100.0,
            high_clip_value: 10.0,
            enable_low_clip: true,
            low_clip_thresh: 50.0,
            low_clip_value: 999.0,
            ..Default::default()
        });
        let result = state.process(&input).unwrap();
        if let NDDataBuffer::F64(ref v) = result.data {
            assert_eq!(v[0], 999.0);
        } else {
            panic!("expected F64 output");
        }
    }

    #[test]
    fn test_recursive_filter() {
        // Test a simple recursive filter: filter = 0.5*filter + 0.5*data, output = filter
        // Using C++ coefficient scheme:
        //   F1 = fScale*(fc1+fc2/N), F2 = fScale*(fc3+fc4/N)
        //   For constant F1=0.5, F2=0.5 regardless of N:
        //   fc1=0.5, fc2=0, fc3=0.5, fc4=0
        let input1 = make_array(&[100, 100, 100]);
        let input2 = make_array(&[0, 0, 0]);

        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                num_filter: 10,
                fc: [0.5, 0.0, 0.5, 0.0], // F1=0.5, F2=0.5
                oc: [1.0, 0.0, 0.0, 0.0], // O1=1, O2=0
                rc: [0.0, 1.0],           // reset: filter = data
                ..Default::default()
            },
            ..Default::default()
        });

        // C++ NDPluginProcess.cpp:220-227 doProcess recurrence:
        //   newData   = oOffset + O1*filter[i] + O2*data[i];
        //   newFilter = fOffset + F1*filter[i] + F2*data[i];  // ORIGINAL data[i]
        //   data[i]   = newData;
        //   filter[i] = newFilter;
        //
        // Frame 0: reset: filter = 0 + 0*100 + 1*100 = 100
        // N=1: F1=0.5, F2=0.5, O1=1, O2=0
        // data   = 0 + 1*100 + 0*100 = 100
        // filter = 0 + 0.5*100 + 0.5*100(orig data) = 100
        let _ = state.process(&input1);

        // Frame 1: data=0, filter=100
        // N=2: F1=0.5, F2=0.5, O1=1, O2=0
        // data   = 0 + 1*100 + 0*0 = 100
        // filter = 0 + 0.5*100 + 0.5*0(orig data) = 50
        let result = state.process(&input2).unwrap();
        if let NDDataBuffer::U8(ref v) = result.data {
            // Output is data = O1*filter = 1*100 = 100
            assert_eq!(v[0], 100);
            assert_eq!(v[1], 100);
        }
    }

    #[test]
    fn test_output_type_conversion() {
        let input = make_array(&[10, 20, 30]);
        let mut state = ProcessState::new(ProcessConfig {
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        let result = state.process(&input).unwrap();
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
            assert_eq!(v[0], 22); // (10+1)*2 = 22 (C++: offset first, then scale)
        }
    }

    // --- New Phase 2-1 tests ---

    #[test]
    fn test_filter_sum_preset() {
        // Sum preset: filter = filter + data, output = filter
        // fc=[1,0,1,0], oc=[1,0,0,0], rc=[0,1]
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                num_filter: 10,
                fc: [1.0, 0.0, 1.0, 0.0],
                oc: [1.0, 0.0, 0.0, 0.0],
                rc: [0.0, 1.0],
                ..Default::default()
            },
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        // C++ NDPluginProcess.cpp:220-227 doProcess (newFilter uses ORIGINAL data[i]):
        //   newData   = oOffset + O1*filter[i] + O2*data[i];
        //   newFilter = fOffset + F1*filter[i] + F2*data[i];
        //   data[i]   = newData; filter[i] = newFilter;
        //
        // Frame 0: reset first: filter = rOffset + rc1*filter + rc2*data
        //          = 0 + 0*100 + 1*100 = 100. Then N increments to 1, normal path:
        // F1=fScale*(fc1+fc2/N)=1*(1+0/1)=1, F2=fScale*(fc3+fc4/N)=1*(1+0/1)=1
        // O1=oScale*(oc1+oc2/N)=1*(1+0/1)=1, O2=oScale*(oc3+oc4/N)=1*(0+0/1)=0
        // data   = oOffset + O1*filter + O2*data = 0 + 1*100 + 0*100 = 100
        // filter = fOffset + F1*filter + F2*data(orig=100) = 0 + 1*100 + 1*100 = 200
        let r0 = state.process(&make_f64_array(&[100.0])).unwrap();
        let v0 = r0.data.get_as_f64(0).unwrap();
        assert!((v0 - 100.0).abs() < 1e-9, "frame 0: got {v0}");

        // Frame 1: data=100, filter=200 (from prev)
        // N increments to 2
        // F1=1*(1+0/2)=1, F2=1*(1+0/2)=1
        // O1=1*(1+0/2)=1, O2=0
        // data   = 0 + 1*200 + 0*100 = 200
        // filter = 0 + 1*200 + 1*data(orig=100) = 300
        let r1 = state.process(&make_f64_array(&[100.0])).unwrap();
        let v1 = r1.data.get_as_f64(0).unwrap();
        assert!((v1 - 200.0).abs() < 1e-9, "frame 1: got {v1}");
    }

    #[test]
    fn test_filter_average_preset() {
        // Average preset: accumulate in filter, output = filter/N
        // fc=[1,0,1,0], oc=[0,1,0,0], rc=[0,1]
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                num_filter: 10,
                fc: [1.0, 0.0, 1.0, 0.0],
                oc: [0.0, 1.0, 0.0, 0.0],
                rc: [0.0, 1.0],
                ..Default::default()
            },
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        // C++ NDPluginProcess.cpp:220-227 doProcess (newFilter uses ORIGINAL data[i]):
        //   newData   = oOffset + O1*filter[i] + O2*data[i];
        //   newFilter = fOffset + F1*filter[i] + F2*data[i];
        //   data[i]   = newData; filter[i] = newFilter;
        //
        // Frame 0 (reset): filter=100. N=1: O1=oScale*(0+1/1)=1, O2=0
        // data   = 0 + 1*100 + 0 = 100
        // filter = 0 + 1*100 + 1*100(orig data) = 200
        let r0 = state.process(&make_f64_array(&[100.0])).unwrap();
        let v0 = r0.data.get_as_f64(0).unwrap();
        assert!((v0 - 100.0).abs() < 1e-9, "frame 0: got {v0}");

        // Frame 1: data=200, filter=200
        // N=2: O1=oScale*(0+1/2)=0.5, O2=0
        // data   = 0 + 0.5*200 + 0 = 100
        // filter = 0 + 1*200 + 1*200(orig data) = 400
        let r1 = state.process(&make_f64_array(&[200.0])).unwrap();
        let v1 = r1.data.get_as_f64(0).unwrap();
        assert!((v1 - 100.0).abs() < 1e-9, "frame 1: got {v1}");

        // Frame 2: data=300, filter=400
        // N=3: O1=1/3, O2=0
        // data   = 0 + (1/3)*400 + 0 = 400/3
        // filter = 0 + 1*400 + 1*300(orig data) = 700
        let r2 = state.process(&make_f64_array(&[300.0])).unwrap();
        let v2 = r2.data.get_as_f64(0).unwrap();
        let expected = 400.0 / 3.0;
        assert!((v2 - expected).abs() < 1e-9, "frame 2: got {v2}");
    }

    #[test]
    fn test_filter_recursive_ave() {
        // RecursiveAve preset matching C++ behavior
        // fc=[1,-1,0,1], oc=[1,0,0,0], rc=[0,1]
        // F1=fScale*(1+(-1)/N)=(N-1)/N, F2=fScale*(0+1/N)=1/N
        // O1=oScale*(1+0/N)=1, O2=0
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                num_filter: 10,
                fc: [1.0, -1.0, 0.0, 1.0],
                oc: [1.0, 0.0, 0.0, 0.0],
                rc: [0.0, 1.0],
                ..Default::default()
            },
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        // C++ NDPluginProcess.cpp:220-227 doProcess (newFilter uses ORIGINAL data[i]):
        //   newData   = oOffset + O1*filter[i] + O2*data[i];
        //   newFilter = fOffset + F1*filter[i] + F2*data[i];
        //   data[i]   = newData; filter[i] = newFilter;
        // With O2=0, newData == O1*filter == filter, and the filter update
        // newFilter = F1*filter + F2*data(orig) tracks the original input.
        //
        // Frame 0: reset filter=100, N=1
        // F1=1*(1-1/1)=0, F2=1*(0+1/1)=1, O1=1*(1+0/1)=1
        // data   = 0 + 1*100 + 0*100 = 100
        // filter = 0 + 0*100 + 1*100(orig data) = 100
        let r0 = state.process(&make_f64_array(&[100.0])).unwrap();
        let v0 = r0.data.get_as_f64(0).unwrap();
        assert!((v0 - 100.0).abs() < 1e-9, "frame 0: got {v0}");

        // Frame 1: data=200, filter=100, N=2
        // F1=(2-1)/2=0.5, F2=1/2=0.5
        // data   = 0 + 1*100 + 0*200 = 100
        // filter = 0 + 0.5*100 + 0.5*200(orig data) = 150
        let r1 = state.process(&make_f64_array(&[200.0])).unwrap();
        let v1 = r1.data.get_as_f64(0).unwrap();
        assert!((v1 - 100.0).abs() < 1e-9, "frame 1: got {v1}");

        // Frame 2: data=300, filter=150, N=3
        // F1=2/3, F2=1/3, O1=1
        // data   = 0 + 1*150 + 0*300 = 150
        // filter = (2/3)*150 + (1/3)*300(orig data) = 100 + 100 = 200
        let r2 = state.process(&make_f64_array(&[300.0])).unwrap();
        let v2 = r2.data.get_as_f64(0).unwrap();
        assert!((v2 - 150.0).abs() < 1e-9, "frame 2: got {v2}");
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

        assert!(
            !state.config.save_background,
            "save_background should be cleared"
        );
        assert!(
            state.config.valid_background,
            "valid_background should be set"
        );
        assert!(state.background.is_some());

        let bg = state.background.as_ref().unwrap();
        assert_eq!(bg.len(), 3);
        assert!((bg[0] - 10.0).abs() < 1e-9);
        assert!((bg[1] - 20.0).abs() < 1e-9);
        assert!((bg[2] - 30.0).abs() < 1e-9);

        // Process again: flag should remain cleared, background should persist
        let input2 = make_array(&[40, 50, 60]);
        let _ = state.process(&input2);

        assert!(
            !state.config.save_background,
            "save_background stays cleared"
        );
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

        assert!(
            !state.config.save_flat_field,
            "save_flat_field should be cleared"
        );
        assert!(
            state.config.valid_flat_field,
            "valid_flat_field should be set"
        );
        assert!(state.flat_field.is_some());

        let ff = state.flat_field.as_ref().unwrap();
        assert_eq!(ff.len(), 3);
        assert!((ff[0] - 50.0).abs() < 1e-9);
        assert!((ff[1] - 100.0).abs() < 1e-9);
        assert!((ff[2] - 150.0).abs() < 1e-9);
    }

    #[test]
    fn test_auto_reset_when_num_filter_reached() {
        // Sum filter with auto_reset after 3 frames
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                num_filter: 3,
                auto_reset: true,
                fc: [1.0, 0.0, 1.0, 0.0], // sum preset
                oc: [1.0, 0.0, 0.0, 0.0],
                rc: [0.0, 1.0],
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

        // Frame 2: num_filtered becomes 3 = num_filter, triggers auto_reset on next
        let _ = state.process(&make_f64_array(&[100.0]));
        assert_eq!(state.num_filtered, 3);

        // Frame 3: auto_reset fires (num_filtered >= num_filter), filter is reset
        let _ = state.process(&make_f64_array(&[200.0]));
        // After reset + processing, num_filtered should be 1
        assert_eq!(state.num_filtered, 1, "fresh start after auto reset");
    }

    #[test]
    fn test_filter_with_offset_scale() {
        // Test that f_offset/f_scale and o_offset/o_scale are applied in C++ manner:
        // F1 = fScale * (fc1 + fc2/N), O1 = oScale * (oc1 + oc2/N)
        // CopyToFilter: fc=[0,0,1,0], oc=[1,0,0,0]
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                num_filter: 10,
                fc: [0.0, 0.0, 1.0, 0.0], // F1=0, F2=fScale*1
                oc: [1.0, 0.0, 0.0, 0.0], // O1=oScale*1, O2=0
                rc: [0.0, 1.0],
                f_offset: 10.0,
                f_scale: 2.0,
                o_offset: 5.0,
                o_scale: 3.0,
                ..Default::default()
            },
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        // C++ NDPluginProcess.cpp:220-227 doProcess (newFilter uses ORIGINAL data[i]):
        //   newData   = oOffset + O1*filter[i] + O2*data[i];
        //   newFilter = fOffset + F1*filter[i] + F2*data[i];
        //   data[i]   = newData; filter[i] = newFilter;
        //
        // Frame 0: reset: filter = 0 + 0*filter + 1*50 = 50
        // N=1: F1=2*(0+0/1)=0, F2=2*(1+0/1)=2, O1=3*(1+0/1)=3, O2=0
        // data   = 5 + 3*50 + 0 = 155
        // filter = 10 + 0*50 + 2*50(orig data) = 110
        let r0 = state.process(&make_f64_array(&[50.0])).unwrap();
        let v0 = r0.data.get_as_f64(0).unwrap();
        assert!((v0 - 155.0).abs() < 1e-9, "frame 0: got {v0}");

        // Frame 1: data=20, filter=110
        // N=2: F1=0, F2=2, O1=3, O2=0
        // data   = 5 + 3*110 + 0 = 335
        // filter = 10 + 0 + 2*20(orig data) = 50
        let r1 = state.process(&make_f64_array(&[20.0])).unwrap();
        let v1 = r1.data.get_as_f64(0).unwrap();
        assert!((v1 - 335.0).abs() < 1e-9, "frame 1: got {v1}");
    }

    #[test]
    fn test_reset_filter_manual() {
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                num_filter: 10,
                fc: [1.0, 0.0, 1.0, 0.0],
                oc: [1.0, 0.0, 0.0, 0.0],
                rc: [0.0, 1.0],
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
        assert_eq!(state.num_filtered, 0);

        // Next frame should act as first frame (reset mode)
        let _ = state.process(&make_f64_array(&[200.0]));
        assert_eq!(state.num_filtered, 1);
    }

    #[test]
    fn test_adp6_auto_offset_scale_arms_next_frame_not_trigger() {
        // C measures the trigger frame's min/max and ARMS scale/offset + clipping
        // for the NEXT frame; the trigger frame itself is emitted with the
        // pre-existing config (NDPluginProcess.cpp:164-178 measures only, 238-250
        // arms after the output array is built).
        let mut state = ProcessState::new(ProcessConfig {
            output_type: Some(NDDataType::UInt8),
            ..Default::default()
        });
        state.config.auto_offset_scale_pending = true;

        // Trigger frame: input range [10, 30]. Offset/scale were OFF going in, so
        // the frame is emitted UNSCALED — output == input converted to u8.
        let out1 = state.process(&make_f64_array(&[10.0, 20.0, 30.0])).unwrap();
        assert!(!state.config.auto_offset_scale_pending); // one-shot consumed
        if let NDDataBuffer::U8(v) = &out1.data {
            assert_eq!(v, &[10, 20, 30]); // trigger frame NOT transformed
        } else {
            panic!("expected u8 output");
        }
        // Params armed from the trigger frame for subsequent frames:
        //   offset=-10, scale=255/20=12.75, offset/scale + clipping enabled.
        assert!(state.config.enable_offset_scale);
        assert!((state.config.offset - (-10.0)).abs() < 1e-9);
        assert!((state.config.scale - 255.0 / 20.0).abs() < 1e-9);

        // NEXT frame IS transformed with the armed params: (v-10)*12.75, clipped.
        let out2 = state.process(&make_f64_array(&[10.0, 20.0, 30.0])).unwrap();
        if let NDDataBuffer::U8(v) = &out2.data {
            assert_eq!(v[0], 0); // (10-10)*12.75 = 0
            assert_eq!(v[2], 255); // (30-10)*12.75 = 255
        } else {
            panic!("expected u8 output");
        }
    }

    #[test]
    fn test_filter_callbacks_drops_suppressed_frame() {
        // Regression: with filter_callbacks set, a frame that has not yet
        // reached num_filter is dropped (process() returns None), not
        // forwarded as the raw input.
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                num_filter: 3,
                filter_callbacks: 1,
                fc: [1.0, 0.0, 1.0, 0.0],
                oc: [0.0, 1.0, 0.0, 0.0],
                rc: [0.0, 1.0],
                ..Default::default()
            },
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        // Frames 1 and 2 are below num_filter => suppressed (None).
        assert!(state.process(&make_f64_array(&[100.0])).is_none());
        assert!(state.process(&make_f64_array(&[100.0])).is_none());
        // Frame 3 reaches num_filter => output produced.
        assert!(state.process(&make_f64_array(&[100.0])).is_some());
    }

    #[test]
    fn test_filter_recurrence_matches_cpp() {
        // Regression: the filter-state update must read the ORIGINAL input
        // data[i], not the just-updated newData. C++ computes both newData
        // and newFilter from data[i] before assigning data[i] = newData.
        //
        // C++ NDPluginProcess.cpp:220-227 doProcess:
        //   newData   = oOffset + O1*filter[i] + O2*data[i];
        //   newFilter = fOffset + F1*filter[i] + F2*data[i];  // ORIGINAL data[i]
        //   data[i]   = newData;
        //   filter[i] = newFilter;
        //
        // Average preset: fc=[1,0,1,0], oc=[0,1,0,0], rc=[0,1].
        // O1=1/N, O2=0, F1=1, F2=1, all offsets/scales default (0/1).
        // With O2=0 and oc default, the C++ recurrence is:
        //   data[k]   = filter / N
        //   filter'   = filter + input   (F2 multiplies the ORIGINAL input)
        //
        // Hand-computed reference (inputs 100, 200, 300, 400):
        //   reset: filter = 100, N = 1
        //   k0: N=1  data = 100/1   = 100      filter = 100 + 100 = 200
        //   k1: N=2  data = 200/2   = 100      filter = 200 + 200 = 400
        //   k2: N=3  data = 400/3   = 133.333  filter = 400 + 300 = 700
        //   k3: N=4  data = 700/4   = 175      filter = 700 + 400 = 1100
        //
        // The STALE/new-data variant (the 650038bb regression) computed
        //   filter' = filter + newData
        // giving filter = 100,200,300,400 and data = 100,100,100,100 —
        // diverging from C++ from frame 1 onward.
        let mut state = ProcessState::new(ProcessConfig {
            enable_filter: true,
            filter: FilterConfig {
                num_filter: 100,
                fc: [1.0, 0.0, 1.0, 0.0],
                oc: [0.0, 1.0, 0.0, 0.0],
                rc: [0.0, 1.0],
                ..Default::default()
            },
            output_type: Some(NDDataType::Float64),
            ..Default::default()
        });

        let inputs = [100.0, 200.0, 300.0, 400.0];
        let expected_data = [100.0, 100.0, 400.0 / 3.0, 175.0];
        let expected_filter = [200.0, 400.0, 700.0, 1100.0];

        for k in 0..inputs.len() {
            let r = state.process(&make_f64_array(&[inputs[k]])).unwrap();
            let v = r.data.get_as_f64(0).unwrap();
            assert!(
                (v - expected_data[k]).abs() < 1e-9,
                "frame {k}: data got {v}, expected {}",
                expected_data[k]
            );
            let fs = state.filter_state.as_ref().unwrap()[0];
            assert!(
                (fs - expected_filter[k]).abs() < 1e-9,
                "frame {k}: filter got {fs}, expected {}",
                expected_filter[k]
            );
        }
    }
}
