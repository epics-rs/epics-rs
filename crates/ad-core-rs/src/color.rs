use crate::error::{ADError, ADResult};
use crate::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use crate::ndarray_pool::NDArrayPool;

/// Color mode for NDArray interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum NDColorMode {
    Mono = 0,
    Bayer = 1,
    RGB1 = 2,
    RGB2 = 3,
    RGB3 = 4,
    YUV444 = 5,
    YUV422 = 6,
    YUV411 = 7,
}

impl NDColorMode {
    pub fn from_i32(v: i32) -> Self {
        match v {
            0 => Self::Mono,
            1 => Self::Bayer,
            2 => Self::RGB1,
            3 => Self::RGB2,
            4 => Self::RGB3,
            5 => Self::YUV444,
            6 => Self::YUV422,
            7 => Self::YUV411,
            _ => Self::Mono,
        }
    }
}

/// Bayer pattern for raw sensor data (matching C++ `NDBayerPattern_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum NDBayerPattern {
    RGGB = 0,
    GBRG = 1,
    GRBG = 2,
    BGGR = 3,
}

impl NDBayerPattern {
    /// Map an integer code to a Bayer pattern. Unknown codes default to `RGGB`.
    pub fn from_i32(v: i32) -> Self {
        match v {
            1 => Self::GBRG,
            2 => Self::GRBG,
            3 => Self::BGGR,
            _ => Self::RGGB,
        }
    }

    /// Numeric code for the EPICS `BAYER_PATTERN` parameter.
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

/// The pooled output of a conversion of `src`: `dims` and `data_type` as the
/// conversion needs them, frame identity copied from the source. C allocates
/// every conversion output through `pNDArrayPool->alloc`
/// (NDPluginColorConvert.cpp:93,119,155,203,344,408,479).
fn output(
    pool: &NDArrayPool,
    src: &NDArray,
    dims: Vec<NDDimension>,
    data_type: NDDataType,
) -> ADResult<NDArray> {
    let mut arr = pool.alloc(dims, data_type)?;
    arr.unique_id = src.unique_id;
    arr.copy_time_stamps_from(src);
    arr.attributes = src.attributes.clone();
    arr.codec = src.codec.clone();
    Ok(arr)
}

/// Run `$body` with `$v` bound to the source slice and `$out` to the output
/// slice of the same element type. The output was allocated in the source
/// type, so a variant mismatch cannot happen.
macro_rules! same_type {
    ($src:expr, $dst:expr, |$v:ident, $out:ident| $body:expr) => {
        match ($src, $dst) {
            (NDDataBuffer::I8($v), NDDataBuffer::I8($out)) => $body,
            (NDDataBuffer::U8($v), NDDataBuffer::U8($out)) => $body,
            (NDDataBuffer::I16($v), NDDataBuffer::I16($out)) => $body,
            (NDDataBuffer::U16($v), NDDataBuffer::U16($out)) => $body,
            (NDDataBuffer::I32($v), NDDataBuffer::I32($out)) => $body,
            (NDDataBuffer::U32($v), NDDataBuffer::U32($out)) => $body,
            (NDDataBuffer::I64($v), NDDataBuffer::I64($out)) => $body,
            (NDDataBuffer::U64($v), NDDataBuffer::U64($out)) => $body,
            (NDDataBuffer::F32($v), NDDataBuffer::F32($out)) => $body,
            (NDDataBuffer::F64($v), NDDataBuffer::F64($out)) => $body,
            _ => unreachable!("the output was allocated in the source type"),
        }
    };
}

/// The 8-bit output slice of a conversion whose output is always `UInt8`.
fn u8_slice(arr: &mut NDArray) -> &mut [u8] {
    match &mut arr.data {
        NDDataBuffer::U8(v) => v.as_mut_slice(),
        _ => unreachable!("the output was allocated as UInt8"),
    }
}

/// Convert a mono 2D array to RGB1 (3-channel interleaved) by replicating the value.
pub fn mono_to_rgb1(pool: &NDArrayPool, src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 2 {
        return Err(ADError::InvalidDimensions(
            "mono_to_rgb1 requires 2D input".into(),
        ));
    }
    let x = src.dims[0].size;
    let y = src.dims[1].size;
    let n = x * y;

    let dims = vec![
        NDDimension::new(3),
        NDDimension::new(x),
        NDDimension::new(y),
    ];
    let mut arr = output(pool, src, dims, src.data.data_type())?;
    same_type!(&src.data, &mut arr.data, |v, out| {
        broadcast3(&v[..n], &mut out[..3 * n])
    });
    Ok(arr)
}

/// Every value of `v` three times over into `out`: on lanes as far as
/// whole vectors reach, then scalar.
fn broadcast3<T: LaneElem>(v: &[T], out: &mut [T]) {
    #[cfg(feature = "simd")]
    let done =
        fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::broadcast3(s, v, out));
    #[cfg(not(feature = "simd"))]
    let done = 0;
    broadcast3_scalar(&v[done..], &mut out[3 * done..]);
}

fn broadcast3_scalar<T: Copy>(v: &[T], out: &mut [T]) {
    for (&p, px) in v.iter().zip(out.as_chunks_mut::<3>().0.iter_mut()) {
        px.fill(p);
    }
}

/// Convert RGB1 (3-channel interleaved) to mono.
///
/// C `NDPluginColorConvert::convertColor` uses the unweighted mean
/// `value = (R + G + B) / 3.` cast to the output type — a C cast that
/// truncates toward zero, not luminance weighting and not rounding
/// (`NDPluginColorConvert.cpp:392-395`; the RGB2/RGB3 mono paths `:462`/`:533`
/// and the Bayer mono path `:331` use the same `(R+G+B)/3`). This is the
/// single chokepoint: ad-plugins routes RGB2/RGB3/Bayer→mono through here
/// after converting to RGB1 (`color_convert.rs:437`).
pub fn rgb1_to_mono(pool: &NDArrayPool, src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 3 || src.dims[0].size != 3 {
        return Err(ADError::InvalidDimensions(
            "rgb1_to_mono requires 3D input with dims[0]=3".into(),
        ));
    }
    let x = src.dims[1].size;
    let y = src.dims[2].size;
    let n = x * y;

    let dims = vec![NDDimension::new(x), NDDimension::new(y)];
    let mut arr = output(pool, src, dims, src.data.data_type())?;
    same_type!(&src.data, &mut arr.data, |v, out| {
        rgb1_mean(&v[..3 * n], &mut out[..n])
    });
    Ok(arr)
}

/// The mean of [`rgb1_to_mono`] over the RGB1 pixels `v` into `out`: on
/// lanes as far as whole vectors of pixels reach, then scalar.
fn rgb1_mean<T: LaneElem>(v: &[T], out: &mut [T]) {
    #[cfg(feature = "simd")]
    let done =
        fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::rgb1_mean(s, v, out));
    #[cfg(not(feature = "simd"))]
    let done = 0;
    rgb1_mean_scalar(&v[3 * done..], &mut out[done..]);
}

fn rgb1_mean_scalar<T: LaneElem>(v: &[T], out: &mut [T]) {
    for (px, o) in v.as_chunks::<3>().0.iter().zip(out) {
        // C: value = (R+G+B)/3. then (epicsType)value — truncate.
        *o = T::from_f64(((px[0].to_f64() + px[1].to_f64()) + px[2].to_f64()) / 3.0);
    }
}

/// Convert between RGB layout orders (RGB1 ↔ RGB2 ↔ RGB3).
/// RGB1: [color, x, y] — pixel-interleaved (RGBRGBRGB...)
/// RGB2: [x, color, y] — row-interleaved
/// RGB3: [x, y, color] — planar
pub fn convert_rgb_layout(
    pool: &NDArrayPool,
    src: &NDArray,
    src_mode: NDColorMode,
    dst_mode: NDColorMode,
) -> ADResult<NDArray> {
    if src.dims.len() != 3 {
        return Err(ADError::InvalidDimensions(
            "RGB conversion requires 3D input".into(),
        ));
    }

    // Determine x, y, color from source layout
    let (color, x, y) = match src_mode {
        NDColorMode::RGB1 => (src.dims[0].size, src.dims[1].size, src.dims[2].size),
        NDColorMode::RGB2 => (src.dims[1].size, src.dims[0].size, src.dims[2].size),
        NDColorMode::RGB3 => (src.dims[2].size, src.dims[0].size, src.dims[1].size),
        _ => {
            return Err(ADError::UnsupportedConversion(format!(
                "convert_rgb_layout: source mode {:?} not RGB",
                src_mode
            )));
        }
    };

    if color != 3 {
        return Err(ADError::InvalidDimensions(
            "RGB conversion requires color dimension = 3".into(),
        ));
    }

    // Build output dimensions
    let out_dims = match dst_mode {
        NDColorMode::RGB1 => vec![
            NDDimension::new(3),
            NDDimension::new(x),
            NDDimension::new(y),
        ],
        NDColorMode::RGB2 => vec![
            NDDimension::new(x),
            NDDimension::new(3),
            NDDimension::new(y),
        ],
        NDColorMode::RGB3 => vec![
            NDDimension::new(x),
            NDDimension::new(y),
            NDDimension::new(3),
        ],
        _ => {
            return Err(ADError::UnsupportedConversion(format!(
                "convert_rgb_layout: target mode {:?} not RGB",
                dst_mode
            )));
        }
    };

    let src_strides = rgb_strides(src_mode, x, y);
    let dst_strides = rgb_strides(dst_mode, x, y);
    let mut arr = output(pool, src, out_dims, src.data.data_type())?;
    same_type!(&src.data, &mut arr.data, |v, out| {
        rgb_layout_rows(v, out, x, y, src_strides, dst_strides)
    });
    // The output is laid out as `dst_mode`, so its ColorMode attribute must say
    // so. Cloning the source attributes copied the *source* ColorMode, which
    // now contradicts the new dims; any consumer that resolves layout from the
    // ColorMode attribute (e.g. NDArray::info) would mis-read the result (the
    // NDFileJPEG RGB2/RGB3 grayscale bug). Replace it so dims and attribute
    // agree by construction.
    {
        use crate::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "Color mode",
            NDAttrSource::Driver,
            NDAttrValue::Int32(dst_mode as i32),
        ));
    }
    Ok(arr)
}

