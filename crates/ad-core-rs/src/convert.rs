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

    macro_rules! cast_all {
        ($v:expr) => {
            match target_type {
                NDDataType::Int8 => NDDataBuffer::I8($v.iter().map(|&x| x as i8).collect()),
                NDDataType::UInt8 => NDDataBuffer::U8($v.iter().map(|&x| x as u8).collect()),
                NDDataType::Int16 => NDDataBuffer::I16($v.iter().map(|&x| x as i16).collect()),
                NDDataType::UInt16 => NDDataBuffer::U16($v.iter().map(|&x| x as u16).collect()),
                NDDataType::Int32 => NDDataBuffer::I32($v.iter().map(|&x| x as i32).collect()),
                NDDataType::UInt32 => NDDataBuffer::U32($v.iter().map(|&x| x as u32).collect()),
                NDDataType::Int64 => NDDataBuffer::I64($v.iter().map(|&x| x as i64).collect()),
                NDDataType::UInt64 => NDDataBuffer::U64($v.iter().map(|&x| x as u64).collect()),
                NDDataType::Float32 => NDDataBuffer::F32($v.iter().map(|&x| x as f32).collect()),
                NDDataType::Float64 => NDDataBuffer::F64($v.iter().map(|&x| x as f64).collect()),
            }
        };
    }

    let out_data = match &src.data {
        NDDataBuffer::I8(v) => cast_all!(v),
        NDDataBuffer::U8(v) => cast_all!(v),
        NDDataBuffer::I16(v) => cast_all!(v),
        NDDataBuffer::U16(v) => cast_all!(v),
        NDDataBuffer::I32(v) => cast_all!(v),
        NDDataBuffer::U32(v) => cast_all!(v),
        NDDataBuffer::I64(v) => cast_all!(v),
        NDDataBuffer::U64(v) => cast_all!(v),
        NDDataBuffer::F32(v) => cast_all!(v),
        NDDataBuffer::F64(v) => cast_all!(v),
    };

    let mut arr = NDArray::new(src.dims.clone(), target_type);
    arr.data = out_data;
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.time_stamp = src.time_stamp;
    arr.attributes = src.attributes.clone();
    arr.codec = src.codec.clone();
    Ok(arr)
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

    // Macro: bin/offset/reverse a single (source -> target) type pair,
    // accumulating directly in the TARGET type to match C `convertDim`
    // (NDArrayPool.cpp:434-471), which sums `(dataTypeOut)*pDIn` in the
    // output type. `$AccT` is the accumulator (`i128` for integer
    // targets, the target float type otherwise — see [`BinAcc`]);
    // `$DstT` / `$variant` are the target element type and its
    // `NDDataBuffer` variant.
    macro_rules! bin_loop {
        ($src_vec:expr, $DstT:ty, $AccT:ty, $variant:ident) => {{
            let mut out = vec![0 as $DstT; total_out];

            // Iterate over all output pixels
            for out_idx in 0..total_out {
                // Decompose flat output index into per-dim coordinates
                let mut remaining = out_idx;
                let mut out_coords = [0usize; 10]; // up to 10 dims
                for i in (0..ndims).rev() {
                    out_coords[i] = remaining / out_strides[i];
                    remaining %= out_strides[i];
                }

                // Apply reverse: flip coordinate in output space
                let mut eff_coords = [0usize; 10];
                for i in 0..ndims {
                    eff_coords[i] = if dims_out[i].reverse {
                        out_sizes[i] - 1 - out_coords[i]
                    } else {
                        out_coords[i]
                    };
                }

                // Sum over the binning window in the TARGET type.
                let mut acc = <$AccT as BinAcc>::ZERO;
                let bin_total: usize = dims_out.iter().map(|d| d.binning.max(1)).product();

                // Iterate over all bin offsets
                for bin_flat in 0..bin_total {
                    let mut br = bin_flat;
                    let mut src_flat = 0usize;
                    let mut valid = true;

                    for i in (0..ndims).rev() {
                        let bin = dims_out[i].binning.max(1);
                        let bin_off = br % bin;
                        br /= bin;

                        let src_coord = dims_out[i].offset + eff_coords[i] * bin + bin_off;
                        if src_coord >= src.dims[i].size {
                            valid = false;
                            break;
                        }
                        src_flat += src_coord * src_strides[i];
                    }

                    if valid {
                        // C `(dataTypeOut)*pDIn`: cast the source element
                        // into the accumulator (target) type, then add.
                        acc = acc.bin_add($src_vec[src_flat] as $AccT);
                    }
                }

                out[out_idx] = acc as $DstT;
            }

            NDDataBuffer::$variant(out)
        }};
    }

    // For a given typed source buffer, dispatch on the target type.
    // Integer targets accumulate in `i128`; float targets in their own
    // float type, matching C `convertDim`'s output-typed accumulator.
    macro_rules! bin_to_target {
        ($src_vec:expr) => {
            match target_type {
                NDDataType::Int8 => bin_loop!($src_vec, i8, i128, I8),
                NDDataType::UInt8 => bin_loop!($src_vec, u8, i128, U8),
                NDDataType::Int16 => bin_loop!($src_vec, i16, i128, I16),
                NDDataType::UInt16 => bin_loop!($src_vec, u16, i128, U16),
                NDDataType::Int32 => bin_loop!($src_vec, i32, i128, I32),
                NDDataType::UInt32 => bin_loop!($src_vec, u32, i128, U32),
                NDDataType::Int64 => bin_loop!($src_vec, i64, i128, I64),
                NDDataType::UInt64 => bin_loop!($src_vec, u64, i128, U64),
                NDDataType::Float32 => bin_loop!($src_vec, f32, f32, F32),
                NDDataType::Float64 => bin_loop!($src_vec, f64, f64, F64),
            }
        };
    }

    let out_data = match &src.data {
        NDDataBuffer::I8(v) => bin_to_target!(v),
        NDDataBuffer::U8(v) => bin_to_target!(v),
        NDDataBuffer::I16(v) => bin_to_target!(v),
        NDDataBuffer::U16(v) => bin_to_target!(v),
        NDDataBuffer::I32(v) => bin_to_target!(v),
        NDDataBuffer::U32(v) => bin_to_target!(v),
        NDDataBuffer::I64(v) => bin_to_target!(v),
        NDDataBuffer::U64(v) => bin_to_target!(v),
        NDDataBuffer::F32(v) => bin_to_target!(v),
        NDDataBuffer::F64(v) => bin_to_target!(v),
    };

    let mut arr = NDArray::new(out_dims, target_type);
    arr.data = out_data;
    arr.timestamp = src.timestamp;
    arr.time_stamp = src.time_stamp;
    arr.attributes.copy_from(&src.attributes);
    Ok(arr)
}
