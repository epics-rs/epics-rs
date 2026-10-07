//! NDArray type/dimension conversion — the single owner of C++
//! `NDArrayPool::convert()` semantics.
//!
//! Everything that turns one NDArray into another with a different element
//! type, sub-region, binning or reversal goes through here. C++ has exactly
//! two conversion kernels and every plugin calls them via
//! `pNDArrayPool->convert()`:
//!
//! * `convertType` (`NDArrayPool.cpp:378-388`) —
//!   `*pDataOut++ = (dataTypeOut)(*pDataIn++)`: a plain C cast per element.
//! * `convertDim` (`NDArrayPool.cpp:434-471`) —
//!   `*pDOut += (dataTypeOut)*pDIn`: each source element is cast to the
//!   OUTPUT type and summed **in the output type**, so a bin sum that
//!   overflows wraps modulo the output width.
//!
//! A C cast is NOT Rust's saturating `as` on the float→int edge, and it is
//! NOT a clamp: narrowing truncates to the low bits (`(epicsUInt8)300 ==
//! 44`), same-width sign changes reinterpret (`(epicsInt8)(epicsUInt8)255
//! == -1`). Both kernels cast through one definition, `CCast`, and never
//! clamp. Any plugin that re-implements extraction with an f64 accumulator
//! plus a clamp/saturate re-opens this divergence — call [`convert_dims`] /
//! [`convert_type`] instead.

use crate::error::{ADError, ADResult};
use crate::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};

/// Accumulator for [`convert_dims`]'s binning sum. C `convertDim`
/// (`NDArrayPool.cpp:465`) does `*pDOut += (dataTypeOut)*pDIn` — each source
/// element is cast to the OUTPUT type and summed in the OUTPUT type, with C
/// integer arithmetic wrapping on overflow. The port does the same in the
/// target element type itself: integer targets wrap on every add, float
/// targets round as C's `+=` does, so no wider accumulator and no final
/// reduction are needed and a bin sum is the C sum bit for bit.
trait BinAcc: Copy {
    const ZERO: Self;
    fn bin_add(self, rhs: Self) -> Self;

    /// The bin widths [`bin_native`](Self::bin_native) adds on lanes; every
    /// other width is added one element at a time.
    const LANE_BINS: &'static [usize];

    /// Adds `src` into `acc`, `bin` consecutive elements per accumulator in
    /// element order: `src.len() == acc.len() * bin`.
    fn bin_native(src: &[Self], bin: usize, acc: &mut [Self]);
}

/// The [`BinAcc`] bin loop with `cast` applied to every source element: the
/// scalar path of every row operation below. The bin sum is kept in a local
/// so the inner loop carries a register and not a store, and is unrolled.
#[inline]
fn bin_row_with<S: Copy, D: BinAcc>(src: &[S], bin: usize, acc: &mut [D], cast: impl Fn(S) -> D) {
    if bin == 1 {
        for (a, &s) in acc.iter_mut().zip(src) {
            *a = a.bin_add(cast(s));
        }
    } else {
        for (a, w) in acc.iter_mut().zip(src.chunks_exact(bin)) {
            let mut t = *a;
            for &s in w {
                t = t.bin_add(cast(s));
            }
            *a = t;
        }
    }
}

/// `$zero`, the add, and the `bin2`/`bin4` kernels of one target type.
macro_rules! bin_acc {
    ($t:ty, $zero:expr, |$a:ident, $b:ident| $add:expr, $bin2:ident, $bin4:ident) => {
        impl BinAcc for $t {
            const ZERO: Self = $zero;

            #[inline]
            fn bin_add(self, rhs: Self) -> Self {
                let ($a, $b) = (self, rhs);
                $add
            }

            #[cfg(feature = "simd")]
            const LANE_BINS: &'static [usize] = &[2, 4];

            #[cfg(feature = "simd")]
            fn bin_native(src: &[Self], bin: usize, acc: &mut [Self]) {
                match bin {
                    2 => fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::$bin2(s, src, acc)),
                    4 => fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::$bin4(s, src, acc)),
                    _ => bin_row_with(src, bin, acc, |x| x),
                }
            }

            #[cfg(not(feature = "simd"))]
            const LANE_BINS: &'static [usize] = &[];

            #[cfg(not(feature = "simd"))]
            #[inline]
            fn bin_native(src: &[Self], bin: usize, acc: &mut [Self]) {
                bin_row_with(src, bin, acc, |x| x);
            }
        }
    };
}

bin_acc!(i8, 0, |a, b| a.wrapping_add(b), bin2_i8, bin4_i8);
bin_acc!(u8, 0, |a, b| a.wrapping_add(b), bin2_u8, bin4_u8);
bin_acc!(i16, 0, |a, b| a.wrapping_add(b), bin2_i16, bin4_i16);
bin_acc!(u16, 0, |a, b| a.wrapping_add(b), bin2_u16, bin4_u16);
bin_acc!(i32, 0, |a, b| a.wrapping_add(b), bin2_i32, bin4_i32);
bin_acc!(u32, 0, |a, b| a.wrapping_add(b), bin2_u32, bin4_u32);
bin_acc!(i64, 0, |a, b| a.wrapping_add(b), bin2_i64, bin4_i64);
bin_acc!(u64, 0, |a, b| a.wrapping_add(b), bin2_u64, bin4_u64);
bin_acc!(f32, 0.0, |a, b| a + b, bin2_f32, bin4_f32);
bin_acc!(f64, 0.0, |a, b| a + b, bin2_f64, bin4_f64);