/// The element strides over (ix, c, iy) of an RGB layout of `x` by `y`
/// pixels; a run along x is then a strided copy, contiguous in RGB2 and
/// RGB3.
fn rgb_strides(mode: NDColorMode, x: usize, y: usize) -> (usize, usize, usize) {
    match mode {
        NDColorMode::RGB1 => (3, 1, x * 3),
        NDColorMode::RGB2 => (1, x, x * 3),
        NDColorMode::RGB3 => (1, x * y, x),
        _ => unreachable!("checked above"),
    }
}

/// `v` with the strides `src` into `out` with the strides `dst`: the
/// pixel-interleaved side of the conversion on lanes as far as whole
/// vectors reach along each row, the rest of every row scalar.
fn rgb_layout_rows<T: LaneElem>(
    v: &[T],
    out: &mut [T],
    x: usize,
    y: usize,
    src: (usize, usize, usize),
    dst: (usize, usize, usize),
) {
    #[cfg(feature = "simd")]
    let x0 = fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::rgb1_rows(s, v, out, x, y, src, dst));
    #[cfg(not(feature = "simd"))]
    let x0 = 0;
    rgb_layout_rows_scalar(v, out, x, y, x0, src, dst);
}

/// [`rgb_layout_rows`] from column `x0` of every row on.
fn rgb_layout_rows_scalar<T: Copy>(
    v: &[T],
    out: &mut [T],
    x: usize,
    y: usize,
    x0: usize,
    (sx, sc, sy): (usize, usize, usize),
    (dx, dc, dy): (usize, usize, usize),
) {
    if x0 == x {
        return;
    }
    for iy in 0..y {
        for c in 0..3usize {
            let s = &v[c * sc + iy * sy + x0 * sx..];
            let d = &mut out[c * dc + iy * dy + x0 * dx..];
            if sx == 1 && dx == 1 {
                d[..x - x0].copy_from_slice(&s[..x - x0]);
            } else {
                for (o, i) in d
                    .iter_mut()
                    .step_by(dx)
                    .zip(s.iter().step_by(sx))
                    .take(x - x0)
                {
                    *o = *i;
                }
            }
        }
    }
}

/// Convert NDArray element type using C cast semantics.
///
/// Thin alias for [`crate::convert::convert_type`] — C++ `convertType`
/// (`NDArrayPool.cpp:378-388`), `*pDataOut++ = (dataTypeOut)(*pDataIn++)`.
/// A C cast truncates to the low bits on narrowing (`(epicsUInt8)300 == 44`);
/// it does **not** clamp, so this must not reintroduce a `clamp()`.
pub fn convert_data_type(src: &NDArray, target_type: NDDataType) -> ADResult<NDArray> {
    crate::convert::convert_type(src, target_type)
}

/// [`convert_data_type`] into a buffer the caller owns — the alias for
/// [`crate::convert::convert_type_into`].
pub fn convert_data_type_into(src: &NDArray, out: &mut NDDataBuffer) -> ADResult<()> {
    crate::convert::convert_type_into(src, out)
}

/// Convert RGB1 to YUV444 using BT.601 coefficients.
/// Input: RGB1 `[3, x, y]`, Output: YUV444 `[3, x, y]`
pub fn rgb1_to_yuv444(pool: &NDArrayPool, src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 3 || src.dims[0].size != 3 {
        return Err(ADError::InvalidDimensions(
            "rgb1_to_yuv444 requires 3D input with dims[0]=3".into(),
        ));
    }
    let x = src.dims[1].size;
    let y = src.dims[2].size;
    let n = x * y;

    let dims = vec![
        NDDimension::new(3),
        NDDimension::new(x),
        NDDimension::new(y),
    ];
    let mut arr = output(pool, src, dims, src.data.data_type())?;
    match (&src.data, &mut arr.data) {
        (NDDataBuffer::U8(v), NDDataBuffer::U8(out)) => {
            yuv444_forward(&v[..n * 3], &mut out[..n * 3], U8_HALF, U8_MAX);
        }
        (NDDataBuffer::U16(v), NDDataBuffer::U16(out)) => {
            yuv444_forward(&v[..n * 3], &mut out[..n * 3], U16_HALF, U16_MAX);
        }
        _ => {
            return Err(ADError::UnsupportedConversion(
                "rgb1_to_yuv444 only supports UInt8 and UInt16".into(),
            ));
        }
    }
    Ok(arr)
}

/// The chroma zero point and the clamp ceiling of the two YUV element
/// types.
const U8_HALF: f64 = 128.0;
const U8_MAX: f64 = 255.0;
const U16_HALF: f64 = 32768.0;
const U16_MAX: f64 = 65535.0;

/// BT.601 forward on one pixel before rounding, and its inverse on chroma
/// already offset to zero: the expressions every YUV conversion here
/// shares, in the association the vector kernels reproduce operation for
/// operation.
#[inline(always)]
fn rgb_to_yuv(r: f64, g: f64, b: f64, half: f64) -> (f64, f64, f64) {
    (
        0.299 * r + 0.587 * g + 0.114 * b,
        -0.169 * r - 0.331 * g + 0.5 * b + half,
        0.5 * r - 0.419 * g - 0.081 * b + half,
    )
}

#[inline(always)]
fn yuv_to_rgb(y: f64, cb: f64, cr: f64) -> (f64, f64, f64) {
    (y + 1.402 * cr, y - 0.344 * cb - 0.714 * cr, y + 1.772 * cb)
}

/// Round half away from zero and clamp to `0..=max`, the value the element
/// then truncates to.
#[inline(always)]
fn round_clamp(v: f64, max: f64) -> f64 {
    v.round().clamp(0.0, max)
}

#[cfg(feature = "simd")]
use crate::simd::LaneVec;
/// Without lanes every element type qualifies; the bound is only there so
/// the kernels can name the native vector.
#[cfg(not(feature = "simd"))]
trait LaneVec: Copy {}
#[cfg(not(feature = "simd"))]
impl<T: Copy> LaneVec for T {}

/// An element type the lane conversions run in: the widening to `f64` and
/// back that the arithmetic kernels use, on a slice or on one vector.
trait LaneElem: LaneVec {
    fn to_f64(self) -> f64;
    fn from_f64(v: f64) -> Self;
    /// One vector as its `f64` vectors in element order, into the head
    /// of `out`.
    #[cfg(feature = "simd")]
    fn widen_vec<S: fearless_simd::Simd>(simd: S, v: Self::Vec<S>, out: &mut [S::f64s]);
    /// The head of `w` as one vector; the inverse of
    /// [`widen_vec`](Self::widen_vec).
    #[cfg(feature = "simd")]
    fn narrow_vec<S: fearless_simd::Simd>(simd: S, w: &[S::f64s]) -> Self::Vec<S>;
}

macro_rules! lane_elem {
    ($t:ty, $widen_vec:ident, $narrow_vec:ident) => {
        impl LaneElem for $t {
            #[inline(always)]
            fn to_f64(self) -> f64 {
                self as f64
            }
            #[inline(always)]
            fn from_f64(v: f64) -> Self {
                v as $t
            }
            #[cfg(feature = "simd")]
            #[inline(always)]
            fn widen_vec<S: fearless_simd::Simd>(_simd: S, v: Self::Vec<S>, out: &mut [S::f64s]) {
                let wide = crate::simd::$widen_vec::<S>(v);
                out[..wide.len()].copy_from_slice(&wide);
            }
            #[cfg(feature = "simd")]
            #[inline(always)]
            fn narrow_vec<S: fearless_simd::Simd>(simd: S, w: &[S::f64s]) -> Self::Vec<S> {
                crate::simd::$narrow_vec::<S>(simd, w.try_into().expect("a whole vector"))
            }
        }
    };
}

lane_elem!(i8, to_f64s_i8, from_f64s_i8);
lane_elem!(u8, to_f64s_u8, from_f64s_u8);
lane_elem!(i16, to_f64s_i16, from_f64s_i16);
lane_elem!(u16, to_f64s_u16, from_f64s_u16);
lane_elem!(i32, to_f64s_i32, from_f64s_i32);
lane_elem!(u32, to_f64s_u32, from_f64s_u32);
lane_elem!(i64, to_f64s_i64, from_f64s_i64);
lane_elem!(u64, to_f64s_u64, from_f64s_u64);
lane_elem!(f32, to_f64s_f32, from_f64s_f32);

impl LaneElem for f64 {
    #[inline(always)]
    fn to_f64(self) -> f64 {
        self
    }
    #[inline(always)]
    fn from_f64(v: f64) -> Self {
        v
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn widen_vec<S: fearless_simd::Simd>(_simd: S, v: Self::Vec<S>, out: &mut [S::f64s]) {
        out[0] = v;
    }
    #[cfg(feature = "simd")]
    #[inline(always)]
    fn narrow_vec<S: fearless_simd::Simd>(_simd: S, w: &[S::f64s]) -> Self::Vec<S> {
        w[0]
    }
}

/// RGB1 pixels to YUV444 pixels, both `[3, n]`.
fn yuv444_forward<T: LaneElem>(v: &[T], out: &mut [T], half: f64, max: f64) {
    #[cfg(feature = "simd")]
    {
        let done = fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::yuv444_forward(s, v, out, half, max));
        yuv444_forward_scalar(&v[done..], &mut out[done..], half, max);
    }
    #[cfg(not(feature = "simd"))]
    yuv444_forward_scalar(v, out, half, max);
}

