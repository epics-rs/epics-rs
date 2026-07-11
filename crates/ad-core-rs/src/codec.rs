use crate::ndarray::NDDataType;

/// Codec names for compressed NDArray data (matching C++ `NDCodecName`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecName {
    None,
    JPEG,
    /// Zlib (deflate) compression (C++ `NDCODEC_ZLIB`).
    Zlib,
    LZ4,
    Blosc,
    BSLZ4,
    /// LZ4 in the HDF5 framing variant (C++ `NDCODEC_LZ4HDF5`).
    LZ4HDF5,
}

impl CodecName {
    /// Codec name string as published in the EPICS `CODEC` parameter and the
    /// `NDArray.codec.name` field (matching C++ `NDCodecName`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "",
            Self::JPEG => "jpeg",
            Self::Zlib => "zlib",
            Self::LZ4 => "lz4",
            Self::Blosc => "blosc",
            Self::BSLZ4 => "bslz4",
            Self::LZ4HDF5 => "lz4hdf5",
        }
    }

    /// Value of the `COMPRESSOR` parameter that selects this codec, i.e. C
    /// `NDCodecCompressor_t` (Codec.h:12-18): NONE=0, JPEG=1, BLOSC=2, LZ4=3,
    /// BSLZ4=4. The Rust-only zlib/lz4hdf5 codecs take ordinals after the C set
    /// so they never shadow a C ordinal.
    ///
    /// This is the single mapping between the `COMPRESSOR` wire value and the
    /// codec, used both when the operator writes the parameter and when the
    /// Codec plugin reports the codec it found on a decompressed array
    /// (C `setIntegerParam(NDCodecCompressor, ...)`, NDPluginCodec.cpp:734-757).
    pub fn ordinal(self) -> i32 {
        match self {
            Self::None => 0,
            Self::JPEG => 1,
            Self::Blosc => 2,
            Self::LZ4 => 3,
            Self::BSLZ4 => 4,
            Self::Zlib => 5,
            Self::LZ4HDF5 => 6,
        }
    }

    /// Inverse of [`CodecName::ordinal`]. Unknown ordinals select `None`, as C
    /// does for any compressor value outside the enum (NDPluginCodec.cpp:680-683
    /// `case NDCODEC_NONE: default:`).
    pub fn from_ordinal(ordinal: i32) -> Self {
        match ordinal {
            1 => Self::JPEG,
            2 => Self::Blosc,
            3 => Self::LZ4,
            4 => Self::BSLZ4,
            5 => Self::Zlib,
            6 => Self::LZ4HDF5,
            _ => Self::None,
        }
    }
}

/// Severity the Codec plugin reports in its `CodecStatus` parameter, i.e. C
/// `NDCodecStatus_t` (NDPluginCodec.h:42-46).
///
/// C keeps three levels apart and the numeric values are the contract seen by
/// every client of the `CodecStatus` PV:
///
/// - `Success` (0): the codec did its job, or the array passed through because
///   there was nothing to do (NDPluginCodec.cpp:659, :732-735).
/// - `Warning` (1): benign — the operation was skipped, not failed. C uses it
///   only for "array is already compressed" (:672-675, :466-469, :361-365,
///   :537-541), where the frame flows on unchanged.
/// - `Error` (2): a genuine failure — unsupported input, an encoder/decoder
///   error, a failed allocation, an unexpected codec (:141, :167, :202, :252,
///   :279, :392-397, :760).
///
/// The value is carried as this enum rather than an integer so no call site can
/// invent a level that C does not have, and so a benign skip cannot be reported
/// with the same number as a real failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecStatus {
    Success = 0,
    Warning = 1,
    Error = 2,
}

