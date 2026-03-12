use std::sync::Arc;

use ad_core::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::registry::{build_plugin_base_registry, ParamInfo, ParamRegistry};
use ad_core::plugin::runtime::{NDPluginProcess, ParamUpdate, PluginParamSnapshot, PluginRuntimeHandle, ProcessResult};
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;

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
        Self { min: 0, size: 0, bin: 1, reverse: false, enable: true, auto_size: false }
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
            dims: [ROIDimConfig::default(), ROIDimConfig::default(), ROIDimConfig::default()],
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

/// Extract ROI sub-region from a 2D array.
pub fn extract_roi_2d(src: &NDArray, config: &ROIConfig) -> Option<NDArray> {
    if src.dims.len() < 2 {
        return None;
    }

    let src_x = src.dims[0].size;
    let src_y = src.dims[1].size;

    // Resolve effective min/size for X dimension
    let (eff_x_min, eff_x_size) = if !config.dims[0].enable {
        (0, src_x)
    } else if config.dims[0].auto_size {
        let min = config.dims[0].min.min(src_x);
        (min, src_x.saturating_sub(min))
    } else {
        let min = config.dims[0].min.min(src_x);
        let size = config.dims[0].size.min(src_x - min);
        (min, size)
    };

    // Resolve effective min/size for Y dimension
    let (eff_y_min, eff_y_size) = if !config.dims[1].enable {
        (0, src_y)
    } else if config.dims[1].auto_size {
        let min = config.dims[1].min.min(src_y);
        (min, src_y.saturating_sub(min))
    } else {
        let min = config.dims[1].min.min(src_y);
        let size = config.dims[1].size.min(src_y - min);
        (min, size)
    };

    // Apply autocenter: shift ROI min so that the ROI is centered on the
    // centroid or peak, keeping the effective size the same.
    let (roi_x_min, roi_y_min) = match config.autocenter {
        AutoCenter::None => (eff_x_min, eff_y_min),
        AutoCenter::CenterOfMass => {
            let (cx, cy) = find_centroid_2d(&src.data, src_x, src_y);
            let mx = cx.saturating_sub(eff_x_size / 2).min(src_x.saturating_sub(eff_x_size));
            let my = cy.saturating_sub(eff_y_size / 2).min(src_y.saturating_sub(eff_y_size));
            (mx, my)
        }
        AutoCenter::PeakPosition => {
            let (px, py) = find_peak_2d(&src.data, src_x, src_y);
            let mx = px.saturating_sub(eff_x_size / 2).min(src_x.saturating_sub(eff_x_size));
            let my = py.saturating_sub(eff_y_size / 2).min(src_y.saturating_sub(eff_y_size));
            (mx, my)
        }
    };

    let roi_x_size = eff_x_size;
    let roi_y_size = eff_y_size;

    if roi_x_size == 0 || roi_y_size == 0 {
        return None;
    }

    let bin_x = config.dims[0].bin.max(1);
    let bin_y = config.dims[1].bin.max(1);
    let out_x = roi_x_size / bin_x;
    let out_y = roi_y_size / bin_y;

    if out_x == 0 || out_y == 0 {
        return None;
    }

    macro_rules! extract {
        ($vec:expr, $T:ty, $zero:expr) => {{
            let mut out = vec![$zero; out_x * out_y];
            for oy in 0..out_y {
                for ox in 0..out_x {
                    let mut sum = 0.0f64;
                    let mut count = 0usize;
                    for by in 0..bin_y {
                        for bx in 0..bin_x {
                            let sx = roi_x_min + ox * bin_x + bx;
                            let sy = roi_y_min + oy * bin_y + by;
                            if sx < src_x && sy < src_y {
                                sum += $vec[sy * src_x + sx] as f64;
                                count += 1;
                            }
                        }
                    }
                    let val = if count > 0 { sum / count as f64 } else { 0.0 };
                    let idx = if config.dims[0].reverse { out_x - 1 - ox } else { ox }
                        + if config.dims[1].reverse { out_y - 1 - oy } else { oy } * out_x;
                    let scaled = if config.enable_scale { val * config.scale } else { val };
                    out[idx] = scaled as $T;
                }
            }
            out
        }};
    }

    let out_data = match &src.data {
        NDDataBuffer::U8(v) => NDDataBuffer::U8(extract!(v, u8, 0)),
        NDDataBuffer::U16(v) => NDDataBuffer::U16(extract!(v, u16, 0)),
        NDDataBuffer::I8(v) => NDDataBuffer::I8(extract!(v, i8, 0)),
        NDDataBuffer::I16(v) => NDDataBuffer::I16(extract!(v, i16, 0)),
        NDDataBuffer::I32(v) => NDDataBuffer::I32(extract!(v, i32, 0)),
        NDDataBuffer::U32(v) => NDDataBuffer::U32(extract!(v, u32, 0)),
        NDDataBuffer::I64(v) => NDDataBuffer::I64(extract!(v, i64, 0)),
        NDDataBuffer::U64(v) => NDDataBuffer::U64(extract!(v, u64, 0)),
        NDDataBuffer::F32(v) => NDDataBuffer::F32(extract!(v, f32, 0.0)),
        NDDataBuffer::F64(v) => NDDataBuffer::F64(extract!(v, f64, 0.0)),
    };

    let out_dims = if config.collapse_dims && out_y == 1 {
        vec![NDDimension::new(out_x)]
    } else {
        vec![NDDimension::new(out_x), NDDimension::new(out_y)]
    };

    // Apply data type conversion if requested
    let target_type = config.data_type.unwrap_or(src.data.data_type());

    let mut arr = NDArray::new(out_dims, target_type);
    if target_type == src.data.data_type() {
        arr.data = out_data;
    } else {
        // Convert via color module
        let mut temp = NDArray::new(arr.dims.clone(), src.data.data_type());
        temp.data = out_data;
        if let Ok(converted) = ad_core::color::convert_data_type(&temp, target_type) {
            arr.data = converted.data;
        } else {
            arr.data = out_data_fallback(&temp.data, target_type, temp.data.len());
        }
    }

    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    Some(arr)
}