fn yuv444_forward_scalar<T: LaneElem>(v: &[T], out: &mut [T], half: f64, max: f64) {
    for (px, o) in v
        .as_chunks::<3>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<3>().0.iter_mut())
    {
        let (y, cb, cr) = rgb_to_yuv(px[0].to_f64(), px[1].to_f64(), px[2].to_f64(), half);
        o[0] = T::from_f64(round_clamp(y, max));
        o[1] = T::from_f64(round_clamp(cb, max));
        o[2] = T::from_f64(round_clamp(cr, max));
    }
}

/// YUV444 pixels to RGB1 pixels, both `[3, n]`.
fn yuv444_inverse<T: LaneElem>(v: &[T], out: &mut [T], half: f64, max: f64) {
    #[cfg(feature = "simd")]
    {
        let done = fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::yuv444_inverse(s, v, out, half, max));
        yuv444_inverse_scalar(&v[done..], &mut out[done..], half, max);
    }
    #[cfg(not(feature = "simd"))]
    yuv444_inverse_scalar(v, out, half, max);
}

fn yuv444_inverse_scalar<T: LaneElem>(v: &[T], out: &mut [T], half: f64, max: f64) {
    for (px, o) in v
        .as_chunks::<3>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<3>().0.iter_mut())
    {
        let (r, g, b) = yuv_to_rgb(px[0].to_f64(), px[1].to_f64() - half, px[2].to_f64() - half);
        o[0] = T::from_f64(round_clamp(r, max));
        o[1] = T::from_f64(round_clamp(g, max));
        o[2] = T::from_f64(round_clamp(b, max));
    }
}

/// RGB1 pixel pairs to packed UYVY: `v` is `[3, 2 * pairs]`, `out`
/// `[4 * pairs]`, the two chroma values averaged over the pair.
fn yuv422_forward(v: &[u8], out: &mut [u8]) {
    #[cfg(feature = "simd")]
    {
        let done = fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::yuv422_forward(s, v, out));
        yuv422_forward_scalar(&v[done * 6..], &mut out[done * 4..]);
    }
    #[cfg(not(feature = "simd"))]
    yuv422_forward_scalar(v, out);
}

fn yuv422_forward_scalar(v: &[u8], out: &mut [u8]) {
    for (px, o) in v
        .as_chunks::<6>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        let (y0, cb0, cr0) = rgb_to_yuv(px[0] as f64, px[1] as f64, px[2] as f64, U8_HALF);
        let (y1, cb1, cr1) = rgb_to_yuv(px[3] as f64, px[4] as f64, px[5] as f64, U8_HALF);
        o[0] = round_clamp((cb0 + cb1) / 2.0, U8_MAX) as u8;
        o[1] = round_clamp(y0, U8_MAX) as u8;
        o[2] = round_clamp((cr0 + cr1) / 2.0, U8_MAX) as u8;
        o[3] = round_clamp(y1, U8_MAX) as u8;
    }
}

/// Packed UYVY to RGB1 pixel pairs: `v` is `[4 * pairs]`, `out`
/// `[3, 2 * pairs]`.
fn yuv422_inverse(v: &[u8], out: &mut [u8]) {
    #[cfg(feature = "simd")]
    {
        let done = fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::yuv422_inverse(s, v, out));
        yuv422_inverse_scalar(&v[done * 4..], &mut out[done * 6..]);
    }
    #[cfg(not(feature = "simd"))]
    yuv422_inverse_scalar(v, out);
}

fn yuv422_inverse_scalar(v: &[u8], out: &mut [u8]) {
    for (px, o) in v
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<6>().0.iter_mut())
    {
        let u = px[0] as f64 - U8_HALF;
        let vc = px[2] as f64 - U8_HALF;
        for (k, &y) in [px[1], px[3]].iter().enumerate() {
            let (r, g, b) = yuv_to_rgb(y as f64, u, vc);
            o[k * 3] = round_clamp(r, U8_MAX) as u8;
            o[k * 3 + 1] = round_clamp(g, U8_MAX) as u8;
            o[k * 3 + 2] = round_clamp(b, U8_MAX) as u8;
        }
    }
}

/// RGB1 pixel quads to packed UYYVYY: `v` is `[3, 4 * groups]`, `out`
/// `[6 * groups]`, the two chroma values averaged over the quad.
fn yuv411_forward(v: &[u8], out: &mut [u8]) {
    #[cfg(feature = "simd")]
    {
        let done = fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::yuv411_forward(s, v, out));
        yuv411_forward_scalar(&v[done * 12..], &mut out[done * 6..]);
    }
    #[cfg(not(feature = "simd"))]
    yuv411_forward_scalar(v, out);
}

fn yuv411_forward_scalar(v: &[u8], out: &mut [u8]) {
    for (px, o) in v
        .as_chunks::<12>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<6>().0.iter_mut())
    {
        let mut ys = [0u8; 4];
        let mut cbs = [0.0f64; 4];
        let mut crs = [0.0f64; 4];
        for (p, q) in px.as_chunks::<3>().0.iter().enumerate() {
            let (y, cb, cr) = rgb_to_yuv(q[0] as f64, q[1] as f64, q[2] as f64, U8_HALF);
            ys[p] = round_clamp(y, U8_MAX) as u8;
            cbs[p] = cb;
            crs[p] = cr;
        }
        o[0] = round_clamp((cbs[0] + cbs[1] + cbs[2] + cbs[3]) / 4.0, U8_MAX) as u8;
        o[1] = ys[0];
        o[2] = ys[1];
        o[3] = round_clamp((crs[0] + crs[1] + crs[2] + crs[3]) / 4.0, U8_MAX) as u8;
        o[4] = ys[2];
        o[5] = ys[3];
    }
}

/// Packed UYYVYY to RGB1 pixel quads: `v` is `[6 * groups]`, `out`
/// `[3, 4 * groups]`.
fn yuv411_inverse(v: &[u8], out: &mut [u8]) {
    #[cfg(feature = "simd")]
    {
        let done = fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::yuv411_inverse(s, v, out));
        yuv411_inverse_scalar(&v[done * 6..], &mut out[done * 12..]);
    }
    #[cfg(not(feature = "simd"))]
    yuv411_inverse_scalar(v, out);
}

fn yuv411_inverse_scalar(v: &[u8], out: &mut [u8]) {
    for (px, o) in v
        .as_chunks::<6>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<12>().0.iter_mut())
    {
        let u = px[0] as f64 - U8_HALF;
        let vc = px[3] as f64 - U8_HALF;
        for (k, &y) in [px[1], px[2], px[4], px[5]].iter().enumerate() {
            let (r, g, b) = yuv_to_rgb(y as f64, u, vc);
            o[k * 3] = round_clamp(r, U8_MAX) as u8;
            o[k * 3 + 1] = round_clamp(g, U8_MAX) as u8;
            o[k * 3 + 2] = round_clamp(b, U8_MAX) as u8;
        }
    }
}

/// Convert YUV444 to RGB1 using inverse BT.601.
/// Input: YUV444 `[3, x, y]`, Output: RGB1 `[3, x, y]`
pub fn yuv444_to_rgb1(pool: &NDArrayPool, src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 3 || src.dims[0].size != 3 {
        return Err(ADError::InvalidDimensions(
            "yuv444_to_rgb1 requires 3D input with dims[0]=3".into(),
        ));
    }
    let x = src.dims[1].size;
    let y = src.dims[2].size;
    let n = x * y;

    let dims = vec![
        NDDimension::new(3),
        NDDimension::new(x),
        NDDimension::new(y),
    ];
    let mut arr = output(pool, src, dims, src.data.data_type())?;
    match (&src.data, &mut arr.data) {
        (NDDataBuffer::U8(v), NDDataBuffer::U8(out)) => {
            yuv444_inverse(&v[..n * 3], &mut out[..n * 3], U8_HALF, U8_MAX);
        }
        (NDDataBuffer::U16(v), NDDataBuffer::U16(out)) => {
            yuv444_inverse(&v[..n * 3], &mut out[..n * 3], U16_HALF, U16_MAX);
        }
        _ => {
            return Err(ADError::UnsupportedConversion(
                "yuv444_to_rgb1 only supports UInt8 and UInt16".into(),
            ));
        }
    }
    Ok(arr)
}

/// Convert RGB1 to YUV422 packed format (UYVY byte order).
/// Input: RGB1 `[3, x, y]`, Output: packed `[x*2, y]` as UInt8.
/// Width (x) must be even.
pub fn rgb1_to_yuv422(pool: &NDArrayPool, src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 3 || src.dims[0].size != 3 {
        return Err(ADError::InvalidDimensions(
            "rgb1_to_yuv422 requires 3D input with dims[0]=3".into(),
        ));
    }
    let x = src.dims[1].size;
    let y = src.dims[2].size;
    if x % 2 != 0 {
        return Err(ADError::InvalidDimensions(
            "rgb1_to_yuv422 requires even width".into(),
        ));
    }

    let v = match &src.data {
        NDDataBuffer::U8(v) => v,
        _ => {
            return Err(ADError::UnsupportedConversion(
                "rgb1_to_yuv422 only supports UInt8".into(),
            ));
        }
    };

    let packed_x = x * 2;
    let dims = vec![NDDimension::new(packed_x), NDDimension::new(y)];
    let mut arr = output(pool, src, dims, NDDataType::UInt8)?;
    let out = u8_slice(&mut arr);

    // Every row is a whole number of pairs, so the pairs of the frame are
    // one contiguous run in both layouts.
    yuv422_forward(&v[..x * y * 3], &mut out[..packed_x * y]);

    Ok(arr)
}

