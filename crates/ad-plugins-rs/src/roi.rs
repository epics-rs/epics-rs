use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamUpdate, PluginParamSnapshot, ProcessResult,
};
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;
use parking_lot::Mutex;

/// Per-dimension ROI configuration.
#[derive(Debug, Clone)]
pub struct ROIDimConfig {
    pub min: usize,
    pub size: usize,
    pub bin: usize,
    pub reverse: bool,
    pub enable: bool,
    /// If true, size is computed as src_dim - min.
    pub auto_size: bool,
}

impl Default for ROIDimConfig {
    fn default() -> Self {
        Self {
            min: 0,
            size: 0,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        }
    }
}

/// Auto-centering mode for ROI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoCenter {
    None,
    CenterOfMass,
    PeakPosition,
}

/// ROI plugin configuration.
#[derive(Debug, Clone)]
pub struct ROIConfig {
    pub dims: [ROIDimConfig; 3],
    pub data_type: Option<NDDataType>,
    pub enable_scale: bool,
    pub scale: f64,
    pub collapse_dims: bool,
    pub autocenter: AutoCenter,
}

impl Default for ROIConfig {
    fn default() -> Self {
        Self {
            dims: [
                ROIDimConfig::default(),
                ROIDimConfig::default(),
                ROIDimConfig::default(),
            ],
            data_type: None,
            enable_scale: false,
            scale: 1.0,
            collapse_dims: false,
            autocenter: AutoCenter::None,
        }
    }
}

/// Compute the centroid (center of mass) of a 2D image.
fn find_centroid_2d(data: &NDDataBuffer, x_size: usize, y_size: usize) -> (usize, usize) {
    let mut cx = 0.0f64;
    let mut cy = 0.0f64;
    let mut total = 0.0f64;
    for iy in 0..y_size {
        for ix in 0..x_size {
            let val = data.get_as_f64(iy * x_size + ix).unwrap_or(0.0);
            total += val;
            cx += val * ix as f64;
            cy += val * iy as f64;
        }
    }
    if total > 0.0 {
        ((cx / total) as usize, (cy / total) as usize)
    } else {
        (x_size / 2, y_size / 2)
    }
}

/// Find the position of the maximum value in a 2D image.
fn find_peak_2d(data: &NDDataBuffer, x_size: usize, y_size: usize) -> (usize, usize) {
    let mut max_val = f64::NEG_INFINITY;
    let mut max_x = 0;
    let mut max_y = 0;
    for iy in 0..y_size {
        for ix in 0..x_size {
            let val = data.get_as_f64(iy * x_size + ix).unwrap_or(0.0);
            if val > max_val {
                max_val = val;
                max_x = ix;
                max_y = iy;
            }
        }
    }
    (max_x, max_y)
}

/// Extract an ROI from an NDArray, dispatching on dimensionality.
///
/// 2-D arrays go through [`extract_roi_2d`]. 3-D color arrays
/// (RGB1/RGB2/RGB3) are handled by [`extract_roi_3d`], which — like C++
/// `NDPluginROI` via `userDims = {xDim, yDim, colorDim}` — keeps ROI Dim0/Dim1
/// bound to the image X/Y axes and Dim2 to the color axis regardless of the
/// physical dimension order.
pub fn extract_roi(src: &NDArray, config: &ROIConfig) -> Option<NDArray> {
    use ad_core_rs::color::NDColorMode;
    if src.dims.len() >= 3 {
        let info = src.info();
        if matches!(
            info.color_mode,
            NDColorMode::RGB1 | NDColorMode::RGB2 | NDColorMode::RGB3
        ) {
            return extract_roi_3d(src, config);
        }
    }
    extract_roi_2d(src, config)
}

/// Resolve one ROI axis to C's clamped `(offset, size, binning)` — C++
/// `NDPluginROI` (`NDPluginROI.cpp:85-103`).
///
/// C decides all THREE values in one branch, keyed on that axis's Enable flag:
///
/// * enabled (`:87-97`):
///   `offset = MAX(offset, 0); offset = MIN(offset, dimSize-1);`
///   `size = autoSize ? dimSize : size;`
///   `size = MAX(size, 1); size = MIN(size, dimSize - offset);`
///   `binning = MAX(binning, 1); binning = MIN(binning, size);`
/// * disabled (`:98-102`): `offset = 0; size = dimSize; binning = 1;`
///
/// The binning belongs to that same decision, so it is resolved here and nowhere
/// else. Deriving it separately in the callers is what let a *disabled* axis keep
/// a stale `DimNBin > 1` and emit a shrunken axis of bin-sums where C outputs the
/// full-resolution axis (R9-66).
fn resolve_axis(cfg: &ROIDimConfig, dim_size: usize) -> (usize, usize, usize) {
    if !cfg.enable || dim_size == 0 {
        return (0, dim_size, 1);
    }
    let offset = cfg.min.min(dim_size - 1);
    let size = if cfg.auto_size { dim_size } else { cfg.size };
    let size = size.max(1).min(dim_size - offset);
    // C++: binning = MAX(binning, 1); binning = MIN(binning, size). A bin larger
    // than the ROI is clamped to the ROI size, yielding a 1-pixel output rather
    // than collapsing to an empty (sink) result.
    let binning = cfg.bin.max(1).min(size);
    (offset, size, binning)
}