fn out_data_fallback(_src: &NDDataBuffer, target: NDDataType, len: usize) -> NDDataBuffer {
    NDDataBuffer::zeros(target, len)
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
    config: ROIConfig,
    params: ROIParams,
}

impl ROIProcessor {
    pub fn new(config: ROIConfig) -> Self {
        Self { config, params: ROIParams::default() }
    }

    /// Access the registered ROI param reasons.
    pub fn params(&self) -> &ROIParams {
        &self.params
    }
}

impl NDPluginProcess for ROIProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        // Report input array dimensions as MaxSize params
        let mut updates = Vec::new();
        for (i, dim_params) in self.params.dims.iter().enumerate() {
            let dim_size = array.dims.get(i).map(|d| d.size as i32).unwrap_or(0);
            updates.push(ParamUpdate::int32(dim_params.max_size, dim_size));
        }

        match extract_roi_2d(array, &self.config) {
            Some(roi_arr) => ProcessResult {
                output_arrays: vec![Arc::new(roi_arr)],
                param_updates: updates,
            },
            None => ProcessResult::sink(updates),
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginROI"
    }

    fn register_params(&mut self, base: &mut PortDriverBase) -> Result<(), asyn_rs::error::AsynError> {
        let dim_names = ["DIM0", "DIM1", "DIM2"];
        for (i, prefix) in dim_names.iter().enumerate() {
            self.params.dims[i].min = base.create_param(&format!("{prefix}_MIN"), ParamType::Int32)?;
            self.params.dims[i].size = base.create_param(&format!("{prefix}_SIZE"), ParamType::Int32)?;
            self.params.dims[i].bin = base.create_param(&format!("{prefix}_BIN"), ParamType::Int32)?;
            self.params.dims[i].reverse = base.create_param(&format!("{prefix}_REVERSE"), ParamType::Int32)?;
            self.params.dims[i].enable = base.create_param(&format!("{prefix}_ENABLE"), ParamType::Int32)?;
            self.params.dims[i].auto_size = base.create_param(&format!("{prefix}_AUTO_SIZE"), ParamType::Int32)?;
            self.params.dims[i].max_size = base.create_param(&format!("{prefix}_MAX_SIZE"), ParamType::Int32)?;

            // Set initial values from config
            base.set_int32_param(self.params.dims[i].min, 0, self.config.dims[i].min as i32)?;
            base.set_int32_param(self.params.dims[i].size, 0, self.config.dims[i].size as i32)?;
            base.set_int32_param(self.params.dims[i].bin, 0, self.config.dims[i].bin as i32)?;
            base.set_int32_param(self.params.dims[i].reverse, 0, self.config.dims[i].reverse as i32)?;
            base.set_int32_param(self.params.dims[i].enable, 0, self.config.dims[i].enable as i32)?;
            base.set_int32_param(self.params.dims[i].auto_size, 0, self.config.dims[i].auto_size as i32)?;
        }
        self.params.enable_scale = base.create_param("ENABLE_SCALE", ParamType::Int32)?;
        self.params.scale = base.create_param("SCALE_VALUE", ParamType::Float64)?;
        self.params.data_type = base.create_param("ROI_DATA_TYPE", ParamType::Int32)?;
        self.params.collapse_dims = base.create_param("COLLAPSE_DIMS", ParamType::Int32)?;
        self.params.name = base.create_param("NAME", ParamType::Octet)?;

        base.set_int32_param(self.params.enable_scale, 0, self.config.enable_scale as i32)?;
        base.set_float64_param(self.params.scale, 0, self.config.scale)?;
        base.set_int32_param(self.params.data_type, 0, -1)?; // -1 = Automatic
        base.set_int32_param(self.params.collapse_dims, 0, self.config.collapse_dims as i32)?;

        Ok(())
    }

    fn on_param_change(&mut self, reason: usize, snapshot: &PluginParamSnapshot) {
        let p = &self.params;
        for i in 0..3 {
            if reason == p.dims[i].min {
                self.config.dims[i].min = snapshot.value.as_i32().max(0) as usize;
                return;
            }
            if reason == p.dims[i].size {
                self.config.dims[i].size = snapshot.value.as_i32().max(0) as usize;
                return;
            }
            if reason == p.dims[i].bin {
                self.config.dims[i].bin = snapshot.value.as_i32().max(1) as usize;
                return;
            }
            if reason == p.dims[i].reverse {
                self.config.dims[i].reverse = snapshot.value.as_i32() != 0;
                return;
            }
            if reason == p.dims[i].enable {
                self.config.dims[i].enable = snapshot.value.as_i32() != 0;
                return;
            }
            if reason == p.dims[i].auto_size {
                self.config.dims[i].auto_size = snapshot.value.as_i32() != 0;
                return;
            }
        }
        if reason == p.enable_scale {
            self.config.enable_scale = snapshot.value.as_i32() != 0;
        } else if reason == p.scale {
            self.config.scale = snapshot.value.as_f64();
        } else if reason == p.data_type {
            let v = snapshot.value.as_i32();
            self.config.data_type = if v < 0 { None } else { NDDataType::from_ordinal(v as u8) };
        } else if reason == p.collapse_dims {
            self.config.collapse_dims = snapshot.value.as_i32() != 0;
        }
    }
}