/// Convert YUV422 packed format (UYVY) to RGB1.
/// Input: packed `[packed_x, y]` as UInt8, Output: RGB1 `[3, packed_x/2, y]`.
/// packed_x must be divisible by 4.
pub fn yuv422_to_rgb1(pool: &NDArrayPool, src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 2 {
        return Err(ADError::InvalidDimensions(
            "yuv422_to_rgb1 requires 2D input".into(),
        ));
    }
    let packed_x = src.dims[0].size;
    let y = src.dims[1].size;
    if packed_x % 4 != 0 {
        return Err(ADError::InvalidDimensions(
            "yuv422_to_rgb1 requires packed_x divisible by 4".into(),
        ));
    }

    let v = match &src.data {
        NDDataBuffer::U8(v) => v,
        _ => {
            return Err(ADError::UnsupportedConversion(
                "yuv422_to_rgb1 only supports UInt8".into(),
            ));
        }
    };

    let width = packed_x / 2;
    let dims = vec![
        NDDimension::new(3),
        NDDimension::new(width),
        NDDimension::new(y),
    ];
    let mut arr = output(pool, src, dims, NDDataType::UInt8)?;
    let out = u8_slice(&mut arr);

    yuv422_inverse(&v[..packed_x * y], &mut out[..width * y * 3]);

    Ok(arr)
}

/// Convert RGB1 to YUV411 packed format (UYYVYY byte order).
/// Input: RGB1 `[3, x, y]`, Output: packed `[x*3/2, y]` as UInt8.
/// Width (x) must be divisible by 4.
pub fn rgb1_to_yuv411(pool: &NDArrayPool, src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 3 || src.dims[0].size != 3 {
        return Err(ADError::InvalidDimensions(
            "rgb1_to_yuv411 requires 3D input with dims[0]=3".into(),
        ));
    }
    let x = src.dims[1].size;
    let y = src.dims[2].size;
    if x % 4 != 0 {
        return Err(ADError::InvalidDimensions(
            "rgb1_to_yuv411 requires width divisible by 4".into(),
        ));
    }

    let v = match &src.data {
        NDDataBuffer::U8(v) => v,
        _ => {
            return Err(ADError::UnsupportedConversion(
                "rgb1_to_yuv411 only supports UInt8".into(),
            ));
        }
    };

    let packed_x = x * 3 / 2;
    let dims = vec![NDDimension::new(packed_x), NDDimension::new(y)];
    let mut arr = output(pool, src, dims, NDDataType::UInt8)?;
    let out = u8_slice(&mut arr);

    yuv411_forward(&v[..x * y * 3], &mut out[..packed_x * y]);

    Ok(arr)
}

/// Convert YUV411 packed format (UYYVYY) to RGB1.
/// Input: packed `[packed_x, y]` as UInt8, Output: RGB1 `[3, packed_x*2/3, y]`.
/// packed_x must be divisible by 6.
pub fn yuv411_to_rgb1(pool: &NDArrayPool, src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 2 {
        return Err(ADError::InvalidDimensions(
            "yuv411_to_rgb1 requires 2D input".into(),
        ));
    }
    let packed_x = src.dims[0].size;
    let y = src.dims[1].size;
    if packed_x % 6 != 0 {
        return Err(ADError::InvalidDimensions(
            "yuv411_to_rgb1 requires packed_x divisible by 6".into(),
        ));
    }

    let v = match &src.data {
        NDDataBuffer::U8(v) => v,
        _ => {
            return Err(ADError::UnsupportedConversion(
                "yuv411_to_rgb1 only supports UInt8".into(),
            ));
        }
    };

    let width = packed_x * 2 / 3;
    let dims = vec![
        NDDimension::new(3),
        NDDimension::new(width),
        NDDimension::new(y),
    ];
    let mut arr = output(pool, src, dims, NDDataType::UInt8)?;
    let out = u8_slice(&mut arr);

    yuv411_inverse(&v[..packed_x * y], &mut out[..width * y * 3]);

    Ok(arr)
}

/// The YUV conversions on explicit vectors, a vector of pixels, of pixel
/// pairs or of pixel quads per step and all of it in registers: the
/// pixels split into their planes with the byte shuffles of
/// [`crate::simd`], each plane widens to `f64`, the BT.601 arithmetic
/// runs plane against plane, and the results narrow and join back into
/// the packed layout. The subsampled layouts split the pixels of a pair
/// or quad into one plane per position first, so the chroma of a pair or
/// quad averages lane against lane. Each kernel converts the whole prefix
/// of its input that fills steps and returns how many elements, pairs or
/// quads it covered; the scalar loop finishes the rest.
///
/// The arithmetic is the scalar expression, operation for operation, and
/// the round-half-away-from-zero of `f64::round` is spelled out on lanes
/// (the vector `round_ties_even` is not it), so a frame is bit for bit
/// the same on either path.
#[cfg(feature = "simd")]
mod simd_kernels {
    use super::LaneElem;
    use crate::simd::{join_tables, load_vecs, shuffle, split_tables, store_vecs};
    use fearless_simd::{Simd, prelude::*};
    use fearless_simd_macros::simd;

    /// `f64::round` on lanes: the truncation, plus one away from zero
    /// where the fraction reaches a half. `v - trunc(v)` is exact and so
    /// is its double, whose truncation is that one with the sign of `v`.
    /// Only `-0.0` itself comes back as `+0.0`, which no element cast
    /// tells apart.
    #[inline(always)]
    fn round<S: Simd>(simd: S, v: S::f64s) -> S::f64s {
        let t = v.trunc();
        t + ((v - t) * S::f64s::splat(simd, 2.0)).trunc()
    }

    /// [`round`] on the first vector of `v`, for the tests.
    #[cfg(test)]
    pub(super) fn round_slice<S: Simd>(simd: S, v: &[f64]) -> Vec<f64> {
        let n = S::f64s::LEN;
        round(simd, S::f64s::from_slice(simd, &v[..n]))
            .as_slice()
            .to_vec()
    }

    /// [`super::round_clamp`] on lanes.
    #[inline(always)]
    fn round_clamp<S: Simd>(simd: S, v: S::f64s, max: f64) -> S::f64s {
        round(simd, v)
            .max(S::f64s::splat(simd, 0.0))
            .min(S::f64s::splat(simd, max))
    }

    /// [`super::rgb_to_yuv`] on lanes: `k[c]` the coefficients of the
    /// three inputs for output channel `c`.
    struct Forward<S: Simd> {
        k: [[S::f64s; 3]; 3],
        half: S::f64s,
    }

    impl<S: Simd> Forward<S> {
        fn new(simd: S, half: f64) -> Self {
            let k = [
                [0.299, 0.587, 0.114],
                [-0.169, -0.331, 0.5],
                [0.5, -0.419, -0.081],
            ];
            let mut lanes = [[S::f64s::splat(simd, 0.0); 3]; 3];
            for (row, ks) in lanes.iter_mut().zip(k) {
                for (lane, &c) in row.iter_mut().zip(&ks) {
                    *lane = S::f64s::splat(simd, c);
                }
            }
            Forward {
                k: lanes,
                half: S::f64s::splat(simd, half),
            }
        }

        /// `[y, cb, cr]` of the pixels `(r, g, b)`, unrounded.
        #[inline(always)]
        fn apply(&self, r: S::f64s, g: S::f64s, b: S::f64s) -> [S::f64s; 3] {
            let row = |k: &[S::f64s; 3]| (k[0] * r + k[1] * g) + k[2] * b;
            [
                row(&self.k[0]),
                row(&self.k[1]) + self.half,
                row(&self.k[2]) + self.half,
            ]
        }
    }

    /// [`super::yuv_to_rgb`] on lanes.
    struct Inverse<S: Simd> {
        r_cr: S::f64s,
        g_cb: S::f64s,
        g_cr: S::f64s,
        b_cb: S::f64s,
    }

    impl<S: Simd> Inverse<S> {
        fn new(simd: S) -> Self {
            Inverse {
                r_cr: S::f64s::splat(simd, 1.402),
                g_cb: S::f64s::splat(simd, 0.344),
                g_cr: S::f64s::splat(simd, 0.714),
                b_cb: S::f64s::splat(simd, 1.772),
            }
        }

        /// `[r, g, b]` of the pixels `(y, cb, cr)`, the chroma already
        /// offset to zero, unrounded.
        #[inline(always)]
        fn apply(&self, y: S::f64s, cb: S::f64s, cr: S::f64s) -> [S::f64s; 3] {
            [
                y + self.r_cr * cr,
                (y - self.g_cb * cb) - self.g_cr * cr,
                y + self.b_cb * cb,
            ]
        }
    }

    /// `f64` vectors per vector of the narrowest element, on every level.
    const WIDE: usize = 8;

    /// A vector of `T` as its `f64` vectors, into the head of `w`.
    #[inline(always)]
    fn widen<S: Simd, T: LaneElem>(simd: S, p: S::u8s, w: &mut [S::f64s; WIDE]) {
        T::widen_vec(simd, T::Vec::<S>::from_bytes(p), w);
    }

