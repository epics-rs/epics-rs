// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — all 1 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p ad-plugins-rs
// --all-features`, 556/556). ad-plugins-rs became a census subject when
// its `build.rs` began deriving `tokio_backend`; nothing here builds a
// CA server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.
use std::borrow::Cow;
use std::io::{Read, Write};
use std::sync::Arc;

use ad_core_rs::codec::{Codec, CodecName, CodecStatus};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ParamUpdate, ProcessResult};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use lz4_flex::block::{compress, decompress};
use parking_lot::Mutex;
use rust_hdf5::format::messages::filter::{
    FILTER_BLOSC, Filter, FilterPipeline, apply_filters, reverse_filters,
};

/// The original (uncompressed) element type of an NDArray.
///
/// For an uncompressed array this is the buffer's own type. For a compressed
/// array the typed buffer has collapsed to raw bytes (`UInt8`), so the original
/// type is read from [`Codec::original_data_type`], which the codec plugin set
/// on compress — mirroring C ADCore keeping it in `NDArray::dataType`
/// (NDPluginCodec.cpp:35-36). Shared by the decompress round-trip and the
/// NTNDArray converter, which needs it to publish `uncompressedSize` and
/// `codec.parameters` (C `NDDataTypeToScalar[src->dataType]`,
/// ntndArrayConverter.cpp:413-419) since a compressed array's value union no
/// longer carries the element type.
pub fn original_data_type(array: &NDArray) -> NDDataType {
    match &array.codec {
        Some(c) => c.original_data_type,
        None => array.data.data_type(),
    }
}

/// Reconstruct an `NDDataBuffer` from raw bytes and a target data type.
///
/// The byte slice is reinterpreted as the target type using native endianness.
/// Returns `None` if the byte count is not a multiple of the element size.
pub(crate) fn buffer_from_bytes(bytes: &[u8], data_type: NDDataType) -> Option<NDDataBuffer> {
    let elem_size = data_type.element_size();
    if bytes.len() % elem_size != 0 {
        return None;
    }
    let count = bytes.len() / elem_size;

    Some(match data_type {
        NDDataType::Int8 => {
            let mut v = vec![0i8; count];
            // SAFETY: i8 and u8 have the same size/alignment
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    v.as_mut_ptr() as *mut u8,
                    bytes.len(),
                );
            }
            NDDataBuffer::I8(v)
        }
        NDDataType::UInt8 => NDDataBuffer::U8(bytes.to_vec()),
        NDDataType::Int16 => {
            let mut v = vec![0i16; count];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    v.as_mut_ptr() as *mut u8,
                    bytes.len(),
                );
            }
            NDDataBuffer::I16(v)
        }
        NDDataType::UInt16 => {
            let mut v = vec![0u16; count];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    v.as_mut_ptr() as *mut u8,
                    bytes.len(),
                );
            }
            NDDataBuffer::U16(v)
        }
        NDDataType::Int32 => {
            let mut v = vec![0i32; count];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    v.as_mut_ptr() as *mut u8,
                    bytes.len(),
                );
            }
            NDDataBuffer::I32(v)
        }
        NDDataType::UInt32 => {
            let mut v = vec![0u32; count];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    v.as_mut_ptr() as *mut u8,
                    bytes.len(),
                );
            }
            NDDataBuffer::U32(v)
        }
        NDDataType::Int64 => {
            let mut v = vec![0i64; count];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    v.as_mut_ptr() as *mut u8,
                    bytes.len(),
                );
            }
            NDDataBuffer::I64(v)
        }
        NDDataType::UInt64 => {
            let mut v = vec![0u64; count];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    v.as_mut_ptr() as *mut u8,
                    bytes.len(),
                );
            }
            NDDataBuffer::U64(v)
        }
        NDDataType::Float32 => {
            let mut v = vec![0f32; count];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    v.as_mut_ptr() as *mut u8,
                    bytes.len(),
                );
            }
            NDDataBuffer::F32(v)
        }
        NDDataType::Float64 => {
            let mut v = vec![0f64; count];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    v.as_mut_ptr() as *mut u8,
                    bytes.len(),
                );
            }
            NDDataBuffer::F64(v)
        }
    })
}

/// Compress an NDArray using LZ4.
///
/// The raw bytes of the data buffer are compressed with LZ4 (block mode, size-prepended).
/// The original data type ordinal is stored as an attribute so decompression can
/// reconstruct the correct typed buffer.
pub fn compress_lz4(src: &NDArray) -> NDArray {
    let raw = src.data.as_u8_slice();
    let original_data_type = src.data.data_type();
    let original_size = raw.len();
    // C++ uses raw LZ4_compress_default (no size header)
    let compressed = compress(raw);
    let compressed_size = compressed.len();

    let mut arr = src.clone();
    arr.data = NDDataBuffer::U8(compressed);
    arr.codec = Some(Codec {
        name: CodecName::LZ4,
        compressed_size,
        level: 0,
        shuffle: 0,
        compressor: 0,
        // The original element type travels in the codec (C `NDArray::dataType`,
        // NDPluginCodec.cpp:35-36), so decompression can rebuild the buffer.
        original_data_type,
    });

    tracing::debug!(
        original_size,
        compressed_size,
        ratio = original_size as f64 / compressed_size.max(1) as f64,
        "LZ4 compress"
    );

    arr
}

/// Decompress an LZ4-compressed NDArray.
///
/// Returns `None` if the codec is not LZ4 or decompression fails.
/// The original typed buffer is reconstructed using the stored data type attribute.
pub fn decompress_lz4(src: &NDArray) -> Option<NDArray> {
    if src.codec.as_ref().map(|c| c.name) != Some(CodecName::LZ4) {
        return None;
    }
    let compressed = src.data.as_u8_slice();
    // C++ uses LZ4_decompress_fast with a known uncompressed size; the original
    // element type travels in the codec (C `NDArray::dataType`).
    let original_type = original_data_type(src);
    let num_elements: usize = src.dims.iter().map(|d| d.size).product();
    let uncompressed_size = num_elements * original_type.element_size();
    let decompressed = decompress(compressed, uncompressed_size).ok()?;

    let buffer = buffer_from_bytes(&decompressed, original_type)?;

    let mut arr = src.clone();
    arr.data = buffer;
    arr.codec = None;

    Some(arr)
}

// ---------------------------------------------------------------------------
// Zlib (deflate) — port of the C++ NDCodec ZLIB codec
// ---------------------------------------------------------------------------
//
// C++ `compressZlib`/`decompressZlib` call zlib `compress2`/`uncompress` on the
// raw element bytes. We use `flate2`'s `ZlibEncoder`/`ZlibDecoder`, which emit
// and parse the same zlib (RFC 1950) stream. The original data type is stored
// as an attribute so decompression can rebuild the typed buffer.

/// Default zlib compression level (mirrors `Compression::default()`, level 6).
const ZLIB_DEFAULT_LEVEL: u32 = 6;

/// Compress an NDArray using zlib (deflate).
///
/// Mirrors C++ `compressZlib`. The raw bytes of the data buffer are compressed
/// with a zlib stream. The original data type ordinal is stored as an attribute
/// so decompression can reconstruct the correct typed buffer.
pub fn compress_zlib(src: &NDArray) -> NDArray {
    let raw = src.data.as_u8_slice();
    let original_data_type = src.data.data_type();
    let original_size = raw.len();

    let mut encoder = ZlibEncoder::new(Vec::<u8>::new(), Compression::new(ZLIB_DEFAULT_LEVEL));
    // Writing to a `Vec` and finishing the stream are infallible here.
    if encoder.write_all(raw).is_err() {
        return src.clone();
    }
    let compressed = match encoder.finish() {
        Ok(buf) => buf,
        Err(_) => return src.clone(),
    };
    let compressed_size = compressed.len();

    let mut arr = src.clone();
    arr.data = NDDataBuffer::U8(compressed);
    arr.codec = Some(Codec {
        name: CodecName::Zlib,
        compressed_size,
        level: ZLIB_DEFAULT_LEVEL as i32,
        shuffle: 0,
        compressor: 0,
        original_data_type,
    });

    tracing::debug!(
        original_size,
        compressed_size,
        ratio = original_size as f64 / compressed_size.max(1) as f64,
        "Zlib compress"
    );
    arr
}

/// Decompress a zlib-compressed NDArray.
///
/// Returns `None` if the codec is not Zlib or decompression fails.
/// The original typed buffer is reconstructed using the stored data type attribute.
pub fn decompress_zlib(src: &NDArray) -> Option<NDArray> {
    if src.codec.as_ref().map(|c| c.name) != Some(CodecName::Zlib) {
        return None;
    }
    let compressed = src.data.as_u8_slice();

    let original_type = original_data_type(src);
    let num_elements: usize = src.dims.iter().map(|d| d.size).product();
    let uncompressed_size = num_elements * original_type.element_size();

    let mut decoder = ZlibDecoder::new(compressed);
    let mut decompressed = Vec::with_capacity(uncompressed_size);
    decoder.read_to_end(&mut decompressed).ok()?;

    let buffer = buffer_from_bytes(&decompressed, original_type)?;

    let mut arr = src.clone();
    arr.data = buffer;
    arr.codec = None;
    Some(arr)
}

// ---------------------------------------------------------------------------
// LZ4HDF5 — port of the C++ NDCodec LZ4HDF5 codec
// ---------------------------------------------------------------------------
//
// C++ `compressLZ4`/`decompressLZ4` (the HAVE_BITSHUFFLE LZ4 variant) use the
// HDF5 LZ4 filter block framing. The container layout is:
//
//   8 bytes  total uncompressed size  (big-endian u64)
//   4 bytes  block size in bytes      (big-endian u32)
//   then, per block:
//     4 bytes  compressed block byte length (big-endian u32)
//     LZ4-block-compressed payload
//
// Each block compresses up to `block_size` raw bytes with the LZ4 block codec.
// The HDF5 LZ4 filter stores a block uncompressed when LZ4 does not shrink it;
// the framed length then equals the raw block length, which decompression uses
// to detect and copy the block verbatim.

/// Default LZ4HDF5 block size in bytes (HDF5 LZ4 filter `DEFAULT_BLOCK_SIZE`, 1 MiB).
const LZ4HDF5_DEFAULT_BLOCK_SIZE: usize = 1 << 20;

/// Compress an NDArray with the HDF5 LZ4 filter framing (`lz4hdf5`).
///
/// Mirrors C++ `compressLZ4` (the HDF5 LZ4 filter variant). The raw data buffer
/// is split into fixed-size blocks, each LZ4-block-compressed, and the HDF5 LZ4
/// container header is prepended. The original data type is stored as an
/// attribute so decompression can rebuild the typed buffer.
pub fn compress_lz4hdf5(src: &NDArray) -> NDArray {
    let raw = src.data.as_u8_slice();
    let data_type = src.data.data_type();
    let original_size = raw.len();
    let block_size = LZ4HDF5_DEFAULT_BLOCK_SIZE;

    // HDF5 LZ4 header: 8-byte total uncompressed size, 4-byte block size.
    let mut out: Vec<u8> = Vec::with_capacity(original_size / 2 + 12);
    out.extend_from_slice(&(original_size as u64).to_be_bytes());
    out.extend_from_slice(&(block_size as u32).to_be_bytes());

    let mut pos = 0usize;
    while pos < raw.len() {
        let n = block_size.min(raw.len() - pos);
        let block = &raw[pos..pos + n];
        let comp = compress(block);
        // The HDF5 LZ4 filter stores the block uncompressed when LZ4 does not
        // shrink it; the framed length then equals the raw block length.
        if comp.len() < n {
            out.extend_from_slice(&(comp.len() as u32).to_be_bytes());
            out.extend_from_slice(&comp);
        } else {
            out.extend_from_slice(&(n as u32).to_be_bytes());
            out.extend_from_slice(block);
        }
        pos += n;
    }

    let compressed_size = out.len();
    let mut arr = src.clone();
    arr.data = NDDataBuffer::U8(out);
    arr.codec = Some(Codec {
        name: CodecName::LZ4HDF5,
        compressed_size,
        level: 0,
        shuffle: 0,
        compressor: 0,
        original_data_type: data_type,
    });

    tracing::debug!(
        original_size,
        compressed_size,
        ratio = original_size as f64 / compressed_size.max(1) as f64,
        "LZ4HDF5 compress"
    );
    arr
}

/// Decompress an LZ4HDF5-compressed NDArray.
///
/// Returns `None` if the codec is not LZ4HDF5 or the container is malformed.
pub fn decompress_lz4hdf5(src: &NDArray) -> Option<NDArray> {
    if src.codec.as_ref().map(|c| c.name) != Some(CodecName::LZ4HDF5) {
        return None;
    }
    let buf = src.data.as_u8_slice();
    if buf.len() < 12 {
        return None;
    }
    let total_bytes = u64::from_be_bytes(buf[0..8].try_into().ok()?) as usize;
    let block_size = u32::from_be_bytes(buf[8..12].try_into().ok()?) as usize;
    if block_size == 0 {
        return None;
    }

    let original_type = original_data_type(src);

    let mut out: Vec<u8> = Vec::with_capacity(total_bytes);
    let mut pos = 12usize;
    while out.len() < total_bytes {
        let n = block_size.min(total_bytes - out.len());
        if pos + 4 > buf.len() {
            return None;
        }
        let clen = u32::from_be_bytes(buf[pos..pos + 4].try_into().ok()?) as usize;
        pos += 4;
        if pos + clen > buf.len() {
            return None;
        }
        let block_payload = &buf[pos..pos + clen];
        if clen == n {
            // Block was stored uncompressed (LZ4 did not shrink it).
            out.extend_from_slice(block_payload);
        } else {
            let block = decompress(block_payload, n).ok()?;
            if block.len() != n {
                return None;
            }
            out.extend_from_slice(&block);
        }
        pos += clen;
    }
    if out.len() != total_bytes {
        return None;
    }

    let buffer = buffer_from_bytes(&out, original_type)?;
    let mut arr = src.clone();
    arr.data = buffer;
    arr.codec = None;
    Some(arr)
}

