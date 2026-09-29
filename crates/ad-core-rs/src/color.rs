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
    arr.timestamp = src.timestamp;
    arr.time_stamp = src.time_stamp;
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
        for (i, px) in out.chunks_exact_mut(3).enumerate().take(n) {
            px.fill(v[i]);
        }
    });
    Ok(arr)
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

    macro_rules! rgb1_to_mono_typed {
        ($v:expr, $out:expr, $T:ty) => {{
            let out: &mut [$T] = $out;
            for (i, o) in out.iter_mut().enumerate().take(n) {
                let r = $v[i * 3] as f64;
                let g = $v[i * 3 + 1] as f64;
                let b = $v[i * 3 + 2] as f64;
                // C: value = (R+G+B)/3. then (epicsType)value — truncate.
                *o = ((r + g + b) / 3.0) as $T;
            }
        }};
    }

    let dims = vec![NDDimension::new(x), NDDimension::new(y)];
    let mut arr = output(pool, src, dims, src.data.data_type())?;
    match (&src.data, &mut arr.data) {
        (NDDataBuffer::I8(v), NDDataBuffer::I8(out)) => rgb1_to_mono_typed!(v, out, i8),
        (NDDataBuffer::U8(v), NDDataBuffer::U8(out)) => rgb1_to_mono_typed!(v, out, u8),
        (NDDataBuffer::I16(v), NDDataBuffer::I16(out)) => rgb1_to_mono_typed!(v, out, i16),
        (NDDataBuffer::U16(v), NDDataBuffer::U16(out)) => rgb1_to_mono_typed!(v, out, u16),
        (NDDataBuffer::I32(v), NDDataBuffer::I32(out)) => rgb1_to_mono_typed!(v, out, i32),
        (NDDataBuffer::U32(v), NDDataBuffer::U32(out)) => rgb1_to_mono_typed!(v, out, u32),
        (NDDataBuffer::I64(v), NDDataBuffer::I64(out)) => rgb1_to_mono_typed!(v, out, i64),
        (NDDataBuffer::U64(v), NDDataBuffer::U64(out)) => rgb1_to_mono_typed!(v, out, u64),
        (NDDataBuffer::F32(v), NDDataBuffer::F32(out)) => rgb1_to_mono_typed!(v, out, f32),
        (NDDataBuffer::F64(v), NDDataBuffer::F64(out)) => rgb1_to_mono_typed!(v, out, f64),
        _ => unreachable!("the output was allocated in the source type"),
    }
    Ok(arr)
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

    // Each layout is a stride triple over (ix, c, iy); a run along x in
    // either layout is then a strided copy, contiguous in RGB2 and RGB3.
    let strides = |mode: NDColorMode| match mode {
        NDColorMode::RGB1 => (3, 1, x * 3),
        NDColorMode::RGB2 => (1, x, x * 3),
        NDColorMode::RGB3 => (1, x * y, x),
        _ => unreachable!("checked above"),
    };
    let (sx, sc, sy) = strides(src_mode);
    let (dx, dc, dy) = strides(dst_mode);
    let mut arr = output(pool, src, out_dims, src.data.data_type())?;
    same_type!(&src.data, &mut arr.data, |v, out| {
        for iy in 0..y {
            for c in 0..3usize {
                let s = &v[c * sc + iy * sy..];
                let d = &mut out[c * dc + iy * dy..];
                if sx == 1 && dx == 1 {
                    d[..x].copy_from_slice(&s[..x]);
                } else {
                    for (o, i) in d.iter_mut().step_by(dx).zip(s.iter().step_by(sx)).take(x) {
                        *o = *i;
                    }
                }
            }
        }
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

/// An element type the YUV conversions run in.
trait YuvElem: Copy {
    fn to_f64(self) -> f64;
    fn from_f64(v: f64) -> Self;
    /// [`to_f64`](Self::to_f64) over a slice, on lanes.
    #[cfg(feature = "simd")]
    fn widen<S: fearless_simd::Simd>(simd: S, v: &[Self], out: &mut [f64]);
    /// [`from_f64`](Self::from_f64) over a slice, on lanes.
    #[cfg(feature = "simd")]
    fn narrow<S: fearless_simd::Simd>(simd: S, v: &[f64], out: &mut [Self]);
}

macro_rules! yuv_elem {
    ($t:ty, $widen:ident, $narrow:ident) => {
        impl YuvElem for $t {
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
            fn widen<S: fearless_simd::Simd>(simd: S, v: &[Self], out: &mut [f64]) {
                crate::simd::$widen(simd, v, out)
            }
            #[cfg(feature = "simd")]
            #[inline(always)]
            fn narrow<S: fearless_simd::Simd>(simd: S, v: &[f64], out: &mut [Self]) {
                crate::simd::$narrow(simd, v, out)
            }
        }
    };
}

yuv_elem!(u8, to_f64_u8, from_f64_u8);
yuv_elem!(u16, to_f64_u16, from_f64_u16);

/// RGB1 pixels to YUV444 pixels, both `[3, n]`.
fn yuv444_forward<T: YuvElem>(v: &[T], out: &mut [T], half: f64, max: f64) {
    #[cfg(feature = "simd")]
    {
        let done = fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::yuv444_forward(s, v, out, half, max));
        yuv444_forward_scalar(&v[done..], &mut out[done..], half, max);
    }
    #[cfg(not(feature = "simd"))]
    yuv444_forward_scalar(v, out, half, max);
}

fn yuv444_forward_scalar<T: YuvElem>(v: &[T], out: &mut [T], half: f64, max: f64) {
    for (px, o) in v.chunks_exact(3).zip(out.chunks_exact_mut(3)) {
        let (y, cb, cr) = rgb_to_yuv(px[0].to_f64(), px[1].to_f64(), px[2].to_f64(), half);
        o[0] = T::from_f64(round_clamp(y, max));
        o[1] = T::from_f64(round_clamp(cb, max));
        o[2] = T::from_f64(round_clamp(cr, max));
    }
}

/// YUV444 pixels to RGB1 pixels, both `[3, n]`.
fn yuv444_inverse<T: YuvElem>(v: &[T], out: &mut [T], half: f64, max: f64) {
    #[cfg(feature = "simd")]
    {
        let done = fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::yuv444_inverse(s, v, out, half, max));
        yuv444_inverse_scalar(&v[done..], &mut out[done..], half, max);
    }
    #[cfg(not(feature = "simd"))]
    yuv444_inverse_scalar(v, out, half, max);
}