    /// The inverse of [`widen`].
    #[inline(always)]
    fn narrow<S: Simd, T: LaneElem>(simd: S, w: &[S::f64s]) -> S::u8s {
        T::narrow_vec(simd, w).to_bytes()
    }

    /// [`super::rgb1_mean_scalar`] on lanes, a vector of pixels per step
    /// and all of it in registers: the pixels split into their planes,
    /// each plane widens, and the mean narrows back. Returns the pixels
    /// done, every whole vector of them.
    #[simd]
    pub(super) fn rgb1_mean<S: Simd, T: LaneElem>(simd: S, v: &[T], out: &mut [T]) -> usize {
        let per = T::Vec::<S>::LEN;
        let k = per / S::f64s::LEN;
        let t = split_tables::<S, 3>(simd, std::mem::size_of::<T>());
        let zero = S::f64s::splat(simd, 0.0);
        let three = S::f64s::splat(simd, 3.0);
        let steps = (v.len() / 3).min(out.len()) / per;
        for (src, dst) in v.chunks_exact(3 * per).zip(out.chunks_exact_mut(per)) {
            let split = shuffle::<S, 3>(&t, load_vecs::<S, T, 3>(simd, src));
            let mut wide = [[zero; WIDE]; 3];
            for (c, p) in split.into_iter().enumerate() {
                T::widen_vec(simd, T::Vec::<S>::from_bytes(p), &mut wide[c]);
            }
            let [r, g, b] = wide;
            let mut mean = [zero; WIDE];
            for j in 0..k {
                mean[j] = ((r[j] + g[j]) + b[j]) / three;
            }
            T::narrow_vec(simd, &mean[..k]).store_slice(dst);
        }
        steps * per
    }

    /// [`super::broadcast3_scalar`] on lanes: a vector of values joins
    /// with two copies of itself. Returns the values done.
    #[simd]
    pub(super) fn broadcast3<S: Simd, T: LaneElem>(simd: S, v: &[T], out: &mut [T]) -> usize {
        let per = T::Vec::<S>::LEN;
        let t = join_tables::<S, 3>(simd, std::mem::size_of::<T>());
        let steps = v.len().min(out.len() / 3) / per;
        for (src, dst) in v.chunks_exact(per).zip(out.chunks_exact_mut(3 * per)) {
            let p = T::Vec::<S>::from_slice(simd, src).to_bytes();
            store_vecs::<S, T, 3>(shuffle::<S, 3>(&t, [p; 3]), dst);
        }
        steps * per
    }

    /// The pixel-interleaved side of an RGB layout conversion on lanes, a
    /// vector of pixels per step: RGB1 → RGB2/RGB3 splits each row into
    /// its channel runs, RGB2/RGB3 → RGB1 joins them. `src` and `dst` are
    /// the (ix, c, iy) strides of the two layouts. Returns the columns of
    /// every row that are done: 0 when neither side is RGB1.
    #[simd]
    pub(super) fn rgb1_rows<S: Simd, T: LaneElem>(
        simd: S,
        v: &[T],
        out: &mut [T],
        x: usize,
        y: usize,
        (sx, sc, sy): (usize, usize, usize),
        (dx, dc, dy): (usize, usize, usize),
    ) -> usize {
        let per = T::Vec::<S>::LEN;
        let e = std::mem::size_of::<T>();
        let x0 = x / per * per;
        if sx == 3 && dx == 1 {
            let t = split_tables::<S, 3>(simd, e);
            for iy in 0..y {
                for ix in (0..x0).step_by(per) {
                    let split =
                        shuffle::<S, 3>(&t, load_vecs::<S, T, 3>(simd, &v[iy * sy + ix * 3..]));
                    for (c, p) in split.into_iter().enumerate() {
                        T::Vec::<S>::from_bytes(p)
                            .store_slice(&mut out[c * dc + iy * dy + ix..][..per]);
                    }
                }
            }
            x0
        } else if sx == 1 && dx == 3 {
            let t = join_tables::<S, 3>(simd, e);
            for iy in 0..y {
                for ix in (0..x0).step_by(per) {
                    let planes = std::array::from_fn(|c| {
                        T::Vec::<S>::from_slice(simd, &v[c * sc + iy * sy + ix..][..per]).to_bytes()
                    });
                    store_vecs::<S, T, 3>(
                        shuffle::<S, 3>(&t, planes),
                        &mut out[iy * dy + ix * 3..],
                    );
                }
            }
            x0
        } else {
            0
        }
    }

    /// [`super::yuv444_forward_scalar`] on lanes. Returns the elements
    /// done.
    #[simd]
    pub(super) fn yuv444_forward<S: Simd, T: LaneElem>(
        simd: S,
        v: &[T],
        out: &mut [T],
        half: f64,
        max: f64,
    ) -> usize {
        let per = T::Vec::<S>::LEN;
        let count = per / S::f64s::LEN;
        let e = std::mem::size_of::<T>();
        let (split, join) = (split_tables::<S, 3>(simd, e), join_tables::<S, 3>(simd, e));
        let f = Forward::new(simd, half);
        let zero = S::f64s::splat(simd, 0.0);
        let steps = v.len().min(out.len()) / (3 * per);
        for (src, dst) in v.chunks_exact(3 * per).zip(out.chunks_exact_mut(3 * per)) {
            let planes = shuffle::<S, 3>(&split, load_vecs::<S, T, 3>(simd, src));
            let mut rgb = [[zero; WIDE]; 3];
            for c in 0..3 {
                widen::<S, T>(simd, planes[c], &mut rgb[c]);
            }
            let mut yuv = [[zero; WIDE]; 3];
            for j in 0..count {
                let p = f.apply(rgb[0][j], rgb[1][j], rgb[2][j]);
                for c in 0..3 {
                    yuv[c][j] = round_clamp(simd, p[c], max);
                }
            }
            let mut planes = [S::u8s::splat(simd, 0); 3];
            for c in 0..3 {
                planes[c] = narrow::<S, T>(simd, &yuv[c][..count]);
            }
            store_vecs::<S, T, 3>(shuffle::<S, 3>(&join, planes), dst);
        }
        steps * 3 * per
    }

    /// [`super::yuv444_inverse_scalar`] on lanes. Returns the elements
    /// done.
    #[simd]
    pub(super) fn yuv444_inverse<S: Simd, T: LaneElem>(
        simd: S,
        v: &[T],
        out: &mut [T],
        half: f64,
        max: f64,
    ) -> usize {
        let per = T::Vec::<S>::LEN;
        let count = per / S::f64s::LEN;
        let e = std::mem::size_of::<T>();
        let (split, join) = (split_tables::<S, 3>(simd, e), join_tables::<S, 3>(simd, e));
        let inv = Inverse::new(simd);
        let half = S::f64s::splat(simd, half);
        let zero = S::f64s::splat(simd, 0.0);
        let steps = v.len().min(out.len()) / (3 * per);
        for (src, dst) in v.chunks_exact(3 * per).zip(out.chunks_exact_mut(3 * per)) {
            let planes = shuffle::<S, 3>(&split, load_vecs::<S, T, 3>(simd, src));
            let mut yuv = [[zero; WIDE]; 3];
            for c in 0..3 {
                widen::<S, T>(simd, planes[c], &mut yuv[c]);
            }
            let mut rgb = [[zero; WIDE]; 3];
            for j in 0..count {
                let p = inv.apply(yuv[0][j], yuv[1][j] - half, yuv[2][j] - half);
                for c in 0..3 {
                    rgb[c][j] = round_clamp(simd, p[c], max);
                }
            }
            let mut planes = [S::u8s::splat(simd, 0); 3];
            for c in 0..3 {
                planes[c] = narrow::<S, T>(simd, &rgb[c][..count]);
            }
            store_vecs::<S, T, 3>(shuffle::<S, 3>(&join, planes), dst);
        }
        steps * 3 * per
    }

    /// [`super::yuv422_forward_scalar`] on lanes, a vector of pairs per
    /// step. Returns the pairs done.
    #[simd]
    pub(super) fn yuv422_forward<S: Simd>(simd: S, v: &[u8], out: &mut [u8]) -> usize {
        let n = S::u8s::LEN;
        let count = n / S::f64s::LEN;
        let split = split_tables::<S, 3>(simd, 1);
        let pair = split_tables::<S, 2>(simd, 1);
        let join = join_tables::<S, 4>(simd, 1);
        let f = Forward::new(simd, super::U8_HALF);
        let zero = S::f64s::splat(simd, 0.0);
        let two = S::f64s::splat(simd, 2.0);
        let steps = (v.len() / 6).min(out.len() / 4) / n;
        for (src, dst) in v.chunks_exact(6 * n).zip(out.chunks_exact_mut(4 * n)) {
            let a = shuffle::<S, 3>(&split, load_vecs::<S, u8, 3>(simd, src));
            let b = shuffle::<S, 3>(&split, load_vecs::<S, u8, 3>(simd, &src[3 * n..]));
            // The planes of the first and of the second pixel of each pair.
            let mut rgb = [[[zero; WIDE]; 2]; 3];
            for c in 0..3 {
                let pos = shuffle::<S, 2>(&pair, [a[c], b[c]]);
                for k in 0..2 {
                    widen::<S, u8>(simd, pos[k], &mut rgb[c][k]);
                }
            }
            let mut o = [[zero; WIDE]; 4];
            for j in 0..count {
                let [y0, cb0, cr0] = f.apply(rgb[0][0][j], rgb[1][0][j], rgb[2][0][j]);
                let [y1, cb1, cr1] = f.apply(rgb[0][1][j], rgb[1][1][j], rgb[2][1][j]);
                o[0][j] = round_clamp(simd, (cb0 + cb1) / two, super::U8_MAX);
                o[1][j] = round_clamp(simd, y0, super::U8_MAX);
                o[2][j] = round_clamp(simd, (cr0 + cr1) / two, super::U8_MAX);
                o[3][j] = round_clamp(simd, y1, super::U8_MAX);
            }
            let mut planes = [S::u8s::splat(simd, 0); 4];
            for c in 0..4 {
                planes[c] = narrow::<S, u8>(simd, &o[c][..count]);
            }
            store_vecs::<S, u8, 4>(shuffle::<S, 4>(&join, planes), dst);
        }
        steps * n
    }