/// The C cast `(D)value` of a source element, and the row operations of
/// [`convert_dims`] built on it.
///
/// Between integer types and between float types Rust's `as` is the C cast.
/// Float to integer is the one edge where they differ: C truncates toward
/// zero and then reduces modulo the target width (`(epicsUInt8)300.0 ==
/// 44`), Rust's `as` saturates. `value as i128 as D` is the C result for
/// every finite value below 2^127 in magnitude, which is every value a
/// detector produces; `convertDim` leaves larger ones and NaN undefined, and
/// the port makes them the saturated `i128` reduced the same way (NaN is 0).
trait CCast<D: BinAcc>: Copy {
    fn c_cast(self) -> D;

    /// Appends the cast of every element of `src` to `out`.
    #[inline]
    fn cast_extend(src: &[Self], out: &mut Vec<D>) {
        out.extend(src.iter().map(|&s| s.c_cast()));
    }

    /// Adds the cast of `src` into `acc`, `bin` consecutive source elements
    /// per accumulator: `src.len() == acc.len() * bin`. A bin width the
    /// target adds on lanes is cast a chunk at a time and added as the
    /// target type; any other width is cast and added in one scalar loop.
    #[inline]
    fn bin_row(src: &[Self], bin: usize, acc: &mut [D]) {
        if D::LANE_BINS.contains(&bin) {
            bin_row_chunked(src, bin, acc, |c, o| {
                for (o, &s) in o.iter_mut().zip(c) {
                    *o = s.c_cast();
                }
            });
        } else {
            bin_row_with(src, bin, acc, CCast::c_cast);
        }
    }
}

/// [`CCast::bin_row`] through a cast chunk: `cast` fills `out[i] = src[i]`
/// as `D` for a run of whole bins that stays in cache, and the run is added
/// by [`BinAcc::bin_native`].
fn bin_row_chunked<S: Copy, D: BinAcc>(
    src: &[S],
    bin: usize,
    acc: &mut [D],
    cast: impl Fn(&[S], &mut [D]),
) {
    const CHUNK: usize = 1024;
    let mut stack = [D::ZERO; CHUNK];
    let mut heap = Vec::new();
    let (buf, step): (&mut [D], usize) = if bin <= CHUNK {
        (&mut stack, CHUNK / bin * bin)
    } else {
        heap.resize(bin, D::ZERO);
        (&mut heap, bin)
    };
    for (c, a) in src.chunks(step).zip(acc.chunks_mut(step / bin)) {
        let buf = &mut buf[..c.len()];
        cast(c, buf);
        D::bin_native(buf, bin, a);
    }
}

/// The identity cast: the row is the target type already, so binning is
/// [`BinAcc::bin_native`] on the source itself.
macro_rules! c_cast_same {
    ($($t:ty),*) => {$(
        impl CCast<$t> for $t {
            #[inline]
            fn c_cast(self) -> $t {
                self
            }

            #[inline]
            fn bin_row(src: &[Self], bin: usize, acc: &mut [$t]) {
                <$t as BinAcc>::bin_native(src, bin, acc);
            }
        }
    )*};
}

c_cast_same!(i8, u8, i16, u16, i32, u32, i64, u64, f32, f64);

macro_rules! c_cast_as {
    ($s:ty => $($d:ty),*) => {$(
        impl CCast<$d> for $s {
            #[inline]
            fn c_cast(self) -> $d {
                self as $d
            }
        }
    )*};
}

c_cast_as!(i8 => u8, i16, u16, i32, u32, i64, u64, f32, f64);
c_cast_as!(u8 => i8, i16, u16, i32, u32, i64, u64, f32, f64);
c_cast_as!(i16 => i8, u8, u16, i32, u32, i64, u64, f32, f64);
c_cast_as!(u16 => i8, u8, i16, i32, u32, i64, u64, f32, f64);
c_cast_as!(i32 => i8, u8, i16, u16, u32, i64, u64, f32, f64);
c_cast_as!(u32 => i8, u8, i16, u16, i32, i64, u64, f32, f64);
c_cast_as!(i64 => i8, u8, i16, u16, i32, u32, u64, f32, f64);
c_cast_as!(u64 => i8, u8, i16, u16, i32, u32, i64, f32, f64);
c_cast_as!(f32 => f64);
c_cast_as!(f64 => f32);