// ---------------------------------------------------------------------------
// Bitshuffle / LZ4 (bslz4) — port of the C++ NDCodec BSLZ4 codec
// ---------------------------------------------------------------------------
//
// C++ `compressBSLZ4`/`decompressBSLZ4` call `bshuf_compress_lz4` /
// `bshuf_decompress_lz4` from the Bitshuffle library. We reproduce both the
// bitshuffle bit-transpose and the bslz4 container format here so the output
// is byte-compatible with the HDF5 `bslz4` filter:
//
//   8 bytes  total uncompressed size  (big-endian u64)
//   4 bytes  block size in elements   (big-endian u32)
//   then, per block:
//     4 bytes  compressed block byte length (big-endian u32)
//     LZ4-block-compressed, bit-shuffled block payload
//
// Bitshuffle transposes the *bit* matrix of a block: a block of `n` elements
// of `elem_size` bytes is viewed as an `n` x `(elem_size*8)` bit matrix and
// transposed to `(elem_size*8)` x `n`. Bitshuffle requires the per-block
// element count to be a multiple of 8 for the bit transpose; a trailing
// partial block is byte-transposed only (this matches the reference library).

/// Bitshuffle target block size in bytes (library `BSHUF_TARGET_BLOCK_SIZE_B`).
const BSHUF_TARGET_BLOCK_SIZE_B: usize = 8192;
/// Block element count must be a multiple of this (`BSHUF_BLOCKED_MULT`).
const BSHUF_BLOCKED_MULT: usize = 8;
/// Recommended minimum block size in elements (`BSHUF_MIN_RECOMMEND_BLOCK`).
const BSHUF_MIN_RECOMMEND_BLOCK: usize = 128;

/// Default bitshuffle block size in elements for a given element size.
///
/// Mirrors `bshuf_default_block_size` (bitshuffle_core.c:1828): `TARGET /
/// elem_size` rounded down to a multiple of `BSHUF_BLOCKED_MULT`, floored at
/// `BSHUF_MIN_RECOMMEND_BLOCK`. This value must stay stable across versions or
/// previously-encoded streams become undecodable.
pub(crate) fn bshuf_default_block_size(elem_size: usize) -> usize {
    let bs = BSHUF_TARGET_BLOCK_SIZE_B / elem_size.max(1);
    let bs = (bs / BSHUF_BLOCKED_MULT) * BSHUF_BLOCKED_MULT;
    bs.max(BSHUF_MIN_RECOMMEND_BLOCK)
}

/// 8x8 bit-matrix transpose of a quadword, little-endian convention
/// (library macro `TRANS_BIT_8X8`, bitshuffle_core.c:89).
#[inline]
fn trans_bit_8x8(mut x: u64) -> u64 {
    let t = (x ^ (x >> 7)) & 0x00AA_00AA_00AA_00AA;
    x = x ^ t ^ (t << 7);
    let t = (x ^ (x >> 14)) & 0x0000_CCCC_0000_CCCC;
    x = x ^ t ^ (t << 14);
    let t = (x ^ (x >> 28)) & 0x0000_0000_F0F0_F0F0;
    x = x ^ t ^ (t << 28);
    x
}

/// Read 8 bytes at `off` as a little-endian quadword.
#[inline]
fn read_u64_le(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// Transpose bytes within elements (library `bshuf_trans_byte_elem_scal`,
/// bitshuffle_core.c:174). `size` is a multiple of 8 for every shuffled block.
fn bshuf_trans_byte_elem(input: &[u8], out: &mut [u8], size: usize, elem_size: usize) {
    let mut ii = 0;
    while ii + 7 < size {
        for jj in 0..elem_size {
            for kk in 0..8 {
                out[jj * size + ii + kk] = input[ii * elem_size + kk * elem_size + jj];
            }
        }
        ii += 8;
    }
    // Remainder (size % 8); never taken for a shuffled block but kept faithful.
    let mut ii = size - size % 8;
    while ii < size {
        for jj in 0..elem_size {
            out[jj * size + ii] = input[ii * elem_size + jj];
        }
        ii += 1;
    }
}

/// Transpose bits within bytes (library `bshuf_trans_bit_byte_scal`,
/// bitshuffle_core.c:219, little-endian path).
fn bshuf_trans_bit_byte(input: &[u8], out: &mut [u8], size: usize, elem_size: usize) {
    let nbyte = elem_size * size;
    let nbyte_bitrow = nbyte / 8;
    for ii in 0..nbyte_bitrow {
        let mut x = trans_bit_8x8(read_u64_le(input, ii * 8));
        for kk in 0..8 {
            out[kk * nbyte_bitrow + ii] = x as u8;
            x >>= 8;
        }
    }
}

/// Transpose rows of shuffled bits within groups of eight (library
/// `bshuf_trans_bitrow_eight` -> `bshuf_trans_elem`, lda=8, ldb=elem_size).
fn bshuf_trans_bitrow_eight(input: &[u8], out: &mut [u8], size: usize, elem_size: usize) {
    let nbyte_bitrow = size / 8;
    for ii in 0..8 {
        for jj in 0..elem_size {
            let src = (ii * elem_size + jj) * nbyte_bitrow;
            let dst = (jj * 8 + ii) * nbyte_bitrow;
            out[dst..dst + nbyte_bitrow].copy_from_slice(&input[src..src + nbyte_bitrow]);
        }
    }
}

/// Bit-transpose one block of `size` elements (a multiple of 8) — library
/// `bshuf_trans_bit_elem_scal` (bitshuffle_core.c:256): byte transpose, then
/// bit-within-byte transpose, then bit-row transpose.
fn bshuf_trans_bit_elem(input: &[u8], size: usize, elem_size: usize) -> Vec<u8> {
    debug_assert_eq!(size % 8, 0);
    let nbyte = size * elem_size;
    let mut a = vec![0u8; nbyte];
    bshuf_trans_byte_elem(input, &mut a, size, elem_size);
    let mut b = vec![0u8; nbyte];
    bshuf_trans_bit_byte(&a, &mut b, size, elem_size);
    let mut out = vec![0u8; nbyte];
    bshuf_trans_bitrow_eight(&b, &mut out, size, elem_size);
    out
}

/// Transpose bytes for data organized as one row per bit (library
/// `bshuf_trans_byte_bitrow_scal`, bitshuffle_core.c:281).
fn bshuf_trans_byte_bitrow(input: &[u8], out: &mut [u8], size: usize, elem_size: usize) {
    let nbyte_row = size / 8;
    for jj in 0..elem_size {
        for ii in 0..nbyte_row {
            for kk in 0..8 {
                out[ii * 8 * elem_size + jj * 8 + kk] = input[(jj * 8 + kk) * nbyte_row + ii];
            }
        }
    }
}

/// Shuffle bits within the bytes of eight-element groups (library
/// `bshuf_shuffle_bit_eightelem_scal`, bitshuffle_core.c:308, LE path).
fn bshuf_shuffle_bit_eightelem(input: &[u8], out: &mut [u8], size: usize, elem_size: usize) {
    let nbyte = elem_size * size;
    let mut jj = 0;
    while jj < 8 * elem_size {
        let mut ii = 0;
        while ii + 8 * elem_size - 1 < nbyte {
            let mut x = trans_bit_8x8(read_u64_le(input, ii + jj));
            for kk in 0..8 {
                out[ii + jj / 8 + kk * elem_size] = x as u8;
                x >>= 8;
            }
            ii += 8 * elem_size;
        }
        jj += 8;
    }
}

/// Inverse of [`bshuf_trans_bit_elem`] — library `bshuf_untrans_bit_elem_scal`
/// (bitshuffle_core.c:349).
fn bshuf_untrans_bit_elem(input: &[u8], size: usize, elem_size: usize) -> Vec<u8> {
    debug_assert_eq!(size % 8, 0);
    let nbyte = size * elem_size;
    let mut tmp = vec![0u8; nbyte];
    bshuf_trans_byte_bitrow(input, &mut tmp, size, elem_size);
    let mut out = vec![0u8; nbyte];
    bshuf_shuffle_bit_eightelem(&tmp, &mut out, size, elem_size);
    out
}

/// Bit-transpose and LZ4-block-compress one block, framed `[u32 nbytes_BE][lz4]`
/// (library `bshuf_compress_lz4_block`, bitshuffle.c:32). `size` is a multiple
/// of 8.
fn bshuf_compress_lz4_block(
    out: &mut Vec<u8>,
    raw: &[u8],
    elem_start: usize,
    size: usize,
    elem_size: usize,
) {
    let off = elem_start * elem_size;
    let shuffled = bshuf_trans_bit_elem(&raw[off..off + size * elem_size], size, elem_size);
    let comp = compress(&shuffled);
    out.extend_from_slice(&(comp.len() as u32).to_be_bytes());
    out.extend_from_slice(&comp);
}

/// Read one `[u32 nbytes_BE][lz4]` frame at `pos`, LZ4-decode and bit-untranspose
/// it (library `bshuf_decompress_lz4_block`, bitshuffle.c:78). Returns the
/// unshuffled block bytes and the buffer offset past the frame.
fn bshuf_decompress_lz4_block(
    buf: &[u8],
    pos: usize,
    size: usize,
    elem_size: usize,
) -> Option<(Vec<u8>, usize)> {
    if pos + 4 > buf.len() {
        return None;
    }
    let clen = u32::from_be_bytes(buf[pos..pos + 4].try_into().ok()?) as usize;
    let dstart = pos + 4;
    if dstart + clen > buf.len() {
        return None;
    }
    let shuffled = decompress(&buf[dstart..dstart + clen], size * elem_size).ok()?;
    if shuffled.len() != size * elem_size {
        return None;
    }
    Some((
        bshuf_untrans_bit_elem(&shuffled, size, elem_size),
        dstart + clen,
    ))
}

/// Compress an NDArray with the Bitshuffle + LZ4 (`bslz4`) codec.
///
/// Produces the per-block stream exactly as the bitshuffle library's
/// `bshuf_compress_lz4` emits it (bitshuffle.c:153, blocked via
/// `bshuf_blocked_wrap_fun`, bitshuffle_core.c:1667): every full block plus one
/// trailing partial block (the remainder rounded down to a multiple of 8) is
/// bit-transposed, LZ4-block-compressed and framed `[u32 nbytes_BE][lz4]`; the
/// final `size % 8` elements are copied verbatim. There is NO global
/// `[total][block_bytes]` header — that HDF5-chunk framing is added by the file
/// writer (NDFileHDF5Dataset::writeFile), so this payload matches C
/// `pArray->pData`. The original element type is recorded in the codec so
/// decompression can rebuild the typed buffer and derive the element count.
pub fn compress_bslz4(src: &NDArray) -> NDArray {
    let raw = src.data.as_u8_slice();
    let data_type = src.data.data_type();
    let elem_size = data_type.element_size();
    let total_elems = if elem_size > 0 {
        raw.len() / elem_size
    } else {
        0
    };
    let block_size = bshuf_default_block_size(elem_size);

    let mut out: Vec<u8> = Vec::with_capacity(raw.len() / 2 + 16);

    let n_full = total_elems / block_size;
    let mut elem = 0usize;
    for _ in 0..n_full {
        bshuf_compress_lz4_block(&mut out, raw, elem, block_size, elem_size);
        elem += block_size;
    }
    // One trailing partial block, rounded down to a multiple of 8.
    let mut last_block = total_elems % block_size;
    last_block -= last_block % BSHUF_BLOCKED_MULT;
    if last_block > 0 {
        bshuf_compress_lz4_block(&mut out, raw, elem, last_block, elem_size);
        elem += last_block;
    }
    // The final `size % 8` elements are copied raw (no shuffle, no frame).
    if elem < total_elems {
        out.extend_from_slice(&raw[elem * elem_size..total_elems * elem_size]);
    }

    let compressed_size = out.len();
    let mut arr = src.clone();
    arr.data = NDDataBuffer::U8(out);
    arr.codec = Some(Codec {
        name: CodecName::BSLZ4,
        compressed_size,
        level: 0,
        shuffle: 0,
        compressor: 0,
        original_data_type: data_type,
    });

    tracing::debug!(
        original_size = raw.len(),
        compressed_size,
        ratio = raw.len() as f64 / compressed_size.max(1) as f64,
        "BSLZ4 compress"
    );
    arr
}

/// Decompress a Bitshuffle + LZ4 (`bslz4`) NDArray.
///
/// Inverse of [`compress_bslz4`], mirroring `bshuf_decompress_lz4`
/// (bitshuffle.c:160). The uncompressed element count comes from the preserved
/// array dims (matching C, which passes `nElements` from the NDArray, not from
/// the payload), so the codec buffer carries no global header. Returns `None`
/// if the codec is not BSLZ4 or the stream is malformed.
pub fn decompress_bslz4(src: &NDArray) -> Option<NDArray> {
    let codec = src.codec.as_ref()?;
    if codec.name != CodecName::BSLZ4 {
        return None;
    }
    let buf = src.data.as_u8_slice();
    let original_type = original_data_type(src);
    let elem_size = original_type.element_size();
    if elem_size == 0 {
        return None;
    }
    let total_elems: usize = src.dims.iter().map(|d| d.size).product();
    let total_bytes = total_elems * elem_size;
    let block_size = bshuf_default_block_size(elem_size);

    let mut out: Vec<u8> = Vec::with_capacity(total_bytes);
    let mut pos = 0usize;

    let n_full = total_elems / block_size;
    for _ in 0..n_full {
        let (block, next) = bshuf_decompress_lz4_block(buf, pos, block_size, elem_size)?;
        out.extend_from_slice(&block);
        pos = next;
    }
    // One trailing partial block, rounded down to a multiple of 8.
    let mut last_block = total_elems % block_size;
    last_block -= last_block % BSHUF_BLOCKED_MULT;
    if last_block > 0 {
        let (block, next) = bshuf_decompress_lz4_block(buf, pos, last_block, elem_size)?;
        out.extend_from_slice(&block);
        pos = next;
    }
    // The final `size % 8` elements were copied raw.
    let leftover_bytes = (total_elems % BSHUF_BLOCKED_MULT) * elem_size;
    if leftover_bytes > 0 {
        if pos + leftover_bytes > buf.len() {
            return None;
        }
        out.extend_from_slice(&buf[pos..pos + leftover_bytes]);
    }
    if out.len() != total_bytes {
        return None;
    }

    let buffer = buffer_from_bytes(&out, original_type)?;
    let mut arr = src.clone();
    arr.data = buffer;
    arr.codec = None;
    Some(arr)
}

/// Compress an NDArray to JPEG.
///
/// Mirrors C `compressJPEG` (NDPluginCodec.cpp:109-266), which decides the JPEG
/// geometry from the dimension count (:146-169) and the *source pixel layout*
/// from the `ColorMode` attribute (:181-227):
/// - 2-D: grayscale, `[x, y]`.
/// - 3-D RGB1 `[3, x, y]`: already pixel-interleaved, encoded as-is.
/// - 3-D RGB2 `[x, 3, y]` and RGB3 `[x, y, 3]`: C walks the three colour planes
///   (`pRed`/`pGreen`/`pBlue`, plane step `sizeX*3` for RGB2 and `sizeX` for
///   RGB3) and re-interleaves each scanline into an RGB row before encoding.
///   The port reaches the same pixel order through `convert_rgb_layout`, the
///   single owner of RGB layout conversion (also used by the JPEG/TIFF/Magick
///   file writers), so the interleave rule is not re-implemented here.
///
/// Both 8-bit types are accepted, as in C (`case NDInt8: case NDUInt8:`,
/// :135-143). Returns `None` for anything C rejects: a non-8-bit type, a
/// dimension count other than 2 or 3, or a 3-D array whose `ColorMode` is not
/// one of the three RGB layouts.
pub fn compress_jpeg(src: &NDArray, quality: u8) -> Result<NDArray, JpegCompressError> {
    use ad_core_rs::color::{NDColorMode, convert_rgb_layout};

    // C `:135-143` — the dataType switch comes first.
    match src.data.data_type() {
        NDDataType::UInt8 | NDDataType::Int8 => {}
        _ => return Err(JpegCompressError::NotEightBit),
    }

    let info = src.info();

    // C `:146-169` — the ndims switch: 2-D and 3-D have arms, anything else is
    // "Unsupported array structure".
    if !matches!(src.dims.len(), 2 | 3) {
        return Err(JpegCompressError::UnsupportedArrayStructure);
    }

    // C `:181-204` — the colorMode switch: Mono/RGB1/RGB2/RGB3 have arms, and
    // every other mode (Bayer, the three YUVs) falls to "Unknown color mode %d".
    // `info.color_mode` is the ColorMode attribute defaulting to Mono, exactly
    // C's `int colorMode = NDColorModeMono; if (pAttribute) getValue(...)` (:117-121).
    match info.color_mode {
        NDColorMode::Mono | NDColorMode::RGB1 | NDColorMode::RGB2 | NDColorMode::RGB3 => {}
        mode => return Err(JpegCompressError::UnknownColorMode(mode as i32)),
    }

    // JPEG dimensions must fit in u16 — see `JpegCompressError::EncodeFailed`.
    if info.x_size > u16::MAX as usize || info.y_size > u16::MAX as usize {
        return Err(JpegCompressError::EncodeFailed);
    }

    // RGB2/RGB3 are re-interleaved to RGB1 first; every other accepted layout
    // encodes straight out of the input buffer.
    let (color_type, interleaved) = match (src.dims.len(), info.color_mode) {
        (2, NDColorMode::Mono | NDColorMode::RGB1) => (jpeg_encoder::ColorType::Luma, None),
        (3, NDColorMode::RGB1) if info.color_size == 3 => (jpeg_encoder::ColorType::Rgb, None),
        (3, mode @ (NDColorMode::RGB2 | NDColorMode::RGB3)) if info.color_size == 3 => (
            jpeg_encoder::ColorType::Rgb,
            Some(
                convert_rgb_layout(src, mode, NDColorMode::RGB1)
                    .map_err(|_| JpegCompressError::EncodeFailed)?,
            ),
        ),
        // Layouts C leaves `image_width`/`image_height` unset for, or reads out
        // of bounds on — see `JpegCompressError::EncodeFailed`.
        _ => return Err(JpegCompressError::EncodeFailed),
    };

    let width = info.x_size as u16;
    let height = info.y_size as u16;
    let pixels = interleaved
        .as_ref()
        .map_or_else(|| src.data.as_u8_slice(), |a| a.data.as_u8_slice());

    let mut jpeg_buf = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut jpeg_buf, quality);
    if encoder.encode(pixels, width, height, color_type).is_err() {
        return Err(JpegCompressError::EncodeFailed);
    }

    let compressed_size = jpeg_buf.len();
    let original_size = src.data.as_u8_slice().len();

    let mut arr = src.clone();
    arr.data = NDDataBuffer::U8(jpeg_buf);
    arr.codec = Some(Codec {
        name: CodecName::JPEG,
        compressed_size,
        level: 0,
        shuffle: 0,
        compressor: 0,
        // Record the source type so the codec carries the original element type
        // uniformly (C `NDArray::dataType`, NDPluginCodec.cpp:35-36).
        original_data_type: src.data.data_type(),
    });

    tracing::debug!(
        original_size,
        compressed_size,
        ratio = original_size as f64 / compressed_size.max(1) as f64,
        "JPEG compress (quality={})",
        quality,
    );

    Ok(arr)
}

