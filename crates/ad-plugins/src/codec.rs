use std::sync::Arc;

use ad_core::codec::{Codec, CodecName};
use ad_core::ndarray::{NDArray, NDDataType};
use ad_core::ndarray_pool::NDArrayPool;
use ad_core::plugin::runtime::NDPluginProcess;

/// Compress an NDArray using LZ4.
/// For now, uses a simple run-length encoding as placeholder until lz4_flex is added.
pub fn compress_lz4(src: &NDArray) -> NDArray {
    let raw = src.data.as_u8_slice();
    // Simple "compression": just store as-is but mark codec
    let compressed = raw.to_vec();
    let compressed_size = compressed.len();

    let mut arr = src.clone();
    arr.codec = Some(Codec {
        name: CodecName::LZ4,
        compressed_size,
    });
    arr
}

/// Decompress an LZ4-compressed NDArray.
pub fn decompress_lz4(src: &NDArray) -> Option<NDArray> {
    if src.codec.as_ref().map(|c| c.name) != Some(CodecName::LZ4) {
        return None;
    }
    let mut arr = src.clone();
    arr.codec = None;
    Some(arr)
}

/// Compress an NDArray to JPEG (UInt8 only).
/// Placeholder: just marks codec without actual JPEG encoding (requires `image` crate).
pub fn compress_jpeg(src: &NDArray, _quality: u8) -> Option<NDArray> {
    if src.data.data_type() != NDDataType::UInt8 {
        return None;
    }
    let compressed_size = src.data.total_bytes(); // placeholder
    let mut arr = src.clone();
    arr.codec = Some(Codec {
        name: CodecName::JPEG,
        compressed_size,
    });
    Some(arr)
}

/// Decompress a JPEG-compressed NDArray.
pub fn decompress_jpeg(src: &NDArray) -> Option<NDArray> {
    if src.codec.as_ref().map(|c| c.name) != Some(CodecName::JPEG) {
        return None;
    }
    let mut arr = src.clone();
    arr.codec = None;
    Some(arr)
}

/// Codec operation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecMode {
    CompressLZ4,
    DecompressLZ4,
    CompressJPEG { quality: u8 },
    DecompressJPEG,
}

/// Pure codec processing logic.
pub struct CodecProcessor {
    mode: CodecMode,
}

impl CodecProcessor {
    pub fn new(mode: CodecMode) -> Self {
        Self { mode }
    }
}

impl NDPluginProcess for CodecProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> Vec<Arc<NDArray>> {
        let result = match self.mode {
            CodecMode::CompressLZ4 => Some(compress_lz4(array)),
            CodecMode::DecompressLZ4 => decompress_lz4(array),
            CodecMode::CompressJPEG { quality } => compress_jpeg(array, quality),
            CodecMode::DecompressJPEG => decompress_jpeg(array),
        };
        match result {
            Some(out) => vec![Arc::new(out)],
            None => vec![],
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginCodec"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core::ndarray::{NDDataBuffer, NDDimension};

    fn make_array() -> NDArray {
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..16 { v[i] = i as u8; }
        }
        arr
    }

    #[test]
    fn test_lz4_roundtrip() {
        let arr = make_array();
        let compressed = compress_lz4(&arr);
        assert_eq!(compressed.codec.as_ref().unwrap().name, CodecName::LZ4);

        let decompressed = decompress_lz4(&compressed).unwrap();
        assert!(decompressed.codec.is_none());
        assert_eq!(decompressed.data.as_u8_slice(), arr.data.as_u8_slice());
    }

    #[test]
    fn test_jpeg_roundtrip() {
        let arr = make_array();
        let compressed = compress_jpeg(&arr, 90).unwrap();
        assert_eq!(compressed.codec.as_ref().unwrap().name, CodecName::JPEG);

        let decompressed = decompress_jpeg(&compressed).unwrap();
        assert!(decompressed.codec.is_none());
    }

    #[test]
    fn test_decompress_wrong_codec() {
        let arr = make_array();
        assert!(decompress_lz4(&arr).is_none());
        assert!(decompress_jpeg(&arr).is_none());
    }
}
