use std::sync::Arc;

use ad_core_rs::color::NDColorMode;
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use parking_lot::Mutex;

/// Transform types matching C++ `NDPluginTransformType_t`.
///
/// The numeric ordering is the C++ enum order:
/// `None=0, Rotate90=1, Rotate180=2, Rotate270=3, Mirror=4,
/// Rotate90Mirror=5, Rotate180Mirror=6, Rotate270Mirror=7`.
///
/// - `Mirror` is a horizontal flip.
/// - `Rotate90Mirror` is the transpose (main-diagonal flip).
/// - `Rotate180Mirror` is a vertical flip.
/// - `Rotate270Mirror` is the anti-diagonal flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TransformType {
    None = 0,
    Rot90CW = 1,
    Rot180 = 2,
    Rot90CCW = 3,
    FlipHoriz = 4,
    /// C++ `Rotate90Mirror`: transpose / main-diagonal flip.
    FlipDiag = 5,
    /// C++ `Rotate180Mirror`: vertical flip.
    FlipVert = 6,
    /// C++ `Rotate270Mirror`: anti-diagonal flip.
    FlipAntiDiag = 7,
}

impl TransformType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Rot90CW,
            2 => Self::Rot180,
            3 => Self::Rot90CCW,
            4 => Self::FlipHoriz,
            // C++ TransformRotate90Mirror == transpose.
            5 => Self::FlipDiag,
            // C++ TransformRotate180Mirror == vertical flip.
            6 => Self::FlipVert,
            7 => Self::FlipAntiDiag,
            _ => Self::None,
        }
    }

    /// Whether this transform swaps x and y dimensions.
    pub fn swaps_dims(&self) -> bool {
        matches!(
            self,
            Self::Rot90CW | Self::Rot90CCW | Self::FlipDiag | Self::FlipAntiDiag
        )
    }
}

/// Map source (x, y) to destination (x, y) for the given transform.
fn map_coords(
    sx: usize,
    sy: usize,
    src_w: usize,
    src_h: usize,
    transform: TransformType,
) -> (usize, usize) {
    match transform {
        TransformType::None => (sx, sy),
        TransformType::Rot90CW => (src_h - 1 - sy, sx),
        TransformType::Rot180 => (src_w - 1 - sx, src_h - 1 - sy),
        TransformType::Rot90CCW => (sy, src_w - 1 - sx),
        TransformType::FlipHoriz => (src_w - 1 - sx, sy),
        TransformType::FlipVert => (sx, src_h - 1 - sy),
        TransformType::FlipDiag => (sy, sx),
        TransformType::FlipAntiDiag => (src_h - 1 - sy, src_w - 1 - sx),
    }
}

/// Per-color-mode element strides for a 2-D or 3-D image of the given
/// X/Y/color sizes. Mirrors C++ `NDArray::getInfo` stride layout: returns
/// `(x_stride, y_stride, color_stride)` and the destination dimension order.
fn strides_for(color_mode: NDColorMode, xs: usize, ys: usize, cs: usize) -> (usize, usize, usize) {
    match color_mode {
        NDColorMode::RGB1 => (cs, xs * cs, 1),
        NDColorMode::RGB2 => (1, xs * cs, xs),
        // RGB3 / Mono / others: planar X-fastest layout.
        _ => (1, xs, xs * ys),
    }
}

/// Build the destination dimension vector for `color_mode` with the given
/// X/Y/color sizes, matching the C++ dimension order per color mode.
fn dims_for(
    color_mode: NDColorMode,
    xs: usize,
    ys: usize,
    cs: usize,
    ndims: usize,
) -> Vec<NDDimension> {
    if ndims < 3 {
        return vec![NDDimension::new(xs), NDDimension::new(ys)];
    }
    match color_mode {
        NDColorMode::RGB1 => vec![
            NDDimension::new(cs),
            NDDimension::new(xs),
            NDDimension::new(ys),
        ],
        NDColorMode::RGB2 => vec![
            NDDimension::new(xs),
            NDDimension::new(cs),
            NDDimension::new(ys),
        ],
        _ => vec![
            NDDimension::new(xs),
            NDDimension::new(ys),
            NDDimension::new(cs),
        ],
    }
}