/// Why `compress_jpeg` refused an array, carrying the exact `errorMessage` C
/// writes at that rejection point.
///
/// C's `compressJPEG` sets a *different* string at each failure and the plugin
/// copies it verbatim into the `CodecError` PV, so the message is part of the
/// observable contract — which means the encoder, not its caller, has to name the
/// failure. A bare `Option` forced the caller to invent one generic text for all
/// of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegCompressError {
    /// C `:135-143` — only `NDInt8`/`NDUInt8` reach the encoder.
    NotEightBit,
    /// C `:165-169` — `ndims` is neither 2 nor 3.
    UnsupportedArrayStructure,
    /// C `:200-204` (and the identical guard inside the scanline loop, `:228-232`)
    /// — the colorMode switch has no arm for this mode: Bayer and the YUVs.
    UnknownColorMode(i32),
    /// C `:234-238` — libjpeg would not take the data.
    ///
    /// The port also lands here for the arrays C hands to libjpeg's *fatal* error
    /// handler: `jpeg_std_error` (`:115`) exits the process on error, so C has no
    /// recovery path for dimensions past libjpeg's limit, nor for the 3-D layouts
    /// its `else if` chain (`:155-164`) leaves `image_width`/`image_height` unset
    /// for — a 3-D array whose ColorMode is Mono, or whose colour axis is not 3.
    /// The port reports the failure instead of aborting the IOC, under C's own
    /// text for "the encoder would not take this array".
    EncodeFailed,
}

impl JpegCompressError {
    /// The `errorMessage` C writes (NDPluginCodec.cpp:140, :166, :201, :235).
    pub fn message(&self) -> Cow<'static, str> {
        match self {
            Self::NotEightBit => "JPEG only supports 8-bit data".into(),
            Self::UnsupportedArrayStructure => "Unsupported array structure".into(),
            // C `sprintf(errorMessage, "Unknown color mode %d", colorMode)` —
            // NDColorMode's discriminants are C's NDColorMode_t values.
            Self::UnknownColorMode(mode) => format!("Unknown color mode {}", mode).into(),
            Self::EncodeFailed => "Error writing JPEG data".into(),
        }
    }
}

/// Decompress a JPEG-compressed NDArray.
///
/// Uses jpeg-decoder to decode the JPEG data back to pixel data.
/// Reconstructs proper dimensions and color layout (mono or RGB1).
///
/// A decoded JPEG is always 8-bit mono or 8-bit RGB1 (C comment at
/// NDPluginCodec.cpp:268-272), whatever the layout of the array that was
/// compressed, so C overwrites the `ColorMode` attribute on the output
/// (:318-322). Without that write an RGB2/RGB3 source's stale `ColorMode` would
/// survive onto RGB1 data and every downstream `getInfo` would read the planes
/// in the wrong order.
///
/// Returns `None` if the codec is not JPEG or decoding fails.
pub fn decompress_jpeg(src: &NDArray) -> Option<NDArray> {
    use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
    use ad_core_rs::color::NDColorMode;

    if src.codec.as_ref().map(|c| c.name) != Some(CodecName::JPEG) {
        return None;
    }

    let compressed = src.data.as_u8_slice();
    let mut decoder = jpeg_decoder::Decoder::new(compressed);
    let pixels = decoder.decode().ok()?;
    let metadata = decoder.info()?;

    let width = metadata.width as usize;
    let height = metadata.height as usize;

    let (dims, color_mode) = match metadata.pixel_format {
        jpeg_decoder::PixelFormat::L8 => (
            vec![NDDimension::new(width), NDDimension::new(height)],
            NDColorMode::Mono,
        ),
        jpeg_decoder::PixelFormat::RGB24 => (
            vec![
                NDDimension::new(3),
                NDDimension::new(width),
                NDDimension::new(height),
            ],
            NDColorMode::RGB1,
        ),
        _ => return None,
    };

    let mut arr = src.clone();
    arr.dims = dims;
    arr.data = NDDataBuffer::U8(pixels);
    arr.codec = None;
    arr.attributes.add(NDAttribute::new_static(
        "ColorMode",
        "Color Mode",
        NDAttrSource::Driver,
        NDAttrValue::Int32(color_mode as i32),
    ));

    Some(arr)
}

/// Blosc compression settings.
#[derive(Debug, Clone, Copy)]
pub struct BloscConfig {
    /// Sub-compressor: 0=BloscLZ, 1=LZ4, 2=LZ4HC, 3=Snappy, 4=Zlib, 5=Zstd
    pub compressor: u32,
    /// Compression level (0-9).
    pub clevel: u32,
    /// Shuffle mode: 0=None, 1=ByteShuffle, 2=BitShuffle.
    pub shuffle: u32,
}

impl Default for BloscConfig {
    fn default() -> Self {
        Self {
            compressor: 0,
            // C NDPluginCodec sets the default NDCodecBloscCLevel to 5
            // (NDPluginCodec.cpp:894); a lower default would yield different
            // compressed bytes and NDCompressedSize than C for an unconfigured
            // plugin.
            clevel: 5,
            shuffle: 0,
        }
    }
}

/// Compress an NDArray using Blosc via rust-hdf5's filter pipeline.
pub fn compress_blosc(src: &NDArray, config: &BloscConfig) -> NDArray {
    let raw = src.data.as_u8_slice();
    let element_size = src.data.data_type().element_size();

    // Standard H5Zblosc cd_values layout (c-blosc `blosc_filter.c`):
    // [filter_ver, blosc_ver, typesize, nbytes, clevel, shuffle, compcode].
    // The HDF5 reader keys on typesize@2, doshuffle@5 and compcode@6; placing
    // the sub-compressor anywhere but index 6 makes the pipeline compress with
    // the wrong codec (e.g. clevel 5 at slot 6 selects ZSTD instead of the
    // configured BloscLZ).
    let pipeline = FilterPipeline {
        filters: vec![Filter {
            id: FILTER_BLOSC,
            flags: 0,
            cd_values: vec![
                2,                   // filter version (cd_values[0])
                2,                   // blosc version (cd_values[1])
                element_size as u32, // type size (cd_values[2])
                raw.len() as u32,    // uncompressed chunk size (cd_values[3])
                config.clevel,       // compression level (cd_values[4])
                config.shuffle,      // shuffle (cd_values[5])
                config.compressor,   // sub-compressor (cd_values[6])
            ],
        }],
    };

    let compressed = match apply_filters(&pipeline, raw) {
        Ok(data) => data,
        Err(_) => return src.clone(),
    };

    let compressed_size = compressed.len();
    let original_data_type = src.data.data_type();
    let mut arr = src.clone();
    arr.data = NDDataBuffer::U8(compressed);
    arr.codec = Some(Codec {
        name: CodecName::Blosc,
        compressed_size,
        // C records the real Blosc params in the codec (NDPluginCodec.cpp:
        // 400-402: codec.level = clevel; shuffle; compressor), not zeros.
        level: config.clevel as i32,
        shuffle: config.shuffle as i32,
        compressor: config.compressor as i32,
        original_data_type,
    });
    arr
}

