use std::io::{Read, Write};
use std::sync::Arc;

use ad_core_rs::codec::{Codec, CodecName};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ParamUpdate, ProcessResult};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use lz4_flex::block::{compress, decompress};
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
fn buffer_from_bytes(bytes: &[u8], data_type: NDDataType) -> Option<NDDataBuffer> {
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
/// Mirrors `bshuf_default_block_size` (bitshuffle_core.c:2009): `TARGET /
/// elem_size` rounded down to a multiple of `BSHUF_BLOCKED_MULT`, floored at
/// `BSHUF_MIN_RECOMMEND_BLOCK`. This value must stay stable across versions or
/// previously-encoded streams become undecodable.
pub(crate) fn bshuf_default_block_size(elem_size: usize) -> usize {
    let bs = BSHUF_TARGET_BLOCK_SIZE_B / elem_size.max(1);
    let bs = (bs / BSHUF_BLOCKED_MULT) * BSHUF_BLOCKED_MULT;
    bs.max(BSHUF_MIN_RECOMMEND_BLOCK)
}

/// 8x8 bit-matrix transpose of a quadword, little-endian convention
/// (library macro `TRANS_BIT_8X8`, bitshuffle_core.c:110).
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
/// bitshuffle_core.c:166). `size` is a multiple of 8 for every shuffled block.
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
/// bitshuffle_core.c:205, little-endian path).
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
/// `bshuf_trans_bit_elem_scal` (bitshuffle_core.c:280): byte transpose, then
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
/// `bshuf_trans_byte_bitrow_scal`, bitshuffle_core.c:306).
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
/// `bshuf_shuffle_bit_eightelem_scal`, bitshuffle_core.c:331, LE path).
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
/// (bitshuffle_core.c:373).
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
/// (library `bshuf_compress_lz4_block`, bitshuffle.c:34). `size` is a multiple
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
/// it (library `bshuf_decompress_lz4_block`, bitshuffle.c:82). Returns the
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
/// `bshuf_compress_lz4` emits it (bitshuffle.c:237, blocked via
/// `bshuf_blocked_wrap_fun`, bitshuffle_core.c:1852): every full block plus one
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
/// (bitshuffle.c:244). The uncompressed element count comes from the preserved
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
/// Only supports UInt8 data. Handles:
/// - 2D arrays (mono/grayscale)
/// - 3D arrays with dims\[0\]=3 (RGB1 interleaved)
///
/// Returns `None` if the data type is not UInt8 or the layout is unsupported.
pub fn compress_jpeg(src: &NDArray, quality: u8) -> Option<NDArray> {
    if src.data.data_type() != NDDataType::UInt8 {
        return None;
    }

    let raw = src.data.as_u8_slice();
    let info = src.info();

    // JPEG dimensions must fit in u16
    if info.x_size > u16::MAX as usize || info.y_size > u16::MAX as usize {
        return None;
    }

    let (width, height, color_type) = match src.dims.len() {
        2 => {
            // Mono: dims = [x, y]
            (
                info.x_size as u16,
                info.y_size as u16,
                jpeg_encoder::ColorType::Luma,
            )
        }
        3 if src.dims[0].size == 3 => {
            // RGB1: dims = [3, x, y], pixel-interleaved
            (
                info.x_size as u16,
                info.y_size as u16,
                jpeg_encoder::ColorType::Rgb,
            )
        }
        _ => return None,
    };

    let mut jpeg_buf = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut jpeg_buf, quality);
    if encoder.encode(raw, width, height, color_type).is_err() {
        return None;
    }

    let compressed_size = jpeg_buf.len();
    let original_size = raw.len();

    let mut arr = src.clone();
    arr.data = NDDataBuffer::U8(jpeg_buf);
    arr.codec = Some(Codec {
        name: CodecName::JPEG,
        compressed_size,
        level: 0,
        shuffle: 0,
        compressor: 0,
        // JPEG input is constrained to UInt8 above; record the source type so
        // the codec carries the original element type uniformly (C
        // `NDArray::dataType`, NDPluginCodec.cpp:35-36).
        original_data_type: src.data.data_type(),
    });

    tracing::debug!(
        original_size,
        compressed_size,
        ratio = original_size as f64 / compressed_size.max(1) as f64,
        "JPEG compress (quality={})",
        quality,
    );

    Some(arr)
}