/// Create an ROI plugin runtime, returning the handle and param reasons.
pub fn create_roi_runtime(
    port_name: &str,
    pool: Arc<NDArrayPool>,
    queue_size: usize,
    ndarray_port: &str,
    wiring: Arc<ad_core::plugin::wiring::WiringRegistry>,
) -> (
    ad_core::plugin::runtime::PluginRuntimeHandle,
    ROIParams,
    std::thread::JoinHandle<()>,
) {
    let processor = ROIProcessor::new(ROIConfig::default());
    let (handle, jh) = ad_core::plugin::runtime::create_plugin_runtime(
        port_name,
        processor,
        pool,
        queue_size,
        ndarray_port,
        wiring,
    );
    // Recreate param layout on a scratch PortDriverBase to get matching reasons.
    let params = {
        let mut base = asyn_rs::port::PortDriverBase::new("_scratch_", 1, asyn_rs::port::PortFlags::default());
        let _ = ad_core::params::ndarray_driver::NDArrayDriverParams::create(&mut base);
        let _ = ad_core::plugin::params::PluginBaseParams::create(&mut base);
        let mut p = ROIParams::default();
        let dim_names = ["DIM0", "DIM1", "DIM2"];
        for (i, prefix) in dim_names.iter().enumerate() {
            p.dims[i].min = base.create_param(&format!("{prefix}_MIN"), asyn_rs::param::ParamType::Int32).unwrap();
            p.dims[i].size = base.create_param(&format!("{prefix}_SIZE"), asyn_rs::param::ParamType::Int32).unwrap();
            p.dims[i].bin = base.create_param(&format!("{prefix}_BIN"), asyn_rs::param::ParamType::Int32).unwrap();
            p.dims[i].reverse = base.create_param(&format!("{prefix}_REVERSE"), asyn_rs::param::ParamType::Int32).unwrap();
            p.dims[i].enable = base.create_param(&format!("{prefix}_ENABLE"), asyn_rs::param::ParamType::Int32).unwrap();
            p.dims[i].auto_size = base.create_param(&format!("{prefix}_AUTO_SIZE"), asyn_rs::param::ParamType::Int32).unwrap();
            p.dims[i].max_size = base.create_param(&format!("{prefix}_MAX_SIZE"), asyn_rs::param::ParamType::Int32).unwrap();
        }
        p.enable_scale = base.create_param("ENABLE_SCALE", asyn_rs::param::ParamType::Int32).unwrap();
        p.scale = base.create_param("SCALE_VALUE", asyn_rs::param::ParamType::Float64).unwrap();
        p.data_type = base.create_param("ROI_DATA_TYPE", asyn_rs::param::ParamType::Int32).unwrap();
        p.collapse_dims = base.create_param("COLLAPSE_DIMS", asyn_rs::param::ParamType::Int32).unwrap();
        p.name = base.create_param("NAME", asyn_rs::param::ParamType::Octet).unwrap();
        p
    };
    (handle, params, jh)
}