/// Run C's ROI extraction: `pNDArrayPool->convert()` with the ROI's
/// offset/size/binning/reverse per dimension (`NDPluginROI.cpp:166-175`).
///
/// The pixel math is NOT open-coded here — it is
/// [`ad_core_rs::convert::convert_dims`], the single owner of C's
/// `convertDim` semantics (bin sums accumulate in the OUTPUT type and wrap
/// modulo its width; casts are C casts, never clamped/saturated).
///
/// C picks between two paths, and the choice is observable:
/// * `enableScale && scale != 0 && scale != 1` (`:160`): convert to Float64
///   *first* (so the bin sum is exact), divide by scale, then cast to the
///   output type. This is the only path that avoids the modulo wrap.
/// * otherwise (`:174`): convert straight to the output type — the bin sum
///   accumulates in that type and wraps (UInt8 3x3 bin of 100s -> 900 % 256
///   == 132).
fn convert_roi(src: &NDArray, dims_out: &[NDDimension], config: &ROIConfig) -> Option<NDArray> {
    use ad_core_rs::convert::{convert_dims, convert_type};

    let target_type = config.data_type.unwrap_or(src.data.data_type());
    let scaled = config.enable_scale && config.scale != 0.0 && config.scale != 1.0;

    let result = if scaled {
        convert_dims(src, dims_out, NDDataType::Float64).and_then(|mut scratch| {
            if let NDDataBuffer::F64(v) = &mut scratch.data {
                for x in v.iter_mut() {
                    *x /= config.scale;
                }
            }
            convert_type(&scratch, target_type)
        })
    } else {
        convert_dims(src, dims_out, target_type)
    };

    match result {
        Ok(arr) => Some(arr),
        Err(e) => {
            // A conversion failure must NOT publish an all-zero buffer as if
            // it were valid ROI output. Drop the frame; the caller treats
            // `None` as "no output this frame".
            tracing::warn!(
                error = %e,
                from = ?src.data.data_type(),
                to = ?target_type,
                "ROI extraction failed; dropping frame"
            );
            None
        }
    }
}

/// Extract an ROI from a 3-D RGB color array.
///
/// Mirrors C++ `NDPluginROI`: ROI `dims[0]` selects the X axis, `dims[1]` the
/// Y axis and `dims[2]` the color axis (`userDims = {xDim, yDim, colorDim}`),
/// so the ROI geometry is independent of the RGB1/RGB2/RGB3 memory layout.
/// Per-axis binning and reverse are applied; the output keeps the source
/// color mode and dimension order.
pub fn extract_roi_3d(src: &NDArray, config: &ROIConfig) -> Option<NDArray> {
    let info = src.info();
    let (src_x, src_y, src_c) = (info.x_size, info.y_size, info.color_size.max(1));
    if src_x == 0 || src_y == 0 || src_c == 0 {
        return None;
    }

    let (x_min, x_roi, bin_x) = resolve_axis(&config.dims[0], src_x);
    let (y_min, y_roi, bin_y) = resolve_axis(&config.dims[1], src_y);
    let (c_min, c_roi, bin_c) = resolve_axis(&config.dims[2], src_c);

    let (out_x, out_y, out_c) = (x_roi / bin_x, y_roi / bin_y, c_roi / bin_c);
    if out_x == 0 || out_y == 0 || out_c == 0 {
        return None;
    }

    // C `userDims = {xDim, yDim, colorDim}` (NDPluginROI.cpp:80-82): the ROI
    // axes are written into the ARRAY dimension slots the image info reports,
    // so the geometry is layout-independent.
    let user_dims = info.user_dims();
    let mut dims_out: Vec<NDDimension> = src.dims.clone();
    for d in dims_out.iter_mut() {
        d.offset = 0;
        d.binning = 1;
        d.reverse = false;
    }
    for (roi_dim, (min, size, bin)) in [
        (x_min, x_roi, bin_x),
        (y_min, y_roi, bin_y),
        (c_min, c_roi, bin_c),
    ]
    .into_iter()
    .enumerate()
    {
        let d = dims_out.get_mut(user_dims[roi_dim])?;
        d.offset = min;
        d.size = size;
        d.binning = bin;
        d.reverse = config.dims[roi_dim].reverse;
    }

    let mut arr = convert_roi(src, &dims_out, config)?;

    // Single-color selection: when the color axis collapses to 1 and the
    // source is an RGB mode, C forces collapseDims and tags the output Mono
    // (NDPluginROI.cpp:177-200). The user collapseDims param otherwise still
    // collapses any size-1 dimension (NDPluginROI.cpp:202-215). When out_c == 1
    // the extracted buffer is already in [x, y] row-major order for every RGB
    // layout, so dropping the size-1 axes needs no data reordering.
    let single_color = out_c == 1
        && matches!(
            info.color_mode,
            ad_core_rs::color::NDColorMode::RGB1
                | ad_core_rs::color::NDColorMode::RGB2
                | ad_core_rs::color::NDColorMode::RGB3
        );
    if single_color || config.collapse_dims {
        let collapsed: Vec<NDDimension> = arr.dims.iter().filter(|d| d.size > 1).cloned().collect();
        arr.dims = if collapsed.is_empty() {
            vec![NDDimension::new(1)]
        } else {
            collapsed
        };
    }

    arr.unique_id = src.unique_id;
    // A single selected color plane is mono (C NDPluginROI.cpp:185/192/199
    // overrides the ColorMode attribute on the collapsed output).
    if single_color {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "Color mode",
            NDAttrSource::Driver,
            NDAttrValue::Int32(ad_core_rs::color::NDColorMode::Mono as i32),
        ));
    }
    Some(arr)
}