    /// [`super::yuv422_inverse_scalar`] on lanes, a vector of pairs per
    /// step. Returns the pairs done.
    #[simd]
    pub(super) fn yuv422_inverse<S: Simd>(simd: S, v: &[u8], out: &mut [u8]) -> usize {
        let n = S::u8s::LEN;
        let count = n / S::f64s::LEN;
        let split = split_tables::<S, 4>(simd, 1);
        let pair = join_tables::<S, 2>(simd, 1);
        let join = join_tables::<S, 3>(simd, 1);
        let inv = Inverse::new(simd);
        let half = S::f64s::splat(simd, super::U8_HALF);
        let zero = S::f64s::splat(simd, 0.0);
        let steps = (v.len() / 4).min(out.len() / 6) / n;
        for (src, dst) in v.chunks_exact(4 * n).zip(out.chunks_exact_mut(6 * n)) {
            let planes = shuffle::<S, 4>(&split, load_vecs::<S, u8, 4>(simd, src));
            let mut uyvy = [[zero; WIDE]; 4];
            for c in 0..4 {
                widen::<S, u8>(simd, planes[c], &mut uyvy[c]);
            }
            let [u, y0, vc, y1] = uyvy;
            // The planes of the first and of the second pixel of each pair.
            let mut rgb = [[[zero; WIDE]; 2]; 3];
            for j in 0..count {
                let (cb, cr) = (u[j] - half, vc[j] - half);
                let p = [inv.apply(y0[j], cb, cr), inv.apply(y1[j], cb, cr)];
                for c in 0..3 {
                    for k in 0..2 {
                        rgb[c][k][j] = round_clamp(simd, p[k][c], super::U8_MAX);
                    }
                }
            }
            let mut planes = [[S::u8s::splat(simd, 0); 2]; 3];
            for c in 0..3 {
                let mut pos = [S::u8s::splat(simd, 0); 2];
                for k in 0..2 {
                    pos[k] = narrow::<S, u8>(simd, &rgb[c][k][..count]);
                }
                planes[c] = shuffle::<S, 2>(&pair, pos);
            }
            for k in 0..2 {
                let px = [planes[0][k], planes[1][k], planes[2][k]];
                store_vecs::<S, u8, 3>(shuffle::<S, 3>(&join, px), &mut dst[3 * n * k..]);
            }
        }
        steps * n
    }

    /// [`super::yuv411_forward_scalar`] on lanes, a vector of quads per
    /// step. Returns the quads done.
    #[simd]
    pub(super) fn yuv411_forward<S: Simd>(simd: S, v: &[u8], out: &mut [u8]) -> usize {
        let n = S::u8s::LEN;
        let count = n / S::f64s::LEN;
        let split = split_tables::<S, 3>(simd, 1);
        let quad = split_tables::<S, 4>(simd, 1);
        let join = join_tables::<S, 6>(simd, 1);
        let f = Forward::new(simd, super::U8_HALF);
        let zero = S::f64s::splat(simd, 0.0);
        let four = S::f64s::splat(simd, 4.0);
        let steps = (v.len() / 12).min(out.len() / 6) / n;
        for (src, dst) in v.chunks_exact(12 * n).zip(out.chunks_exact_mut(6 * n)) {
            let mut q = [[S::u8s::splat(simd, 0); 3]; 4];
            for k in 0..4 {
                q[k] = shuffle::<S, 3>(&split, load_vecs::<S, u8, 3>(simd, &src[3 * n * k..]));
            }
            // The planes of each of the four pixels of a quad.
            let mut pos = [[S::u8s::splat(simd, 0); 4]; 3];
            for c in 0..3 {
                pos[c] = shuffle::<S, 4>(&quad, [q[0][c], q[1][c], q[2][c], q[3][c]]);
            }
            let mut ys = [[zero; WIDE]; 4];
            let mut cb = [zero; WIDE];
            let mut cr = [zero; WIDE];
            for k in 0..4 {
                let mut rgb = [[zero; WIDE]; 3];
                for c in 0..3 {
                    widen::<S, u8>(simd, pos[c][k], &mut rgb[c]);
                }
                for j in 0..count {
                    let [y, cb1, cr1] = f.apply(rgb[0][j], rgb[1][j], rgb[2][j]);
                    ys[k][j] = round_clamp(simd, y, super::U8_MAX);
                    (cb[j], cr[j]) = if k == 0 {
                        (cb1, cr1)
                    } else {
                        (cb[j] + cb1, cr[j] + cr1)
                    };
                }
            }
            let mut planes = [S::u8s::splat(simd, 0); 6];
            for j in 0..count {
                cb[j] = round_clamp(simd, cb[j] / four, super::U8_MAX);
                cr[j] = round_clamp(simd, cr[j] / four, super::U8_MAX);
            }
            planes[0] = narrow::<S, u8>(simd, &cb[..count]);
            planes[3] = narrow::<S, u8>(simd, &cr[..count]);
            for (k, slot) in [1, 2, 4, 5].into_iter().enumerate() {
                planes[slot] = narrow::<S, u8>(simd, &ys[k][..count]);
            }
            store_vecs::<S, u8, 6>(shuffle::<S, 6>(&join, planes), dst);
        }
        steps * n
    }

    /// [`super::yuv411_inverse_scalar`] on lanes, a vector of quads per
    /// step. Returns the quads done.
    #[simd]
    pub(super) fn yuv411_inverse<S: Simd>(simd: S, v: &[u8], out: &mut [u8]) -> usize {
        let n = S::u8s::LEN;
        let count = n / S::f64s::LEN;
        let split = split_tables::<S, 6>(simd, 1);
        let quad = join_tables::<S, 4>(simd, 1);
        let join = join_tables::<S, 3>(simd, 1);
        let inv = Inverse::new(simd);
        let half = S::f64s::splat(simd, super::U8_HALF);
        let zero = S::f64s::splat(simd, 0.0);
        let steps = (v.len() / 6).min(out.len() / 12) / n;
        for (src, dst) in v.chunks_exact(6 * n).zip(out.chunks_exact_mut(12 * n)) {
            let planes = shuffle::<S, 6>(&split, load_vecs::<S, u8, 6>(simd, src));
            let mut uyyvyy = [[zero; WIDE]; 6];
            for c in 0..6 {
                widen::<S, u8>(simd, planes[c], &mut uyyvyy[c]);
            }
            let [u, y0, y1, vc, y2, y3] = uyyvyy;
            // The planes of each of the four pixels of a quad.
            let mut pos = [[S::u8s::splat(simd, 0); 4]; 3];
            for (k, y) in [y0, y1, y2, y3].iter().enumerate() {
                let mut rgb = [[zero; WIDE]; 3];
                for j in 0..count {
                    let p = inv.apply(y[j], u[j] - half, vc[j] - half);
                    for c in 0..3 {
                        rgb[c][j] = round_clamp(simd, p[c], super::U8_MAX);
                    }
                }
                for c in 0..3 {
                    pos[c][k] = narrow::<S, u8>(simd, &rgb[c][..count]);
                }
            }
            let mut ordered = [[S::u8s::splat(simd, 0); 4]; 3];
            for c in 0..3 {
                ordered[c] = shuffle::<S, 4>(&quad, pos[c]);
            }
            for k in 0..4 {
                let px = [ordered[0][k], ordered[1][k], ordered[2][k]];
                store_vecs::<S, u8, 3>(shuffle::<S, 3>(&join, px), &mut dst[3 * n * k..]);
            }
        }
        steps * n
    }
}

#[cfg(all(test, feature = "simd"))]
mod simd_tests {
    use super::*;
    use fearless_simd::{Level, dispatch};