/// Decompress a Blosc-compressed NDArray via rust-hdf5's filter pipeline.
pub fn decompress_blosc(src: &NDArray) -> Option<NDArray> {
    let codec = src.codec.as_ref()?;
    if codec.name != CodecName::Blosc {
        return None;
    }

    let compressed = src.data.as_u8_slice();
    let original_type = original_data_type(src);
    let element_size = original_type.element_size();

    // The blosc chunk header self-describes typesize/nbytes/flags, but the HDF5
    // reader takes the sub-compressor from cd_values[6] (defaulting to LZ4). An
    // empty cd_values therefore mis-decodes any non-LZ4 buffer, so author the
    // standard layout with the codec's recorded sub-compressor at index 6.
    let pipeline = FilterPipeline {
        filters: vec![Filter {
            id: FILTER_BLOSC,
            flags: 0,
            cd_values: vec![
                2,
                2,
                element_size as u32,
                0,
                codec.level as u32,
                codec.shuffle as u32,
                codec.compressor as u32,
            ],
        }],
    };

    let decompressed = reverse_filters(&pipeline, compressed).ok()?;

    let buffer = buffer_from_bytes(&decompressed, original_type)?;

    let mut arr = src.clone();
    arr.data = buffer;
    arr.codec = None;
    Some(arr)
}

/// Codec operation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecMode {
    /// Compress using the specified codec. `quality` is used for JPEG (1-100).
    Compress { codec: CodecName, quality: u8 },
    /// Decompress: auto-detect codec from the array's codec field.
    Decompress,
}

/// Pure codec processing logic.
///
/// Reports compression ratio after each operation via `compression_ratio()`.
#[derive(Default)]
struct CodecParamIndices {
    mode: Option<usize>,
    compressor: Option<usize>,
    comp_factor: Option<usize>,
    jpeg_quality: Option<usize>,
    blosc_compressor: Option<usize>,
    blosc_clevel: Option<usize>,
    blosc_shuffle: Option<usize>,
    blosc_numthreads: Option<usize>,
    codec_status: Option<usize>,
    codec_error: Option<usize>,
}

/// The operator-selected codec settings plus the ratio the last frame
/// produced. `on_param_change` rebuilds `mode` from `jpeg_quality`, so the two
/// must move together.
struct CodecState {
    mode: CodecMode,
    compression_ratio: f64,
    jpeg_quality: u8,
    blosc_config: BloscConfig,
}

pub struct CodecProcessor {
    state: Mutex<CodecState>,
    params: CodecParamIndices,
}

impl CodecProcessor {
    pub fn new(mode: CodecMode) -> Self {
        let quality = match mode {
            CodecMode::Compress { quality, .. } => quality,
            _ => 85,
        };
        Self {
            state: Mutex::new(CodecState {
                mode,
                compression_ratio: 1.0,
                jpeg_quality: quality,
                blosc_config: BloscConfig::default(),
            }),
            params: CodecParamIndices::default(),
        }
    }

    /// Last computed compression ratio (original_size / compressed_size).
    /// Returns 1.0 if no compression has been performed yet or on decompression.
    pub fn compression_ratio(&self) -> f64 {
        self.state.lock().compression_ratio
    }
}