/// Extract ROI sub-region from a 2-D (mono) array.
pub fn extract_roi_2d(src: &NDArray, config: &ROIConfig) -> Option<NDArray> {
    if src.dims.len() < 2 {
        return None;
    }

    let src_x = src.dims[0].size;
    let src_y = src.dims[1].size;

    let (eff_x_min, eff_x_size, bin_x) = resolve_axis(&config.dims[0], src_x);
    let (eff_y_min, eff_y_size, bin_y) = resolve_axis(&config.dims[1], src_y);

    // Apply autocenter: shift ROI min so that the ROI is centered on the
    // centroid or peak, keeping the effective size the same.
    let (roi_x_min, roi_y_min) = match config.autocenter {
        AutoCenter::None => (eff_x_min, eff_y_min),
        AutoCenter::CenterOfMass => {
            let (cx, cy) = find_centroid_2d(&src.data, src_x, src_y);
            let mx = cx
                .saturating_sub(eff_x_size / 2)
                .min(src_x.saturating_sub(eff_x_size));
            let my = cy
                .saturating_sub(eff_y_size / 2)
                .min(src_y.saturating_sub(eff_y_size));
            (mx, my)
        }
        AutoCenter::PeakPosition => {
            let (px, py) = find_peak_2d(&src.data, src_x, src_y);
            let mx = px
                .saturating_sub(eff_x_size / 2)
                .min(src_x.saturating_sub(eff_x_size));
            let my = py
                .saturating_sub(eff_y_size / 2)
                .min(src_y.saturating_sub(eff_y_size));
            (mx, my)
        }
    };

    if eff_x_size == 0 || eff_y_size == 0 {
        return None;
    }

    if eff_x_size / bin_x == 0 || eff_y_size / bin_y == 0 {
        return None;
    }

    // Dimensions beyond X/Y (a non-RGB array with ndims > 2) pass through
    // whole and unbinned, as C's `for (dim=0; dim<pArray->ndims; dim++)` loop
    // leaves any dimension without an ROI axis.
    let mut dims_out: Vec<NDDimension> = src.dims.clone();
    for d in dims_out.iter_mut() {
        d.offset = 0;
        d.binning = 1;
        d.reverse = false;
    }
    dims_out[0] = NDDimension {
        size: eff_x_size,
        offset: roi_x_min,
        binning: bin_x,
        reverse: config.dims[0].reverse,
    };
    dims_out[1] = NDDimension {
        size: eff_y_size,
        offset: roi_y_min,
        binning: bin_y,
        reverse: config.dims[1].reverse,
    };

    let mut arr = convert_roi(src, &dims_out, config)?;

    if config.collapse_dims {
        let collapsed: Vec<NDDimension> = arr.dims.iter().filter(|d| d.size > 1).cloned().collect();
        arr.dims = if collapsed.is_empty() {
            vec![NDDimension::new(arr.dims[0].size)]
        } else {
            collapsed
        };
    }

    arr.unique_id = src.unique_id;
    Some(arr)
}

/// Per-dimension param reasons.
#[derive(Default, Clone, Copy)]
pub struct ROIDimParams {
    pub min: usize,
    pub size: usize,
    pub bin: usize,
    pub reverse: usize,
    pub enable: usize,
    pub auto_size: usize,
    pub max_size: usize,
}

/// Param reasons for all ROI params.
#[derive(Default)]
pub struct ROIParams {
    pub dims: [ROIDimParams; 3],
    pub enable_scale: usize,
    pub scale: usize,
    pub data_type: usize,
    pub collapse_dims: usize,
    pub name: usize,
}

/// Pure ROI processing logic.
pub struct ROIProcessor {
    config: Mutex<ROIConfig>,
    params: ROIParams,
}

impl ROIProcessor {
    pub fn new(config: ROIConfig) -> Self {
        Self {
            config: Mutex::new(config),
            params: ROIParams::default(),
        }
    }

    /// Access the registered ROI param reasons.
    pub fn params(&self) -> &ROIParams {
        &self.params
    }
}

impl NDPluginProcess for ROIProcessor {
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        // C `NDPluginROI.cpp:105-131`: DimNMaxSize is the size of the axis ROI
        // dim N *controls*, i.e. `pArray->dims[userDims[N]].size` with
        // `userDims = {xDim, yDim, colorDim}` (`:80-82`) — the same logical
        // mapping the ROI geometry itself uses. It is 0 for a dim beyond
        // `pArray->ndims` (`:106-108` zero all three first, `:111/:120/:129`
        // only override within ndims).
        let user_dims = array.info().user_dims();
        let mut updates = Vec::new();
        for (i, dim_params) in self.params.dims.iter().enumerate() {
            let dim_size = if i < array.dims.len() {
                array.dims[user_dims[i]].size as i32
            } else {
                0
            };
            updates.push(ParamUpdate::int32(dim_params.max_size, dim_size));
        }

        // C reads the ROI geometry under the port lock and releases it before
        // extracting (NDPluginROI.cpp:140). A guard in the match scrutinee
        // would live to the end of the match, i.e. across the whole copy.
        let config = self.config.lock().clone();
        match extract_roi(array, &config) {
            Some(roi_arr) => ProcessResult {
                output_arrays: vec![Arc::new(roi_arr)],
                param_updates: updates,
                scatter: false,
            },
            None => ProcessResult::sink(updates),
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginROI"
    }