/// Apply a transform to an NDArray.
///
/// Handles 2-D mono images and 3-D RGB1/RGB2/RGB3 color images. The per-color
/// reindexing mirrors C++ `transformNDArray`: source `(x, y)` is geometrically
/// mapped to destination `(x, y)` and every color component is copied with the
/// destination strides recomputed for the (possibly swapped) X/Y sizes.
pub fn apply_transform(src: &NDArray, transform: TransformType) -> NDArray {
    if transform == TransformType::None || src.dims.len() < 2 {
        return src.clone();
    }

    let info = src.info();
    let src_w = info.x_size;
    let src_h = info.y_size;
    let color = info.color_size.max(1);
    if src_w == 0 || src_h == 0 {
        return src.clone();
    }

    let (dst_w, dst_h) = if transform.swaps_dims() {
        (src_h, src_w)
    } else {
        (src_w, src_h)
    };

    let (sxs, sys, scs) = (
        info.x_stride,
        info.y_stride.max(1),
        info.color_stride.max(1),
    );
    let (dxs, dys, dcs) = strides_for(info.color_mode, dst_w, dst_h, color);
    let total = dst_w * dst_h * color;

    macro_rules! transform_buf {
        ($vec:expr, $zero:expr) => {{
            let mut out = vec![$zero; total];
            for sy in 0..src_h {
                for sx in 0..src_w {
                    let (dx, dy) = map_coords(sx, sy, src_w, src_h, transform);
                    let s_base = sy * sys + sx * sxs;
                    let d_base = dy * dys + dx * dxs;
                    for c in 0..color {
                        out[d_base + c * dcs] = $vec[s_base + c * scs];
                    }
                }
            }
            out
        }};
    }

    let out_data = match &src.data {
        NDDataBuffer::U8(v) => NDDataBuffer::U8(transform_buf!(v, 0)),
        NDDataBuffer::U16(v) => NDDataBuffer::U16(transform_buf!(v, 0)),
        NDDataBuffer::I8(v) => NDDataBuffer::I8(transform_buf!(v, 0)),
        NDDataBuffer::I16(v) => NDDataBuffer::I16(transform_buf!(v, 0)),
        NDDataBuffer::I32(v) => NDDataBuffer::I32(transform_buf!(v, 0)),
        NDDataBuffer::U32(v) => NDDataBuffer::U32(transform_buf!(v, 0)),
        NDDataBuffer::I64(v) => NDDataBuffer::I64(transform_buf!(v, 0)),
        NDDataBuffer::U64(v) => NDDataBuffer::U64(transform_buf!(v, 0)),
        NDDataBuffer::F32(v) => NDDataBuffer::F32(transform_buf!(v, 0.0)),
        NDDataBuffer::F64(v) => NDDataBuffer::F64(transform_buf!(v, 0.0)),
    };

    let dims = dims_for(info.color_mode, dst_w, dst_h, color, src.dims.len());
    let mut arr = NDArray::new(dims, src.data.data_type());
    arr.data = out_data;
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.time_stamp = src.time_stamp;
    arr.attributes = src.attributes.clone();
    arr
}

// --- New TransformProcessor (NDPluginProcess-based) ---

/// Pure transform processing logic.
pub struct TransformProcessor {
    transform: Mutex<TransformType>,
    transform_type_idx: Option<usize>,
}

impl TransformProcessor {
    pub fn new(transform: TransformType) -> Self {
        Self {
            transform: Mutex::new(transform),
            transform_type_idx: None,
        }
    }
}

