use std::sync::Arc;

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
use ad_core_rs::codec::{Codec, CodecName};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};

use lz4_flex::{compress_prepend_size, decompress_size_prepended};

/// Attribute name used to store the original NDDataType ordinal before compression.
const ATTR_ORIGINAL_DATA_TYPE: &str = "CODEC_ORIGINAL_DATA_TYPE";

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
    let compressed = compress_prepend_size(raw);
    let compressed_size = compressed.len();

    let mut arr = src.clone();
    arr.data = NDDataBuffer::U8(compressed);
    arr.codec = Some(Codec {
        name: CodecName::LZ4,
        compressed_size,
    });

    // Store original data type so decompression can reconstruct the buffer.
    arr.attributes.add(NDAttribute {
        name: ATTR_ORIGINAL_DATA_TYPE.into(),
        description: "Original NDDataType ordinal before codec compression".into(),
        source: NDAttrSource::Driver,
        value: NDAttrValue::UInt8(original_data_type as u8),
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
    let decompressed = decompress_size_prepended(compressed).ok()?;

    // Recover original data type from attribute.
    let original_type = src
        .attributes
        .get(ATTR_ORIGINAL_DATA_TYPE)
        .and_then(|a| a.value.as_i64())
        .and_then(|ord| NDDataType::from_ordinal(ord as u8))
        .unwrap_or(NDDataType::UInt8);

    let buffer = buffer_from_bytes(&decompressed, original_type)?;

    let mut arr = src.clone();
    arr.data = buffer;
    arr.codec = None;
    arr.attributes.remove(ATTR_ORIGINAL_DATA_TYPE);

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
pub struct CodecProcessor {
    mode: CodecMode,
    compression_ratio: f64,
}

impl CodecProcessor {
    pub fn new(mode: CodecMode) -> Self {
        Self {
            mode,
            compression_ratio: 1.0,
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
            CodecMode::Compress {
                codec: CodecName::LZ4,
                ..
            } => Some(compress_lz4(array)),
            CodecMode::Compress {
                codec: CodecName::JPEG,
                quality,
            } => compress_jpeg(array, quality),
            CodecMode::Compress { .. } => None,
            CodecMode::Decompress => {
                // Auto-detect codec from the array
                match array.codec.as_ref().map(|c| c.name) {
                    Some(CodecName::LZ4) => decompress_lz4(array),
                    Some(CodecName::JPEG) => decompress_jpeg(array),
                    _ => None,
                }
            }
        };

        match result {
            Some(ref out) => {
                let output_bytes = out.data.as_u8_slice().len();
                match self.mode {
                    CodecMode::Compress { .. } => {
                        // ratio = original / compressed
                        self.compression_ratio = original_bytes as f64 / output_bytes.max(1) as f64;
                    }
                    CodecMode::Decompress => {
                        // ratio = decompressed / compressed
                        self.compression_ratio = output_bytes as f64 / original_bytes.max(1) as f64;
                    }
                }
                ProcessResult::arrays(vec![Arc::new(out.clone())])
            }
            None => {
                self.compression_ratio = 1.0;
                ProcessResult::empty()
            }
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginCodec"
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
        Ok(())
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
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(width),
                NDDimension::new(height),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = (i % 256) as u8;
            }
        }
        arr
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
        // The original data type attribute should be set
        let dt_attr = compressed.attributes.get(ATTR_ORIGINAL_DATA_TYPE).unwrap();
        assert_eq!(dt_attr.value, NDAttrValue::UInt8(NDDataType::UInt16 as u8));

        let decompressed = decompress_lz4(&compressed).unwrap();
        assert!(decompressed.codec.is_none());
        assert_eq!(decompressed.data.data_type(), NDDataType::UInt16);
        assert_eq!(decompressed.data.as_u8_slice(), original_bytes.as_slice());
        // Attribute should be cleaned up
        assert!(
            decompressed
                .attributes
                .get(ATTR_ORIGINAL_DATA_TYPE)
                .is_none()
        );
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

    // ---- Decompress wrong codec ----

    #[test]
    fn test_decompress_wrong_codec() {
        let arr = make_u8_array(4, 4);
        assert!(decompress_lz4(&arr).is_none());
        assert!(decompress_jpeg(&arr).is_none());
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
        assert!(result.output_arrays.is_empty());
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