    fn register_params(
        &mut self,
        base: &mut PortDriverBase,
    ) -> Result<(), asyn_rs::error::AsynError> {
        let dim_names = ["DIM0", "DIM1", "DIM2"];
        for (i, prefix) in dim_names.iter().enumerate() {
            self.params.dims[i].min =
                base.create_param(&format!("{prefix}_MIN"), ParamType::Int32)?;
            self.params.dims[i].size =
                base.create_param(&format!("{prefix}_SIZE"), ParamType::Int32)?;
            self.params.dims[i].bin =
                base.create_param(&format!("{prefix}_BIN"), ParamType::Int32)?;
            self.params.dims[i].reverse =
                base.create_param(&format!("{prefix}_REVERSE"), ParamType::Int32)?;
            self.params.dims[i].enable =
                base.create_param(&format!("{prefix}_ENABLE"), ParamType::Int32)?;
            self.params.dims[i].auto_size =
                base.create_param(&format!("{prefix}_AUTO_SIZE"), ParamType::Int32)?;
            self.params.dims[i].max_size =
                base.create_param(&format!("{prefix}_MAX_SIZE"), ParamType::Int32)?;

            // Set initial values from config
            let config = self.config.lock();
            base.set_int32_param(self.params.dims[i].min, 0, config.dims[i].min as i32)?;
            base.set_int32_param(self.params.dims[i].size, 0, config.dims[i].size as i32)?;
            base.set_int32_param(self.params.dims[i].bin, 0, config.dims[i].bin as i32)?;
            base.set_int32_param(
                self.params.dims[i].reverse,
                0,
                config.dims[i].reverse as i32,
            )?;
            base.set_int32_param(self.params.dims[i].enable, 0, config.dims[i].enable as i32)?;
            base.set_int32_param(
                self.params.dims[i].auto_size,
                0,
                config.dims[i].auto_size as i32,
            )?;
        }
        self.params.enable_scale = base.create_param("ENABLE_SCALE", ParamType::Int32)?;
        self.params.scale = base.create_param("SCALE_VALUE", ParamType::Float64)?;
        self.params.data_type = base.create_param("ROI_DATA_TYPE", ParamType::Int32)?;
        self.params.collapse_dims = base.create_param("COLLAPSE_DIMS", ParamType::Int32)?;
        self.params.name = base.create_param("NAME", ParamType::Octet)?;

        let config = self.config.lock();
        base.set_int32_param(self.params.enable_scale, 0, config.enable_scale as i32)?;
        base.set_float64_param(self.params.scale, 0, config.scale)?;
        base.set_int32_param(self.params.data_type, 0, -1)?; // -1 = Automatic
        base.set_int32_param(self.params.collapse_dims, 0, config.collapse_dims as i32)?;

        Ok(())
    }

    fn on_param_change(
        &self,
        reason: usize,
        snapshot: &PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        let p = &self.params;
        let mut config = self.config.lock();
        for i in 0..3 {
            if reason == p.dims[i].min {
                config.dims[i].min = snapshot.value.as_i32().max(0) as usize;
                return ad_core_rs::plugin::runtime::ParamChangeResult::empty();
            }
            if reason == p.dims[i].size {
                config.dims[i].size = snapshot.value.as_i32().max(0) as usize;
                return ad_core_rs::plugin::runtime::ParamChangeResult::empty();
            }
            if reason == p.dims[i].bin {
                config.dims[i].bin = snapshot.value.as_i32().max(1) as usize;
                return ad_core_rs::plugin::runtime::ParamChangeResult::empty();
            }
            if reason == p.dims[i].reverse {
                config.dims[i].reverse = snapshot.value.as_i32() != 0;
                return ad_core_rs::plugin::runtime::ParamChangeResult::empty();
            }
            if reason == p.dims[i].enable {
                config.dims[i].enable = snapshot.value.as_i32() != 0;
                return ad_core_rs::plugin::runtime::ParamChangeResult::empty();
            }
            if reason == p.dims[i].auto_size {
                config.dims[i].auto_size = snapshot.value.as_i32() != 0;
                return ad_core_rs::plugin::runtime::ParamChangeResult::empty();
            }
        }
        if reason == p.enable_scale {
            config.enable_scale = snapshot.value.as_i32() != 0;
        } else if reason == p.scale {
            config.scale = snapshot.value.as_f64();
        } else if reason == p.data_type {
            let v = snapshot.value.as_i32();
            config.data_type = if v < 0 {
                None
            } else {
                NDDataType::from_ordinal(v as u8)
            };
        } else if reason == p.collapse_dims {
            config.collapse_dims = snapshot.value.as_i32() != 0;
        }
        ad_core_rs::plugin::runtime::ParamChangeResult::empty()
    }
}