/// What the codec plugin decided for one input array, mirroring the exits of C
/// `NDPluginCodec::processCallbacks` (NDPluginCodec.cpp:649-782).
///
/// C distinguishes "the input *is* the result" (`result = pArray`, no error,
/// codecStatus untouched) from "the codec produced nothing" (`result = NULL` +
/// errorMessage, and the `finish:` block then substitutes `pArray` so the frame
/// still flows downstream). Both end up publishing the input array, so an
/// `Option<NDArray>` cannot tell them apart — collapsing them is what made an
/// uncompressed input to a Decompress plugin report a codec failure.
///
/// The reported severity and error string are derived from the variant
/// ([`CodecOutcome::status`] / [`CodecOutcome::error_message`]), so they are a
/// property of what happened rather than integers picked at the publish site: a
/// benign skip cannot be reported with a failure's severity, and no site can
/// invent a level C does not have.
enum CodecOutcome {
    /// C `result = pArray`, codecStatus SUCCESS: the input is the output,
    /// unchanged and not an error.
    PassThrough,
    /// C `result = pArray` + errorMessage + `NDCODEC_WARNING` (:671-676, and the
    /// same guard inside each compressor, e.g. :466-469): the operation was
    /// skipped, not failed — the frame flows on unchanged.
    Skipped(&'static str),
    /// C `result = <new array>`, codecStatus SUCCESS: the codec produced a new
    /// array.
    Converted(NDArray),
    /// C `result = NULL` + errorMessage + `NDCODEC_ERROR`: the codec failed; the
    /// input is republished but the error is reported.
    ///
    /// Owned, because C composes some of these with `sprintf` (e.g. "Unknown
    /// color mode %d", NDPluginCodec.cpp:201) — the text belongs to the codec
    /// that failed, not to the caller.
    Failed(Cow<'static, str>),
}

impl CodecOutcome {
    /// Severity reported in `CodecStatus` (C `NDCodecStatus_t`).
    fn status(&self) -> CodecStatus {
        match self {
            Self::PassThrough | Self::Converted(_) => CodecStatus::Success,
            Self::Skipped(_) => CodecStatus::Warning,
            Self::Failed(_) => CodecStatus::Error,
        }
    }

    /// Text reported in `CodecError` (C `errorMessage`, empty unless the codec
    /// had something to say).
    fn error_message(&self) -> &str {
        match self {
            Self::PassThrough | Self::Converted(_) => "",
            Self::Skipped(message) => message,
            Self::Failed(message) => message,
        }
    }
}

impl NDPluginProcess for CodecProcessor {
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        // C reads the codec selection under the port lock and releases it
        // around every codec call (`NDPluginCodec.cpp:556`, `:596`), so the
        // compression itself never holds it. `mode`, `jpeg_quality` and
        // `blosc_config` are the selection; `compression_ratio` is this
        // frame's result and goes back under the lock at the end.
        let (mode, jpeg_quality, blosc_config) = {
            let state = self.state.lock();
            (state.mode, state.jpeg_quality, state.blosc_config)
        };
        let original_bytes = array.data.as_u8_slice().len();

        // C sets NDCodecCompressor from the codec it found on the input on every
        // decompress branch, including the empty-codec one (NDPluginCodec.cpp:
        // 732-757). Compress mode never writes it — there it is the operator's
        // selection.
        let mut compressor: Option<i32> = None;

        let outcome = match mode {
            // C: `algo` NONE short-circuits both the already-compressed check
            // (:671, gated on `algo`) and the codec switch (:680-683
            // `case NDCODEC_NONE: default: result = pArray`) — a pass-through,
            // never a failure.
            CodecMode::Compress {
                codec: CodecName::None,
                ..
            } => CodecOutcome::PassThrough,
            CodecMode::Compress { .. } if array.codec.is_some() => {
                // Already compressed — C passes the input through, but reports it
                // as a benign WARNING with an error string (:671-676).
                CodecOutcome::Skipped("Array already compressed")
            }
            CodecMode::Compress { codec, .. } => match codec {
                CodecName::LZ4 => CodecOutcome::Converted(compress_lz4(array)),
                // The encoder names its own failure (C writes a different
                // errorMessage at each rejection, NDPluginCodec.cpp:140, :166,
                // :201, :235); the caller must not invent one.
                CodecName::JPEG => match compress_jpeg(array, jpeg_quality) {
                    Ok(out) => CodecOutcome::Converted(out),
                    Err(e) => CodecOutcome::Failed(e.message()),
                },
                CodecName::Zlib => CodecOutcome::Converted(compress_zlib(array)),
                CodecName::Blosc => CodecOutcome::Converted(compress_blosc(array, &blosc_config)),
                CodecName::LZ4HDF5 => CodecOutcome::Converted(compress_lz4hdf5(array)),
                CodecName::BSLZ4 => CodecOutcome::Converted(compress_bslz4(array)),
                // Matched by the first arm above.
                CodecName::None => CodecOutcome::PassThrough,
            },
            CodecMode::Decompress => {
                // C keys the decompress dispatch on the input's codec *name*, so
                // an empty name is simply "not compressed" (`codec.empty()`,
                // Codec.h:37-39) — the Rust `Option` and a `CodecName::None`
                // inside it mean the same thing and must decide the same way.
                let name = array
                    .codec
                    .as_ref()
                    .map(|c| c.name)
                    .unwrap_or(CodecName::None);
                compressor = Some(name.ordinal());
                match name {
                    // C `NDPluginCodec.cpp:732-735` — uncompressed input: result = pArray,
                    // COMPRESSOR = NDCODEC_NONE, codecStatus stays SUCCESS.
                    CodecName::None => CodecOutcome::PassThrough,
                    CodecName::LZ4 => match decompress_lz4(array) {
                        Some(out) => CodecOutcome::Converted(out),
                        None => CodecOutcome::Failed("Failed to LZ4 decompress".into()),
                    },
                    CodecName::JPEG => match decompress_jpeg(array) {
                        Some(out) => CodecOutcome::Converted(out),
                        None => CodecOutcome::Failed("Error decoding JPEG".into()),
                    },
                    CodecName::Zlib => match decompress_zlib(array) {
                        Some(out) => CodecOutcome::Converted(out),
                        None => CodecOutcome::Failed("Failed to Zlib decompress".into()),
                    },
                    CodecName::Blosc => match decompress_blosc(array) {
                        Some(out) => CodecOutcome::Converted(out),
                        None => CodecOutcome::Failed("Failed to Blosc decompress".into()),
                    },
                    CodecName::LZ4HDF5 => match decompress_lz4hdf5(array) {
                        Some(out) => CodecOutcome::Converted(out),
                        None => CodecOutcome::Failed("Failed to LZ4 decompress".into()),
                    },
                    // C's decompressBSLZ4 reports "Failed to Blosc decompress"
                    // (NDPluginCodec.cpp:601) — a copy-paste from decompressBlosc
                    // (:431), but it is the text the CodecError PV shows for a
                    // corrupt BSLZ4 frame, so it is the contract.
                    CodecName::BSLZ4 => match decompress_bslz4(array) {
                        Some(out) => CodecOutcome::Converted(out),
                        None => CodecOutcome::Failed("Failed to Blosc decompress".into()),
                    },
                }
            }
        };

        let status = outcome.status();
        let error = outcome.error_message().to_string();

        // C recomputes NDCodecCompFactor only when `result != pArray`
        // (:726-730, :763-767); on any exit that republishes the input it stays
        // at 1.0.
        let mut compression_ratio = 1.0;
        let output = match outcome {
            CodecOutcome::Converted(out) => {
                let output_bytes = out.data.as_u8_slice().len();
                compression_ratio = match mode {
                    CodecMode::Compress { .. } => {
                        original_bytes as f64 / output_bytes.max(1) as f64
                    }
                    CodecMode::Decompress => output_bytes as f64 / original_bytes.max(1) as f64,
                };
                out
            }
            CodecOutcome::PassThrough | CodecOutcome::Skipped(_) | CodecOutcome::Failed(_) => {
                array.clone()
            }
        };
        self.state.lock().compression_ratio = compression_ratio;

        let mut updates = Vec::new();
        if let Some(idx) = self.params.comp_factor {
            updates.push(ParamUpdate::float64(idx, compression_ratio));
        }
        if let (Some(idx), Some(value)) = (self.params.compressor, compressor) {
            updates.push(ParamUpdate::int32(idx, value));
        }
        if let Some(idx) = self.params.codec_status {
            updates.push(ParamUpdate::int32(idx, status.as_i32()));
        }
        if let Some(idx) = self.params.codec_error {
            updates.push(ParamUpdate::Octet {
                reason: idx,
                addr: 0,
                value: error,
            });
        }

        let mut r = ProcessResult::arrays(vec![Arc::new(output)]);
        r.param_updates = updates;
        r
    }

    fn plugin_type(&self) -> &str {
        "NDPluginCodec"
    }

    /// C `NDPluginCodec` passes `compressionAware=true` to the base constructor
    /// (`NDPluginCodec.cpp:865-870`), unconditionally regardless of mode, so
    /// compressed arrays reach it for decompression. Without this override the
    /// runtime drop gate (`if compressed && !compression_aware`) would discard
    /// every compressed input before `process_array`, making `Decompress` dead.
    /// Returned unconditionally because the same instance can switch
    /// Compress↔Decompress at runtime, while this flag is read once at
    /// construction.
    fn compression_aware(&self) -> bool {
        true
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        base.create_param("MODE", ParamType::Int32)?;
        base.create_param("COMPRESSOR", ParamType::Int32)?;
        base.create_param("COMP_FACTOR", ParamType::Float64)?;
        base.create_param("JPEG_QUALITY", ParamType::Int32)?;
        base.create_param("BLOSC_COMPRESSOR", ParamType::Int32)?;
        base.create_param("BLOSC_CLEVEL", ParamType::Int32)?;
        base.create_param("BLOSC_SHUFFLE", ParamType::Int32)?;
        base.create_param("BLOSC_NUMTHREADS", ParamType::Int32)?;
        base.create_param("CODEC_STATUS", ParamType::Int32)?;
        base.create_param("CODEC_ERROR", ParamType::Octet)?;

        self.params.mode = base.find_param("MODE");
        self.params.compressor = base.find_param("COMPRESSOR");
        self.params.comp_factor = base.find_param("COMP_FACTOR");
        self.params.jpeg_quality = base.find_param("JPEG_QUALITY");
        self.params.blosc_compressor = base.find_param("BLOSC_COMPRESSOR");
        self.params.blosc_clevel = base.find_param("BLOSC_CLEVEL");
        self.params.blosc_shuffle = base.find_param("BLOSC_SHUFFLE");
        self.params.blosc_numthreads = base.find_param("BLOSC_NUMTHREADS");
        self.params.codec_status = base.find_param("CODEC_STATUS");
        self.params.codec_error = base.find_param("CODEC_ERROR");
        Ok(())
    }

    fn on_param_change(
        &self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        let mut state = self.state.lock();
        if Some(reason) == self.params.mode {
            let v = params.value.as_i32();
            if v == 0 {
                // Compress — keep current codec
                let codec = match state.mode {
                    CodecMode::Compress { codec, .. } => codec,
                    _ => CodecName::LZ4,
                };
                state.mode = CodecMode::Compress {
                    codec,
                    quality: state.jpeg_quality,
                };
            } else {
                state.mode = CodecMode::Decompress;
            }
        } else if Some(reason) == self.params.compressor {
            // C `NDCodecCompressor_t` (Codec.h:12-18) — the ordinal mapping lives
            // in `CodecName::from_ordinal`, shared with the COMPRESSOR value the
            // decompress path reports back.
            let codec = CodecName::from_ordinal(params.value.as_i32());
            if let CodecMode::Compress { .. } = state.mode {
                state.mode = CodecMode::Compress {
                    codec,
                    quality: state.jpeg_quality,
                };
            }
        } else if Some(reason) == self.params.jpeg_quality {
            state.jpeg_quality = params.value.as_i32().clamp(1, 100) as u8;
            if let CodecMode::Compress { codec, .. } = state.mode {
                state.mode = CodecMode::Compress {
                    codec,
                    quality: state.jpeg_quality,
                };
            }
        } else if Some(reason) == self.params.blosc_compressor {
            state.blosc_config.compressor = params.value.as_i32().max(0) as u32;
        } else if Some(reason) == self.params.blosc_clevel {
            state.blosc_config.clevel = params.value.as_i32().clamp(0, 9) as u32;
        } else if Some(reason) == self.params.blosc_shuffle {
            state.blosc_config.shuffle = params.value.as_i32().max(0) as u32;
        }

        ad_core_rs::plugin::runtime::ParamChangeResult::updates(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_u8_array(width: usize, height: usize) -> NDArray {
        let mut arr = NDArray::new(
            vec![NDDimension::new(width), NDDimension::new(height)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = (i % 256) as u8;
            }
        }
        arr
    }

    fn make_rgb_array(width: usize, height: usize) -> NDArray {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(width),
                NDDimension::new(height),
            ],
            NDDataType::UInt8,
        );
        // info() reads ColorMode for 3D arrays
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "Color Mode",
            NDAttrSource::Driver,
            NDAttrValue::Int32(2), // RGB1
        ));
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = (i % 256) as u8;
            }
        }
        arr
    }

    /// Every compressor must record the original element type STRUCTURALLY in
    /// `codec.original_data_type` (C `NDArray::dataType`, NDPluginCodec.cpp:35-36)
    /// and must attach NO carrier attribute, so the attribute list a compressed
    /// frame carries holds only genuine driver/user attributes at every output
    /// boundary by construction.
    #[test]
    fn compressors_record_type_in_codec_not_an_attribute() {
        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::UInt16);
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 7) as u16;
            }
        }
        for compressed in [
            compress_lz4(&arr),
            compress_zlib(&arr),
            compress_lz4hdf5(&arr),
            compress_bslz4(&arr),
            compress_blosc(&arr, &BloscConfig::default()),
        ] {
            assert_eq!(
                compressed.codec.as_ref().unwrap().original_data_type,
                NDDataType::UInt16,
                "the original element type must travel in the codec"
            );
            assert!(
                compressed
                    .attributes
                    .get("CODEC_ORIGINAL_DATA_TYPE")
                    .is_none(),
                "no codec carrier attribute may be attached to a compressed frame"
            );
        }
    }

    #[test]
    fn test_adp29_blosc_default_clevel_and_codec_params() {
        // C NDPluginCodec default BloscCLevel = 5 (NDPluginCodec.cpp:894); a
        // lower default would change the compressed bytes and NDCompressedSize.
        assert_eq!(
            BloscConfig::default().clevel,
            5,
            "default Blosc clevel must be 5 (C parity)"
        );

        // C records the real level/shuffle/compressor in the codec
        // (NDPluginCodec.cpp:400-402), not zeros.
        let mut arr = NDArray::new(vec![NDDimension::new(8)], NDDataType::UInt16);
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 7) as u16;
            }
        }
        let out = compress_blosc(&arr, &BloscConfig::default());
        let codec = out.codec.as_ref().expect("blosc codec metadata");
        // codec.level = 5 (not the old hardcoded 0) proves the real clevel is
        // recorded; shuffle/compressor likewise mirror the config.
        assert_eq!(codec.level, 5, "codec.level records the default clevel 5");
        assert_eq!(codec.shuffle, 0, "codec.shuffle records shuffle");
        assert_eq!(codec.compressor, 0, "codec.compressor records compressor");
    }

    #[test]
    fn test_blosc_roundtrip_u16_default_compressor() {
        // Regression: the cd_values were mis-ordered so the sub-compressor slot
        // (index 6) held the clevel, selecting ZSTD instead of the configured
        // BloscLZ; the buffer then failed to reverse. Round-trip with the
        // default config (compressor 0 = BloscLZ, clevel 5) must reconstruct the
        // exact bytes.
        let mut arr = NDArray::new(
            vec![NDDimension::new(100), NDDimension::new(20)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 37 % 65521) as u16;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_blosc(&arr, &BloscConfig::default());
        assert_eq!(compressed.codec.as_ref().unwrap().name, CodecName::Blosc);
        assert_ne!(
            compressed.data.as_u8_slice(),
            original.as_slice(),
            "blosc must actually compress (not fall back to the raw clone)"
        );

        let decompressed = decompress_blosc(&compressed).expect("blosc round-trip");
        assert!(decompressed.codec.is_none());
        assert_eq!(decompressed.data.data_type(), NDDataType::UInt16);
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    #[test]
    fn test_blosc_roundtrip_u16_lz4_subcompressor() {
        // A non-default sub-compressor (LZ4 = 1) must round-trip too — the
        // recorded cd_values[6] drives the reader's sub-codec dispatch.
        let cfg = BloscConfig {
            compressor: 1,
            clevel: 5,
            shuffle: 1,
        };
        let mut arr = NDArray::new(vec![NDDimension::new(256)], NDDataType::UInt16);
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 13 % 65521) as u16;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_blosc(&arr, &cfg);
        let codec = compressed.codec.as_ref().unwrap();
        assert_eq!(codec.compressor, 1, "records the LZ4 sub-compressor");
        assert_eq!(codec.shuffle, 1, "records byte shuffle");

        let decompressed = decompress_blosc(&compressed).expect("blosc lz4 round-trip");
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    // ---- LZ4 tests ----

    #[test]
    fn test_lz4_roundtrip_u8() {
        let arr = make_u8_array(4, 4);
        let original_data = arr.data.as_u8_slice().to_vec();

        let compressed = compress_lz4(&arr);
        assert_eq!(compressed.codec.as_ref().unwrap().name, CodecName::LZ4);
        // Data buffer should now be the compressed bytes
        assert_ne!(compressed.data.as_u8_slice(), original_data.as_slice());

        let decompressed = decompress_lz4(&compressed).unwrap();
        assert!(decompressed.codec.is_none());
        assert_eq!(decompressed.data.data_type(), NDDataType::UInt8);
        assert_eq!(decompressed.data.as_u8_slice(), original_data.as_slice());
    }

    #[test]
    fn test_decompress_runtime_does_not_drop_compressed_input() {
        // ADP-98: a Codec plugin in Decompress mode is compression-aware
        // (C NDPluginCodec passes compressionAware=true, NDPluginCodec.cpp:870),
        // so the runtime drop gate (runtime.rs:1785 `if compressed &&
        // !compression_aware`) must NOT discard its compressed input. Without the
        // compression_aware() override the compressed array is dropped before
        // process_array runs and the entire Decompress path is dead.
        use ad_core_rs::plugin::channel::{NDArrayOutput, ndarray_channel};
        use ad_core_rs::plugin::runtime::create_plugin_runtime_with_output;
        use ad_core_rs::plugin::wiring::WiringRegistry;
        use std::sync::atomic::Ordering;

        // A genuinely-compressed input array (codec = LZ4).
        let mut raw = make_u8_array(4, 4);
        raw.unique_id = 1;
        let original_data = raw.data.as_u8_slice().to_vec();
        let compressed = compress_lz4(&raw);
        assert_eq!(compressed.codec.as_ref().unwrap().name, CodecName::LZ4);
        assert_eq!(compressed.unique_id, 1);

        // Sentinel uncompressed array: even if the compressed one is dropped, this
        // reaches downstream, so a wrong first unique_id pinpoints the drop (no
        // reliance on a timeout).
        let mut sentinel = make_u8_array(4, 4);
        sentinel.unique_id = 2;

        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let (ds_sender, mut ds_rx) = ndarray_channel("DS", 10);
        let mut output = NDArrayOutput::new();
        output.add(ds_sender);
        let (handle, _jh) = create_plugin_runtime_with_output(
            "CODEC_DECOMP",
            CodecProcessor::new(CodecMode::Decompress),
            pool,
            10,
            output,
            "",
            Arc::new(WiringRegistry::new()),
        );
        let dropped = handle.array_sender().dropped_arrays_counter().clone();
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        // Fence: the write only queues the enable for the data thread.
        assert!(
            handle.wait_params_applied(std::time::Duration::from_secs(10)),
            "data thread did not apply EnableCallbacks"
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(handle.array_sender().publish(Arc::new(compressed)));
        rt.block_on(handle.array_sender().publish(Arc::new(sentinel)));

        let first = ds_rx.blocking_recv().expect("downstream array");
        assert_eq!(
            first.unique_id, 1,
            "compressed input must be decompressed and delivered, not dropped"
        );
        assert!(
            first.codec.is_none(),
            "delivered array must be decompressed (codec cleared)"
        );
        assert_eq!(first.data.as_u8_slice(), original_data.as_slice());
        assert_eq!(
            dropped.load(Ordering::Acquire),
            0,
            "compression-aware Codec must not count its compressed input as dropped"
        );
    }

    #[test]
    fn test_lz4_roundtrip_u16() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = (i * 100) as u16;
            }
        }
        let original_bytes = arr.data.as_u8_slice().to_vec();

        let compressed = compress_lz4(&arr);
        assert_eq!(compressed.codec.as_ref().unwrap().name, CodecName::LZ4);
        // The original data type is recorded structurally in the codec.
        assert_eq!(
            compressed.codec.as_ref().unwrap().original_data_type,
            NDDataType::UInt16
        );
        // No carrier attribute leaks onto the array.
        assert!(
            compressed
                .attributes
                .get("CODEC_ORIGINAL_DATA_TYPE")
                .is_none()
        );

        let decompressed = decompress_lz4(&compressed).unwrap();
        assert!(decompressed.codec.is_none());
        assert_eq!(decompressed.data.data_type(), NDDataType::UInt16);
        assert_eq!(decompressed.data.as_u8_slice(), original_bytes.as_slice());
    }

    #[test]
    fn test_lz4_roundtrip_f64() {
        let mut arr = NDArray::new(vec![NDDimension::new(16)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = i as f64 * 1.5;
            }
        }
        let original_bytes = arr.data.as_u8_slice().to_vec();

        let compressed = compress_lz4(&arr);
        let decompressed = decompress_lz4(&compressed).unwrap();
        assert_eq!(decompressed.data.data_type(), NDDataType::Float64);
        assert_eq!(decompressed.data.as_u8_slice(), original_bytes.as_slice());
    }

    #[test]
    fn test_lz4_compresses_repetitive_data() {
        // Highly repetitive data should compress well
        let mut arr = NDArray::new(
            vec![NDDimension::new(256), NDDimension::new(256)],
            NDDataType::UInt8,
        );
        // All zeros = very compressible
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for x in v.iter_mut() {
                *x = 0;
            }
        }
        let original_size = arr.data.as_u8_slice().len();

        let compressed = compress_lz4(&arr);
        let compressed_size = compressed.codec.as_ref().unwrap().compressed_size;
        assert!(
            compressed_size < original_size,
            "compressed ({}) should be smaller than original ({})",
            compressed_size,
            original_size,
        );
    }

    #[test]
    fn test_lz4_preserves_metadata() {
        let mut arr = make_u8_array(4, 4);
        arr.unique_id = 42;

        let compressed = compress_lz4(&arr);
        assert_eq!(compressed.unique_id, 42);
        assert_eq!(compressed.dims.len(), 2);
        assert_eq!(compressed.dims[0].size, 4);
        assert_eq!(compressed.dims[1].size, 4);
    }

    // ---- Bitshuffle / LZ4 (bslz4) tests ----

    #[test]
    fn test_bitshuffle_block_transpose_roundtrip() {
        // The canonical bit transpose must be its own paired inverse for a
        // block whose element count is a multiple of 8, across element sizes.
        for &(n, elem_size) in &[(16usize, 4usize), (8, 2), (256, 8), (128, 1)] {
            let input: Vec<u8> = (0..n * elem_size).map(|i| (i * 7 + 3) as u8).collect();
            let shuffled = bshuf_trans_bit_elem(&input, n, elem_size);
            assert_eq!(shuffled.len(), input.len());
            let restored = bshuf_untrans_bit_elem(&shuffled, n, elem_size);
            assert_eq!(restored, input, "elem_size {elem_size}, n {n}");
        }
    }

    #[test]
    fn test_bitshuffle_matches_c_reference_vector() {
        // Locks the on-disk byte format to the canonical bitshuffle library
        // (the one h5py / libhdf5 / C areaDetector use). The expected vector was
        // produced by compiling hdf5_plugins/BSHUF/src/bitshuffle_core.c
        // (scalar path) and running `bshuf_bitshuffle(in, out, 16, 2, 0)` on the
        // u16 ramp 0..15: bit-row 0 (LSB of each elem) packs elem k -> output
        // bit k (little-endian element order), giving 0xAA/0xCC/0xF0 for the
        // varying low nibble and 0xFF where bit 3 separates elems 8..15.
        let input: Vec<u8> = (0..16u16).flat_map(|v| v.to_le_bytes()).collect();
        let shuffled = bshuf_trans_bit_elem(&input, 16, 2);
        let mut expected = vec![0u8; 32];
        expected[..8].copy_from_slice(&[170, 170, 204, 204, 240, 240, 0, 255]);
        assert_eq!(
            shuffled, expected,
            "canonical bitshuffle transpose must match the C library bytes"
        );
    }

    #[test]
    fn test_bslz4_roundtrip_u8() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(64), NDDimension::new(64)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i % 251) as u8;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_bslz4(&arr);
        assert_eq!(compressed.codec.as_ref().unwrap().name, CodecName::BSLZ4);
        assert_ne!(compressed.data.as_u8_slice(), original.as_slice());

        let decompressed = decompress_bslz4(&compressed).unwrap();
        assert!(decompressed.codec.is_none());
        assert_eq!(decompressed.data.data_type(), NDDataType::UInt8);
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    #[test]
    fn test_bslz4_roundtrip_u16() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(100), NDDimension::new(20)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 37 % 65521) as u16;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_bslz4(&arr);
        assert_eq!(
            compressed.codec.as_ref().unwrap().original_data_type,
            NDDataType::UInt16
        );
        let decompressed = decompress_bslz4(&compressed).unwrap();
        assert_eq!(decompressed.data.data_type(), NDDataType::UInt16);
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    #[test]
    fn test_bslz4_roundtrip_f64_with_negatives() {
        let mut arr = NDArray::new(vec![NDDimension::new(73)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i as f64 - 36.0) * 2.5;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_bslz4(&arr);
        let decompressed = decompress_bslz4(&compressed).unwrap();
        assert_eq!(decompressed.data.data_type(), NDDataType::Float64);
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    #[test]
    fn test_bslz4_roundtrip_multi_block() {
        // A buffer larger than the default block size exercises the
        // per-block container framing and a trailing partial block.
        let elem_size = 4usize;
        let block = bshuf_default_block_size(elem_size);
        // 2.5 blocks worth of i32 elements.
        let count = block * 2 + block / 2 + 3;
        let mut arr = NDArray::new(vec![NDDimension::new(count)], NDDataType::Int32);
        if let NDDataBuffer::I32(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i as i32).wrapping_mul(2_654_435_761u32 as i32);
            }
        }
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_bslz4(&arr);
        let decompressed = decompress_bslz4(&compressed).unwrap();
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    #[test]
    fn test_bslz4_compresses_repetitive_data() {
        // Bitshuffle makes near-constant data extremely compressible.
        let arr = NDArray::new(
            vec![NDDimension::new(256), NDDimension::new(256)],
            NDDataType::UInt16,
        );
        let original_size = arr.data.as_u8_slice().len();
        let compressed = compress_bslz4(&arr);
        let compressed_size = compressed.codec.as_ref().unwrap().compressed_size;
        assert!(
            compressed_size < original_size,
            "bslz4 compressed ({compressed_size}) should be < original ({original_size})"
        );
    }

    #[test]
    fn test_r9_71_corrupt_bslz4_reports_cs_blosc_text() {
        // R9-71. C's decompressBSLZ4 reports "Failed to Blosc decompress"
        // (NDPluginCodec.cpp:601) — a copy-paste from decompressBlosc (:431), but
        // it is what the CodecError PV shows for a corrupt BSLZ4 frame, so the
        // port must emit it verbatim rather than the "corrected" BSLZ4 wording.
        use ad_core_rs::plugin::runtime::ParamUpdate;

        let mut arr = NDArray::new(
            vec![NDDimension::new(32), NDDimension::new(32)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 11) as u16;
            }
        }
        let pool = NDArrayPool::new(10_000_000);

        // A genuine BSLZ4 frame, then corrupt the compressed payload.
        let mut compressed = compress_bslz4(&arr);
        if let NDDataBuffer::U8(ref mut v) = compressed.data {
            for b in v.iter_mut() {
                *b = 0xFF;
            }
        }
        assert!(
            decompress_bslz4(&compressed).is_none(),
            "the corrupted frame must fail to decompress"
        );

        let mut decomp = CodecProcessor::new(CodecMode::Decompress);
        decomp.params.codec_error = Some(13);
        let result = decomp.process_array(&compressed, &pool);
        let text = result
            .param_updates
            .iter()
            .find_map(|u| match u {
                ParamUpdate::Octet {
                    reason: 13, value, ..
                } => Some(value.clone()),
                _ => None,
            })
            .expect("CodecError posted");
        assert_eq!(text, "Failed to Blosc decompress");
    }

    #[test]
    fn test_bslz4_via_processor() {
        // The CodecProcessor must round-trip through the BSLZ4 codec.
        let mut arr = NDArray::new(
            vec![NDDimension::new(32), NDDimension::new(32)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 11) as u16;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();
        let pool = NDArrayPool::new(10_000_000);

        let comp = CodecProcessor::new(CodecMode::Compress {
            codec: CodecName::BSLZ4,
            quality: 0,
        });
        let compressed = comp.process_array(&arr, &pool);
        let compressed_arr = &compressed.output_arrays[0];
        assert_eq!(
            compressed_arr.codec.as_ref().unwrap().name,
            CodecName::BSLZ4
        );

        let decomp = CodecProcessor::new(CodecMode::Decompress);
        let result = decomp.process_array(compressed_arr, &pool);
        assert_eq!(
            result.output_arrays[0].data.as_u8_slice(),
            original.as_slice()
        );
    }

    // ---- JPEG tests ----

    #[test]
    fn test_jpeg_compress_mono() {
        let arr = make_u8_array(16, 16);
        let compressed = compress_jpeg(&arr, 90).unwrap();
        assert_eq!(compressed.codec.as_ref().unwrap().name, CodecName::JPEG);
        // Compressed data should be valid JPEG (starts with SOI marker)
        let data = compressed.data.as_u8_slice();
        assert_eq!(&data[0..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn test_jpeg_compress_rgb() {
        let arr = make_rgb_array(16, 16);
        let compressed = compress_jpeg(&arr, 90).unwrap();
        assert_eq!(compressed.codec.as_ref().unwrap().name, CodecName::JPEG);
        let data = compressed.data.as_u8_slice();
        assert_eq!(&data[0..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn test_jpeg_roundtrip_mono() {
        let arr = make_u8_array(16, 16);
        let compressed = compress_jpeg(&arr, 100).unwrap();
        let decompressed = decompress_jpeg(&compressed).unwrap();
        assert!(decompressed.codec.is_none());
        assert_eq!(decompressed.dims.len(), 2);
        assert_eq!(decompressed.dims[0].size, 16); // width
        assert_eq!(decompressed.dims[1].size, 16); // height
        assert_eq!(decompressed.data.data_type(), NDDataType::UInt8);
        // JPEG is lossy, so data won't be identical, but dimensions match
        assert_eq!(decompressed.data.len(), 16 * 16);
    }

    #[test]
    fn test_jpeg_roundtrip_rgb() {
        let arr = make_rgb_array(16, 16);
        let compressed = compress_jpeg(&arr, 100).unwrap();
        let decompressed = decompress_jpeg(&compressed).unwrap();
        assert!(decompressed.codec.is_none());
        assert_eq!(decompressed.dims.len(), 3);
        assert_eq!(decompressed.dims[0].size, 3); // color
        assert_eq!(decompressed.dims[1].size, 16); // width
        assert_eq!(decompressed.dims[2].size, 16); // height
        assert_eq!(decompressed.data.len(), 3 * 16 * 16);
    }

    // ---- R8-62: JPEG compression of RGB2 / RGB3 ----

    /// The same RGB image in one of the three AD layouts. `pixel(x, y, c)` is
    /// deterministic so the three arrays hold identical pixels, only ordered
    /// differently.
    fn make_rgb_layout(mode: ad_core_rs::color::NDColorMode, w: usize, h: usize) -> NDArray {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        use ad_core_rs::color::NDColorMode;

        let pixel = |x: usize, y: usize, c: usize| ((x * 7 + y * 13 + c * 61) % 256) as u8;
        let dims = match mode {
            NDColorMode::RGB1 => vec![3, w, h],
            NDColorMode::RGB2 => vec![w, 3, h],
            NDColorMode::RGB3 => vec![w, h, 3],
            other => panic!("not an RGB layout: {other:?}"),
        };
        let mut arr = NDArray::new(
            dims.into_iter().map(NDDimension::new).collect(),
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "Color Mode",
            NDAttrSource::Driver,
            NDAttrValue::Int32(mode as i32),
        ));
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for y in 0..h {
                for x in 0..w {
                    for c in 0..3 {
                        let idx = match mode {
                            NDColorMode::RGB1 => c + x * 3 + y * w * 3,
                            NDColorMode::RGB2 => x + c * w + y * w * 3,
                            NDColorMode::RGB3 => x + y * w + c * w * h,
                            _ => unreachable!(),
                        };
                        v[idx] = pixel(x, y, c);
                    }
                }
            }
        }
        arr
    }

    #[test]
    fn test_r8_62_jpeg_compresses_rgb2_and_rgb3_as_reinterleaved_rgb() {
        // C compressJPEG walks the RGB2 (plane step sizeX*3) and RGB3 (plane step
        // sizeX) colour planes and re-interleaves each scanline before encoding
        // (NDPluginCodec.cpp:186-227), producing exactly the JPEG of the
        // equivalent RGB1 image. The port rejected both layouts outright.
        use ad_core_rs::color::NDColorMode;

        let rgb1 = make_rgb_layout(NDColorMode::RGB1, 16, 8);
        let reference = compress_jpeg(&rgb1, 90).expect("RGB1 must compress");

        for mode in [NDColorMode::RGB2, NDColorMode::RGB3] {
            let src = make_rgb_layout(mode, 16, 8);
            let out = compress_jpeg(&src, 90)
                .unwrap_or_else(|e| panic!("{mode:?} must compress, C encodes it: {e:?}"));
            assert_eq!(out.codec.as_ref().unwrap().name, CodecName::JPEG);
            assert_eq!(&out.data.as_u8_slice()[0..2], &[0xFF, 0xD8], "SOI marker");
            assert_eq!(
                out.data.as_u8_slice(),
                reference.data.as_u8_slice(),
                "{mode:?} must encode the same pixels as the RGB1 image — a wrong \
                 (or missing) scanline re-interleave changes the JPEG bytes"
            );
            // C's allocArray copies the input dimensions onto the output.
            assert_eq!(out.dims.len(), 3);
        }
    }

    #[test]
    fn test_r8_62_decompressed_jpeg_reports_rgb1_colormode() {
        // A decoded JPEG is always mono or RGB1 (C's comment at :268-272), so C overwrites the
        // ColorMode attribute on the output (:318-322). An RGB2 source's stale
        // ColorMode=3 on RGB1 data would make every downstream getInfo read the
        // planes in the wrong order.
        use ad_core_rs::color::NDColorMode;

        let src = make_rgb_layout(NDColorMode::RGB2, 16, 8);
        let compressed = compress_jpeg(&src, 90).expect("rgb2 jpeg");
        assert_eq!(
            compressed
                .attributes
                .get("ColorMode")
                .unwrap()
                .value
                .as_i64(),
            Some(NDColorMode::RGB2 as i64),
            "the compressed frame keeps the source ColorMode"
        );

        let out = decompress_jpeg(&compressed).expect("jpeg decode");
        assert_eq!(
            out.attributes.get("ColorMode").unwrap().value.as_i64(),
            Some(NDColorMode::RGB1 as i64),
            "decompressed JPEG must be reported as RGB1"
        );
        assert_eq!(out.dims[0].size, 3);
        assert_eq!(out.dims[1].size, 16);
        assert_eq!(out.dims[2].size, 8);
        assert_eq!(out.info().color_mode, NDColorMode::RGB1);

        // Mono round-trip reports Mono, not a stale colour mode.
        let mono = decompress_jpeg(&compress_jpeg(&make_u8_array(16, 16), 90).unwrap()).unwrap();
        assert_eq!(
            mono.attributes.get("ColorMode").unwrap().value.as_i64(),
            Some(NDColorMode::Mono as i64)
        );
    }

    #[test]
    fn test_r8_62_jpeg_accepts_int8_like_c() {
        // C accepts both 8-bit types (`case NDInt8: case NDUInt8:`, :135-143) and
        // encodes the raw bytes; only wider types are rejected ("JPEG only
        // supports 8-bit data").
        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::Int8,
        );
        if let NDDataBuffer::I8(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i as i32 - 32) as i8;
            }
        }
        let out = compress_jpeg(&arr, 90).expect("Int8 must compress");
        assert_eq!(&out.data.as_u8_slice()[0..2], &[0xFF, 0xD8]);
        assert_eq!(
            out.codec.as_ref().unwrap().original_data_type,
            NDDataType::Int8
        );
    }

    #[test]
    fn test_r8_62_jpeg_rejects_3d_without_an_rgb_colormode() {
        // A 3-D array whose ColorMode is Mono (the default when the attribute is
        // absent, C :117-121) is not JPEG-encodable: C's `else if` chain (:155-164)
        // matches none of RGB1/2/3, so image_width/image_height are never set, and
        // the empty image reaches libjpeg's FATAL handler (jpeg_std_error, :115 —
        // its error_exit calls exit()). It is NOT the "Unknown color mode" branch,
        // which this test used to claim: C's colorMode switch does have a
        // `case NDColorModeMono` arm (:182). The port refuses instead of aborting,
        // under C's "Error writing JPEG data" (:235).
        let arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(8),
                NDDimension::new(8),
            ],
            NDDataType::UInt8,
        );
        assert_eq!(
            compress_jpeg(&arr, 90).unwrap_err(),
            JpegCompressError::EncodeFailed,
            "3-D Mono (no ColorMode attribute) is not a JPEG-encodable layout in C"
        );
    }

    #[test]
    fn test_jpeg_rejects_non_u8() {
        // R8-74: C `:139-142` — "JPEG only supports 8-bit data".
        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        let err = compress_jpeg(&arr, 90).unwrap_err();
        assert_eq!(err, JpegCompressError::NotEightBit);
        assert_eq!(err.message(), "JPEG only supports 8-bit data");
    }

    #[test]
    fn test_jpeg_rejects_1d() {
        // R8-74: C `:165-168` — "Unsupported array structure" for ndims ∉ {2,3}.
        let arr = NDArray::new(vec![NDDimension::new(64)], NDDataType::UInt8);
        let err = compress_jpeg(&arr, 90).unwrap_err();
        assert_eq!(err, JpegCompressError::UnsupportedArrayStructure);
        assert_eq!(err.message(), "Unsupported array structure");
    }

    #[test]
    fn test_r8_74_jpeg_compress_failures_carry_the_c_error_texts() {
        // R8-74. C writes a *different* errorMessage at each rejection point and
        // the plugin copies it verbatim into the CodecError PV, so each text is
        // part of the contract. The port reported one generic "JPEG compression
        // failed" for all of them, because compress_jpeg returned a bare Option and
        // the caller had to invent the text.
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        use ad_core_rs::color::NDColorMode;

        // C :140 — dataType is not 8-bit.
        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::Float32,
        );
        assert_eq!(
            compress_jpeg(&arr, 90).unwrap_err().message(),
            "JPEG only supports 8-bit data"
        );

        // C :166 — ndims is neither 2 nor 3.
        let arr = NDArray::new(
            vec![
                NDDimension::new(2),
                NDDimension::new(2),
                NDDimension::new(2),
                NDDimension::new(2),
            ],
            NDDataType::UInt8,
        );
        assert_eq!(
            compress_jpeg(&arr, 90).unwrap_err().message(),
            "Unsupported array structure"
        );

        // C :201 — a colorMode with no arm in the switch. Bayer is 1, YUV444 is 5;
        // NDColorMode's discriminants are C's NDColorMode_t values, so the `%d`
        // must print those numbers.
        for (mode, text) in [
            (NDColorMode::Bayer, "Unknown color mode 1"),
            (NDColorMode::YUV444, "Unknown color mode 5"),
            (NDColorMode::YUV411, "Unknown color mode 7"),
        ] {
            let mut arr = NDArray::new(
                vec![NDDimension::new(8), NDDimension::new(8)],
                NDDataType::UInt8,
            );
            arr.attributes.add(NDAttribute::new_static(
                "ColorMode",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Int32(mode as i32),
            ));
            assert_eq!(compress_jpeg(&arr, 90).unwrap_err().message(), text);
        }
    }

    #[test]
    fn test_r8_74_codec_error_pv_carries_the_jpeg_text() {
        // The typed error must reach the CodecError PV, not just the return value:
        // C copies `errorMessage` into it verbatim.
        use ad_core_rs::plugin::runtime::ParamUpdate;

        let mut proc = CodecProcessor::new(CodecMode::Compress {
            codec: CodecName::JPEG,
            quality: 90,
        });
        proc.params.codec_error = Some(13);
        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        let result = proc.process_array(&arr, &NDArrayPool::new(0));
        let text = result
            .param_updates
            .iter()
            .find_map(|u| match u {
                ParamUpdate::Octet {
                    reason: 13, value, ..
                } => Some(value.clone()),
                _ => None,
            })
            .expect("CodecError posted");
        assert_eq!(text, "JPEG only supports 8-bit data");
    }

    #[test]
    fn test_jpeg_quality_affects_size() {
        let arr = make_u8_array(64, 64);
        let high = compress_jpeg(&arr, 95).unwrap();
        let low = compress_jpeg(&arr, 10).unwrap();
        let high_size = high.codec.as_ref().unwrap().compressed_size;
        let low_size = low.codec.as_ref().unwrap().compressed_size;
        assert!(
            high_size > low_size,
            "high quality ({}) should produce larger output than low quality ({})",
            high_size,
            low_size,
        );
    }

    // ---- Zlib tests ----

    #[test]
    fn test_zlib_roundtrip_u8() {
        let arr = make_u8_array(8, 8);
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_zlib(&arr);
        assert_eq!(compressed.codec.as_ref().unwrap().name, CodecName::Zlib);
        assert_ne!(compressed.data.as_u8_slice(), original.as_slice());

        let decompressed = decompress_zlib(&compressed).unwrap();
        assert!(decompressed.codec.is_none());
        assert_eq!(decompressed.data.data_type(), NDDataType::UInt8);
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    #[test]
    fn test_zlib_roundtrip_u16() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(16), NDDimension::new(16)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 257 % 65521) as u16;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_zlib(&arr);
        assert_eq!(
            compressed.codec.as_ref().unwrap().original_data_type,
            NDDataType::UInt16
        );

        let decompressed = decompress_zlib(&compressed).unwrap();
        assert_eq!(decompressed.data.data_type(), NDDataType::UInt16);
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    #[test]
    fn test_zlib_roundtrip_f64_with_negatives() {
        let mut arr = NDArray::new(vec![NDDimension::new(64)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i as f64 - 32.0) * 3.25;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_zlib(&arr);
        let decompressed = decompress_zlib(&compressed).unwrap();
        assert_eq!(decompressed.data.data_type(), NDDataType::Float64);
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    #[test]
    fn test_zlib_compresses_repetitive_data() {
        let arr = NDArray::new(
            vec![NDDimension::new(256), NDDimension::new(256)],
            NDDataType::UInt8,
        );
        let original_size = arr.data.as_u8_slice().len();
        let compressed = compress_zlib(&arr);
        let compressed_size = compressed.codec.as_ref().unwrap().compressed_size;
        assert!(
            compressed_size < original_size,
            "zlib compressed ({compressed_size}) should be < original ({original_size})"
        );
    }

    #[test]
    fn test_zlib_via_processor() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(32), NDDimension::new(32)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 13) as u16;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();
        let pool = NDArrayPool::new(10_000_000);

        let comp = CodecProcessor::new(CodecMode::Compress {
            codec: CodecName::Zlib,
            quality: 0,
        });
        let compressed = comp.process_array(&arr, &pool);
        let compressed_arr = &compressed.output_arrays[0];
        assert_eq!(compressed_arr.codec.as_ref().unwrap().name, CodecName::Zlib);

        let decomp = CodecProcessor::new(CodecMode::Decompress);
        let result = decomp.process_array(compressed_arr, &pool);
        assert_eq!(
            result.output_arrays[0].data.as_u8_slice(),
            original.as_slice()
        );
    }

    // ---- LZ4HDF5 tests ----

    #[test]
    fn test_lz4hdf5_roundtrip_u8() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(64), NDDimension::new(64)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i % 251) as u8;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_lz4hdf5(&arr);
        assert_eq!(compressed.codec.as_ref().unwrap().name, CodecName::LZ4HDF5);
        assert_ne!(compressed.data.as_u8_slice(), original.as_slice());

        let decompressed = decompress_lz4hdf5(&compressed).unwrap();
        assert!(decompressed.codec.is_none());
        assert_eq!(decompressed.data.data_type(), NDDataType::UInt8);
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    #[test]
    fn test_lz4hdf5_roundtrip_u16() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(80), NDDimension::new(40)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 37 % 65521) as u16;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_lz4hdf5(&arr);
        assert_eq!(
            compressed.codec.as_ref().unwrap().original_data_type,
            NDDataType::UInt16
        );

        let decompressed = decompress_lz4hdf5(&compressed).unwrap();
        assert_eq!(decompressed.data.data_type(), NDDataType::UInt16);
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    #[test]
    fn test_lz4hdf5_roundtrip_f64_with_negatives() {
        let mut arr = NDArray::new(vec![NDDimension::new(97)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i as f64 - 48.0) * 1.75;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_lz4hdf5(&arr);
        let decompressed = decompress_lz4hdf5(&compressed).unwrap();
        assert_eq!(decompressed.data.data_type(), NDDataType::Float64);
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    #[test]
    fn test_lz4hdf5_multi_block_roundtrip() {
        // A buffer larger than the default block size exercises the per-block
        // container framing and a trailing partial block.
        let block = LZ4HDF5_DEFAULT_BLOCK_SIZE;
        let count = block * 2 + block / 3 + 7; // 2.33 blocks of u8.
        let mut arr = NDArray::new(vec![NDDimension::new(count)], NDDataType::UInt8);
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i.wrapping_mul(2_654_435_761) % 251) as u8;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();

        let compressed = compress_lz4hdf5(&arr);
        let decompressed = decompress_lz4hdf5(&compressed).unwrap();
        assert_eq!(decompressed.data.as_u8_slice(), original.as_slice());
    }

    #[test]
    fn test_lz4hdf5_compresses_repetitive_data() {
        let arr = NDArray::new(
            vec![NDDimension::new(256), NDDimension::new(256)],
            NDDataType::UInt16,
        );
        let original_size = arr.data.as_u8_slice().len();
        let compressed = compress_lz4hdf5(&arr);
        let compressed_size = compressed.codec.as_ref().unwrap().compressed_size;
        assert!(
            compressed_size < original_size,
            "lz4hdf5 compressed ({compressed_size}) should be < original ({original_size})"
        );
    }

    #[test]
    fn test_lz4hdf5_via_processor() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(48), NDDimension::new(48)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for (i, x) in v.iter_mut().enumerate() {
                *x = (i * 7) as u16;
            }
        }
        let original = arr.data.as_u8_slice().to_vec();
        let pool = NDArrayPool::new(10_000_000);

        let comp = CodecProcessor::new(CodecMode::Compress {
            codec: CodecName::LZ4HDF5,
            quality: 0,
        });
        let compressed = comp.process_array(&arr, &pool);
        let compressed_arr = &compressed.output_arrays[0];
        assert_eq!(
            compressed_arr.codec.as_ref().unwrap().name,
            CodecName::LZ4HDF5
        );

        let decomp = CodecProcessor::new(CodecMode::Decompress);
        let result = decomp.process_array(compressed_arr, &pool);
        assert_eq!(
            result.output_arrays[0].data.as_u8_slice(),
            original.as_slice()
        );
    }

    // ---- COMPRESSOR ordinal mapping ----

    #[test]
    fn test_compressor_ordinal_mapping() {
        // C `NDCodecCompressor_t` (Codec.h:12-18): 0=None, 1=JPEG, 2=Blosc,
        // 3=LZ4, 4=BSLZ4. Rust-only zlib/lz4hdf5 follow at 5/6. Selecting a
        // compressor by its C ordinal must pick the matching `CodecName`.
        use ad_core_rs::plugin::runtime::{ParamChangeValue, PluginParamSnapshot};

        let cases = [
            (0i32, CodecName::None),
            (1, CodecName::JPEG),
            (2, CodecName::Blosc),
            (3, CodecName::LZ4),
            (4, CodecName::BSLZ4),
            (5, CodecName::Zlib),
            (6, CodecName::LZ4HDF5),
        ];

        for (ordinal, expected) in cases {
            let mut proc = CodecProcessor::new(CodecMode::Compress {
                codec: CodecName::LZ4,
                quality: 85,
            });
            // The compressor param index is otherwise discovered via
            // `register_params`; set it directly for the unit test.
            proc.params.compressor = Some(0);
            let snapshot = PluginParamSnapshot {
                enable_callbacks: true,
                reason: 0,
                addr: 0,
                value: ParamChangeValue::Int32(ordinal),
            };
            proc.on_param_change(0, &snapshot);
            match proc.state.lock().mode {
                CodecMode::Compress { codec, .. } => assert_eq!(
                    codec, expected,
                    "ordinal {ordinal} should select {expected:?}"
                ),
                other => panic!("expected Compress mode, got {other:?}"),
            }
        }
    }

    // ---- Decompress wrong codec ----

    #[test]
    fn test_decompress_wrong_codec() {
        let arr = make_u8_array(4, 4);
        assert!(decompress_lz4(&arr).is_none());
        assert!(decompress_jpeg(&arr).is_none());
        assert!(decompress_zlib(&arr).is_none());
        assert!(decompress_lz4hdf5(&arr).is_none());
    }

    // ---- CodecProcessor tests ----

    #[test]
    fn test_processor_lz4_compress() {
        let pool = NDArrayPool::new(1_000_000);
        let proc = CodecProcessor::new(CodecMode::Compress {
            codec: CodecName::LZ4,
            quality: 0,
        });
        let arr = make_u8_array(32, 32);
        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(
            result.output_arrays[0].codec.as_ref().unwrap().name,
            CodecName::LZ4
        );
        assert!(proc.compression_ratio() >= 1.0);
    }

    #[test]
    fn test_processor_jpeg_compress() {
        let pool = NDArrayPool::new(1_000_000);
        let proc = CodecProcessor::new(CodecMode::Compress {
            codec: CodecName::JPEG,
            quality: 80,
        });
        let arr = make_u8_array(16, 16);
        let result = proc.process_array(&arr, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(
            result.output_arrays[0].codec.as_ref().unwrap().name,
            CodecName::JPEG
        );
    }

    #[test]
    fn test_processor_decompress_auto_lz4() {
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_u8_array(16, 16);
        let compressed = compress_lz4(&arr);

        let proc = CodecProcessor::new(CodecMode::Decompress);
        let result = proc.process_array(&compressed, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert!(result.output_arrays[0].codec.is_none());
        assert_eq!(
            result.output_arrays[0].data.as_u8_slice(),
            arr.data.as_u8_slice()
        );
        assert!(proc.compression_ratio() > 0.0);
    }

    #[test]
    fn test_processor_decompress_auto_jpeg() {
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_u8_array(16, 16);
        let compressed = compress_jpeg(&arr, 90).unwrap();

        let proc = CodecProcessor::new(CodecMode::Decompress);
        let result = proc.process_array(&compressed, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert!(result.output_arrays[0].codec.is_none());
    }

    #[test]
    fn test_processor_decompress_no_codec() {
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_u8_array(8, 8);
        let proc = CodecProcessor::new(CodecMode::Decompress);
        let result = proc.process_array(&arr, &pool);
        // C++: on failure, pass through original array unchanged
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(proc.compression_ratio(), 1.0);
    }

    // ---- R8-61: pass-through vs failure on the Codec plugin's exits ----

    /// Param indices used by the R8-61 tests; `register_params` normally
    /// discovers them from the port, which a unit test has no need to build.
    fn processor_with_params(mode: CodecMode) -> CodecProcessor {
        let mut proc = CodecProcessor::new(mode);
        proc.params.comp_factor = Some(10);
        proc.params.compressor = Some(11);
        proc.params.codec_status = Some(12);
        proc.params.codec_error = Some(13);
        proc
    }

    fn int32_update(updates: &[ParamUpdate], reason: usize) -> Option<i32> {
        updates.iter().find_map(|u| match u {
            ParamUpdate::Int32 {
                reason: r, value, ..
            } if *r == reason => Some(*value),
            _ => None,
        })
    }

    fn octet_update(updates: &[ParamUpdate], reason: usize) -> Option<String> {
        updates.iter().find_map(|u| match u {
            ParamUpdate::Octet {
                reason: r, value, ..
            } if *r == reason => Some(value.clone()),
            _ => None,
        })
    }

    #[test]
    fn test_r8_61_decompress_uncompressed_input_is_success_passthrough() {
        // C NDPluginCodec.cpp:732-735 — Decompress mode on an array with an empty
        // codec: result = pArray, COMPRESSOR = NDCODEC_NONE, codecStatus stays
        // SUCCESS and no error string is set. The port reported CodecStatus=1 +
        // "codec operation failed or unsupported" and never wrote COMPRESSOR.
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_u8_array(8, 8);
        let proc = processor_with_params(CodecMode::Decompress);
        let result = proc.process_array(&arr, &pool);

        assert_eq!(
            int32_update(&result.param_updates, 12),
            Some(0),
            "CodecStatus must stay SUCCESS on an uncompressed input"
        );
        assert_eq!(
            octet_update(&result.param_updates, 13),
            Some(String::new()),
            "no error string on a pass-through"
        );
        assert_eq!(
            int32_update(&result.param_updates, 11),
            Some(0),
            "COMPRESSOR must be set to NDCODEC_NONE"
        );
        assert_eq!(
            result.output_arrays[0].data.as_u8_slice(),
            arr.data.as_u8_slice(),
            "the input array is passed through unchanged"
        );
        assert_eq!(proc.compression_ratio(), 1.0);
    }

    #[test]
    fn test_r8_61_decompress_reports_compressor_of_the_input_codec() {
        // C sets NDCodecCompressor on every decompress branch (:739/:747/:752/
        // :757) from the codec found on the input; the port never wrote it.
        let pool = NDArrayPool::new(1_000_000);
        let src = make_u8_array(16, 16);
        for (codec, ordinal) in [
            (compress_lz4(&src), 3),
            (compress_blosc(&src, &BloscConfig::default()), 2),
            (compress_bslz4(&src), 4),
            (compress_jpeg(&src, 90).expect("jpeg"), 1),
        ] {
            let proc = processor_with_params(CodecMode::Decompress);
            let result = proc.process_array(&codec, &pool);
            assert_eq!(
                int32_update(&result.param_updates, 11),
                Some(ordinal),
                "COMPRESSOR must report the input codec's C ordinal"
            );
            assert_eq!(
                int32_update(&result.param_updates, 12),
                Some(0),
                "a successful decompress is SUCCESS"
            );
        }
    }

    #[test]
    fn test_r8_61_compress_with_compressor_none_is_success_passthrough() {
        // C :671 gates the already-compressed check on `algo`, and :680-683 maps
        // `case NDCODEC_NONE: default:` to `result = pArray` — a COMPRESSOR=None
        // compress plugin is a SUCCESS pass-through, not a codec failure. The
        // port's catch-all `Compress { .. } => None` sent it to the error branch.
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_u8_array(8, 8);
        let proc = processor_with_params(CodecMode::Compress {
            codec: CodecName::None,
            quality: 85,
        });
        let result = proc.process_array(&arr, &pool);

        assert_eq!(int32_update(&result.param_updates, 12), Some(0));
        assert_eq!(octet_update(&result.param_updates, 13), Some(String::new()));
        assert_eq!(
            int32_update(&result.param_updates, 11),
            None,
            "compress mode must not overwrite the operator's COMPRESSOR selection"
        );
        assert!(result.output_arrays[0].codec.is_none());
        assert_eq!(
            result.output_arrays[0].data.as_u8_slice(),
            arr.data.as_u8_slice()
        );
    }

    #[test]
    fn test_r8_61_genuine_decompress_failure_still_reports_an_error() {
        // The pass-through paths must not swallow real failures: a truncated LZ4
        // payload still reports a non-zero CodecStatus + an error string, and
        // still republishes the input (C `finish:` block, :770-776).
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_u8_array(16, 16);
        let mut corrupted = compress_lz4(&arr);
        if let NDDataBuffer::U8(ref mut v) = corrupted.data {
            v.truncate(3);
        }
        let proc = processor_with_params(CodecMode::Decompress);
        let result = proc.process_array(&corrupted, &pool);

        assert_ne!(
            int32_update(&result.param_updates, 12),
            Some(0),
            "a failed decompress must not report SUCCESS"
        );
        assert_eq!(
            octet_update(&result.param_updates, 13),
            Some("Failed to LZ4 decompress".to_string())
        );
        assert_eq!(int32_update(&result.param_updates, 11), Some(3));
        assert_eq!(
            result.output_arrays[0].data.as_u8_slice(),
            corrupted.data.as_u8_slice(),
            "the input array is republished on failure"
        );
    }

    // ---- R8-63: the three-level CodecStatus contract ----

    #[test]
    fn test_r8_63_status_levels_match_c() {
        // C NDCodecStatus_t (NDPluginCodec.h:42-46): SUCCESS=0, WARNING=1,
        // ERROR=2. These are the values every CodecStatus PV client reads.
        assert_eq!(CodecStatus::Success.as_i32(), 0);
        assert_eq!(CodecStatus::Warning.as_i32(), 1);
        assert_eq!(CodecStatus::Error.as_i32(), 2);
    }

    #[test]
    fn test_r8_63_already_compressed_is_a_warning_not_success() {
        // C `NDPluginCodec.cpp:671-676` — compressing an already-compressed array is benign
        // but not silent: errorMessage "Array already compressed", codecStatus WARNING, and
        // the input passes through. The port reported SUCCESS with no error.
        let pool = NDArrayPool::new(1_000_000);
        let compressed = compress_lz4(&make_u8_array(16, 16));
        let proc = processor_with_params(CodecMode::Compress {
            codec: CodecName::Zlib,
            quality: 85,
        });
        let result = proc.process_array(&compressed, &pool);

        assert_eq!(
            int32_update(&result.param_updates, 12),
            Some(CodecStatus::Warning.as_i32()),
            "already-compressed input must report WARNING(1)"
        );
        assert_eq!(
            octet_update(&result.param_updates, 13),
            Some("Array already compressed".to_string())
        );
        // The frame still flows on, still LZ4-compressed.
        assert_eq!(
            result.output_arrays[0].codec.as_ref().unwrap().name,
            CodecName::LZ4
        );
    }

    #[test]
    fn test_r8_63_genuine_failures_are_error_not_warning() {
        // C `NDPluginCodec.cpp` reports ERROR(2) for real failures: a JPEG-unsupported input
        // (`:141`/`:167`/`:202`/`:252`) and a codec that fails to decode (`:279`, `:760`).
        // The port hardcoded 1 (WARNING) on every failure, making the two levels
        // indistinguishable.
        let pool = NDArrayPool::new(1_000_000);

        // Compress: UInt16 is not JPEG-encodable ("JPEG only supports 8-bit data").
        let wide = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        let proc = processor_with_params(CodecMode::Compress {
            codec: CodecName::JPEG,
            quality: 85,
        });
        let result = proc.process_array(&wide, &pool);
        assert_eq!(
            int32_update(&result.param_updates, 12),
            Some(CodecStatus::Error.as_i32()),
            "an unsupported JPEG input is an ERROR"
        );

        // Decompress: a truncated payload is a decoder failure.
        let mut corrupted = compress_lz4(&make_u8_array(16, 16));
        if let NDDataBuffer::U8(ref mut v) = corrupted.data {
            v.truncate(3);
        }
        let proc = processor_with_params(CodecMode::Decompress);
        let result = proc.process_array(&corrupted, &pool);
        assert_eq!(
            int32_update(&result.param_updates, 12),
            Some(CodecStatus::Error.as_i32()),
            "a failed decompress is an ERROR"
        );
    }

    #[test]
    fn test_r8_63_successful_and_passthrough_paths_report_success() {
        // The other two levels must stay at SUCCESS(0): a real compression, and the
        // pass-through exits (C `NDPluginCodec.cpp:659`, `:680-683`, `:732-735`).
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_u8_array(16, 16);

        let proc = processor_with_params(CodecMode::Compress {
            codec: CodecName::LZ4,
            quality: 85,
        });
        let compressed = proc.process_array(&arr, &pool);
        assert_eq!(
            int32_update(&compressed.param_updates, 12),
            Some(CodecStatus::Success.as_i32())
        );

        let proc = processor_with_params(CodecMode::Decompress);
        let passthrough = proc.process_array(&arr, &pool);
        assert_eq!(
            int32_update(&passthrough.param_updates, 12),
            Some(CodecStatus::Success.as_i32())
        );
    }

    #[test]
    fn test_processor_compression_ratio() {
        let pool = NDArrayPool::new(1_000_000);
        // Create highly compressible data (all zeros)
        let mut arr = NDArray::new(
            vec![NDDimension::new(128), NDDimension::new(128)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for x in v.iter_mut() {
                *x = 0;
            }
        }

        let proc = CodecProcessor::new(CodecMode::Compress {
            codec: CodecName::LZ4,
            quality: 0,
        });
        let _ = proc.process_array(&arr, &pool);
        let ratio = proc.compression_ratio();
        assert!(
            ratio > 2.0,
            "all-zeros 128x128 should compress at least 2x, got {}",
            ratio,
        );
    }

    #[test]
    fn test_processor_plugin_type() {
        let proc = CodecProcessor::new(CodecMode::Decompress);
        assert_eq!(proc.plugin_type(), "NDPluginCodec");
    }

    // ---- buffer_from_bytes tests ----

    #[test]
    fn test_buffer_from_bytes_u8() {
        let data = vec![1u8, 2, 3, 4];
        let buf = buffer_from_bytes(&data, NDDataType::UInt8).unwrap();
        assert_eq!(buf.data_type(), NDDataType::UInt8);
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.as_u8_slice(), &[1, 2, 3, 4]);
    }

    #[test]
    fn test_buffer_from_bytes_u16() {
        let original = vec![1000u16, 2000, 3000];
        let bytes: Vec<u8> = original.iter().flat_map(|v| v.to_ne_bytes()).collect();
        let buf = buffer_from_bytes(&bytes, NDDataType::UInt16).unwrap();
        assert_eq!(buf.data_type(), NDDataType::UInt16);
        assert_eq!(buf.len(), 3);
        if let NDDataBuffer::U16(v) = buf {
            assert_eq!(v, original);
        } else {
            panic!("wrong buffer type");
        }
    }

    #[test]
    fn test_buffer_from_bytes_bad_alignment() {
        // 3 bytes can't form a u16 array
        let data = vec![0u8; 3];
        assert!(buffer_from_bytes(&data, NDDataType::UInt16).is_none());
    }

    #[test]
    fn test_buffer_from_bytes_f64_roundtrip() {
        let original = vec![1.5f64, -2.7, 3.14159];
        let bytes: Vec<u8> = original.iter().flat_map(|v| v.to_ne_bytes()).collect();
        let buf = buffer_from_bytes(&bytes, NDDataType::Float64).unwrap();
        if let NDDataBuffer::F64(v) = buf {
            assert_eq!(v, original);
        } else {
            panic!("wrong buffer type");
        }
    }
}