macro_rules! c_cast_float_to_int {
    ($s:ty => $($d:ty : $kernel:ident),*) => {$(
        impl CCast<$d> for $s {
            #[inline]
            fn c_cast(self) -> $d {
                self as i128 as $d
            }

            #[cfg(feature = "simd")]
            fn cast_extend(src: &[Self], out: &mut Vec<$d>) {
                let n = out.len();
                out.resize(n + src.len(), 0);
                fearless_simd::dispatch!(crate::simd::level(), s => simd_kernels::$kernel(s, src, &mut out[n..]));
            }

            /// Every width goes through the cast chunk, on the vector cast.
            #[cfg(feature = "simd")]
            fn bin_row(src: &[Self], bin: usize, acc: &mut [$d]) {
                let level = crate::simd::level();
                bin_row_chunked(src, bin, acc, |c, o| {
                    fearless_simd::dispatch!(level, s => simd_kernels::$kernel(s, c, o));
                });
            }
        }
    )*};
}

c_cast_float_to_int!(f32 => i8: cast_f32_i8, u8: cast_f32_u8, i16: cast_f32_i16, u16: cast_f32_u16,
    i32: cast_f32_i32, u32: cast_f32_u32, i64: cast_f32_i64, u64: cast_f32_u64);
c_cast_float_to_int!(f64 => i8: cast_f64_i8, u8: cast_f64_u8, i16: cast_f64_i16, u16: cast_f64_u16,
    i32: cast_f64_i32, u32: cast_f64_u32, i64: cast_f64_i64, u64: cast_f64_u64);

/// The float to integer [`CCast::cast_extend`]s and the [`BinAcc::bin_native`]
/// bin 2 and bin 4 adds on `fearless_simd` lanes.
#[cfg(feature = "simd")]
mod simd_kernels {
    use fearless_simd::{Simd, prelude::*};
    use fearless_simd_macros::simd;

    /// `out[i] = values[i] as i128 as $t` on vectors. A chunk is `$per`
    /// `f64` vectors, `$w(k)` the `k`-th of them truncated to `i64` lanes and
    /// `$narrow` packs them into one vector of `$t`, low lanes first. The
    /// truncating conversion is the bare instruction, exact for every lane
    /// below 2^63 in magnitude and unspecified otherwise (NaN included), so
    /// a chunk with any other lane takes the scalar cast instead; narrowing
    /// `i64` lanes to `$t` is the `as` reduction modulo 2^width, and for a
    /// lane below 2^63 that is `as i128 as $t`.
    macro_rules! cast_f64_kernel {
        ($t:ty, $tvec:ident, $name:ident, $per:expr, |$w:ident| $narrow:expr) => {
            #[simd]
            pub(super) fn $name<S: Simd>(simd: S, values: &[f64], out: &mut [$t]) {
                assert_eq!(values.len(), out.len());
                let n = S::f64s::LEN;
                let lim = S::f64s::splat(simd, 9223372036854775808.0);
                let mut chunks = values.chunks_exact(n * $per);
                let mut outs = out.chunks_exact_mut(n * $per);
                for (c, o) in (&mut chunks).zip(&mut outs) {
                    let mut in_range = S::mask64s::splat(simd, true);
                    macro_rules! $w {
                        ($k:expr) => {{
                            let v = S::f64s::from_slice(simd, &c[$k * n..($k + 1) * n]);
                            in_range &= v.abs().simd_lt(lim);
                            S::i64s::truncate_from(v)
                        }};
                    }
                    let x = $narrow;
                    if in_range.all_true() {
                        S::$tvec::from_bytes(x.to_bytes()).store_slice(o);
                    } else {
                        for (&x, o) in c.iter().zip(o.iter_mut()) {
                            *o = x as i128 as $t;
                        }
                    }
                }
                for (&x, o) in chunks.remainder().iter().zip(outs.into_remainder()) {
                    *o = x as i128 as $t;
                }
            }
        };
    }

    cast_f64_kernel!(i64, i64s, cast_f64_i64, 1, |w| w!(0));
    cast_f64_kernel!(u64, u64s, cast_f64_u64, 1, |w| w!(0));
    cast_f64_kernel!(i32, i32s, cast_f64_i32, 2, |w| w!(0).narrow(w!(1)));
    cast_f64_kernel!(u32, u32s, cast_f64_u32, 2, |w| w!(0).narrow(w!(1)));
    cast_f64_kernel!(i16, i16s, cast_f64_i16, 4, |w| w!(0)
        .narrow(w!(1))
        .narrow(w!(2).narrow(w!(3))));
    cast_f64_kernel!(u16, u16s, cast_f64_u16, 4, |w| w!(0)
        .narrow(w!(1))
        .narrow(w!(2).narrow(w!(3))));
    cast_f64_kernel!(i8, i8s, cast_f64_i8, 8, |w| w!(0)
        .narrow(w!(1))
        .narrow(w!(2).narrow(w!(3)))
        .narrow(w!(4).narrow(w!(5)).narrow(w!(6).narrow(w!(7)))));
    cast_f64_kernel!(u8, u8s, cast_f64_u8, 8, |w| w!(0)
        .narrow(w!(1))
        .narrow(w!(2).narrow(w!(3)))
        .narrow(w!(4).narrow(w!(5)).narrow(w!(6).narrow(w!(7)))));

