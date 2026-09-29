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
/// integer arithmetic wrapping on overflow. The port reproduces that by
/// accumulating in the *target* type's arithmetic, then casting the
/// accumulator to the target element type:
///
/// * Integer targets accumulate in `i128` — wide enough that the running sum
///   of any realistic bin window of 8/16/32/64-bit source elements never
///   itself overflows — and the final `as`-cast to the narrower target
///   reduces modulo 2^width, identical to C's per-step wrapping by the ring
///   homomorphism `Z -> Z/2^width`. (An f64 accumulator would also lose
///   precision for |value| > 2^53, corrupting i64/u64 arrays even at
///   binning == 1.)
/// * Float targets accumulate in the target float type (`f32`/`f64`) exactly
///   as C does, so the same rounding / precision applies.
trait BinAcc: Copy {
    const ZERO: Self;
    fn bin_add(self, rhs: Self) -> Self;
}

impl BinAcc for i128 {
    const ZERO: Self = 0;
    #[inline]
    fn bin_add(self, rhs: Self) -> Self {
        self.wrapping_add(rhs)
    }
}

impl BinAcc for f32 {
    const ZERO: Self = 0.0;
    #[inline]
    fn bin_add(self, rhs: Self) -> Self {
        self + rhs
    }
}

impl BinAcc for f64 {
    const ZERO: Self = 0.0;
    #[inline]
    fn bin_add(self, rhs: Self) -> Self {
        self + rhs
    }
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

    // Macro: bin/offset/reverse a single (source -> target) type pair,
    // accumulating directly in the TARGET type to match C `convertDim`
    // (NDArrayPool.cpp:434-471), which sums `(dataTypeOut)*pDIn` in the
    // output type. `$AccT` is the accumulator (`i128` for integer
    // targets, the target float type otherwise — see [`BinAcc`]);
    // `$DstT` is the target element type. Every element goes through
    // `as $AccT as $DstT` on both paths below, so an unbinned frame casts
    // exactly as a binned one does.
    macro_rules! bin_loop {
        ($src_vec:expr, $out:expr, $DstT:ty, $AccT:ty) => {{
            let src_vec = $src_vec;
            let out = $out;
            out.clear();
            out.reserve(total_out);
            if bin0 == 1 && bin_hi == 1 {
                // No binning: each output element is one source element.
                for row in 0..rows {
                    row_bases(row, &mut bases);
                    let base = bases[0];
                    let src_row = src_vec[base..base + used0].iter();
                    if dims_out[0].reverse {
                        out.extend(src_row.rev().map(|&s| s as $AccT as $DstT));
                    } else {
                        out.extend(src_row.map(|&s| s as $AccT as $DstT));
                    }
                }
            } else {
                let mut acc = vec![<$AccT as BinAcc>::ZERO; out0];
                for row in 0..rows {
                    acc.fill(<$AccT as BinAcc>::ZERO);
                    row_bases(row, &mut bases);
                    for &base in &bases {
                        let src_row = &src_vec[base..base + used0];
                        if bin0 == 1 {
                            for (a, &s) in acc.iter_mut().zip(src_row) {
                                *a = a.bin_add(s as $AccT);
                            }
                        } else {
                            for (a, w) in acc.iter_mut().zip(src_row.chunks_exact(bin0)) {
                                for &s in w {
                                    *a = a.bin_add(s as $AccT);
                                }
                            }
                        }
                    }
                    if dims_out[0].reverse {
                        out.extend(acc.iter().rev().map(|&a| a as $DstT));
                    } else {
                        out.extend(acc.iter().map(|&a| a as $DstT));
                    }
                }
            }
        }};
    }

    // For a given typed source buffer, dispatch on the target type.
    // Integer targets accumulate in `i128`; float targets in their own
    // float type, matching C `convertDim`'s output-typed accumulator.
    macro_rules! bin_to_target {
        ($src_vec:expr) => {
            match out {
                NDDataBuffer::I8(o) => bin_loop!($src_vec, o, i8, i128),
                NDDataBuffer::U8(o) => bin_loop!($src_vec, o, u8, i128),
                NDDataBuffer::I16(o) => bin_loop!($src_vec, o, i16, i128),
                NDDataBuffer::U16(o) => bin_loop!($src_vec, o, u16, i128),
                NDDataBuffer::I32(o) => bin_loop!($src_vec, o, i32, i128),
                NDDataBuffer::U32(o) => bin_loop!($src_vec, o, u32, i128),
                NDDataBuffer::I64(o) => bin_loop!($src_vec, o, i64, i128),
                NDDataBuffer::U64(o) => bin_loop!($src_vec, o, u64, i128),
                NDDataBuffer::F32(o) => bin_loop!($src_vec, o, f32, f32),
                NDDataBuffer::F64(o) => bin_loop!($src_vec, o, f64, f64),
            }
        };
    }

    crate::with_buffer!(&src.data, |v| bin_to_target!(v));
    Ok(())
}
