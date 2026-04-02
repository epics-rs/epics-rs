use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};

/// Transform types matching C++ NDPluginTransform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TransformType {
    None = 0,
    Rot90CW = 1,
    Rot180 = 2,
    Rot90CCW = 3,
    FlipHoriz = 4,
    FlipVert = 5,
    FlipDiag = 6,
    FlipAntiDiag = 7,
}

impl TransformType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Rot90CW,
            2 => Self::Rot180,
            3 => Self::Rot90CCW,
            4 => Self::FlipHoriz,
            5 => Self::FlipVert,
            6 => Self::FlipDiag,
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

/// Apply a 2D transform to an NDArray.
pub fn apply_transform(src: &NDArray, transform: TransformType) -> NDArray {
    if transform == TransformType::None || src.dims.len() < 2 {
        return src.clone();
    }

    let src_w = src.dims[0].size;
    let src_h = src.dims[1].size;
    let (dst_w, dst_h) = if transform.swaps_dims() {
        (src_h, src_w)
    } else {
        (src_w, src_h)
    };

    macro_rules! transform_buf {
        ($vec:expr, $T:ty, $zero:expr) => {{
            let mut out = vec![$zero; dst_w * dst_h];
            for sy in 0..src_h {
                for sx in 0..src_w {
                    let (dx, dy) = map_coords(sx, sy, src_w, src_h, transform);
                    out[dy * dst_w + dx] = $vec[sy * src_w + sx];
                }
            }
            out
        }};
    }

    let out_data = match &src.data {
        NDDataBuffer::U8(v) => NDDataBuffer::U8(transform_buf!(v, u8, 0)),
        NDDataBuffer::U16(v) => NDDataBuffer::U16(transform_buf!(v, u16, 0)),
        NDDataBuffer::I8(v) => NDDataBuffer::I8(transform_buf!(v, i8, 0)),
        NDDataBuffer::I16(v) => NDDataBuffer::I16(transform_buf!(v, i16, 0)),
        NDDataBuffer::I32(v) => NDDataBuffer::I32(transform_buf!(v, i32, 0)),
        NDDataBuffer::U32(v) => NDDataBuffer::U32(transform_buf!(v, u32, 0)),
        NDDataBuffer::I64(v) => NDDataBuffer::I64(transform_buf!(v, i64, 0)),
        NDDataBuffer::U64(v) => NDDataBuffer::U64(transform_buf!(v, u64, 0)),
        NDDataBuffer::F32(v) => NDDataBuffer::F32(transform_buf!(v, f32, 0.0)),
        NDDataBuffer::F64(v) => NDDataBuffer::F64(transform_buf!(v, f64, 0.0)),
    };

    let dims = vec![NDDimension::new(dst_w), NDDimension::new(dst_h)];
    let mut arr = NDArray::new(dims, src.data.data_type());
    arr.data = out_data;
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    arr
}

// --- New TransformProcessor (NDPluginProcess-based) ---

/// Pure transform processing logic.
pub struct TransformProcessor {
    transform: TransformType,
}

impl TransformProcessor {
    pub fn new(transform: TransformType) -> Self {
        Self { transform }
    }
}

impl NDPluginProcess for TransformProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let out = apply_transform(array, self.transform);
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
        Ok(())
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

    // --- New TransformProcessor tests ---

    #[test]
    fn test_transform_processor() {
        let mut proc = TransformProcessor::new(TransformType::Rot90CW);
        let pool = NDArrayPool::new(1_000_000);

        let arr = make_3x2();
        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(result.output_arrays[0].dims[0].size, 2); // swapped
        assert_eq!(result.output_arrays[0].dims[1].size, 3);
        assert_eq!(get_u8(&result.output_arrays[0]), &[4, 1, 5, 2, 6, 3]);
    }
}