    /// An `f32` source widens to `f64` a cache-resident chunk at a time and
    /// takes the `f64` kernel: `x as f64` is exact, so `x as f64 as i128` is
    /// `x as i128`.
    macro_rules! cast_f32_kernel {
        ($t:ty, $name:ident, $via:ident) => {
            #[simd]
            pub(super) fn $name<S: Simd>(simd: S, values: &[f32], out: &mut [$t]) {
                assert_eq!(values.len(), out.len());
                let mut wide = [0.0f64; 512];
                for (c, o) in values.chunks(512).zip(out.chunks_mut(512)) {
                    let w = &mut wide[..c.len()];
                    crate::simd::to_f64_f32(simd, c, w);
                    $via(simd, w, o);
                }
            }
        };
    }

    cast_f32_kernel!(i8, cast_f32_i8, cast_f64_i8);
    cast_f32_kernel!(u8, cast_f32_u8, cast_f64_u8);
    cast_f32_kernel!(i16, cast_f32_i16, cast_f64_i16);
    cast_f32_kernel!(u16, cast_f32_u16, cast_f64_u16);
    cast_f32_kernel!(i32, cast_f32_i32, cast_f64_i32);
    cast_f32_kernel!(u32, cast_f32_u32, cast_f64_u32);
    cast_f32_kernel!(i64, cast_f32_i64, cast_f64_i64);
    cast_f32_kernel!(u64, cast_f32_u64, cast_f64_u64);

    /// `acc[i] += src[2i]; acc[i] += src[2i+1]` and the bin 4 form, lane
    /// for lane: `deinterleave` splits a run of two vectors into the even
    /// and the odd elements, twice over for bin 4, and the parts are added
    /// in element order so a float sum rounds as the scalar loop does.
    macro_rules! bin_kernels {
        ($t:ty, $vec:ident, $bin2:ident, $bin4:ident) => {
            #[simd]
            pub(super) fn $bin2<S: Simd>(simd: S, src: &[$t], acc: &mut [$t]) {
                assert_eq!(src.len(), acc.len() * 2);
                let n = S::$vec::LEN;
                let mut chunks = src.chunks_exact(2 * n);
                let mut accs = acc.chunks_exact_mut(n);
                for (c, a) in (&mut chunks).zip(&mut accs) {
                    let v0 = S::$vec::from_slice(simd, &c[..n]);
                    let v1 = S::$vec::from_slice(simd, &c[n..]);
                    let (p, q) = v0.deinterleave(v1);
                    ((S::$vec::from_slice(simd, a) + p) + q).store_slice(a);
                }
                super::bin_row_with(chunks.remainder(), 2, accs.into_remainder(), |x| x);
            }

            #[simd]
            pub(super) fn $bin4<S: Simd>(simd: S, src: &[$t], acc: &mut [$t]) {
                assert_eq!(src.len(), acc.len() * 4);
                let n = S::$vec::LEN;
                let mut chunks = src.chunks_exact(4 * n);
                let mut accs = acc.chunks_exact_mut(n);
                for (c, a) in (&mut chunks).zip(&mut accs) {
                    let v0 = S::$vec::from_slice(simd, &c[..n]);
                    let v1 = S::$vec::from_slice(simd, &c[n..2 * n]);
                    let v2 = S::$vec::from_slice(simd, &c[2 * n..3 * n]);
                    let v3 = S::$vec::from_slice(simd, &c[3 * n..]);
                    let (e01, o01) = v0.deinterleave(v1);
                    let (e23, o23) = v2.deinterleave(v3);
                    let (q0, q2) = e01.deinterleave(e23);
                    let (q1, q3) = o01.deinterleave(o23);
                    ((((S::$vec::from_slice(simd, a) + q0) + q1) + q2) + q3).store_slice(a);
                }
                super::bin_row_with(chunks.remainder(), 4, accs.into_remainder(), |x| x);
            }
        };
    }

    bin_kernels!(i8, i8s, bin2_i8, bin4_i8);
    bin_kernels!(u8, u8s, bin2_u8, bin4_u8);
    bin_kernels!(i16, i16s, bin2_i16, bin4_i16);
    bin_kernels!(u16, u16s, bin2_u16, bin4_u16);
    bin_kernels!(i32, i32s, bin2_i32, bin4_i32);
    bin_kernels!(u32, u32s, bin2_u32, bin4_u32);
    bin_kernels!(i64, i64s, bin2_i64, bin4_i64);
    bin_kernels!(u64, u64s, bin2_u64, bin4_u64);
    bin_kernels!(f32, f32s, bin2_f32, bin4_f32);
    bin_kernels!(f64, f64s, bin2_f64, bin4_f64);
}