/// Decompress a JPEG-compressed NDArray.
///
/// Uses jpeg-decoder to decode the JPEG data back to pixel data.
/// Reconstructs proper dimensions and color layout (mono or RGB1).
///
/// Returns `None` if the codec is not JPEG or decoding fails.
pub fn decompress_jpeg(src: &NDArray) -> Option<NDArray> {
    if src.codec.as_ref().map(|c| c.name) != Some(CodecName::JPEG) {
        return None;
    }

    let compressed = src.data.as_u8_slice();
    let mut decoder = jpeg_decoder::Decoder::new(compressed);
    let pixels = decoder.decode().ok()?;
    let metadata = decoder.info()?;

    let width = metadata.width as usize;
    let height = metadata.height as usize;

    let dims = match metadata.pixel_format {
        jpeg_decoder::PixelFormat::L8 => {
            // Grayscale
            vec![NDDimension::new(width), NDDimension::new(height)]
        }
        jpeg_decoder::PixelFormat::RGB24 => {
            // RGB1 interleaved
            vec![
                NDDimension::new(3),
                NDDimension::new(width),
                NDDimension::new(height),
            ]
        }
        _ => return None,
    };

    let mut arr = src.clone();
    arr.dims = dims;
    arr.data = NDDataBuffer::U8(pixels);
    arr.codec = None;

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

pub struct CodecProcessor {
    mode: CodecMode,
    compression_ratio: f64,
    jpeg_quality: u8,
    blosc_config: BloscConfig,
    params: CodecParamIndices,
}

impl CodecProcessor {
    pub fn new(mode: CodecMode) -> Self {
        let quality = match mode {
            CodecMode::Compress { quality, .. } => quality,
            _ => 85,
        };
        Self {
            mode,
            compression_ratio: 1.0,
            jpeg_quality: quality,
            blosc_config: BloscConfig::default(),
            params: CodecParamIndices::default(),
        }
    }

    /// Last computed compression ratio (original_size / compressed_size).
    /// Returns 1.0 if no compression has been performed yet or on decompression.
    pub fn compression_ratio(&self) -> f64 {
        self.compression_ratio
    }
}

impl NDPluginProcess for CodecProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let original_bytes = array.data.as_u8_slice().len();

        let result = match self.mode {
            CodecMode::Compress { .. } if array.codec.is_some() => {
                // Already compressed — pass through unchanged
                Some(array.clone())
            }
            CodecMode::Compress {
                codec: CodecName::LZ4,
                ..
            } => Some(compress_lz4(array)),
            CodecMode::Compress {
                codec: CodecName::JPEG,
                ..
            } => compress_jpeg(array, self.jpeg_quality),
            CodecMode::Compress {
                codec: CodecName::Zlib,
                ..
            } => Some(compress_zlib(array)),
            CodecMode::Compress {
                codec: CodecName::Blosc,
                ..
            } => Some(compress_blosc(array, &self.blosc_config)),
            CodecMode::Compress {
                codec: CodecName::LZ4HDF5,
                ..
            } => Some(compress_lz4hdf5(array)),
            CodecMode::Compress {
                codec: CodecName::BSLZ4,
                ..
            } => Some(compress_bslz4(array)),
            CodecMode::Compress { .. } => None,
            CodecMode::Decompress => match array.codec.as_ref().map(|c| c.name) {
                Some(CodecName::LZ4) => decompress_lz4(array),
                Some(CodecName::JPEG) => decompress_jpeg(array),
                Some(CodecName::Zlib) => decompress_zlib(array),
                Some(CodecName::Blosc) => decompress_blosc(array),
                Some(CodecName::LZ4HDF5) => decompress_lz4hdf5(array),
                Some(CodecName::BSLZ4) => decompress_bslz4(array),
                _ => None,
            },
        };

        let mut updates = Vec::new();

        match result {
            Some(ref out) => {
                let output_bytes = out.data.as_u8_slice().len();
                match self.mode {
                    CodecMode::Compress { .. } => {
                        self.compression_ratio = original_bytes as f64 / output_bytes.max(1) as f64;
                    }
                    CodecMode::Decompress => {
                        self.compression_ratio = output_bytes as f64 / original_bytes.max(1) as f64;
                    }
                }
                if let Some(idx) = self.params.comp_factor {
                    updates.push(ParamUpdate::float64(idx, self.compression_ratio));
                }
                if let Some(idx) = self.params.codec_status {
                    updates.push(ParamUpdate::int32(idx, 0)); // Success
                }
                if let Some(idx) = self.params.codec_error {
                    updates.push(ParamUpdate::Octet {
                        reason: idx,
                        addr: 0,
                        value: String::new(),
                    });
                }
                let mut r = ProcessResult::arrays(vec![Arc::new(out.clone())]);
                r.param_updates = updates;
                r
            }
            None => {
                // C++: on failure, pass through the original array unchanged
                self.compression_ratio = 1.0;
                if let Some(idx) = self.params.comp_factor {
                    updates.push(ParamUpdate::float64(idx, 1.0));
                }
                if let Some(idx) = self.params.codec_status {
                    updates.push(ParamUpdate::int32(idx, 1)); // Error
                }
                if let Some(idx) = self.params.codec_error {
                    updates.push(ParamUpdate::Octet {
                        reason: idx,
                        addr: 0,
                        value: "codec operation failed or unsupported".to_string(),
                    });
                }
                let mut r = ProcessResult::arrays(vec![Arc::new(array.clone())]);
                r.param_updates = updates;
                r
            }
        }
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
        &mut self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        if Some(reason) == self.params.mode {
            let v = params.value.as_i32();
            if v == 0 {
                // Compress — keep current codec
                let codec = match self.mode {
                    CodecMode::Compress { codec, .. } => codec,
                    _ => CodecName::LZ4,
                };
                self.mode = CodecMode::Compress {
                    codec,
                    quality: self.jpeg_quality,
                };
            } else {
                self.mode = CodecMode::Decompress;
            }
        } else if Some(reason) == self.params.compressor {
            // C `NDCodecCompressor_t` (Codec.h:12-18): NONE=0, JPEG=1,
            // BLOSC=2, LZ4=3, BSLZ4=4. The Rust-only zlib/lz4hdf5 codecs
            // (ADP-26 sign-off) take ordinals after the C set so they never
            // shadow a C ordinal — COMPRESSOR=2 must select Blosc as in C.
            let codec = match params.value.as_i32() {
                0 => CodecName::None,
                1 => CodecName::JPEG,
                2 => CodecName::Blosc,
                3 => CodecName::LZ4,
                4 => CodecName::BSLZ4,
                5 => CodecName::Zlib,
                6 => CodecName::LZ4HDF5,
                _ => CodecName::None,
            };
            if let CodecMode::Compress { .. } = self.mode {
                self.mode = CodecMode::Compress {
                    codec,
                    quality: self.jpeg_quality,
                };
            }
        } else if Some(reason) == self.params.jpeg_quality {
            self.jpeg_quality = params.value.as_i32().clamp(1, 100) as u8;
            if let CodecMode::Compress { codec, .. } = self.mode {
                self.mode = CodecMode::Compress {
                    codec,
                    quality: self.jpeg_quality,
                };
            }
        } else if Some(reason) == self.params.blosc_compressor {
            self.blosc_config.compressor = params.value.as_i32().max(0) as u32;
        } else if Some(reason) == self.params.blosc_clevel {
            self.blosc_config.clevel = params.value.as_i32().clamp(0, 9) as u32;
        } else if Some(reason) == self.params.blosc_shuffle {
            self.blosc_config.shuffle = params.value.as_i32().max(0) as u32;
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
        std::thread::sleep(std::time::Duration::from_millis(10));

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

        let mut comp = CodecProcessor::new(CodecMode::Compress {
            codec: CodecName::BSLZ4,
            quality: 0,
        });
        let compressed = comp.process_array(&arr, &pool);
        let compressed_arr = &compressed.output_arrays[0];
        assert_eq!(
            compressed_arr.codec.as_ref().unwrap().name,
            CodecName::BSLZ4
        );

        let mut decomp = CodecProcessor::new(CodecMode::Decompress);
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

    #[test]
    fn test_jpeg_rejects_non_u8() {
        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        assert!(compress_jpeg(&arr, 90).is_none());
    }

    #[test]
    fn test_jpeg_rejects_1d() {
        let arr = NDArray::new(vec![NDDimension::new(64)], NDDataType::UInt8);
        assert!(compress_jpeg(&arr, 90).is_none());
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

        let mut comp = CodecProcessor::new(CodecMode::Compress {
            codec: CodecName::Zlib,
            quality: 0,
        });
        let compressed = comp.process_array(&arr, &pool);
        let compressed_arr = &compressed.output_arrays[0];
        assert_eq!(compressed_arr.codec.as_ref().unwrap().name, CodecName::Zlib);

        let mut decomp = CodecProcessor::new(CodecMode::Decompress);
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

        let mut comp = CodecProcessor::new(CodecMode::Compress {
            codec: CodecName::LZ4HDF5,
            quality: 0,
        });
        let compressed = comp.process_array(&arr, &pool);
        let compressed_arr = &compressed.output_arrays[0];
        assert_eq!(
            compressed_arr.codec.as_ref().unwrap().name,
            CodecName::LZ4HDF5
        );

        let mut decomp = CodecProcessor::new(CodecMode::Decompress);
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
            match proc.mode {
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
        let mut proc = CodecProcessor::new(CodecMode::Compress {
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
        let mut proc = CodecProcessor::new(CodecMode::Compress {
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

        let mut proc = CodecProcessor::new(CodecMode::Decompress);
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

        let mut proc = CodecProcessor::new(CodecMode::Decompress);
        let result = proc.process_array(&compressed, &pool);
        assert_eq!(result.output_arrays.len(), 1);
        assert!(result.output_arrays[0].codec.is_none());
    }

    #[test]
    fn test_processor_decompress_no_codec() {
        let pool = NDArrayPool::new(1_000_000);
        let arr = make_u8_array(8, 8);
        let mut proc = CodecProcessor::new(CodecMode::Decompress);
        let result = proc.process_array(&arr, &pool);
        // C++: on failure, pass through original array unchanged
        assert_eq!(result.output_arrays.len(), 1);
        assert_eq!(proc.compression_ratio(), 1.0);
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

        let mut proc = CodecProcessor::new(CodecMode::Compress {
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
