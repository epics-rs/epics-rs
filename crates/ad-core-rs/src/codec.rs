/// Codec names for compressed NDArray data (matching C++ `NDCodecName`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecName {
    None,
    JPEG,
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
            Self::LZ4 => "lz4",
            Self::Blosc => "blosc",
            Self::BSLZ4 => "bslz4",
            Self::LZ4HDF5 => "lz4hdf5",
        }
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

/// Codec information attached to an NDArray (matching C++ Codec_t).
#[derive(Debug, Clone)]
pub struct Codec {
    pub name: CodecName,
    pub compressed_size: usize,
    pub level: i32,
    pub shuffle: i32,
    pub compressor: i32,
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
        };
        let c2 = c.clone();
        assert_eq!(c2.name, CodecName::LZ4);
        assert_eq!(c2.compressed_size, 1024);
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