/// Element-type conversion only — C++ `convertType` (`NDArrayPool.cpp:378`).
///
/// Every element goes through the C cast (`(dataTypeOut)value`) of
/// `CCast`: narrowing truncates to the low bits and wraps, it does not
/// clamp, and a float source truncates toward zero before that, exactly as
/// [`convert_dims`] casts it.
///
/// Dimensions, timestamps and attributes are carried over unchanged.
pub fn convert_type(src: &NDArray, target_type: NDDataType) -> ADResult<NDArray> {
    if src.data.data_type() == target_type {
        return Ok(src.clone());
    }

    let mut data = NDDataBuffer::zeros(target_type, 0);
    convert_type_into(src, &mut data)?;
    let mut arr = NDArray::with_data(src.dims.clone(), data);
    arr.unique_id = src.unique_id;
    arr.copy_time_stamps_from(src);
    arr.attributes = src.attributes.clone();
    arr.codec = src.codec.clone();
    Ok(arr)
}

/// [`convert_type`] into a buffer the caller owns: `out` keeps its element
/// type (that is the target) and its allocation when it is large enough, so
/// a pooled output buffer is filled in place as C's `convertType` fills the
/// buffer `NDArrayPool::alloc` handed it.
pub fn convert_type_into(src: &NDArray, out: &mut NDDataBuffer) -> ADResult<()> {
    if src.data.data_type() == out.data_type() {
        out.copy_from(&src.data);
        return Ok(());
    }

    macro_rules! cast_into {
        ($v:expr, $out:expr) => {
            crate::with_buffer_mut_typed!($out, |o: T| {
                o.clear();
                o.reserve($v.len());
                CCast::<T>::cast_extend($v, o);
            })
        };
    }

    crate::with_buffer!(&src.data, |v| cast_into!(v, out));
    Ok(())
}

/// Sub-region + binning + reverse + element-type conversion — C++
/// `NDArrayPool::convert()` (`NDArrayPool.cpp:620-730`, kernel `convertDim`).
///
/// `dims_out` gives offset/size/binning/reverse per source dimension:
/// - output size per dim = `dims_out[i].size / dims_out[i].binning`
/// - source pixels are **summed** (not averaged) across each binning window,
///   accumulated in the TARGET type (so the sum wraps modulo the target width)
/// - `reverse` flips the output along that dimension
/// - cumulative offset: `out.dims[i].offset = src.dims[i].offset + dims_out[i].offset`
/// - cumulative binning: `out.dims[i].binning = src.dims[i].binning * dims_out[i].binning`
/// - cumulative reverse: `out.dims[i].reverse = dims_out[i].reverse ^ src.dims[i].reverse`
///
/// The array is built outside any pool; [`crate::ndarray_pool::NDArrayPool::convert`]
/// wraps this to allocate the result through the pool (C's `convert` calls
/// `alloc()` for its output).
pub fn convert_dims(
    src: &NDArray,
    dims_out: &[NDDimension],
    target_type: NDDataType,
) -> ADResult<NDArray> {
    let out_dims = output_dims(src, dims_out)?;
    let mut data = NDDataBuffer::zeros(target_type, 0);
    convert_dims_into(src, dims_out, &mut data)?;
    let mut arr = NDArray::with_data(out_dims, data);
    arr.unique_id = src.unique_id;
    arr.copy_time_stamps_from(src);
    arr.attributes.copy_from(&src.attributes);
    Ok(arr)
}

/// The dimensions [`convert_dims`] produces for `dims_out`, after the checks
/// it applies (C++ NDArrayPool.cpp:626-724): each output size is
/// `size / binning`, offsets add, binning multiplies, and `reverse` toggles
/// against the source dimension's own flag. Fails as `convert_dims` fails.
pub fn output_dims(src: &NDArray, dims_out: &[NDDimension]) -> ADResult<Vec<NDDimension>> {
    // C parity (NDArrayPool.cpp:620-625): cannot convert compressed data.
    if src.codec.is_some() {
        return Err(ADError::UnsupportedConversion(
            "convert: cannot convert compressed (codec) data".into(),
        ));
    }

    let ndims = src.dims.len();
    if dims_out.len() != ndims {
        return Err(ADError::InvalidDimensions(format!(
            "convert: dims_out length {} != source ndims {}",
            dims_out.len(),
            ndims,
        )));
    }

    // Compute output sizes and validate
    let mut out_sizes = Vec::with_capacity(ndims);
    for (i, d) in dims_out.iter().enumerate() {
        let bin = d.binning.max(1);
        if d.size == 0 {
            return Err(ADError::InvalidDimensions(format!(
                "convert: dims_out[{}].size is 0",
                i,
            )));
        }
        let out_size = d.size / bin;
        if out_size == 0 {
            return Err(ADError::InvalidDimensions(format!(
                "convert: dims_out[{}] size {} / binning {} = 0",
                i, d.size, bin,
            )));
        }
        // Validate that offset + size fits within source dimension
        if d.offset + d.size > src.dims[i].size {
            return Err(ADError::InvalidDimensions(format!(
                "convert: dims_out[{}] offset {} + size {} > src dim size {}",
                i, d.offset, d.size, src.dims[i].size,
            )));
        }
        out_sizes.push(out_size);
    }

    // Build output dimension metadata.
    // C++ NDArrayPool.cpp:719-724 makes `reverse` cumulative:
    //   if (pIn->dims[i].reverse) pOut->dims[i].reverse = !pOut->dims[i].reverse;
    // i.e. out.reverse = dims_out[i].reverse XOR src.dims[i].reverse.
    let mut out_dims = Vec::with_capacity(ndims);
    for i in 0..ndims {
        let bin = dims_out[i].binning.max(1);
        out_dims.push(NDDimension {
            size: out_sizes[i],
            offset: src.dims[i].offset + dims_out[i].offset,
            binning: src.dims[i].binning * bin,
            reverse: dims_out[i].reverse ^ src.dims[i].reverse,
        });
    }
    Ok(out_dims)
}

