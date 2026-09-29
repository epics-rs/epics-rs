use std::sync::Arc;

use ad_core_rs::color::NDColorMode;
use ad_core_rs::error::ADResult;
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

/// Apply a transform to an NDArray, allocating the output from `pool` as
/// C's `NDPluginTransform` does with `pNDArrayPool->copy(pArray, NULL, 0)`
/// (NDPluginTransform.cpp:497).
///
/// Handles 2-D mono images and 3-D RGB1/RGB2/RGB3 color images. The per-color
/// reindexing mirrors C++ `transformNDArray`: source `(x, y)` is geometrically
/// mapped to destination `(x, y)` and every color component is copied with the
/// destination strides recomputed for the (possibly swapped) X/Y sizes.
pub fn apply_transform(
    pool: &NDArrayPool,
    src: &NDArray,
    transform: TransformType,
) -> ADResult<NDArray> {
    if transform == TransformType::None || src.dims.len() < 2 {
        return pool.alloc_copy(src);
    }

    let info = src.info();
    let src_w = info.x_size;
    let src_h = info.y_size;
    let color = info.color_size.max(1);
    if src_w == 0 || src_h == 0 {
        return pool.alloc_copy(src);
    }

    let (dst_w, dst_h) = if transform.swaps_dims() {
        (src_h, src_w)
    } else {
        (src_w, src_h)
    };

    let geometry = Geometry {
        src_w,
        src_h,
        color,
        src_strides: (
            info.x_stride,
            info.y_stride.max(1),
            info.color_stride.max(1),
        ),
        dst_strides: strides_for(info.color_mode, dst_w, dst_h, color),
        transform,
    };

    let dims = dims_for(info.color_mode, dst_w, dst_h, color, src.dims.len());
    let mut arr = pool.alloc(dims, src.data.data_type())?;
    macro_rules! same_type {
        ($($variant:ident),*) => {
            match (&src.data, &mut arr.data) {
                $((NDDataBuffer::$variant(v), NDDataBuffer::$variant(o)) => {
                    geometry.transform_into(v, o)
                })*
                _ => unreachable!("the output was allocated in the source type"),
            }
        };
    }
    same_type!(U8, U16, I8, I16, I32, U32, I64, U64, F32, F64);

    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.time_stamp = src.time_stamp;
    arr.attributes = src.attributes.clone();
    Ok(arr)
}

/// The index mapping of one transform: `(x_stride, y_stride, color_stride)`
/// on each side, applied per source pixel.
struct Geometry {
    src_w: usize,
    src_h: usize,
    color: usize,
    src_strides: (usize, usize, usize),
    dst_strides: (usize, usize, usize),
    transform: TransformType,
}

/// Rows and columns per tile of a transposing transform, so the
/// destination lines a tile writes stay cached across its source rows.
const TILE: usize = 64;

impl Geometry {
    /// The destination element index of source pixel `(sx, sy)`, color 0.
    fn dst_index(&self, sx: usize, sy: usize) -> usize {
        let (dx, dy) = map_coords(sx, sy, self.src_w, self.src_h, self.transform);
        dy * self.dst_strides.1 + dx * self.dst_strides.0
    }

    fn transform_into<T: LaneVec>(&self, src: &[T], out: &mut [T]) {
        #[cfg(feature = "simd")]
        let lanes = self.transpose_lanes(ad_core_rs::simd::level(), src, out);
        #[cfg(not(feature = "simd"))]
        let lanes = (0, 0);
        self.transform_rows(src, out, lanes);
    }

