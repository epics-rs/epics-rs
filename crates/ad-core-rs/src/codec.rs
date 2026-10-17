use std::sync::Arc;

use crate::ndarray::NDDataType;

/// Codec names for compressed NDArray data (matching C++ `NDCodecName`).
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Any other name, kept as given: C `NDCodec_t::name` is a free string
    /// (NDCodec.h:35) that consumers match by name.
    Other(Arc<str>),
}

impl CodecName {
    /// Codec name string as published in the EPICS `CODEC` parameter and the
    /// `NDArray.codec.name` field (matching C++ `NDCodecName`).
    pub fn as_str(&self) -> &str {
        match self {
            Self::None => "",
            Self::JPEG => "jpeg",
            Self::Zlib => "zlib",
            Self::LZ4 => "lz4",
            Self::Blosc => "blosc",
            Self::BSLZ4 => "bslz4",
            Self::LZ4HDF5 => "lz4hdf5",
            Self::Other(name) => name,
        }
    }

    /// Inverse of [`CodecName::as_str`]: C's `codec.name` string.
    pub fn from_name(name: &str) -> Self {
        match name {
            "" => Self::None,
            "jpeg" => Self::JPEG,
            "zlib" => Self::Zlib,
            "lz4" => Self::LZ4,
            "blosc" => Self::Blosc,
            "bslz4" => Self::BSLZ4,
            "lz4hdf5" => Self::LZ4HDF5,
            other => Self::Other(other.into()),
        }
    }

    /// Value of the `COMPRESSOR` parameter that selects this codec, i.e. C
    /// `NDCodecCompressor_t` (NDCodec.h:7-15): NONE=0, JPEG=1, ZLIB=2,
    /// BLOSC=3, LZ4=4, LZ4HDF5=5, BSLZ4=6.
    ///
    /// This is the single mapping between the `COMPRESSOR` wire value and the
    /// codec, used both when the operator writes the parameter and when the
    /// Codec plugin reports the codec it found on a decompressed array
    /// (C `setIntegerParam(NDCodecCompressor, ...)`, NDPluginCodec.cpp:734-757).
    /// An `Other` name has none; C leaves the parameter as it was (:967-970).
    pub fn ordinal(&self) -> Option<i32> {
        match self {
            Self::None => Some(0),
            Self::JPEG => Some(1),
            Self::Zlib => Some(2),
            Self::Blosc => Some(3),
            Self::LZ4 => Some(4),
            Self::LZ4HDF5 => Some(5),
            Self::BSLZ4 => Some(6),
            Self::Other(_) => None,
        }
    }

    /// Inverse of [`CodecName::ordinal`]. Unknown ordinals select `None`, as C
    /// does for any compressor value outside the enum (NDPluginCodec.cpp:680-683
    /// `case NDCODEC_NONE: default:`).
    pub fn from_ordinal(ordinal: i32) -> Self {
        match ordinal {
            1 => Self::JPEG,
            2 => Self::Zlib,
            3 => Self::Blosc,
            4 => Self::LZ4,
            5 => Self::LZ4HDF5,
            6 => Self::BSLZ4,
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

impl Codec {
    /// A level, shuffle or compressor the codec does not set: C
    /// `NDCodec_t::clear` (NDCodec.h:44-49), which every pool allocation runs.
    pub const UNSET: i32 = -1;

    /// The codec C `NDArrayPool::copy` leaves on a copy: only `codec.name`
    /// is assigned (NDArrayPool.cpp:293) over the cleared codec of the
    /// allocation, so level, shuffle and compressor read back UNSET. The
    /// compressed size and the uncompressed type travel with the data.
    pub fn copied(&self) -> Codec {
        Codec {
            level: Self::UNSET,
            shuffle: Self::UNSET,
            compressor: Self::UNSET,
            ..self.clone()
        }
    }

    /// Whether [`copied`](Self::copied) would leave this codec as it is.
    pub fn is_copied(&self) -> bool {
        [self.level, self.shuffle, self.compressor] == [Self::UNSET; 3]
    }
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

    /// C's codec name is a free string (NDCodec.h:35): a name the port does
    /// not implement is kept and given back as received.
    #[test]
    fn from_name_keeps_any_name() {
        for name in [
            "", "jpeg", "zlib", "lz4", "blosc", "bslz4", "lz4hdf5", "szip",
        ] {
            assert_eq!(CodecName::from_name(name).as_str(), name);
        }
        assert_eq!(
            CodecName::from_name("szip"),
            CodecName::Other("szip".into())
        );
        assert_eq!(CodecName::from_name("szip").ordinal(), None);
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