/// Create an ROI plugin runtime, returning the handle and param reasons.
pub fn create_roi_runtime(
    port_name: &str,
    pool: Arc<NDArrayPool>,
    queue_size: usize,
    ndarray_port: &str,
    wiring: Arc<ad_core_rs::plugin::wiring::WiringRegistry>,
) -> (
    ad_core_rs::plugin::runtime::PluginRuntimeHandle,
    ROIParams,
    std::thread::JoinHandle<()>,
) {
    let processor = ROIProcessor::new(ROIConfig::default());
    let (handle, jh) = ad_core_rs::plugin::runtime::create_plugin_runtime(
        port_name,
        processor,
        pool,
        queue_size,
        ndarray_port,
        wiring,
    );
    // Recreate param layout on a scratch PortDriverBase to get matching reasons.
    let params = {
        let mut base =
            asyn_rs::port::PortDriverBase::new("_scratch_", 1, asyn_rs::port::PortFlags::default());
        let _ = ad_core_rs::params::ndarray_driver::NDArrayDriverParams::create(&mut base);
        let _ = ad_core_rs::plugin::params::PluginBaseParams::create(&mut base);
        let mut p = ROIParams::default();
        let dim_names = ["DIM0", "DIM1", "DIM2"];
        for (i, prefix) in dim_names.iter().enumerate() {
            p.dims[i].min = base
                .create_param(&format!("{prefix}_MIN"), asyn_rs::param::ParamType::Int32)
                .unwrap();
            p.dims[i].size = base
                .create_param(&format!("{prefix}_SIZE"), asyn_rs::param::ParamType::Int32)
                .unwrap();
            p.dims[i].bin = base
                .create_param(&format!("{prefix}_BIN"), asyn_rs::param::ParamType::Int32)
                .unwrap();
            p.dims[i].reverse = base
                .create_param(
                    &format!("{prefix}_REVERSE"),
                    asyn_rs::param::ParamType::Int32,
                )
                .unwrap();
            p.dims[i].enable = base
                .create_param(
                    &format!("{prefix}_ENABLE"),
                    asyn_rs::param::ParamType::Int32,
                )
                .unwrap();
            p.dims[i].auto_size = base
                .create_param(
                    &format!("{prefix}_AUTO_SIZE"),
                    asyn_rs::param::ParamType::Int32,
                )
                .unwrap();
            p.dims[i].max_size = base
                .create_param(
                    &format!("{prefix}_MAX_SIZE"),
                    asyn_rs::param::ParamType::Int32,
                )
                .unwrap();
        }
        p.enable_scale = base
            .create_param("ENABLE_SCALE", asyn_rs::param::ParamType::Int32)
            .unwrap();
        p.scale = base
            .create_param("SCALE_VALUE", asyn_rs::param::ParamType::Float64)
            .unwrap();
        p.data_type = base
            .create_param("ROI_DATA_TYPE", asyn_rs::param::ParamType::Int32)
            .unwrap();
        p.collapse_dims = base
            .create_param("COLLAPSE_DIMS", asyn_rs::param::ParamType::Int32)
            .unwrap();
        p.name = base
            .create_param("NAME", asyn_rs::param::ParamType::Octet)
            .unwrap();
        p
    };
    (handle, params, jh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_4x4_u8() -> NDArray {
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }
        arr
    }

    #[test]
    fn test_r9_66_disabled_dimension_is_not_binned() {
        // R9-66. C's per-axis loop decides offset/size/binning together from the
        // Enable flag, and the DISABLED branch forces all three
        // (NDPluginROI.cpp:98-102): offset = 0, size = the full axis, binning = 1.
        // A leftover DimNBin > 1 on a disabled axis is therefore ignored by C. The
        // port resolved offset/size through resolve_axis but re-derived the binning
        // outside it, straight from the config, so a disabled axis was still binned:
        // a shrunken axis of bin-sums instead of the full-resolution axis.
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        // Dim0 disabled, but with stale min/size/bin left over from an earlier ROI.
        config.dims[0] = ROIDimConfig {
            min: 2,
            size: 1,
            bin: 2,
            reverse: false,
            enable: false,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 4,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };

        let roi = extract_roi_2d(&arr, &config).unwrap();
        // Disabled → the whole 4-wide axis at full resolution, NOT 4/2 == 2 bins,
        // and the stale min = 2 is ignored.
        assert_eq!(roi.dims[0].size, 4, "disabled axis keeps its full size");
        assert_eq!(roi.dims[1].size, 4);
        if let NDDataBuffer::U8(ref v) = roi.data {
            // Row 0 of the source, unbinned and unoffset: 0,1,2,3.
            assert_eq!(&v[0..4], &[0, 1, 2, 3], "disabled axis must not bin-sum");
        } else {
            panic!("expected U8");
        }

        // Boundary: the SAME stale bin on an ENABLED axis must still bin (C :96-97),
        // so the fix is keyed on Enable and has not simply dropped binning.
        config.dims[0].enable = true;
        config.dims[0].min = 0;
        config.dims[0].auto_size = true; // size = full axis, bin = 2
        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(
            roi.dims[0].size, 2,
            "enabled axis with bin=2 halves the axis"
        );
        if let NDDataBuffer::U8(ref v) = roi.data {
            // Row 0 binned by 2: 0+1 = 1, 2+3 = 5.
            assert_eq!(&v[0..2], &[1, 5], "enabled axis bin-sums");
        } else {
            panic!("expected U8");
        }
    }

    #[test]
    fn test_r9_66_disabled_color_axis_is_not_binned() {
        // The 3-D path took the binning from the config too (roi.rs:216-218).
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        use ad_core_rs::color::NDColorMode;

        // RGB1: dims are [color, x, y].
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(2),
            ],
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(NDColorMode::RGB1 as i32),
        ));
        let mut config = ROIConfig::default();
        // Dim2 is the COLOR axis (userDims[2] = colorDim): disabled, stale bin = 3.
        config.dims[2] = ROIDimConfig {
            min: 0,
            size: 3,
            bin: 3,
            reverse: false,
            enable: false,
            auto_size: false,
        };
        for i in 0..2 {
            config.dims[i] = ROIDimConfig {
                min: 0,
                size: 0,
                bin: 1,
                reverse: false,
                enable: true,
                auto_size: true,
            };
        }

        let roi = extract_roi_3d(&arr, &config).unwrap();
        // Disabled colour axis: all 3 planes survive, unbinned. With the stale
        // bin = 3 applied it would have collapsed to a single summed plane.
        assert_eq!(
            roi.dims[0].size, 3,
            "disabled colour axis keeps all 3 planes"
        );
        assert_eq!(roi.dims[1].size, 4);
        assert_eq!(roi.dims[2].size, 2);
    }

    #[test]
    fn test_extract_sub_region() {
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 1,
            size: 2,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 1,
            size: 2,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };

        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(roi.dims[0].size, 2);
        assert_eq!(roi.dims[1].size, 2);
        if let NDDataBuffer::U8(ref v) = roi.data {
            // row 1, cols 1-2: [5,6], row 2, cols 1-2: [9,10]
            assert_eq!(v[0], 5);
            assert_eq!(v[1], 6);
            assert_eq!(v[2], 9);
            assert_eq!(v[3], 10);
        }
    }

    #[test]
    fn test_binning_2x2() {
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 0,
            size: 4,
            bin: 2,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 4,
            bin: 2,
            reverse: false,
            enable: true,
            auto_size: false,
        };

        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(roi.dims[0].size, 2);
        assert_eq!(roi.dims[1].size, 2);
        if let NDDataBuffer::U8(ref v) = roi.data {
            // top-left 2x2: sum = 0+1+4+5 = 10 (C++ sums, not averages)
            assert_eq!(v[0], 10);
        }
    }

    #[test]
    fn test_reverse() {
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 0,
            size: 4,
            bin: 1,
            reverse: true,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 1,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };

        let roi = extract_roi_2d(&arr, &config).unwrap();
        if let NDDataBuffer::U8(ref v) = roi.data {
            assert_eq!(v[0], 3);
            assert_eq!(v[1], 2);
            assert_eq!(v[2], 1);
            assert_eq!(v[3], 0);
        }
    }

    #[test]
    fn test_collapse_dims() {
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 0,
            size: 4,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 1,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.collapse_dims = true;

        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(roi.dims.len(), 1);
        assert_eq!(roi.dims[0].size, 4);
    }

    #[test]
    fn test_scale() {
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 0,
            size: 2,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 1,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.enable_scale = true;
        config.scale = 2.0;

        let roi = extract_roi_2d(&arr, &config).unwrap();
        if let NDDataBuffer::U8(ref v) = roi.data {
            // C++: scale is a divisor
            assert_eq!(v[0], 0); // 0 / 2 = 0
            assert_eq!(v[1], 0); // 1 / 2 = 0.5 → 0
        }
    }

    #[test]
    fn test_type_convert() {
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 0,
            size: 2,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 1,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.data_type = Some(NDDataType::UInt16);

        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(roi.data.data_type(), NDDataType::UInt16);
    }

    // --- New ROIProcessor tests ---

    #[test]
    fn test_roi_processor() {
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 1,
            size: 2,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 1,
            size: 2,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };

        let proc = ROIProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        let arr = make_4x4_u8();
        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(result.output_arrays[0].dims[0].size, 2);
        assert_eq!(result.output_arrays[0].dims[1].size, 2);
    }

    #[test]
    fn test_r9_67_max_size_uses_the_user_dims_mapping() {
        // R9-67. C reports DimNMaxSize as `pArray->dims[userDims[N]].size` with
        // `userDims = {xDim, yDim, colorDim}` (NDPluginROI.cpp:80-82, :111/:120/:129),
        // i.e. the LOGICAL axis ROI dim N controls. The port indexed the physical
        // dims slot N, so on an RGB1 array (`[color, x, y]`) Dim0MaxSize reported
        // the colour axis (3) instead of the image width, and every DimNMaxSize
        // was rotated one slot — the operator's ROI limits described the wrong axes.
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        use ad_core_rs::color::NDColorMode;
        use asyn_rs::port::{PortDriverBase, PortFlags};

        // RGB1: dims = [color=3, x=8, y=5]; xDim=1, yDim=2, colorDim=0.
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(8),
                NDDimension::new(5),
            ],
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(NDColorMode::RGB1 as i32),
        ));

        let mut proc = ROIProcessor::new(ROIConfig::default());
        let mut base = PortDriverBase::new("R9_67", 1, PortFlags::default());
        proc.register_params(&mut base).unwrap();
        let reasons = [
            proc.params().dims[0].max_size,
            proc.params().dims[1].max_size,
            proc.params().dims[2].max_size,
        ];

        let pool = NDArrayPool::new(1_000_000);
        let result = proc.process_array(&arr, &pool);
        let max_size = |reason: usize| {
            result
                .param_updates
                .iter()
                .find_map(|u| match u {
                    ParamUpdate::Int32 {
                        reason: r, value, ..
                    } if *r == reason => Some(*value),
                    _ => None,
                })
                .expect("MaxSize update")
        };

        assert_eq!(
            max_size(reasons[0]),
            8,
            "Dim0MaxSize is the X axis (dims[1])"
        );
        assert_eq!(
            max_size(reasons[1]),
            5,
            "Dim1MaxSize is the Y axis (dims[2])"
        );
        assert_eq!(
            max_size(reasons[2]),
            3,
            "Dim2MaxSize is the colour axis (dims[0])"
        );

        // Control: on a mono 2-D array userDims = {0, 1, 0} and the logical and
        // physical indices coincide, so the readback is unchanged — and Dim2, past
        // ndims, stays 0 (C zeroes all three at :106-108 and only overrides within
        // ndims).
        let arr2d = NDArray::new(
            vec![NDDimension::new(6), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        let result = proc.process_array(&arr2d, &pool);
        let max_size = |reason: usize| {
            result
                .param_updates
                .iter()
                .find_map(|u| match u {
                    ParamUpdate::Int32 {
                        reason: r, value, ..
                    } if *r == reason => Some(*value),
                    _ => None,
                })
                .expect("MaxSize update")
        };
        assert_eq!(max_size(reasons[0]), 6);
        assert_eq!(max_size(reasons[1]), 4);
        assert_eq!(max_size(reasons[2]), 0);
    }

    // --- Auto-size / dim-disable / autocenter tests ---

    #[test]
    fn test_auto_size() {
        // 4x4 image, min_x=1 with auto_size => size_x = 4-1 = 3
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 1,
            size: 0,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: true,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 0,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: true,
        };

        let roi = extract_roi_2d(&arr, &config).unwrap();
        // C++: autoSize sets size = dimSize, then size = MIN(size, dimSize -
        // offset). With offset_x = 1 the X size clamps to 4 - 1 = 3; the Y
        // dimension with offset 0 stays at the full 4.
        assert_eq!(roi.dims[0].size, 3);
        assert_eq!(roi.dims[1].size, 4);
    }

    #[test]
    fn test_dim_disable() {
        // Disabled dim uses full range: min=0, size=src_dim
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 2,
            size: 1,
            bin: 1,
            reverse: false,
            enable: false,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 4,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };

        let roi = extract_roi_2d(&arr, &config).unwrap();
        // X dim disabled, so full range: size=4
        assert_eq!(roi.dims[0].size, 4);
        assert_eq!(roi.dims[1].size, 4);
    }

    #[test]
    fn test_autocenter_peak() {
        // Create 8x8 image with a peak at (6, 5)
        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..64 {
                v[i] = 1;
            }
            // Place peak at x=6, y=5
            v[5 * 8 + 6] = 255;
        }

        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 0,
            size: 4,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 4,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.autocenter = AutoCenter::PeakPosition;

        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(roi.dims[0].size, 4);
        assert_eq!(roi.dims[1].size, 4);

        // ROI should be centered on peak (6,5) with size 4x4
        // min_x = 6 - 4/2 = 4, clamped to min(4, 8-4)=4
        // min_y = 5 - 4/2 = 3, clamped to min(3, 8-4)=3
        // So ROI covers x=[4..8), y=[3..7) and the peak at (6,5) should be inside
        // In the ROI, the peak is at local (6-4, 5-3) = (2, 2)
        if let NDDataBuffer::U8(ref v) = roi.data {
            assert_eq!(v[2 * 4 + 2], 255); // peak at local (2,2)
        }
    }

    #[test]
    fn test_offset_clamp_to_last_column() {
        // Regression: an offset equal to the dim size must clamp to dimSize-1
        // and still produce a 1-pixel ROI (C++ MIN(offset, dimSize-1)),
        // instead of collapsing to an empty sink.
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        // min == src_x (4): one past the last valid index.
        config.dims[0] = ROIDimConfig {
            min: 4,
            size: 10,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 1,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        let roi = extract_roi_2d(&arr, &config).unwrap();
        // offset clamps to 3, size clamps to 4-3 = 1.
        assert_eq!(roi.dims[0].size, 1);
        if let NDDataBuffer::U8(ref v) = roi.data {
            assert_eq!(v[0], 3); // last column of row 0
        }
    }

    #[test]
    fn test_bin_larger_than_roi_clamps() {
        // Regression: a bin larger than the ROI is clamped to the ROI size
        // (C++ MIN(binning, size)), yielding a 1-pixel output instead of a
        // None sink.
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 0,
            size: 2,
            bin: 99, // far larger than the ROI
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 1,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        let roi = extract_roi_2d(&arr, &config).unwrap();
        // bin clamps to size 2 => out_x = 2/2 = 1.
        assert_eq!(roi.dims[0].size, 1);
        if let NDDataBuffer::U8(ref v) = roi.data {
            // sum of the 2-pixel bin: 0 + 1 = 1
            assert_eq!(v[0], 1);
        }
    }

    /// 2x2 RGB1 image: index = y*6 + x*3 + c, value = 100*y + 10*x + c.
    fn make_rgb1_2x2() -> NDArray {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(2),
                NDDimension::new(2),
            ],
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(ad_core_rs::color::NDColorMode::RGB1 as i32),
        ));
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for y in 0..2 {
                for x in 0..2 {
                    for c in 0..3 {
                        v[y * 6 + x * 3 + c] = (100 * y + 10 * x + c) as u8;
                    }
                }
            }
        }
        arr
    }

    #[test]
    fn test_roi_3d_rgb1_x_subregion() {
        // ROI on an RGB1 image: Dim0 selects X, the color axis is preserved.
        let arr = make_rgb1_2x2();
        let mut config = ROIConfig::default();
        // X: take only column 1.
        config.dims[0] = ROIDimConfig {
            min: 1,
            size: 1,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 2,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[2] = ROIDimConfig {
            min: 0,
            size: 3,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        let roi = extract_roi(&arr, &config).unwrap();
        // RGB1 layout: dims = [color=3, x=1, y=2].
        assert_eq!(roi.dims[0].size, 3);
        assert_eq!(roi.dims[1].size, 1);
        assert_eq!(roi.dims[2].size, 2);
        if let NDDataBuffer::U8(ref v) = roi.data {
            // pixel (x=1,y=0) channels => 10,11,12
            assert_eq!(&v[0..3], &[10, 11, 12]);
            // pixel (x=1,y=1) channels => 110,111,112
            assert_eq!(&v[3..6], &[110, 111, 112]);
        } else {
            panic!("not u8");
        }
    }

    #[test]
    fn test_adp18_3d_rgb_honors_output_data_type() {
        // A 3-D RGB ROI with ROI_DATA_TYPE set must convert the output, like
        // the 2-D path (C NDPluginROI.cpp:144,166-174). The old 3-D path kept
        // the source type regardless of config.data_type.
        let arr = make_rgb1_2x2();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 0,
            size: 2,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 2,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[2] = ROIDimConfig {
            min: 0,
            size: 3,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.data_type = Some(NDDataType::UInt16);

        let roi = extract_roi(&arr, &config).unwrap();
        assert_eq!(roi.data.data_type(), NDDataType::UInt16);
        // RGB1 dims preserved: [color=3, x=2, y=2].
        assert_eq!(roi.dims[0].size, 3);
        assert_eq!(roi.dims[1].size, 2);
        assert_eq!(roi.dims[2].size, 2);
        // Values preserved through the widening conversion: pixel (0,0) = 0,1,2.
        if let NDDataBuffer::U16(ref v) = roi.data {
            assert_eq!(&v[0..3], &[0, 1, 2]);
        } else {
            panic!("not u16");
        }
    }

    #[test]
    fn test_adp19_single_color_collapses_to_2d_mono() {
        use ad_core_rs::color::NDColorMode;
        // Select a single color plane (channel 1) of an RGB1 image. C forces
        // collapseDims and sets ColorMode=Mono (NDPluginROI.cpp:177-215), so
        // the output is a 2-D mono [x,y] array, not a 3-D RGB1 with a size-1
        // color axis.
        let arr = make_rgb1_2x2();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig {
            min: 0,
            size: 2,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 2,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[2] = ROIDimConfig {
            min: 1,
            size: 1,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };

        let roi = extract_roi(&arr, &config).unwrap();
        // Collapsed to 2-D [x=2, y=2].
        assert_eq!(roi.dims.len(), 2);
        assert_eq!(roi.dims[0].size, 2);
        assert_eq!(roi.dims[1].size, 2);
        // ColorMode forced to Mono.
        let cm = roi
            .attributes
            .get("ColorMode")
            .and_then(|a| a.value.as_i64())
            .unwrap();
        assert_eq!(cm, NDColorMode::Mono as i64);
        // Channel-1 value of pixel (x,y) = 100*y + 10*x + 1, in [x,y] order.
        if let NDDataBuffer::U8(ref v) = roi.data {
            assert_eq!(v, &[1, 11, 101, 111]);
        } else {
            panic!("not u8");
        }
    }
    // ---- R7-62: C truncating casts, not Rust saturating `as` ----

    /// C `NDPluginROI.cpp:174` (no scaling) calls
    /// `pNDArrayPool->convert(pArray, &pOutput, dataType, dims)`, whose
    /// `convertDim` kernel accumulates the bin sum **in the output type**
    /// (`*pDOut += (dataTypeOut)*pDIn`, NDArrayPool.cpp:465). A UInt8 image of
    /// 100s binned 3x3 sums to 900, which wraps: 900 % 256 == 132. The port
    /// must not saturate to 255.
    #[test]
    fn test_bin_sum_wraps_modulo_output_type() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(3), NDDimension::new(3)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v.iter_mut().for_each(|p| *p = 100);
        }

        let mut config = ROIConfig {
            enable_scale: false,
            ..Default::default()
        };
        for d in 0..2 {
            config.dims[d] = ROIDimConfig {
                min: 0,
                size: 3,
                bin: 3,
                reverse: false,
                enable: true,
                auto_size: false,
            };
        }

        let roi = extract_roi_2d(&arr, &config).unwrap();
        if let NDDataBuffer::U8(ref v) = roi.data {
            assert_eq!(v[0], 132, "900 % 256 == 132 (C wraps), not 255");
        } else {
            panic!("not u8");
        }
    }

    /// The EnableScale path (C `NDPluginROI.cpp:160-171`) is the *only* one
    /// that escapes the wrap: it converts to Float64 first, so the 3x3 bin of
    /// 100s sums exactly to 900, and 900/9 == 100 lands in UInt8 unwrapped.
    #[test]
    fn test_bin_sum_with_scale_uses_float64_intermediate() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(3), NDDimension::new(3)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v.iter_mut().for_each(|p| *p = 100);
        }

        let mut config = ROIConfig {
            enable_scale: true,
            scale: 9.0,
            ..Default::default()
        };
        for d in 0..2 {
            config.dims[d] = ROIDimConfig {
                min: 0,
                size: 3,
                bin: 3,
                reverse: false,
                enable: true,
                auto_size: false,
            };
        }

        let roi = extract_roi_2d(&arr, &config).unwrap();
        if let NDDataBuffer::U8(ref v) = roi.data {
            assert_eq!(v[0], 100, "900/9 via the Float64 path");
        } else {
            panic!("not u8");
        }
    }

    /// A non-scale ROI with a narrowing output type converts through the same
    /// C cast: UInt16 300 -> UInt8 is 300 % 256 == 44, not a clamp to 255.
    #[test]
    fn test_narrowing_output_type_wraps() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(1)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            v[0] = 300;
            v[1] = 70;
        }

        let mut config = ROIConfig {
            data_type: Some(NDDataType::UInt8),
            ..Default::default()
        };
        config.dims[0] = ROIDimConfig {
            min: 0,
            size: 2,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };
        config.dims[1] = ROIDimConfig {
            min: 0,
            size: 1,
            bin: 1,
            reverse: false,
            enable: true,
            auto_size: false,
        };

        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(roi.data.data_type(), NDDataType::UInt8);
        if let NDDataBuffer::U8(ref v) = roi.data {
            assert_eq!(v[0], 44, "(epicsUInt8)300 == 44");
            assert_eq!(v[1], 70);
        } else {
            panic!("not u8");
        }
    }

    /// C `NDPluginROI.cpp:174` binning into a WIDER output type accumulates in
    /// that wider type: UInt8 100s, 3x3 bin, ROIDataType=UInt16 -> 900 (no
    /// wrap). The port must not sum in the source type and then widen.
    #[test]
    fn test_bin_sum_accumulates_in_the_output_type() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(3), NDDimension::new(3)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v.iter_mut().for_each(|p| *p = 100);
        }

        let mut config = ROIConfig {
            data_type: Some(NDDataType::UInt16),
            ..Default::default()
        };
        for d in 0..2 {
            config.dims[d] = ROIDimConfig {
                min: 0,
                size: 3,
                bin: 3,
                reverse: false,
                enable: true,
                auto_size: false,
            };
        }

        let roi = extract_roi_2d(&arr, &config).unwrap();
        if let NDDataBuffer::U16(ref v) = roi.data {
            assert_eq!(v[0], 900, "the bin sum accumulates in UInt16");
        } else {
            panic!("not u16");
        }
    }
}