    /// The whole vectors of a transposing transform on lanes: every
    /// channel as a planar matrix, or the three of RGB1 split off each
    /// row and joined back. Returns the source columns and rows covered.
    #[cfg(feature = "simd")]
    fn transpose_lanes<T: LaneVec>(
        &self,
        level: fearless_simd::Level,
        src: &[T],
        out: &mut [T],
    ) -> (usize, usize) {
        use fearless_simd::dispatch;
        let (flip_x, flip_y) = match self.transform {
            TransformType::FlipDiag => (false, false),
            TransformType::Rot90CW => (true, false),
            TransformType::Rot90CCW => (false, true),
            TransformType::FlipAntiDiag => (true, true),
            _ => return (0, 0),
        };
        let (sxs, sys, scs) = self.src_strides;
        let (dxs, dys, dcs) = self.dst_strides;
        let (w, h) = (self.src_w, self.src_h);
        let flips = (flip_x, flip_y);
        if sxs == 1 && dxs == 1 {
            let mut done = (0, 0);
            for c in 0..self.color {
                let (s, o) = (&src[c * scs..], &mut out[c * dcs..]);
                done = dispatch!(level, s_ => simd_kernels::transpose_planar(s_, s, o, w, h, sys, dys, flips));
            }
            done
        } else if sxs == 3 && scs == 1 && dxs == 3 && dcs == 1 {
            dispatch!(level, s_ => simd_kernels::transpose_rgb1(s_, src, out, w, h, sys, dys, flips))
        } else {
            (0, 0)
        }
    }

    /// The scalar rows: from column `w0` on the rows below `h0`, and every
    /// row from `h0` on.
    fn transform_rows<T: Copy>(&self, src: &[T], out: &mut [T], (w0, h0): (usize, usize)) {
        let (sxs, sys, scs) = self.src_strides;
        let (_, _, dcs) = self.dst_strides;
        let (w, h, color) = (self.src_w, self.src_h, self.color);
        // Every transform maps a source row to a destination line the index
        // walks with one constant signed step, so the mapping is evaluated
        // once per row and the row itself is one of three copies.
        let step = if w > 1 {
            self.dst_index(1, 0) as isize - self.dst_index(0, 0) as isize
        } else {
            sxs as isize
        };
        // A row whose elements are one contiguous run on both sides.
        let row_is_contiguous = step == sxs as isize
            && dcs == scs
            && ((color == 1 && sxs == 1) || (sxs == color && scs == 1) || (sxs == 1 && scs == w));
        let row = |sy: usize, sx0: usize, len: usize, out: &mut [T]| {
            let s_base = sy * sys + sx0 * sxs;
            let d_base = (self.dst_index(0, sy) as isize + sx0 as isize * step) as usize;
            if row_is_contiguous {
                out[d_base..d_base + len * color]
                    .copy_from_slice(&src[s_base..s_base + len * color]);
                return;
            }
            if sxs == 1 && step == 1 {
                for c in 0..color {
                    let (s, d) = (s_base + c * scs, d_base + c * dcs);
                    out[d..d + len].copy_from_slice(&src[s..s + len]);
                }
            } else if sxs == 1 && step == -1 {
                for c in 0..color {
                    let (s, d) = (s_base + c * scs, d_base + c * dcs);
                    for (o, &i) in out[d + 1 - len..=d].iter_mut().rev().zip(&src[s..s + len]) {
                        *o = i;
                    }
                }
            } else {
                // Strided on at least one side: the elements of one pixel
                // stay together, which keeps interleaved stores adjacent.
                for sx in 0..len {
                    let s = s_base + sx * sxs;
                    let d = (d_base as isize + sx as isize * step) as usize;
                    for c in 0..color {
                        out[d + c * dcs] = src[s + c * scs];
                    }
                }
            }
        };
        if !self.transform.swaps_dims() {
            for sy in 0..h {
                row(sy, 0, w, out);
            }
            return;
        }
        if w0 < w {
            for sy in 0..h0 {
                row(sy, w0, w - w0, out);
            }
        }
        for ty in (h0..h).step_by(TILE) {
            for tx in (0..w).step_by(TILE) {
                let len = TILE.min(w - tx);
                for sy in ty..(ty + TILE).min(h) {
                    row(sy, tx, len, out);
                }
            }
        }
    }
}