    fn levels() -> Vec<Level> {
        let top = crate::simd::level();
        let mut out = vec![top, Level::baseline()];
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            out.extend(top.as_avx2().map(Level::Avx2));
            out.extend(top.as_sse4_2().map(Level::Sse4_2));
            out.extend(top.as_sse2().map(Level::Sse2));
        }
        out
    }

    /// A pseudo-random frame, with the values whose BT.601 outputs land on
    /// an exact half, on the clamp edges or beyond them mixed in.
    fn frame_u8(n: usize) -> Vec<u8> {
        let mut x = 0x2545_f491u32;
        (0..n)
            .map(|i| match i % 7 {
                0 => 0,
                1 => 255,
                2 => 128,
                3 => 1,
                _ => {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u8
                }
            })
            .collect()
    }

    fn frame_u16(n: usize) -> Vec<u16> {
        let mut x = 0x9e37_79b9u32;
        (0..n)
            .map(|i| match i % 5 {
                0 => 0,
                1 => 65535,
                2 => 32768,
                _ => {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u16
                }
            })
            .collect()
    }

    #[test]
    fn round_matches_f64_round_on_every_level() {
        let cases: Vec<f64> = (-40..40)
            .map(|i| i as f64 * 0.25)
            .chain([
                0.49999999999999994,
                -0.49999999999999994,
                2.5,
                -2.5,
                1e15 + 0.5,
            ])
            .collect();
        for level in levels() {
            for chunk in cases.chunks(2) {
                let mut inp = [0.0; 8];
                inp[..chunk.len()].copy_from_slice(chunk);
                let got: Vec<f64> = dispatch!(level, s => simd_kernels::round_slice(s, &inp));
                for (k, &g) in got.iter().enumerate() {
                    assert_eq!(
                        g.to_bits(),
                        inp[k].round().to_bits(),
                        "{level:?} {}",
                        inp[k]
                    );
                }
            }
        }
    }

    /// Every element type, the integers over their whole range and the
    /// floats with NaN, the infinities and values the `f32` narrowing
    /// rounds mixed in, on a pixel count that leaves a tail.
    #[test]
    fn rgb1_mean_matches_scalar_on_every_level() {
        let pixels = 3 * 64 + 5;
        let mut x = 0x2545_f491_u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        macro_rules! check {
            ($t:ty, $f:expr, $eq:expr) => {{
                let f: fn(u64, usize) -> $t = $f;
                let eq: fn($t, $t) -> bool = $eq;
                let v: Vec<$t> = (0..pixels * 3).map(|i| f(next(), i)).collect();
                let mut want = vec![<$t>::default(); pixels];
                rgb1_mean_scalar(&v, &mut want);
                for level in levels() {
                    let mut got = vec![<$t>::default(); pixels];
                    let done = dispatch!(level, s => simd_kernels::rgb1_mean(s, &v, &mut got));
                    assert!(done > 0 && done <= pixels, "{level:?} {}", stringify!($t));
                    rgb1_mean_scalar(&v[3 * done..], &mut got[done..]);
                    for (i, (&g, &w)) in got.iter().zip(&want).enumerate() {
                        assert!(eq(g, w), "{level:?} {} pixel {i}: {g:?} vs {w:?}", stringify!($t));
                    }
                }
            }};
        }
        macro_rules! ints {
            ($($t:ty),*) => {$(
                check!($t, |x, i| match i % 5 { 0 => <$t>::MIN, 1 => <$t>::MAX, _ => x as $t }, |g, w| g == w);
            )*};
        }
        ints!(i8, u8, i16, u16, i32, u32, i64, u64);
        check!(
            f32,
            |x, i| match i % 7 {
                0 => f32::NAN,
                1 => f32::INFINITY,
                2 => f32::NEG_INFINITY,
                3 => f32::MAX,
                4 => -0.0,
                _ => f32::from_bits(x as u32),
            },
            |g, w| g.to_bits() == w.to_bits() || (g.is_nan() && w.is_nan())
        );
        check!(
            f64,
            |x, i| match i % 7 {
                0 => f64::NAN,
                1 => f64::INFINITY,
                2 => f64::NEG_INFINITY,
                3 => f64::MAX,
                4 => -0.0,
                _ => f64::from_bits(x),
            },
            |g, w| g.to_bits() == w.to_bits() || (g.is_nan() && w.is_nan())
        );
    }

    /// Every element type on a value count that leaves a tail.
    #[test]
    fn broadcast3_matches_scalar_on_every_level() {
        let n = 3 * 64 + 5;
        macro_rules! check {
            ($($t:ty),*) => {$({
                let v: Vec<$t> = (0..n).map(|i| (i * 37 + 11) as $t).collect();
                let mut want = vec![<$t>::default(); 3 * n];
                broadcast3_scalar(&v, &mut want);
                for level in levels() {
                    let mut got = vec![<$t>::default(); 3 * n];
                    let done = dispatch!(level, s => simd_kernels::broadcast3(s, &v, &mut got));
                    assert!(done > 0 && done <= n, "{level:?} {}", stringify!($t));
                    broadcast3_scalar(&v[done..], &mut got[3 * done..]);
                    assert_eq!(got, want, "{level:?} {}", stringify!($t));
                }
            })*};
        }
        check!(i8, u8, i16, u16, i32, u32, i64, u64, f32, f64);
    }

    /// Every layout pair on every element type, at widths below, at and
    /// past one vector of every lane count so the row tails are covered.
    #[test]
    fn rgb1_rows_match_scalar_on_every_level() {
        use NDColorMode::{RGB1, RGB2, RGB3};
        let pairs = [
            (RGB1, RGB2),
            (RGB1, RGB3),
            (RGB2, RGB1),
            (RGB3, RGB1),
            (RGB2, RGB3),
        ];
        macro_rules! check {
            ($($t:ty),*) => {$(
                for (src, dst) in pairs {
                    for (x, y) in [(1, 2), (7, 3), (64, 1), (65, 2), (131, 3)] {
                        let v: Vec<$t> = (0..3 * x * y).map(|i| (i * 37 + 11) as $t).collect();
                        let s = rgb_strides(src, x, y);
                        let d = rgb_strides(dst, x, y);
                        let mut want = vec![<$t>::default(); 3 * x * y];
                        rgb_layout_rows_scalar(&v, &mut want, x, y, 0, s, d);
                        for level in levels() {
                            let mut got = vec![<$t>::default(); 3 * x * y];
                            let x0 = dispatch!(level, s_ => simd_kernels::rgb1_rows(s_, &v, &mut got, x, y, s, d));
                            let lanes = src == RGB1 || dst == RGB1;
                            assert!(if lanes { x < 64 || x0 > 0 } else { x0 == 0 }, "{level:?} {} {src:?}->{dst:?} {x}x{y}: x0 {x0}", stringify!($t));
                            rgb_layout_rows_scalar(&v, &mut got, x, y, x0, s, d);
                            assert_eq!(got, want, "{level:?} {} {src:?}->{dst:?} {x}x{y}", stringify!($t));
                        }
                    }
                }
            )*};
        }
        check!(i8, u8, i16, u16, i32, u32, i64, u64, f32, f64);
    }

    #[test]
    fn yuv444_kernels_match_scalar_on_every_level() {
        let pixels = 2 * 1024 + 97;
        let u8s = frame_u8(pixels * 3);
        let u16s = frame_u16(pixels * 3);
        for level in levels() {
            let mut want = vec![0u8; pixels * 3];
            yuv444_forward_scalar(&u8s, &mut want, U8_HALF, U8_MAX);
            let mut got = vec![0u8; pixels * 3];
            let done = dispatch!(level, s => simd_kernels::yuv444_forward(s, &u8s, &mut got, U8_HALF, U8_MAX));
            assert_eq!(got[..done], want[..done], "{level:?} forward u8");
            let mut want_inv = vec![0u8; pixels * 3];
            yuv444_inverse_scalar(&want, &mut want_inv, U8_HALF, U8_MAX);
            let done = dispatch!(level, s => simd_kernels::yuv444_inverse(s, &want, &mut got, U8_HALF, U8_MAX));
            assert_eq!(got[..done], want_inv[..done], "{level:?} inverse u8");

            let mut want = vec![0u16; pixels * 3];
            yuv444_forward_scalar(&u16s, &mut want, U16_HALF, U16_MAX);
            let mut got = vec![0u16; pixels * 3];
            let done = dispatch!(level, s => simd_kernels::yuv444_forward(s, &u16s, &mut got, U16_HALF, U16_MAX));
            assert_eq!(got[..done], want[..done], "{level:?} forward u16");
            let mut want_inv = vec![0u16; pixels * 3];
            yuv444_inverse_scalar(&want, &mut want_inv, U16_HALF, U16_MAX);
            let done = dispatch!(level, s => simd_kernels::yuv444_inverse(s, &want, &mut got, U16_HALF, U16_MAX));
            assert_eq!(got[..done], want_inv[..done], "{level:?} inverse u16");
        }
    }

    #[test]
    fn yuv422_kernels_match_scalar_on_every_level() {
        let pairs = 1024 + 61;
        let rgb = frame_u8(pairs * 6);
        for level in levels() {
            let mut want = vec![0u8; pairs * 4];
            yuv422_forward_scalar(&rgb, &mut want);
            let mut got = vec![0u8; pairs * 4];
            let done = dispatch!(level, s => simd_kernels::yuv422_forward(s, &rgb, &mut got));
            assert!(done > 0);
            assert_eq!(got[..done * 4], want[..done * 4], "{level:?} forward");
            let mut want_inv = vec![0u8; pairs * 6];
            yuv422_inverse_scalar(&want, &mut want_inv);
            let mut got = vec![0u8; pairs * 6];
            let done = dispatch!(level, s => simd_kernels::yuv422_inverse(s, &want, &mut got));
            assert_eq!(got[..done * 6], want_inv[..done * 6], "{level:?} inverse");
        }
    }

    #[test]
    fn yuv411_kernels_match_scalar_on_every_level() {
        let groups = 512 + 43;
        let rgb = frame_u8(groups * 12);
        for level in levels() {
            let mut want = vec![0u8; groups * 6];
            yuv411_forward_scalar(&rgb, &mut want);
            let mut got = vec![0u8; groups * 6];
            let done = dispatch!(level, s => simd_kernels::yuv411_forward(s, &rgb, &mut got));
            assert!(done > 0);
            assert_eq!(got[..done * 6], want[..done * 6], "{level:?} forward");
            let mut want_inv = vec![0u8; groups * 12];
            yuv411_inverse_scalar(&want, &mut want_inv);
            let mut got = vec![0u8; groups * 12];
            let done = dispatch!(level, s => simd_kernels::yuv411_inverse(s, &want, &mut got));
            assert_eq!(got[..done * 12], want_inv[..done * 12], "{level:?} inverse");
        }
    }

    /// The public entry points on a frame that is not a whole number of
    /// blocks, so the scalar tail is exercised too.
    #[test]
    fn frame_conversions_match_scalar_end_to_end() {
        let (x, y) = (1028, 3);
        let rgb = frame_u8(x * y * 3);
        let mut want = vec![0u8; x * y * 3];
        yuv444_forward_scalar(&rgb, &mut want, U8_HALF, U8_MAX);
        let mut got = vec![0u8; x * y * 3];
        yuv444_forward(&rgb, &mut got, U8_HALF, U8_MAX);
        assert_eq!(got, want);

        let mut want = vec![0u8; x * y * 2];
        yuv422_forward_scalar(&rgb, &mut want);
        let mut got = vec![0u8; x * y * 2];
        yuv422_forward(&rgb, &mut got);
        assert_eq!(got, want);
        let mut back_want = vec![0u8; x * y * 3];
        yuv422_inverse_scalar(&want, &mut back_want);
        let mut back = vec![0u8; x * y * 3];
        yuv422_inverse(&want, &mut back);
        assert_eq!(back, back_want);

        let mut want = vec![0u8; x * y * 6 / 4];
        yuv411_forward_scalar(&rgb, &mut want);
        let mut got = vec![0u8; x * y * 6 / 4];
        yuv411_forward(&rgb, &mut got);
        assert_eq!(got, want);
        let mut back_want = vec![0u8; x * y * 3];
        yuv411_inverse_scalar(&want, &mut back_want);
        let mut back = vec![0u8; x * y * 3];
        yuv411_inverse(&want, &mut back);
        assert_eq!(back, back_want);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> std::sync::Arc<NDArrayPool> {
        NDArrayPool::new(0)
    }

    #[test]
    fn test_mono_to_rgb1() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v[0] = 10;
            v[1] = 20;
            v[2] = 30;
            v[3] = 40;
        }
        let rgb = mono_to_rgb1(&pool(), &arr).unwrap();
        assert_eq!(rgb.dims.len(), 3);
        assert_eq!(rgb.dims[0].size, 3);
        assert_eq!(rgb.dims[1].size, 2);
        assert_eq!(rgb.dims[2].size, 2);
        if let NDDataBuffer::U8(ref v) = rgb.data {
            // First pixel: R=10, G=10, B=10
            assert_eq!(v[0], 10);
            assert_eq!(v[1], 10);
            assert_eq!(v[2], 10);
            // Second pixel
            assert_eq!(v[3], 20);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_rgb1_to_mono() {
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(2),
                NDDimension::new(1),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            // Pixel 0: R=255, G=0, B=0
            v[0] = 255;
            v[1] = 0;
            v[2] = 0;
            // Pixel 1: R=0, G=255, B=0
            v[3] = 0;
            v[4] = 255;
            v[5] = 0;
        }
        let mono = rgb1_to_mono(&pool(), &arr).unwrap();
        assert_eq!(mono.dims.len(), 2);
        assert_eq!(mono.dims[0].size, 2);
        if let NDDataBuffer::U8(ref v) = mono.data {
            // C (R+G+B)/3 truncated, NOT luminance: (255+0+0)/3 = 85.
            assert_eq!(v[0], 85);
            assert_eq!(v[1], 85); // (0+255+0)/3 = 85
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_rgb1_to_rgb2_to_rgb3() {
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(2),
                NDDimension::new(1),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            // pixel 0: R=10, G=20, B=30; pixel 1: R=40, G=50, B=60
            v[0] = 10;
            v[1] = 20;
            v[2] = 30;
            v[3] = 40;
            v[4] = 50;
            v[5] = 60;
        }

        // RGB1 → RGB2
        let rgb2 = convert_rgb_layout(&pool(), &arr, NDColorMode::RGB1, NDColorMode::RGB2).unwrap();
        assert_eq!(rgb2.dims[0].size, 2); // x
        assert_eq!(rgb2.dims[1].size, 3); // color
        assert_eq!(rgb2.dims[2].size, 1); // y

        // RGB2 → RGB3
        let rgb3 =
            convert_rgb_layout(&pool(), &rgb2, NDColorMode::RGB2, NDColorMode::RGB3).unwrap();
        assert_eq!(rgb3.dims[0].size, 2); // x
        assert_eq!(rgb3.dims[1].size, 1); // y
        assert_eq!(rgb3.dims[2].size, 3); // color

        // RGB3 → RGB1 (roundtrip)
        let rgb1_back =
            convert_rgb_layout(&pool(), &rgb3, NDColorMode::RGB3, NDColorMode::RGB1).unwrap();
        if let (NDDataBuffer::U8(orig), NDDataBuffer::U8(back)) = (&arr.data, &rgb1_back.data) {
            assert_eq!(orig, back);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_data_type_u8_to_u16() {
        let mut arr = NDArray::new(vec![NDDimension::new(3)], NDDataType::UInt8);
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v[0] = 10;
            v[1] = 128;
            v[2] = 255;
        }
        let result = convert_data_type(&arr, NDDataType::UInt16).unwrap();
        assert_eq!(result.data.data_type(), NDDataType::UInt16);
        if let NDDataBuffer::U16(ref v) = result.data {
            assert_eq!(v[0], 10);
            assert_eq!(v[1], 128);
            assert_eq!(v[2], 255);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_data_type_f32_to_u8_wraps_like_a_c_cast() {
        let mut arr = NDArray::new(vec![NDDimension::new(3)], NDDataType::Float32);
        if let NDDataBuffer::F32(ref mut v) = arr.data {
            v[0] = -10.0;
            v[1] = 128.7;
            v[2] = 300.0;
        }
        let result = convert_data_type(&arr, NDDataType::UInt8).unwrap();
        if let NDDataBuffer::U8(ref v) = result.data {
            assert_eq!(v[0], 246); // (epicsUInt8)-10
            assert_eq!(v[1], 128); // truncated
            assert_eq!(v[2], 44); // (epicsUInt8)300
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_data_type_same_type_noop() {
        let arr = NDArray::new(vec![NDDimension::new(5)], NDDataType::UInt8);
        let result = convert_data_type(&arr, NDDataType::UInt8).unwrap();
        assert_eq!(result.data.len(), 5);
        assert_eq!(result.data.data_type(), NDDataType::UInt8);
    }

    #[test]
    fn test_color_mode_from_i32() {
        assert_eq!(NDColorMode::from_i32(0), NDColorMode::Mono);
        assert_eq!(NDColorMode::from_i32(2), NDColorMode::RGB1);
        assert_eq!(NDColorMode::from_i32(99), NDColorMode::Mono);
    }

    #[test]
    fn test_rgb1_to_yuv444_roundtrip() {
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(2),
                NDDimension::new(2),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v[0] = 100;
            v[1] = 150;
            v[2] = 200;
            v[3] = 50;
            v[4] = 100;
            v[5] = 50;
            v[6] = 255;
            v[7] = 0;
            v[8] = 0;
            v[9] = 0;
            v[10] = 255;
            v[11] = 0;
        }
        let yuv = rgb1_to_yuv444(&pool(), &arr).unwrap();
        assert_eq!(yuv.dims.len(), 3);
        assert_eq!(yuv.dims[0].size, 3);

        let back = yuv444_to_rgb1(&pool(), &yuv).unwrap();
        if let (NDDataBuffer::U8(orig), NDDataBuffer::U8(result)) = (&arr.data, &back.data) {
            for i in 0..orig.len() {
                assert!(
                    (orig[i] as i16 - result[i] as i16).unsigned_abs() <= 2,
                    "pixel diff at {}: orig={}, result={}",
                    i,
                    orig[i],
                    result[i],
                );
            }
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_rgb1_to_yuv422_roundtrip() {
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(2),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            let colors: [u8; 24] = [
                100, 150, 200, 50, 100, 50, 255, 0, 0, 0, 255, 0, 128, 128, 128, 200, 100, 50, 0,
                0, 255, 255, 255, 0,
            ];
            v[..24].copy_from_slice(&colors);
        }
        let yuv = rgb1_to_yuv422(&pool(), &arr).unwrap();
        assert_eq!(yuv.dims.len(), 2);
        assert_eq!(yuv.dims[0].size, 8);

        let back = yuv422_to_rgb1(&pool(), &yuv).unwrap();
        assert_eq!(back.dims[0].size, 3);
        assert_eq!(back.dims[1].size, 4);
        assert_eq!(back.dims[2].size, 2);
    }

    #[test]
    fn test_rgb1_to_yuv411_roundtrip() {
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(2),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            let colors: [u8; 24] = [
                100, 150, 200, 50, 100, 50, 255, 0, 0, 0, 255, 0, 128, 128, 128, 200, 100, 50, 0,
                0, 255, 255, 255, 0,
            ];
            v[..24].copy_from_slice(&colors);
        }
        let yuv = rgb1_to_yuv411(&pool(), &arr).unwrap();
        assert_eq!(yuv.dims.len(), 2);
        assert_eq!(yuv.dims[0].size, 6);

        let back = yuv411_to_rgb1(&pool(), &yuv).unwrap();
        assert_eq!(back.dims[0].size, 3);
        assert_eq!(back.dims[1].size, 4);
        assert_eq!(back.dims[2].size, 2);
    }

    #[test]
    fn test_mono_to_rgb1_u16() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(1)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            v[0] = 1000;
            v[1] = 2000;
        }
        let rgb = mono_to_rgb1(&pool(), &arr).unwrap();
        if let NDDataBuffer::U16(ref v) = rgb.data {
            assert_eq!(v[0], 1000);
            assert_eq!(v[1], 1000);
            assert_eq!(v[2], 1000);
            assert_eq!(v[3], 2000);
        } else {
            panic!("wrong type");
        }
    }
}
