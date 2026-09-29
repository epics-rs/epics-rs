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
//! == -1`). Rust's `as` between integer types is exactly the C cast, so the
//! kernels below cast with `as` and never clamp. Any plugin that
//! re-implements extraction with an f64 accumulator plus a clamp/saturate
//! re-opens this divergence — call [`convert_dims`] / [`convert_type`]
//! instead.

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
}

macro_rules! bin_acc_int {
    ($($t:ty),*) => {$(
        impl BinAcc for $t {
            const ZERO: Self = 0;
            #[inline]
            fn bin_add(self, rhs: Self) -> Self {
                self.wrapping_add(rhs)
            }
        }
    )*};
}

macro_rules! bin_acc_float {
    ($($t:ty),*) => {$(
        impl BinAcc for $t {
            const ZERO: Self = 0.0;
            #[inline]
            fn bin_add(self, rhs: Self) -> Self {
                self + rhs
            }
        }
    )*};
}

bin_acc_int!(i8, u8, i16, u16, i32, u32, i64, u64);
bin_acc_float!(f32, f64);

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
    /// per accumulator: `src.len() == acc.len() * bin`.
    #[inline]
    fn bin_row(src: &[Self], bin: usize, acc: &mut [D]) {
        bin_row_scalar(src, bin, acc);
    }
}

/// [`CCast::bin_row`] one element at a time; the float to integer pairs run
/// their vector cast first and take this on the cast chunk.
#[inline]
fn bin_row_scalar<S: CCast<D>, D: BinAcc>(src: &[S], bin: usize, acc: &mut [D]) {
    if bin == 1 {
        for (a, &s) in acc.iter_mut().zip(src) {
            *a = a.bin_add(s.c_cast());
        }
    } else {
        for (a, w) in acc.iter_mut().zip(src.chunks_exact(bin)) {
            // The bin sum in a local, so the loop carries a register and
            // not a store, and is unrolled.
            let mut t = *a;
            for &s in w {
                t = t.bin_add(s.c_cast());
            }
            *a = t;
        }
    }
}

macro_rules! c_cast_as {
    ($($s:ty),* => $d:tt) => {$( c_cast_as!(@one $s => $d); )*};
    (@one $s:ty => [$($d:ty),*]) => {$(
        impl CCast<$d> for $s {
            #[inline]
            fn c_cast(self) -> $d {
                self as $d
            }
        }
    )*};
}

c_cast_as!(i8, u8, i16, u16, i32, u32, i64, u64 => [i8, u8, i16, u16, i32, u32, i64, u64, f32, f64]);
c_cast_as!(f32, f64 => [f32, f64]);

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

            /// The row is cast a cache-resident chunk at a time, `step`
            /// source elements that hold whole bins, and each chunk is
            /// added as cast integers.
            #[cfg(feature = "simd")]
            fn bin_row(src: &[Self], bin: usize, acc: &mut [$d]) {
                const CHUNK: usize = 1024;
                if bin > CHUNK {
                    return bin_row_scalar(src, bin, acc);
                }
                let step = CHUNK / bin * bin;
                let mut cast = [0 as $d; CHUNK];
                let level = crate::simd::level();
                for (c, a) in src.chunks(step).zip(acc.chunks_mut(step / bin)) {
                    let cast = &mut cast[..c.len()];
                    fearless_simd::dispatch!(level, s => simd_kernels::$kernel(s, c, cast));
                    bin_row_scalar(cast, bin, a);
                }
            }
        }
    )*};
}

c_cast_float_to_int!(f32 => i8: cast_f32_i8, u8: cast_f32_u8, i16: cast_f32_i16, u16: cast_f32_u16,
    i32: cast_f32_i32, u32: cast_f32_u32, i64: cast_f32_i64, u64: cast_f32_u64);
c_cast_float_to_int!(f64 => i8: cast_f64_i8, u8: cast_f64_u8, i16: cast_f64_i16, u16: cast_f64_u16,
    i32: cast_f64_i32, u32: cast_f64_u32, i64: cast_f64_i64, u64: cast_f64_u64);

/// The float to integer [`CCast::cast_row`]s on `fearless_simd` lanes.
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
}

/// Element-type conversion only — C++ `convertType` (`NDArrayPool.cpp:378`).
///
/// Every element goes through a C cast (`(dataTypeOut)value`): narrowing
/// truncates to the low bits and wraps, it does not clamp. Float sources
/// truncate toward zero (out-of-range float→int is undefined in C; the port
/// keeps Rust's saturation there rather than inventing a trap value).
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
    arr.timestamp = src.timestamp;
    arr.time_stamp = src.time_stamp;
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
                o.extend($v.iter().map(|&x| x as T));
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
    arr.timestamp = src.timestamp;
    arr.time_stamp = src.time_stamp;
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

    #[cfg(feature = "simd")]
    mod simd {
        use super::super::{CCast, simd_kernels};
        use crate::simd::{edge_values, levels};

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
    }
}