#[cfg(feature = "simd")]
use ad_core_rs::simd::LaneVec;
/// Without lanes every element type qualifies; the bound is only there so
/// the kernels can name the native vector.
#[cfg(not(feature = "simd"))]
trait LaneVec: Copy {}
#[cfg(not(feature = "simd"))]
impl<T: Copy> LaneVec for T {}

/// The transposing transforms on explicit vectors. A `K` by `K` block, `K`
/// the lane count of the element, is `K` row vectors; `log2 K` rounds of
/// the perfect shuffle — every vector interleaved with the one `K / 2`
/// after it, the two halves consecutive in the next round's order — leave
/// the `K` column vectors. A rotation is that transpose with the rows
/// loaded in reverse order (the columns of the result run backwards) or
/// the columns stored in reverse order, so the flips cost no lane
/// operation at all.
#[cfg(feature = "simd")]
mod simd_kernels {
    use ad_core_rs::simd::{LaneVec, join_tables, load_vecs, shuffle, split_tables, store_vecs};
    use fearless_simd::{Simd, prelude::*};
    use fearless_simd_macros::simd;

    /// Vectors of the widest block: the lane count of the narrowest
    /// element on every level.
    const MAX_K: usize = 64;

    /// The `k` row vectors in `a` to the `k` column vectors, in `a` or `b`.
    #[inline(always)]
    fn transpose_block<'a, S: Simd, T: LaneVec>(
        a: &'a mut [T::Vec<S>; MAX_K],
        b: &'a mut [T::Vec<S>; MAX_K],
        k: usize,
    ) -> &'a [T::Vec<S>; MAX_K] {
        let (mut from, mut to) = (a, b);
        for _ in 0..k.trailing_zeros() {
            for i in 0..k / 2 {
                let (lo, hi) = from[i].interleave(from[i + k / 2]);
                to[2 * i] = lo;
                to[2 * i + 1] = hi;
            }
            std::mem::swap(&mut from, &mut to);
        }
        from
    }

    /// The source row of block row `i`, and the destination row of block
    /// column `j` with the column the block's rows start at.
    #[inline(always)]
    fn block_rows(
        (ry, cx, k): (usize, usize, usize),
        (w, h): (usize, usize),
        (flip_x, flip_y): (bool, bool),
    ) -> (impl Fn(usize) -> usize, impl Fn(usize) -> usize, usize) {
        let src_row = move |i: usize| if flip_x { ry + k - 1 - i } else { ry + i };
        let dst_row = move |j: usize| if flip_y { w - 1 - (cx + j) } else { cx + j };
        let col = if flip_x { h - k - ry } else { ry };
        (src_row, dst_row, col)
    }

    /// Transpose `src`, `h` rows of `w` elements at the row stride `sys`,
    /// into `out` at the row stride `dys`: source column `x` becomes row
    /// `x` (`w - 1 - x` with `flip_y`) and source row `y` column `y`
    /// (`h - 1 - y` with `flip_x`). Every whole block; returns the source
    /// columns and rows covered.
    #[simd]
    pub(super) fn transpose_planar<S: Simd, T: LaneVec>(
        simd: S,
        src: &[T],
        out: &mut [T],
        w: usize,
        h: usize,
        sys: usize,
        dys: usize,
        flips: (bool, bool),
    ) -> (usize, usize) {
        let k = T::Vec::<S>::LEN;
        let (w0, h0) = (w / k * k, h / k * k);
        let zero = T::Vec::<S>::splat(simd, T::default());
        let (mut a, mut b) = ([zero; MAX_K], [zero; MAX_K]);
        for ry in (0..h0).step_by(k) {
            for cx in (0..w0).step_by(k) {
                let (src_row, dst_row, col) = block_rows((ry, cx, k), (w, h), flips);
                for (i, v) in a[..k].iter_mut().enumerate() {
                    *v = T::Vec::<S>::from_slice(simd, &src[src_row(i) * sys + cx..][..k]);
                }
                let t = transpose_block::<S, T>(&mut a, &mut b, k);
                for (j, v) in t[..k].iter().enumerate() {
                    v.store_slice(&mut out[dst_row(j) * dys + col..][..k]);
                }
            }
        }
        (w0, h0)
    }

    /// [`transpose_planar`] of an RGB1 frame: each block row splits into
    /// its three planes, the planes transpose, and each column joins back.
    #[simd]
    pub(super) fn transpose_rgb1<S: Simd, T: LaneVec>(
        simd: S,
        src: &[T],
        out: &mut [T],
        w: usize,
        h: usize,
        sys: usize,
        dys: usize,
        flips: (bool, bool),
    ) -> (usize, usize) {
        let k = T::Vec::<S>::LEN;
        let (w0, h0) = (w / k * k, h / k * k);
        let e = std::mem::size_of::<T>();
        let (split, join) = (split_tables::<S, 3>(simd, e), join_tables::<S, 3>(simd, e));
        let zero = T::Vec::<S>::splat(simd, T::default());
        let (mut a, mut b) = ([[zero; MAX_K]; 3], [[zero; MAX_K]; 3]);
        for ry in (0..h0).step_by(k) {
            for cx in (0..w0).step_by(k) {
                let (src_row, dst_row, col) = block_rows((ry, cx, k), (w, h), flips);
                for i in 0..k {
                    let planes = shuffle::<S, 3>(
                        &split,
                        load_vecs::<S, T, 3>(simd, &src[src_row(i) * sys + 3 * cx..]),
                    );
                    for (c, p) in planes.into_iter().enumerate() {
                        a[c][i] = T::Vec::<S>::from_bytes(p);
                    }
                }
                let [a0, a1, a2] = &mut a;
                let [b0, b1, b2] = &mut b;
                let t = [
                    transpose_block::<S, T>(a0, b0, k),
                    transpose_block::<S, T>(a1, b1, k),
                    transpose_block::<S, T>(a2, b2, k),
                ];
                for j in 0..k {
                    let planes = std::array::from_fn(|c| t[c][j].to_bytes());
                    store_vecs::<S, T, 3>(
                        shuffle::<S, 3>(&join, planes),
                        &mut out[dst_row(j) * dys + 3 * col..],
                    );
                }
            }
        }
        (w0, h0)
    }
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
    fn process_array(&self, array: &Arc<NDArray>, pool: &NDArrayPool) -> ProcessResult {
        // C reads the transform type under the port lock and releases it
        // before `transformImage` (NDPluginTransform.cpp:500). A guard passed
        // straight into the call would live to the end of the statement and
        // hold across the whole rotation.
        let transform = *self.transform.lock();
        match apply_transform(pool, array, transform) {
            Ok(out) => ProcessResult::arrays(vec![Arc::new(out)]),
            Err(e) => {
                // C's copy() returning NULL ends the frame without output.
                tracing::warn!(error = %e, "transform output allocation failed; dropping frame");
                ProcessResult::empty()
            }
        }
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

    fn pool() -> std::sync::Arc<NDArrayPool> {
        NDArrayPool::new(0)
    }

    /// The rotated frame is a pool array; the next frame reuses its buffer.
    #[test]
    fn transform_output_comes_from_the_pool_and_is_reused() {
        let pool = pool();
        let arr = make_3x2();
        let first = apply_transform(&pool, &arr, TransformType::Rot90CW).unwrap();
        assert_eq!(first.pool_id(), pool.id());
        let ptr = first.data.as_u8_slice().as_ptr();
        drop(first);
        let second = apply_transform(&pool, &arr, TransformType::Rot90CW).unwrap();
        assert_eq!(second.data.as_u8_slice().as_ptr(), ptr);
        assert_eq!(pool.num_alloc_buffers(), 1);
        assert_eq!(get_u8(&second), &[4, 1, 5, 2, 6, 3]);
    }

    /// Every transform on every color layout, on frames wider and taller
    /// than a tile with a partial tile at both edges, against the element
    /// by element mapping of `map_coords`.
    #[test]
    fn transform_into_matches_the_per_pixel_mapping() {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        let (w, h) = (TILE * 2 + 5, TILE + 3);
        let modes = [
            (NDColorMode::Mono, 1),
            (NDColorMode::RGB1, 3),
            (NDColorMode::RGB2, 3),
            (NDColorMode::RGB3, 3),
        ];
        let transforms = [
            TransformType::Rot90CW,
            TransformType::Rot180,
            TransformType::Rot90CCW,
            TransformType::FlipHoriz,
            TransformType::FlipVert,
            TransformType::FlipDiag,
            TransformType::FlipAntiDiag,
        ];
        for (mode, color) in modes {
            let data: Vec<u16> = (0..w * h * color)
                .map(|i| (i * 7919 % 65521) as u16)
                .collect();
            let dims = dims_for(mode, w, h, color, if color == 1 { 2 } else { 3 });
            let mut arr = NDArray::with_data(dims, NDDataBuffer::U16(data.clone()));
            arr.attributes.add(NDAttribute::new_static(
                "ColorMode",
                "Color mode",
                NDAttrSource::Driver,
                NDAttrValue::Int32(mode as i32),
            ));
            let info = arr.info();
            let (sxs, sys, scs) = (
                info.x_stride,
                info.y_stride.max(1),
                info.color_stride.max(1),
            );
            for transform in transforms {
                let out = apply_transform(&pool(), &arr, transform).unwrap();
                let (dw, dh) = if transform.swaps_dims() {
                    (h, w)
                } else {
                    (w, h)
                };
                let (dxs, dys, dcs) = strides_for(mode, dw, dh, color);
                let mut want = vec![0u16; w * h * color];
                for sy in 0..h {
                    for sx in 0..w {
                        let (dx, dy) = map_coords(sx, sy, w, h, transform);
                        for c in 0..color {
                            want[dy * dys + dx * dxs + c * dcs] =
                                data[sy * sys + sx * sxs + c * scs];
                        }
                    }
                }
                let got = match &out.data {
                    NDDataBuffer::U16(v) => v.as_slice(),
                    _ => unreachable!(),
                };
                assert_eq!(got, want.as_slice(), "{mode:?} {transform:?}");
            }
        }
    }

    /// The transposing transforms on every level and element type, on
    /// every color layout, at sizes below, at and past one block of every
    /// lane count, against the element by element mapping of `map_coords`.
    #[cfg(feature = "simd")]
    #[test]
    fn transpose_lanes_match_the_per_pixel_mapping_on_every_level() {
        use fearless_simd::Level;
        let top = ad_core_rs::simd::level();
        let mut levels = vec![top, Level::baseline()];
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            levels.extend(top.as_avx2().map(Level::Avx2));
            levels.extend(top.as_sse4_2().map(Level::Sse4_2));
            levels.extend(top.as_sse2().map(Level::Sse2));
        }
        let modes = [
            (NDColorMode::Mono, 1),
            (NDColorMode::RGB1, 3),
            (NDColorMode::RGB2, 3),
            (NDColorMode::RGB3, 3),
        ];
        let transforms = [
            TransformType::Rot90CW,
            TransformType::Rot90CCW,
            TransformType::FlipDiag,
            TransformType::FlipAntiDiag,
        ];
        macro_rules! check {
            ($($t:ty),*) => {$(
                for (w, h) in [(1, 1), (5, 3), (64, 64), (65, 66), (131, 70)] {
                    for (mode, color) in modes {
                        let data: Vec<$t> = (0..w * h * color).map(|i| (i * 7919 % 65521) as $t).collect();
                        let (sxs, sys, scs) = strides_for(mode, w, h, color);
                        for transform in transforms {
                            let (dw, dh) = (h, w);
                            let dst = strides_for(mode, dw, dh, color);
                            let geometry = Geometry {
                                src_w: w,
                                src_h: h,
                                color,
                                src_strides: (sxs, sys, scs),
                                dst_strides: dst,
                                transform,
                            };
                            let mut want = vec![<$t>::default(); w * h * color];
                            geometry.transform_rows(&data, &mut want, (0, 0));
                            for &level in &levels {
                                let mut got = vec![<$t>::default(); w * h * color];
                                let (w0, h0) = geometry.transpose_lanes(level, &data, &mut got);
                                assert!(w0 <= w && h0 <= h && (w < 64 || w0 > 0) && (h < 64 || h0 > 0), "{level:?} {} {mode:?} {transform:?} {w}x{h}: {w0}x{h0}", stringify!($t));
                                geometry.transform_rows(&data, &mut got, (w0, h0));
                                assert_eq!(got, want, "{level:?} {} {mode:?} {transform:?} {w}x{h}", stringify!($t));
                            }
                        }
                    }
                }
            )*};
        }
        check!(i8, u8, i16, u16, i32, u32, i64, u64, f32, f64);
    }

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
        let out = apply_transform(&pool(), &arr, TransformType::None).unwrap();
        assert_eq!(get_u8(&out), &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn test_rot90cw() {
        let arr = make_3x2();
        let out = apply_transform(&pool(), &arr, TransformType::Rot90CW).unwrap();
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
        let out = apply_transform(&pool(), &arr, TransformType::Rot180).unwrap();
        assert_eq!(out.dims[0].size, 3);
        assert_eq!(out.dims[1].size, 2);
        assert_eq!(get_u8(&out), &[6, 5, 4, 3, 2, 1]);
    }

    #[test]
    fn test_rot90ccw() {
        let arr = make_3x2();
        let out = apply_transform(&pool(), &arr, TransformType::Rot90CCW).unwrap();
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
        let out = apply_transform(&pool(), &arr, TransformType::FlipHoriz).unwrap();
        assert_eq!(get_u8(&out), &[3, 2, 1, 6, 5, 4]);
    }

    #[test]
    fn test_flip_vert() {
        let arr = make_3x2();
        let out = apply_transform(&pool(), &arr, TransformType::FlipVert).unwrap();
        assert_eq!(get_u8(&out), &[4, 5, 6, 1, 2, 3]);
    }

    #[test]
    fn test_flip_diag() {
        let arr = make_3x2();
        let out = apply_transform(&pool(), &arr, TransformType::FlipDiag).unwrap();
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
        let out = apply_transform(&pool(), &arr, TransformType::FlipAntiDiag).unwrap();
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
        let r1 = apply_transform(&pool(), &arr, TransformType::Rot90CW).unwrap();
        let r2 = apply_transform(&pool(), &r1, TransformType::Rot90CW).unwrap();
        let r3 = apply_transform(&pool(), &r2, TransformType::Rot90CW).unwrap();
        let r4 = apply_transform(&pool(), &r3, TransformType::Rot90CW).unwrap();
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
        let out = apply_transform(&pool(), &arr, TransformType::from_u8(5)).unwrap();
        assert_eq!(out.dims[0].size, 2);
        assert_eq!(out.dims[1].size, 3);
        assert_eq!(get_u8(&out), &[1, 4, 2, 5, 3, 6]); // transpose
    }

    #[test]
    fn test_transform_6_is_vertical_flip() {
        // Selecting transform 6 from EPICS must produce a vertical flip.
        let arr = make_3x2();
        let out = apply_transform(&pool(), &arr, TransformType::from_u8(6)).unwrap();
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
        let out = apply_transform(&pool(), &arr, TransformType::FlipHoriz).unwrap();
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
        let out = apply_transform(&pool(), &arr, TransformType::Rot90CW).unwrap();
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
        let result = proc.process_array(&Arc::new(arr), &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(result.output_arrays[0].dims[0].size, 2); // swapped
        assert_eq!(result.output_arrays[0].dims[1].size, 3);
        assert_eq!(get_u8(&result.output_arrays[0]), &[4, 1, 5, 2, 6, 3]);
    }
}