impl CodecStatus {
    /// The `CodecStatus` parameter value (C `(int)codecStatus`,
    /// NDPluginCodec.cpp:776).
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

/// Blosc sub-compressor names (matching C++ `NDCodecBloscCompName`).
///
/// Indexed by `NDCodecBloscComp_t`: `BloscLZ`, `LZ4`, `LZ4HC`, `Snappy`,
/// `ZLIB`, `ZSTD`.
pub const BLOSC_COMP_NAMES: [&str; 6] = ["blosclz", "lz4", "lz4hc", "snappy", "zlib", "zstd"];

/// Resolve a Blosc sub-compressor index to its name, matching C++
/// `NDCodecBloscCompName[compressor]`. Out-of-range indices return `None`.
pub fn blosc_comp_name(compressor: i32) -> Option<&'static str> {
    usize::try_from(compressor)
        .ok()
        .and_then(|i| BLOSC_COMP_NAMES.get(i).copied())
}

/// Codec information attached to a compressed NDArray.
///
/// `name`/`level`/`shuffle`/`compressor` mirror C++ `Codec_t` and
/// `compressed_size` mirrors `NDArray::compressedSize`. `original_data_type`
/// folds in C++ `NDArray::dataType`, which "holds the data type of the
/// *uncompressed* data ... used for decompression" (NDPluginCodec.cpp:35-36).
/// The Rust port's typed [`NDDataBuffer`](crate::ndarray::NDDataBuffer)
/// collapses to raw bytes (`U8`) once compressed and so can no longer carry the
/// element type; because a `Codec` exists exactly when an array is compressed,
/// this field is the single source of truth for the original element type on
/// every compressed array (set by the codec plugin on compress, read on
/// decompress).
#[derive(Debug, Clone)]
pub struct Codec {
    pub name: CodecName,
    pub compressed_size: usize,
    pub level: i32,
    pub shuffle: i32,
    pub compressor: i32,
    /// Element type of the uncompressed data (C++ `NDArray::dataType`),
    /// retained so decompression can rebuild the correctly-typed buffer.
    pub original_data_type: NDDataType,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codec_clone() {
        let c = Codec {
            name: CodecName::LZ4,
            compressed_size: 1024,
            level: 0,
            shuffle: 0,
            compressor: 0,
            original_data_type: NDDataType::UInt16,
        };
        let c2 = c.clone();
        assert_eq!(c2.name, CodecName::LZ4);
        assert_eq!(c2.compressed_size, 1024);
        assert_eq!(c2.original_data_type, NDDataType::UInt16);
    }

    #[test]
    fn test_codec_name_none() {
        assert_eq!(CodecName::None, CodecName::None);
        assert_ne!(CodecName::None, CodecName::JPEG);
    }

    #[test]
    fn test_codec_name_strings() {
        // G12: name strings must match C++ NDCodecName.
        assert_eq!(CodecName::None.as_str(), "");
        assert_eq!(CodecName::JPEG.as_str(), "jpeg");
        assert_eq!(CodecName::Zlib.as_str(), "zlib");
        assert_eq!(CodecName::LZ4.as_str(), "lz4");
        assert_eq!(CodecName::Blosc.as_str(), "blosc");
        assert_eq!(CodecName::BSLZ4.as_str(), "bslz4");
        assert_eq!(CodecName::LZ4HDF5.as_str(), "lz4hdf5");
    }

    #[test]
    fn test_blosc_comp_names() {
        // G12: Blosc sub-compressor table must match C++ NDCodecBloscCompName.
        assert_eq!(blosc_comp_name(0), Some("blosclz"));
        assert_eq!(blosc_comp_name(1), Some("lz4"));
        assert_eq!(blosc_comp_name(2), Some("lz4hc"));
        assert_eq!(blosc_comp_name(3), Some("snappy"));
        assert_eq!(blosc_comp_name(4), Some("zlib"));
        assert_eq!(blosc_comp_name(5), Some("zstd"));
        assert_eq!(blosc_comp_name(6), None);
        assert_eq!(blosc_comp_name(-1), None);
    }
}