/// Build a ParamRegistry that maps NDROI.template record suffixes to asyn param indices.
pub fn build_roi_registry(h: &PluginRuntimeHandle, rp: &ROIParams) -> ParamRegistry {
    let mut map = build_plugin_base_registry(h);

    let dim_suffixes = [
        ("X", 0),
        ("Y", 1),
        ("Z", 2),
    ];
    for (suffix, i) in &dim_suffixes {
        let d = &rp.dims[*i];
        let dim_idx = *i;
        // Min
        map.insert(format!("Min{suffix}"), ParamInfo::int32(d.min, &format!("DIM{dim_idx}_MIN")));
        map.insert(format!("Min{suffix}_RBV"), ParamInfo::int32(d.min, &format!("DIM{dim_idx}_MIN")));
        // Size
        map.insert(format!("Size{suffix}"), ParamInfo::int32(d.size, &format!("DIM{dim_idx}_SIZE")));
        map.insert(format!("Size{suffix}_RBV"), ParamInfo::int32(d.size, &format!("DIM{dim_idx}_SIZE")));
        // Bin
        map.insert(format!("Bin{suffix}"), ParamInfo::int32(d.bin, &format!("DIM{dim_idx}_BIN")));
        map.insert(format!("Bin{suffix}_RBV"), ParamInfo::int32(d.bin, &format!("DIM{dim_idx}_BIN")));
        // Reverse
        map.insert(format!("Reverse{suffix}"), ParamInfo::int32(d.reverse, &format!("DIM{dim_idx}_REVERSE")));
        map.insert(format!("Reverse{suffix}_RBV"), ParamInfo::int32(d.reverse, &format!("DIM{dim_idx}_REVERSE")));
        // Enable
        map.insert(format!("Enable{suffix}"), ParamInfo::int32(d.enable, &format!("DIM{dim_idx}_ENABLE")));
        map.insert(format!("Enable{suffix}_RBV"), ParamInfo::int32(d.enable, &format!("DIM{dim_idx}_ENABLE")));
        // AutoSize
        map.insert(format!("AutoSize{suffix}"), ParamInfo::int32(d.auto_size, &format!("DIM{dim_idx}_AUTO_SIZE")));
        map.insert(format!("AutoSize{suffix}_RBV"), ParamInfo::int32(d.auto_size, &format!("DIM{dim_idx}_AUTO_SIZE")));
        // MaxSize (read-only)
        map.insert(format!("MaxSize{suffix}_RBV"), ParamInfo::int32(d.max_size, &format!("DIM{dim_idx}_MAX_SIZE")));
    }

    // Scaling
    map.insert("EnableScale".into(), ParamInfo::int32(rp.enable_scale, "ENABLE_SCALE"));
    map.insert("EnableScale_RBV".into(), ParamInfo::int32(rp.enable_scale, "ENABLE_SCALE"));
    map.insert("Scale".into(), ParamInfo::float64(rp.scale, "SCALE_VALUE"));
    map.insert("Scale_RBV".into(), ParamInfo::float64(rp.scale, "SCALE_VALUE"));

    // Data type
    map.insert("DataTypeOut".into(), ParamInfo::int32(rp.data_type, "ROI_DATA_TYPE"));
    map.insert("DataTypeOut_RBV".into(), ParamInfo::int32(rp.data_type, "ROI_DATA_TYPE"));

    // CollapseDims
    map.insert("CollapseDims".into(), ParamInfo::int32(rp.collapse_dims, "COLLAPSE_DIMS"));
    map.insert("CollapseDims_RBV".into(), ParamInfo::int32(rp.collapse_dims, "COLLAPSE_DIMS"));

    // Name
    map.insert("Name".into(), ParamInfo::string(rp.name, "NAME"));
    map.insert("Name_RBV".into(), ParamInfo::string(rp.name, "NAME"));

    map
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
            for i in 0..16 { v[i] = i as u8; }
        }
        arr
    }

    #[test]
    fn test_extract_sub_region() {
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig { min: 1, size: 2, bin: 1, reverse: false, enable: true, auto_size: false };
        config.dims[1] = ROIDimConfig { min: 1, size: 2, bin: 1, reverse: false, enable: true, auto_size: false };

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
        config.dims[0] = ROIDimConfig { min: 0, size: 4, bin: 2, reverse: false, enable: true, auto_size: false };
        config.dims[1] = ROIDimConfig { min: 0, size: 4, bin: 2, reverse: false, enable: true, auto_size: false };

        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(roi.dims[0].size, 2);
        assert_eq!(roi.dims[1].size, 2);
        if let NDDataBuffer::U8(ref v) = roi.data {
            // top-left 2x2: (0+1+4+5)/4 = 2.5 → 2
            assert_eq!(v[0], 2);
        }
    }

    #[test]
    fn test_reverse() {
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig { min: 0, size: 4, bin: 1, reverse: true, enable: true, auto_size: false };
        config.dims[1] = ROIDimConfig { min: 0, size: 1, bin: 1, reverse: false, enable: true, auto_size: false };

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
        config.dims[0] = ROIDimConfig { min: 0, size: 4, bin: 1, reverse: false, enable: true, auto_size: false };
        config.dims[1] = ROIDimConfig { min: 0, size: 1, bin: 1, reverse: false, enable: true, auto_size: false };
        config.collapse_dims = true;

        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(roi.dims.len(), 1);
        assert_eq!(roi.dims[0].size, 4);
    }

    #[test]
    fn test_scale() {
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig { min: 0, size: 2, bin: 1, reverse: false, enable: true, auto_size: false };
        config.dims[1] = ROIDimConfig { min: 0, size: 1, bin: 1, reverse: false, enable: true, auto_size: false };
        config.enable_scale = true;
        config.scale = 2.0;

        let roi = extract_roi_2d(&arr, &config).unwrap();
        if let NDDataBuffer::U8(ref v) = roi.data {
            assert_eq!(v[0], 0); // 0 * 2 = 0
            assert_eq!(v[1], 2); // 1 * 2 = 2
        }
    }

    #[test]
    fn test_type_convert() {
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig { min: 0, size: 2, bin: 1, reverse: false, enable: true, auto_size: false };
        config.dims[1] = ROIDimConfig { min: 0, size: 1, bin: 1, reverse: false, enable: true, auto_size: false };
        config.data_type = Some(NDDataType::UInt16);

        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(roi.data.data_type(), NDDataType::UInt16);
    }

    // --- New ROIProcessor tests ---

    #[test]
    fn test_roi_processor() {
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig { min: 1, size: 2, bin: 1, reverse: false, enable: true, auto_size: false };
        config.dims[1] = ROIDimConfig { min: 1, size: 2, bin: 1, reverse: false, enable: true, auto_size: false };

        let mut proc = ROIProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        let arr = make_4x4_u8();
        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(result.output_arrays[0].dims[0].size, 2);
        assert_eq!(result.output_arrays[0].dims[1].size, 2);
    }

    // --- Auto-size / dim-disable / autocenter tests ---

    #[test]
    fn test_auto_size() {
        // 4x4 image, min_x=1 with auto_size => size_x = 4-1 = 3
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig { min: 1, size: 0, bin: 1, reverse: false, enable: true, auto_size: true };
        config.dims[1] = ROIDimConfig { min: 0, size: 0, bin: 1, reverse: false, enable: true, auto_size: true };

        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(roi.dims[0].size, 3); // 4 - 1 = 3
        assert_eq!(roi.dims[1].size, 4); // 4 - 0 = 4

        if let NDDataBuffer::U8(ref v) = roi.data {
            // First row (y=0): pixels at x=1,2,3 => values 1,2,3
            assert_eq!(v[0], 1);
            assert_eq!(v[1], 2);
            assert_eq!(v[2], 3);
        }
    }

    #[test]
    fn test_dim_disable() {
        // Disabled dim uses full range: min=0, size=src_dim
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig { min: 2, size: 1, bin: 1, reverse: false, enable: false, auto_size: false };
        config.dims[1] = ROIDimConfig { min: 0, size: 4, bin: 1, reverse: false, enable: true, auto_size: false };

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
            for i in 0..64 { v[i] = 1; }
            // Place peak at x=6, y=5
            v[5 * 8 + 6] = 255;
        }

        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig { min: 0, size: 4, bin: 1, reverse: false, enable: true, auto_size: false };
        config.dims[1] = ROIDimConfig { min: 0, size: 4, bin: 1, reverse: false, enable: true, auto_size: false };
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
}