impl NDPluginProcess for TransformProcessor {
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        // C reads the transform type under the port lock and releases it
        // before `transformImage` (NDPluginTransform.cpp:500). A guard passed
        // straight into the call would live to the end of the statement and
        // hold across the whole rotation.
        let transform = *self.transform.lock();
        let out = apply_transform(array, transform);
        ProcessResult::arrays(vec![Arc::new(out)])
    }

    fn plugin_type(&self) -> &str {
        "NDPluginTransform"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        base.create_param("TRANSFORM_TYPE", ParamType::Int32)?;
        self.transform_type_idx = base.find_param("TRANSFORM_TYPE");
        Ok(())
    }

    fn on_param_change(
        &self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        if Some(reason) == self.transform_type_idx {
            *self.transform.lock() = TransformType::from_u8(params.value.as_i32() as u8);
        }
        ad_core_rs::plugin::runtime::ParamChangeResult::updates(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::NDDataType;

    /// Create a 3x2 array:
    /// [1, 2, 3]
    /// [4, 5, 6]
    fn make_3x2() -> NDArray {
        let mut arr = NDArray::new(
            vec![NDDimension::new(3), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            *v = vec![1, 2, 3, 4, 5, 6];
        }
        arr
    }

    fn get_u8(arr: &NDArray) -> &[u8] {
        match &arr.data {
            NDDataBuffer::U8(v) => v,
            _ => panic!("not u8"),
        }
    }

    #[test]
    fn test_none() {
        let arr = make_3x2();
        let out = apply_transform(&arr, TransformType::None);
        assert_eq!(get_u8(&out), &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn test_rot90cw() {
        let arr = make_3x2();
        let out = apply_transform(&arr, TransformType::Rot90CW);
        assert_eq!(out.dims[0].size, 2);
        assert_eq!(out.dims[1].size, 3);
        // Expected:
        // [4, 1]
        // [5, 2]
        // [6, 3]
        assert_eq!(get_u8(&out), &[4, 1, 5, 2, 6, 3]);
    }

    #[test]
    fn test_rot180() {
        let arr = make_3x2();
        let out = apply_transform(&arr, TransformType::Rot180);
        assert_eq!(out.dims[0].size, 3);
        assert_eq!(out.dims[1].size, 2);
        assert_eq!(get_u8(&out), &[6, 5, 4, 3, 2, 1]);
    }

    #[test]
    fn test_rot90ccw() {
        let arr = make_3x2();
        let out = apply_transform(&arr, TransformType::Rot90CCW);
        assert_eq!(out.dims[0].size, 2);
        assert_eq!(out.dims[1].size, 3);
        // Expected:
        // [3, 6]
        // [2, 5]
        // [1, 4]
        assert_eq!(get_u8(&out), &[3, 6, 2, 5, 1, 4]);
    }

    #[test]
    fn test_flip_horiz() {
        let arr = make_3x2();
        let out = apply_transform(&arr, TransformType::FlipHoriz);
        assert_eq!(get_u8(&out), &[3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn test_flip_vert() {
        let arr = make_3x2();
        let out = apply_transform(&arr, TransformType::FlipVert);
        assert_eq!(get_u8(&out), &[4, 5, 6, 1, 2, 3]);
    }

    #[test]
    fn test_flip_diag() {
        let arr = make_3x2();
        let out = apply_transform(&arr, TransformType::FlipDiag);
        assert_eq!(out.dims[0].size, 2);
        assert_eq!(out.dims[1].size, 3);
        // Transpose:
        // [1, 4]
        // [2, 5]
        // [3, 6]
        assert_eq!(get_u8(&out), &[1, 4, 2, 5, 3, 6]);
    }

    #[test]
    fn test_flip_anti_diag() {
        let arr = make_3x2();
        let out = apply_transform(&arr, TransformType::FlipAntiDiag);
        assert_eq!(out.dims[0].size, 2);
        assert_eq!(out.dims[1].size, 3);
        // Anti-transpose:
        // [6, 3]
        // [5, 2]
        // [4, 1]
        assert_eq!(get_u8(&out), &[6, 3, 5, 2, 4, 1]);
    }

    #[test]
    fn test_rot90_roundtrip() {
        let arr = make_3x2();
        let r1 = apply_transform(&arr, TransformType::Rot90CW);
        let r2 = apply_transform(&r1, TransformType::Rot90CW);
        let r3 = apply_transform(&r2, TransformType::Rot90CW);
        let r4 = apply_transform(&r3, TransformType::Rot90CW);
        assert_eq!(get_u8(&r4), get_u8(&arr));
        assert_eq!(r4.dims[0].size, arr.dims[0].size);
        assert_eq!(r4.dims[1].size, arr.dims[1].size);
    }

    #[test]
    fn test_from_u8_cpp_enum_order() {
        // C++ NDPluginTransformType_t order: value 5 is Rotate90Mirror
        // (transpose), value 6 is Rotate180Mirror (vertical flip).
        assert_eq!(TransformType::from_u8(0), TransformType::None);
        assert_eq!(TransformType::from_u8(1), TransformType::Rot90CW);
        assert_eq!(TransformType::from_u8(2), TransformType::Rot180);
        assert_eq!(TransformType::from_u8(3), TransformType::Rot90CCW);
        assert_eq!(TransformType::from_u8(4), TransformType::FlipHoriz);
        assert_eq!(TransformType::from_u8(5), TransformType::FlipDiag);
        assert_eq!(TransformType::from_u8(6), TransformType::FlipVert);
        assert_eq!(TransformType::from_u8(7), TransformType::FlipAntiDiag);
    }

    #[test]
    fn test_transform_5_is_transpose() {
        // Selecting transform 5 from EPICS must produce a transpose.
        let arr = make_3x2();
        let out = apply_transform(&arr, TransformType::from_u8(5));
        assert_eq!(out.dims[0].size, 2);
        assert_eq!(out.dims[1].size, 3);
        assert_eq!(get_u8(&out), &[1, 4, 2, 5, 3, 6]); // transpose
    }

    #[test]
    fn test_transform_6_is_vertical_flip() {
        // Selecting transform 6 from EPICS must produce a vertical flip.
        let arr = make_3x2();
        let out = apply_transform(&arr, TransformType::from_u8(6));
        assert_eq!(out.dims[0].size, 3);
        assert_eq!(out.dims[1].size, 2);
        assert_eq!(get_u8(&out), &[4, 5, 6, 1, 2, 3]); // vertical flip
    }

    /// Build a 2x2 RGB1 image (color-interleaved): pixel (x,y) channel c.
    /// dims = [color=3, x=2, y=2]. Pixel value encodes 100*y + 10*x + c.
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
            NDAttrValue::Int32(NDColorMode::RGB1 as i32),
        ));
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            // layout: index = y*(x*c) + x*c + c, with x_stride=3, y_stride=6
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
    fn test_rgb1_flip_horiz_keeps_color_grouping() {
        // Horizontal flip of an RGB1 image: each pixel's 3 channels stay
        // together; only the x coordinate is mirrored.
        let arr = make_rgb1_2x2();
        let out = apply_transform(&arr, TransformType::FlipHoriz);
        // dims unchanged for a non-swapping transform
        assert_eq!(out.dims[0].size, 3);
        assert_eq!(out.dims[1].size, 2);
        assert_eq!(out.dims[2].size, 2);
        if let NDDataBuffer::U8(v) = &out.data {
            // pixel (x=0,y=0) should now hold source (x=1,y=0): 10,11,12
            assert_eq!(&v[0..3], &[10, 11, 12]);
            // pixel (x=1,y=0) holds source (x=0,y=0): 0,1,2
            assert_eq!(&v[3..6], &[0, 1, 2]);
            // pixel (x=0,y=1) holds source (x=1,y=1): 110,111,112
            assert_eq!(&v[6..9], &[110, 111, 112]);
        } else {
            panic!("not u8");
        }
    }

    #[test]
    fn test_rgb1_rot90cw_swaps_dims_and_keeps_color() {
        let arr = make_rgb1_2x2();
        let out = apply_transform(&arr, TransformType::Rot90CW);
        // x/y swapped (both 2 here), color dim preserved
        assert_eq!(out.dims[0].size, 3);
        assert_eq!(out.dims[1].size, 2);
        assert_eq!(out.dims[2].size, 2);
        if let NDDataBuffer::U8(v) = &out.data {
            // Rot90CW maps src (sx,sy) -> (src_h-1-sy, sx).
            // dest (0,0) <- src (sx,sy) with src_h-1-sy=0, sx=0 => sy=1,sx=0
            // src (0,1) = 100,101,102
            assert_eq!(&v[0..3], &[100, 101, 102]);
        } else {
            panic!("not u8");
        }
    }

    // --- New TransformProcessor tests ---

    #[test]
    fn test_transform_processor() {
        let proc = TransformProcessor::new(TransformType::Rot90CW);
        let pool = NDArrayPool::new(1_000_000);

        let arr = make_3x2();
        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(result.output_arrays[0].dims[0].size, 2); // swapped
        assert_eq!(result.output_arrays[0].dims[1].size, 3);
        assert_eq!(get_u8(&result.output_arrays[0]), &[4, 1, 5, 2, 6, 3]);
    }
}
