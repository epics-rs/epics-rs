use std::sync::Arc;

use ad_core::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::NDPluginProcess;

/// Per-dimension ROI configuration.
#[derive(Debug, Clone)]
pub struct ROIDimConfig {
    pub min: usize,
    pub size: usize,
    pub bin: usize,
    pub reverse: bool,
    pub enable: bool,
}

impl Default for ROIDimConfig {
    fn default() -> Self {
        Self { min: 0, size: 0, bin: 1, reverse: false, enable: true }
    }
}

/// ROI plugin configuration.
#[derive(Debug, Clone)]
pub struct ROIConfig {
    pub dims: [ROIDimConfig; 3],
    pub data_type: Option<NDDataType>,
    pub enable_scale: bool,
    pub scale: f64,
    pub collapse_dims: bool,
}

impl Default for ROIConfig {
    fn default() -> Self {
        Self {
            dims: [ROIDimConfig::default(), ROIDimConfig::default(), ROIDimConfig::default()],
            data_type: None,
            enable_scale: false,
            scale: 1.0,
            collapse_dims: false,
        }
    }
}

/// Extract ROI sub-region from a 2D array.
pub fn extract_roi_2d(src: &NDArray, config: &ROIConfig) -> Option<NDArray> {
    if src.dims.len() < 2 {
        return None;
    }

    let src_x = src.dims[0].size;
    let src_y = src.dims[1].size;

    let roi_x_min = config.dims[0].min.min(src_x);
    let roi_x_size = config.dims[0].size.min(src_x - roi_x_min);
    let roi_y_min = config.dims[1].min.min(src_y);
    let roi_y_size = config.dims[1].size.min(src_y - roi_y_min);

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

/// Pure ROI processing logic.
pub struct ROIProcessor {
    config: ROIConfig,
}

impl ROIProcessor {
    pub fn new(config: ROIConfig) -> Self {
        Self { config }
    }
}

impl NDPluginProcess for ROIProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        match extract_roi_2d(array, &self.config) {
            Some(roi_arr) => vec![Arc::new(roi_arr)],
            None => vec![],
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginROI"
    }
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
        config.dims[0] = ROIDimConfig { min: 1, size: 2, bin: 1, reverse: false, enable: true };
        config.dims[1] = ROIDimConfig { min: 1, size: 2, bin: 1, reverse: false, enable: true };

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
        config.dims[0] = ROIDimConfig { min: 0, size: 4, bin: 2, reverse: false, enable: true };
        config.dims[1] = ROIDimConfig { min: 0, size: 4, bin: 2, reverse: false, enable: true };

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
        config.dims[0] = ROIDimConfig { min: 0, size: 4, bin: 1, reverse: true, enable: true };
        config.dims[1] = ROIDimConfig { min: 0, size: 1, bin: 1, reverse: false, enable: true };

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
        config.dims[0] = ROIDimConfig { min: 0, size: 4, bin: 1, reverse: false, enable: true };
        config.dims[1] = ROIDimConfig { min: 0, size: 1, bin: 1, reverse: false, enable: true };
        config.collapse_dims = true;

        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(roi.dims.len(), 1);
        assert_eq!(roi.dims[0].size, 4);
    }

    #[test]
    fn test_scale() {
        let arr = make_4x4_u8();
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig { min: 0, size: 2, bin: 1, reverse: false, enable: true };
        config.dims[1] = ROIDimConfig { min: 0, size: 1, bin: 1, reverse: false, enable: true };
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
        config.dims[0] = ROIDimConfig { min: 0, size: 2, bin: 1, reverse: false, enable: true };
        config.dims[1] = ROIDimConfig { min: 0, size: 1, bin: 1, reverse: false, enable: true };
        config.data_type = Some(NDDataType::UInt16);

        let roi = extract_roi_2d(&arr, &config).unwrap();
        assert_eq!(roi.data.data_type(), NDDataType::UInt16);
    }

    // --- New ROIProcessor tests ---

    #[test]
    fn test_roi_processor() {
        let mut config = ROIConfig::default();
        config.dims[0] = ROIDimConfig { min: 1, size: 2, bin: 1, reverse: false, enable: true };
        config.dims[1] = ROIDimConfig { min: 1, size: 2, bin: 1, reverse: false, enable: true };

        let mut proc = ROIProcessor::new(config);
        let pool = NDArrayPool::new(1_000_000);

        let arr = make_4x4_u8();
        let outputs = proc.process_array(&arr, &pool);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].dims[0].size, 2);
        assert_eq!(outputs[0].dims[1].size, 2);
    }
}