/// [`convert_dims`] into a buffer the caller owns: `out`'s element type is
/// the target type, and its allocation is kept when it is large enough, so a
/// pooled output buffer is filled in place as C's `convert` fills the buffer
/// `NDArrayPool::alloc` handed it. `out` ends up holding exactly the
/// elements of [`output_dims`].
pub fn convert_dims_into(
    src: &NDArray,
    dims_out: &[NDDimension],
    out: &mut NDDataBuffer,
) -> ADResult<()> {
    let ndims = src.dims.len();
    let out_sizes: Vec<usize> = output_dims(src, dims_out)?.iter().map(|d| d.size).collect();
    let total_out: usize = out_sizes.iter().product();

    // Precompute source strides (row-major: dim[0] varies fastest)
    let mut src_strides = vec![1usize; ndims];
    for i in 1..ndims {
        src_strides[i] = src_strides[i - 1] * src.dims[i - 1].size;
    }

    // Precompute output strides
    let mut out_strides = vec![1usize; ndims];
    for i in 1..ndims {
        out_strides[i] = out_strides[i - 1] * out_sizes[i - 1];
    }

    // The output row by row: a row is the run along dim 0, and every output
    // row is the bin sum of `bin_hi` source rows (the binning window in the
    // outer dims), each binned along dim 0 by `bin0`. The decomposition of
    // an index into coordinates happens per row, not per element, and the
    // per-element loops are plain strided runs the compiler vectorizes. The
    // source coordinates never leave the source: `output_dims` checked
    // `offset + size <= src size` per dim and the window covers
    // `out_size * bin <= size` of it.
    let bin0 = dims_out[0].binning.max(1);
    let out0 = out_sizes[0];
    let used0 = out0 * bin0;
    let off0 = dims_out[0].offset;
    let rows = total_out / out0;
    let bin_hi: usize = dims_out[1..].iter().map(|d| d.binning.max(1)).product();
    let mut out_coords = vec![0usize; ndims];
    let mut bases = vec![0usize; bin_hi];
    // The flat source index of each source row in output row `row`'s
    // binning window, into `bases`.
    let mut row_bases = |row: usize, bases: &mut [usize]| {
        // The output row's coordinates in dims 1.., reversed where asked.
        let mut remaining = row;
        for i in (1..ndims).rev() {
            let stride = out_strides[i] / out0;
            let c = remaining / stride;
            remaining %= stride;
            out_coords[i] = if dims_out[i].reverse {
                out_sizes[i] - 1 - c
            } else {
                c
            };
        }
        for (b, base) in bases.iter_mut().enumerate() {
            let mut br = b;
            let mut flat = off0;
            for i in (1..ndims).rev() {
                let bin = dims_out[i].binning.max(1);
                let win = br % bin;
                br /= bin;
                flat += (dims_out[i].offset + out_coords[i] * bin + win) * src_strides[i];
            }
            *base = flat;
        }
    };

    // Macro: bin/offset/reverse a single (source -> target) type pair.
    // Every source element is cast to the target type by `CCast` and, when
    // binning, summed in the target type by `BinAcc::bin_add`, which is C
    // `convertDim` (NDArrayPool.cpp:434-471) `*pDOut += (dataTypeOut)*pDIn`
    // operation for operation; an unbinned frame goes through the same cast.
    macro_rules! bin_loop {
        ($src_vec:expr, $out:expr, $DstT:ty) => {{
            let src_vec = $src_vec;
            let out = $out;
            out.clear();
            out.reserve(total_out);
            if bin0 == 1 && bin_hi == 1 {
                // No binning: each output element is one source element.
                for row in 0..rows {
                    row_bases(row, &mut bases);
                    let base = bases[0];
                    let start = out.len();
                    CCast::<$DstT>::cast_extend(&src_vec[base..base + out0], out);
                    if dims_out[0].reverse {
                        out[start..].reverse();
                    }
                }
            } else {
                let mut acc = vec![<$DstT as BinAcc>::ZERO; out0];
                for row in 0..rows {
                    acc.fill(<$DstT as BinAcc>::ZERO);
                    row_bases(row, &mut bases);
                    for &base in &bases {
                        CCast::<$DstT>::bin_row(&src_vec[base..base + used0], bin0, &mut acc);
                    }
                    if dims_out[0].reverse {
                        acc.reverse();
                    }
                    out.extend_from_slice(&acc);
                }
            }
        }};
    }

    // For a given typed source buffer, dispatch on the target type.
    macro_rules! bin_to_target {
        ($src_vec:expr) => {
            match out {
                NDDataBuffer::I8(o) => bin_loop!($src_vec, o, i8),
                NDDataBuffer::U8(o) => bin_loop!($src_vec, o, u8),
                NDDataBuffer::I16(o) => bin_loop!($src_vec, o, i16),
                NDDataBuffer::U16(o) => bin_loop!($src_vec, o, u16),
                NDDataBuffer::I32(o) => bin_loop!($src_vec, o, i32),
                NDDataBuffer::U32(o) => bin_loop!($src_vec, o, u32),
                NDDataBuffer::I64(o) => bin_loop!($src_vec, o, i64),
                NDDataBuffer::U64(o) => bin_loop!($src_vec, o, u64),
                NDDataBuffer::F32(o) => bin_loop!($src_vec, o, f32),
                NDDataBuffer::F64(o) => bin_loop!($src_vec, o, f64),
            }
        };
    }

    crate::with_buffer!(&src.data, |v| bin_to_target!(v));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::CCast;

    #[test]
    fn float_to_int_casts_truncate_then_wrap_like_c() {
        assert_eq!(<f32 as CCast<u8>>::c_cast(300.0), 44);
        assert_eq!(<f64 as CCast<u8>>::c_cast(-1.5), 255);
        assert_eq!(<f64 as CCast<i8>>::c_cast(200.9), -56);
        assert_eq!(<f32 as CCast<i16>>::c_cast(-40000.0), 25536);
        assert_eq!(<f64 as CCast<u32>>::c_cast(4294967296.0 + 7.0), 7);
        assert_eq!(<f64 as CCast<i64>>::c_cast(1e19), -8446744073709551616);
        assert_eq!(<f64 as CCast<u64>>::c_cast(-1.0), u64::MAX);
        assert_eq!(<f64 as CCast<u16>>::c_cast(f64::NAN), 0);
        assert_eq!(<f32 as CCast<i32>>::c_cast(f32::INFINITY), -1);
    }

    /// Both free-function converts carry the source `uniqueId`, as C++
    /// `NDArrayPool::convert` does (NDArrayPool.cpp:660).
    #[test]
    fn both_converts_carry_the_source_unique_id() {
        use crate::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
        let mut src = NDArray::with_data(
            vec![NDDimension::new(4)],
            NDDataBuffer::U8(vec![1, 2, 3, 4]),
        );
        src.unique_id = 91;

        assert_eq!(
            super::convert_type(&src, NDDataType::Float64)
                .unwrap()
                .unique_id,
            91
        );
        let mut d = NDDimension::new(2);
        d.offset = 1;
        assert_eq!(
            super::convert_dims(&src, &[d], NDDataType::UInt8)
                .unwrap()
                .unique_id,
            91
        );
    }

    #[test]
    fn convert_type_casts_float_sources_as_convert_dims_does() {
        use crate::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
        let values = vec![
            300.0f32,
            -1.5,
            255.9,
            f32::NAN,
            f32::INFINITY,
            65536.0 + 44.0,
        ];
        let src = NDArray::with_data(
            vec![NDDimension::new(values.len())],
            NDDataBuffer::F32(values),
        );
        let typed = super::convert_type(&src, NDDataType::UInt8).unwrap();
        let dims = super::convert_dims(
            &src,
            &[NDDimension::new(src.dims[0].size)],
            NDDataType::UInt8,
        )
        .unwrap();
        let (NDDataBuffer::U8(typed), NDDataBuffer::U8(dims)) = (&typed.data, &dims.data) else {
            panic!("target type not honoured");
        };
        // Infinity saturates the i128 and its low byte is 0xff.
        assert_eq!(*typed, vec![44, 255, 255, 0, 255, 44]);
        assert_eq!(typed, dims);
    }

    #[cfg(feature = "simd")]
    mod simd {
        use super::super::{BinAcc, CCast, bin_row_with, simd_kernels};
        use crate::simd::{edge_values, levels};
        use fearless_simd::Level;

        macro_rules! check_cast {
            ($s:ty, $d:ty, $kernel:ident) => {
                // Several cast chunks and a tail, so the chunking of
                // `bin_row` is crossed by whole and by split bins.
                let values: Vec<$s> = edge_values().iter().cycle().take(3270).map(|&x| x as $s).collect();
                let want: Vec<$d> = values.iter().map(|&x| x as i128 as $d).collect();
                for lvl in levels() {
                    let mut got = vec![0 as $d; values.len()];
                    fearless_simd::dispatch!(lvl, s => simd_kernels::$kernel(s, &values, &mut got));
                    assert_eq!(got, want, "{} -> {} at {lvl:?}", stringify!($s), stringify!($d));
                }
                let mut got = vec![7 as $d; 3];
                <$s as CCast<$d>>::cast_extend(&values, &mut got);
                assert_eq!(got[3..], want[..], "{} -> {}", stringify!($s), stringify!($d));
                for bin in [1usize, 2, 3, 7, 1024, 1025] {
                    let mut got = vec![1 as $d; values.len() / bin];
                    <$s as CCast<$d>>::bin_row(&values[..got.len() * bin], bin, &mut got);
                    for (i, g) in got.iter().enumerate() {
                        let mut w: $d = 1;
                        for &x in &want[i * bin..(i + 1) * bin] {
                            w = w.wrapping_add(x);
                        }
                        assert_eq!(*g, w, "{} -> {} bin {bin} at {i}", stringify!($s), stringify!($d));
                    }
                }
            };
        }

        #[test]
        fn cast_row_matches_the_c_cast_on_every_level() {
            check_cast!(f32, i8, cast_f32_i8);
            check_cast!(f32, u8, cast_f32_u8);
            check_cast!(f32, i16, cast_f32_i16);
            check_cast!(f32, u16, cast_f32_u16);
            check_cast!(f32, i32, cast_f32_i32);
            check_cast!(f32, u32, cast_f32_u32);
            check_cast!(f32, i64, cast_f32_i64);
            check_cast!(f32, u64, cast_f32_u64);
            check_cast!(f64, i8, cast_f64_i8);
            check_cast!(f64, u8, cast_f64_u8);
            check_cast!(f64, i16, cast_f64_i16);
            check_cast!(f64, u16, cast_f64_u16);
            check_cast!(f64, i32, cast_f64_i32);
            check_cast!(f64, u32, cast_f64_u32);
            check_cast!(f64, i64, cast_f64_i64);
            check_cast!(f64, u64, cast_f64_u64);
        }

        macro_rules! check_bin {
            ($t:ty, $bin2:ident, $bin4:ident, $f:expr) => {{
                let f: fn(usize) -> $t = $f;
                // 3 * 4 * 64 elements plus a tail that is whole bins.
                let src: Vec<$t> = (0..780).map(f).collect();
                let runs: [(usize, &dyn Fn(Level, &[$t], &mut [$t])); 2] = [
                    (2, &|lvl, s, a| fearless_simd::dispatch!(lvl, sm => simd_kernels::$bin2(sm, s, a))),
                    (4, &|lvl, s, a| fearless_simd::dispatch!(lvl, sm => simd_kernels::$bin4(sm, s, a))),
                ];
                for (bin, run) in runs {
                    let n = src.len() / bin * bin;
                    let init: Vec<$t> = (0..n / bin).map(|i| f(i + 3)).collect();
                    let mut want = init.clone();
                    bin_row_with(&src[..n], bin, &mut want, |x| x);
                    for lvl in levels() {
                        let mut got = init.clone();
                        run(lvl, &src[..n], &mut got);
                        let same = got.iter().zip(&want).all(|(g, w)| g.to_bits() == w.to_bits());
                        assert!(same, "{} bin {bin} at {lvl:?}: {got:?} vs {want:?}", stringify!($t));
                    }
                    let mut got = init.clone();
                    <$t as BinAcc>::bin_native(&src[..n], bin, &mut got);
                    let same = got.iter().zip(&want).all(|(g, w)| g.to_bits() == w.to_bits());
                    assert!(same, "{} bin {bin} native", stringify!($t));
                }
            }};
        }

        trait Bits {
            fn to_bits(self) -> u64;
        }
        macro_rules! bits {
            ($($t:ty),*) => {$(
                impl Bits for $t {
                    fn to_bits(self) -> u64 {
                        self as u64
                    }
                }
            )*};
        }
        bits!(i8, u8, i16, u16, i32, u32, i64, u64);

        /// The kernels add on lanes what the scalar loop adds in order:
        /// integer bins that wrap, float bins whose rounding depends on
        /// the order.
        #[test]
        fn bin_kernels_match_the_scalar_loop_on_every_level() {
            check_bin!(i8, bin2_i8, bin4_i8, |i| (i.wrapping_mul(97) % 251) as i8);
            check_bin!(u8, bin2_u8, bin4_u8, |i| (i.wrapping_mul(97) % 251) as u8);
            check_bin!(i16, bin2_i16, bin4_i16, |i| (i.wrapping_mul(7919) % 65521)
                as i16);
            check_bin!(u16, bin2_u16, bin4_u16, |i| (i.wrapping_mul(7919) % 65521)
                as u16);
            check_bin!(
                i32,
                bin2_i32,
                bin4_i32,
                |i| (i.wrapping_mul(2654435761) % 4294967291) as i32
            );
            check_bin!(
                u32,
                bin2_u32,
                bin4_u32,
                |i| (i.wrapping_mul(2654435761) % 4294967291) as u32
            );
            check_bin!(i64, bin2_i64, bin4_i64, |i| (i as i64)
                .wrapping_mul(0x9e3779b97f4a7c15u64 as i64));
            check_bin!(u64, bin2_u64, bin4_u64, |i| (i as u64)
                .wrapping_mul(0x9e3779b97f4a7c15));
            check_bin!(f32, bin2_f32, bin4_f32, |i| (i as f32 - 390.0) * 0.37
                + 1.0 / (i as f32 + 1.0));
            check_bin!(f64, bin2_f64, bin4_f64, |i| (i as f64 - 390.0) * 0.37
                + 1.0 / (i as f64 + 1.0));
        }
    }
}