fn yuv444_inverse_scalar<T: YuvElem>(v: &[T], out: &mut [T], half: f64, max: f64) {
    for (px, o) in v.chunks_exact(3).zip(out.chunks_exact_mut(3)) {
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
    for (px, o) in v.chunks_exact(6).zip(out.chunks_exact_mut(4)) {
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
    for (px, o) in v.chunks_exact(4).zip(out.chunks_exact_mut(6)) {
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
    for (px, o) in v.chunks_exact(12).zip(out.chunks_exact_mut(6)) {
        let mut ys = [0u8; 4];
        let mut cbs = [0.0f64; 4];
        let mut crs = [0.0f64; 4];
        for (p, q) in px.chunks_exact(3).enumerate() {
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
    for (px, o) in v.chunks_exact(6).zip(out.chunks_exact_mut(12)) {
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

/// The YUV conversions on explicit vectors, one block of [`BLOCK`] pixels
/// at a time: the block is widened to `f64` with the cast kernels, the
/// BT.601 arithmetic runs on the interleaved stream in place, and the
/// result narrows back with the cast kernels. Each kernel converts the
/// whole prefix of its input that fills blocks and returns how many
/// elements, pairs or quads it covered; the scalar loop finishes the rest.
///
/// The interleaved stream needs no gather: element `i` of a three-channel
/// stream belongs to the pixel starting at `i - c` with `c = i % 3`, so
/// its three inputs are the unaligned loads at offsets `-2..=2` from `i`,
/// picked per lane by masks that depend only on `c` — and so do the
/// coefficients, which become per-lane vectors too. With the vector start
/// advancing by the lane count, `c` of lane 0 cycles through three
/// phases, each with its own mask and coefficient set built once per
/// call. The arithmetic between is the scalar expression, operation for
/// operation, and the round-half-away-from-zero of `f64::round` is
/// spelled out on lanes (the vector `round_ties_even` is not it), so a
/// frame is bit for bit the same on either path. The channels whose
/// scalar expression lacks a term get the coefficient `0.0`; that adds a
/// signed zero to a value that is never `-0.0`, which leaves it unchanged.
#[cfg(feature = "simd")]
mod simd_kernels {
    use super::YuvElem;
    use fearless_simd::{Simd, prelude::*};
    use fearless_simd_macros::simd;

    /// Pixels per block. Three, two and one-and-a-half elements per pixel
    /// all give a whole number of vectors of every lane count.
    const BLOCK: usize = 1024;
    /// Elements either side of the widened block that the shifted loads
    /// may reach.
    const PAD: usize = 8;

    /// `f64::round` on lanes: the truncation, plus one away from zero
    /// where the fraction reaches a half. `v - trunc(v)` is exact, and
    /// the added term takes the sign of `v` even when it is zero so that
    /// `-0.25` rounds to `-0.0` as the scalar does.
    #[inline(always)]
    fn round<S: Simd>(simd: S, v: S::f64s) -> S::f64s {
        let t = v.trunc();
        let away = (v - t).abs().simd_ge(S::f64s::splat(simd, 0.5));
        t + away
            .select(S::f64s::splat(simd, 1.0), S::f64s::splat(simd, 0.0))
            .copysign(v)
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

    /// [`round_clamp`] over a slice, in place.
    #[inline(always)]
    fn round_clamp_slice<S: Simd>(simd: S, v: &mut [f64], max: f64) {
        for c in v.chunks_exact_mut(S::f64s::LEN) {
            round_clamp(simd, S::f64s::from_slice(simd, c), max).store_slice(c);
        }
    }

    /// The lane masks and coefficients for the vectors whose lane 0 is
    /// channel `phase` of its pixel.
    struct Phase<S: Simd> {
        /// Lanes that are channel 1 and channel 2 of their pixel.
        m1: S::mask64s,
        m2: S::mask64s,
        /// The coefficient of each of the three inputs, and the constant.
        k: [S::f64s; 3],
        h: S::f64s,
    }

    /// The three phases of a coefficient table with one row per channel.
    fn phases<S: Simd>(simd: S, table: [[f64; 4]; 3]) -> [Phase<S>; 3] {
        std::array::from_fn(|phase| {
            let chan = |lane: usize| (phase + lane) % 3;
            let row = |j: usize| S::f64s::from_fn(simd, |lane| table[chan(lane)][j]);
            let is = |c: usize| {
                S::f64s::from_fn(simd, |lane| (chan(lane) == c) as u8 as f64)
                    .simd_eq(S::f64s::splat(simd, 1.0))
            };
            Phase {
                m1: is(1),
                m2: is(2),
                k: [row(0), row(1), row(2)],
                h: row(3),
            }
        })
    }

    /// The three inputs of the pixel each lane belongs to, for the vector
    /// starting at element `at` of the padded stream `x`.
    #[inline(always)]
    fn pixel_lanes<S: Simd>(
        simd: S,
        ph: &Phase<S>,
        x: &[f64],
        at: usize,
    ) -> (S::f64s, S::f64s, S::f64s) {
        let n = S::f64s::LEN;
        let load = |o: usize| S::f64s::from_slice(simd, &x[at + o - 2..at + o - 2 + n]);
        let (lm2, lm1, l0, l1, l2) = (load(0), load(1), load(2), load(3), load(4));
        (
            ph.m1.select(lm1, ph.m2.select(lm2, l0)),
            ph.m1.select(l0, ph.m2.select(lm1, l1)),
            ph.m1.select(l1, ph.m2.select(l0, l2)),
        )
    }

    /// [`super::rgb_to_yuv`] over the padded RGB stream `x` into `out`,
    /// rounded and clamped when `max` is given.
    #[inline(always)]
    fn forward_block<S: Simd>(
        simd: S,
        ph: &[Phase<S>; 3],
        x: &[f64],
        out: &mut [f64],
        max: Option<f64>,
    ) {
        let n = S::f64s::LEN;
        for (k, o) in out.chunks_exact_mut(n).enumerate() {
            let ph = &ph[(k * n) % 3];
            let (r, g, b) = pixel_lanes(simd, ph, x, PAD + k * n);
            let v = ((ph.k[0] * r + ph.k[1] * g) + ph.k[2] * b) + ph.h;
            match max {
                Some(max) => round_clamp(simd, v, max).store_slice(o),
                None => v.store_slice(o),
            }
        }
    }

    /// [`super::yuv_to_rgb`] over the padded YUV stream `x` into `out`,
    /// rounded and clamped.
    #[inline(always)]
    fn inverse_block<S: Simd>(
        simd: S,
        ph: &[Phase<S>; 3],
        x: &[f64],
        out: &mut [f64],
        half: f64,
        max: f64,
    ) {
        let n = S::f64s::LEN;
        let half = S::f64s::splat(simd, half);
        for (k, o) in out.chunks_exact_mut(n).enumerate() {
            let ph = &ph[(k * n) % 3];
            let (y, cb, cr) = pixel_lanes(simd, ph, x, PAD + k * n);
            let v = (y + ph.k[1] * (cb - half)) + ph.k[2] * (cr - half);
            round_clamp(simd, v, max).store_slice(o);
        }
    }

    fn forward_table(half: f64) -> [[f64; 4]; 3] {
        [
            [0.299, 0.587, 0.114, 0.0],
            [-0.169, -0.331, 0.5, half],
            [0.5, -0.419, -0.081, half],
        ]
    }

    /// Column 0 is unused: the inverse takes `y` itself.
    const INVERSE_TABLE: [[f64; 4]; 3] = [
        [0.0, 0.0, 1.402, 0.0],
        [0.0, -0.344, -0.714, 0.0],
        [0.0, 1.772, 0.0, 0.0],
    ];

    fn padded() -> Vec<f64> {
        vec![0.0; PAD + 3 * BLOCK + PAD]
    }

    #[simd]
    pub(super) fn yuv444_forward<S: Simd, T: YuvElem>(
        simd: S,
        v: &[T],
        out: &mut [T],
        half: f64,
        max: f64,
    ) -> usize {
        let ph = phases(simd, forward_table(half));
        let mut x = padded();
        let mut y = vec![0.0; 3 * BLOCK];
        let blocks = v.len() / (3 * BLOCK);
        for (src, dst) in v
            .chunks_exact(3 * BLOCK)
            .zip(out.chunks_exact_mut(3 * BLOCK))
        {
            T::widen(simd, src, &mut x[PAD..PAD + 3 * BLOCK]);
            forward_block(simd, &ph, &x, &mut y, Some(max));
            T::narrow(simd, &y, dst);
        }
        blocks * 3 * BLOCK
    }

    #[simd]
    pub(super) fn yuv444_inverse<S: Simd, T: YuvElem>(
        simd: S,
        v: &[T],
        out: &mut [T],
        half: f64,
        max: f64,
    ) -> usize {
        let ph = phases(simd, INVERSE_TABLE);
        let mut x = padded();
        let mut y = vec![0.0; 3 * BLOCK];
        let blocks = v.len() / (3 * BLOCK);
        for (src, dst) in v
            .chunks_exact(3 * BLOCK)
            .zip(out.chunks_exact_mut(3 * BLOCK))
        {
            T::widen(simd, src, &mut x[PAD..PAD + 3 * BLOCK]);
            inverse_block(simd, &ph, &x, &mut y, half, max);
            T::narrow(simd, &y, dst);
        }
        blocks * 3 * BLOCK
    }

    #[simd]
    pub(super) fn yuv422_forward<S: Simd>(simd: S, v: &[u8], out: &mut [u8]) -> usize {
        let ph = phases(simd, forward_table(super::U8_HALF));
        let mut x = padded();
        let mut y = vec![0.0; 3 * BLOCK];
        let mut packed = vec![0.0; 2 * BLOCK];
        let blocks = v.len() / (3 * BLOCK);
        for (src, dst) in v
            .chunks_exact(3 * BLOCK)
            .zip(out.chunks_exact_mut(2 * BLOCK))
        {
            u8::widen(simd, src, &mut x[PAD..PAD + 3 * BLOCK]);
            forward_block(simd, &ph, &x, &mut y, None);
            for (p, o) in y.chunks_exact(6).zip(packed.chunks_exact_mut(4)) {
                o[0] = (p[1] + p[4]) / 2.0;
                o[1] = p[0];
                o[2] = (p[2] + p[5]) / 2.0;
                o[3] = p[3];
            }
            round_clamp_slice(simd, &mut packed, super::U8_MAX);
            u8::narrow(simd, &packed, dst);
        }
        blocks * BLOCK / 2
    }

    #[simd]
    pub(super) fn yuv422_inverse<S: Simd>(simd: S, v: &[u8], out: &mut [u8]) -> usize {
        let ph = phases(simd, INVERSE_TABLE);
        let mut packed = vec![0.0; 2 * BLOCK];
        let mut x = padded();
        let mut y = vec![0.0; 3 * BLOCK];
        let blocks = v.len() / (2 * BLOCK);
        for (src, dst) in v
            .chunks_exact(2 * BLOCK)
            .zip(out.chunks_exact_mut(3 * BLOCK))
        {
            u8::widen(simd, src, &mut packed);
            for (p, o) in packed.chunks_exact(4).zip(x[PAD..].chunks_exact_mut(6)) {
                o[0] = p[1];
                o[1] = p[0];
                o[2] = p[2];
                o[3] = p[3];
                o[4] = p[0];
                o[5] = p[2];
            }
            inverse_block(simd, &ph, &x, &mut y, super::U8_HALF, super::U8_MAX);
            u8::narrow(simd, &y, dst);
        }
        blocks * BLOCK / 2
    }

    #[simd]
    pub(super) fn yuv411_forward<S: Simd>(simd: S, v: &[u8], out: &mut [u8]) -> usize {
        let ph = phases(simd, forward_table(super::U8_HALF));
        let mut x = padded();
        let mut y = vec![0.0; 3 * BLOCK];
        let mut packed = vec![0.0; 6 * BLOCK / 4];
        let blocks = v.len() / (3 * BLOCK);
        for (src, dst) in v
            .chunks_exact(3 * BLOCK)
            .zip(out.chunks_exact_mut(6 * BLOCK / 4))
        {
            u8::widen(simd, src, &mut x[PAD..PAD + 3 * BLOCK]);
            forward_block(simd, &ph, &x, &mut y, None);
            for (p, o) in y.chunks_exact(12).zip(packed.chunks_exact_mut(6)) {
                o[0] = (((p[1] + p[4]) + p[7]) + p[10]) / 4.0;
                o[1] = p[0];
                o[2] = p[3];
                o[3] = (((p[2] + p[5]) + p[8]) + p[11]) / 4.0;
                o[4] = p[6];
                o[5] = p[9];
            }
            round_clamp_slice(simd, &mut packed, super::U8_MAX);
            u8::narrow(simd, &packed, dst);
        }
        blocks * BLOCK / 4
    }

    #[simd]
    pub(super) fn yuv411_inverse<S: Simd>(simd: S, v: &[u8], out: &mut [u8]) -> usize {
        let ph = phases(simd, INVERSE_TABLE);
        let mut packed = vec![0.0; 6 * BLOCK / 4];
        let mut x = padded();
        let mut y = vec![0.0; 3 * BLOCK];
        let blocks = v.len() / (6 * BLOCK / 4);
        for (src, dst) in v
            .chunks_exact(6 * BLOCK / 4)
            .zip(out.chunks_exact_mut(3 * BLOCK))
        {
            u8::widen(simd, src, &mut packed);
            for (p, o) in packed.chunks_exact(6).zip(x[PAD..].chunks_exact_mut(12)) {
                for (k, &yv) in [p[1], p[2], p[4], p[5]].iter().enumerate() {
                    o[k * 3] = yv;
                    o[k * 3 + 1] = p[0];
                    o[k * 3 + 2] = p[3];
                }
            }
            inverse_block(simd, &ph, &x, &mut y, super::U8_HALF, super::U8_MAX);
            u8::narrow(simd, &y, dst);
        }
        blocks * BLOCK / 4
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
