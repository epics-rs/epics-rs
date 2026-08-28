use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
use ad_core_rs::color::NDColorMode;
use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::{NDArray, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{NDFileMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::FilePluginController;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamChangeResult, PluginParamSnapshot, ProcessResult,
};
use parking_lot::Mutex;

// TIFF tag numbers. 256-339 are the baseline tags C sets through TIFFSetField
// (NDFileTIFF.cpp:231-238, :244-267); 65000-65003 and 65010+ are the custom
// EPICS tags (:38-42).
const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_BITS_PER_SAMPLE: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_IMAGE_DESCRIPTION: u16 = 270;
const TAG_MAKE: u16 = 271;
const TAG_MODEL: u16 = 272;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_ROWS_PER_STRIP: u16 = 278;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_PLANAR_CONFIG: u16 = 284;
const TAG_SOFTWARE: u16 = 305;
const TAG_SAMPLE_FORMAT: u16 = 339;
const TIFFTAG_NDTIMESTAMP: u16 = 65000;
const TIFFTAG_UNIQUEID: u16 = 65001;
const TIFFTAG_EPICSTSSEC: u16 = 65002;
const TIFFTAG_EPICSTSNSEC: u16 = 65003;
const TIFFTAG_FIRST_ATTRIBUTE: u16 = 65010;

// TIFF field types.
const TYPE_ASCII: u16 = 2;
const TYPE_SHORT: u16 = 3;
const TYPE_LONG: u16 = 4;
const TYPE_DOUBLE: u16 = 12;

// libtiff constants, as passed by C.
const COMPRESSION_NONE: u16 = 1;
const PHOTOMETRIC_MINISBLACK: u16 = 1;
const PHOTOMETRIC_RGB: u16 = 2;
const PLANARCONFIG_CONTIG: u16 = 1;
const PLANARCONFIG_SEPARATE: u16 = 2;
const SAMPLEFORMAT_UINT: u16 = 1;
const SAMPLEFORMAT_INT: u16 = 2;
const SAMPLEFORMAT_IEEEFP: u16 = 3;

/// The image geometry C derives in `NDFileTIFF::openFile` (NDFileTIFF.cpp:
/// 180-224) together with the strip layout its `writeFile` (:383-406) writes.
///
/// The two are one decision: RGB2 and RGB3 are stored as three *separate* colour
/// planes (`PLANARCONFIG_SEPARATE`), and RGB2 additionally uses one row per strip
/// because its source rows interleave the planes. Deriving both from a single
/// struct is what keeps the tag values and the strip bytes from disagreeing.
struct TiffLayout {
    width: usize,
    height: usize,
    samples_per_pixel: u16,
    photometric: u16,
    planar_config: u16,
    rows_per_strip: usize,
    color_mode: NDColorMode,
}

/// One IFD entry, ready to be laid out.
struct IfdEntry {
    tag: u16,
    field_type: u16,
    count: u32,
    /// Encoded value bytes; inlined into the entry when ≤ 4 bytes, otherwise
    /// written to the value area and referenced by offset.
    data: Vec<u8>,
}

impl IfdEntry {
    fn ascii(tag: u16, value: &str) -> Self {
        let mut data = value.as_bytes().to_vec();
        data.push(0); // TIFF ASCII values are NUL-terminated, and the NUL counts.
        Self {
            tag,
            field_type: TYPE_ASCII,
            count: data.len() as u32,
            data,
        }
    }

    fn short(tag: u16, values: &[u16]) -> Self {
        Self {
            tag,
            field_type: TYPE_SHORT,
            count: values.len() as u32,
            data: values.iter().flat_map(|v| v.to_le_bytes()).collect(),
        }
    }

    fn long(tag: u16, values: &[u32]) -> Self {
        Self {
            tag,
            field_type: TYPE_LONG,
            count: values.len() as u32,
            data: values.iter().flat_map(|v| v.to_le_bytes()).collect(),
        }
    }

    /// libtiff writes the size tags (ImageWidth, ImageLength, RowsPerStrip) as
    /// SHORT when the value fits and LONG otherwise (`TIFFWriteDirectoryTag
    /// ShortLong`), so a C-written file uses whichever the value calls for.
    fn short_or_long(tag: u16, value: usize) -> ADResult<Self> {
        let value = u32::try_from(value)
            .map_err(|_| ADError::InvalidDimensions("TIFF dimension exceeds 2^32".into()))?;
        Ok(if value <= u32::from(u16::MAX) {
            Self::short(tag, &[value as u16])
        } else {
            Self::long(tag, &[value])
        })
    }

    fn double(tag: u16, value: f64) -> Self {
        Self {
            tag,
            field_type: TYPE_DOUBLE,
            count: 1,
            data: value.to_le_bytes().to_vec(),
        }
    }
}

/// Format an NDAttribute value as the C++ `epicsSnprintf` "name:value" tag
/// string (NDFileTIFF.cpp:303-327). Numeric values keep their type, not a
/// generic stringification: signed `%lld`, unsigned `%llu`, float `%f`.
fn attribute_tag_string(attr: &NDAttribute) -> String {
    let value = match &attr.value {
        NDAttrValue::Int8(v) => format!("{}", v),
        NDAttrValue::Int16(v) => format!("{}", v),
        NDAttrValue::Int32(v) => format!("{}", v),
        NDAttrValue::Int64(v) => format!("{}", v),
        NDAttrValue::UInt8(v) => format!("{}", v),
        NDAttrValue::UInt16(v) => format!("{}", v),
        NDAttrValue::UInt32(v) => format!("{}", v),
        NDAttrValue::UInt64(v) => format!("{}", v),
        // C++ uses "%f" which is 6 fractional digits.
        NDAttrValue::Float32(v) => format!("{:.6}", v),
        NDAttrValue::Float64(v) => format!("{:.6}", v),
        NDAttrValue::String(s) => s.clone(),
        NDAttrValue::Undefined => String::new(),
    };
    format!("{}:{}", attr.name, value)
}

/// TIFF file writer using the `tiff` crate for proper encoding/decoding.
pub struct TiffWriter {
    current_path: Option<PathBuf>,
}

impl TiffWriter {
    pub fn new() -> Self {
        Self { current_path: None }
    }

    /// C's `openFile` structure switch (NDFileTIFF.cpp:180-224). Note the two
    /// RGB planar layouts: RGB2 gets `rowsPerStrip = 1` because its rows
    /// interleave the three planes, RGB3 keeps `rowsPerStrip = sizeY` because
    /// each plane is already contiguous. Both are `PLANARCONFIG_SEPARATE`.
    fn layout(array: &NDArray) -> ADResult<TiffLayout> {
        // The ColorMode *attribute* is the only source of truth, defaulting to
        // Mono when it is absent — C `int colorMode = NDColorModeMono;` (:81)
        // overwritten only by the attribute (:135-136). `info().color_mode` is
        // that rule's single owner. Inferring a colour layout from the dimensions
        // instead made a 3-D array with no ColorMode attribute look like RGB1 and
        // write a file, where C matches none of its 3-D branches (each requires
        // the attribute, :196/:204/:212), falls through to the else, and returns
        // asynError (:220-224).
        let color_mode = array.info().color_mode;
        let mono = |width: usize, height: usize| TiffLayout {
            width,
            height,
            samples_per_pixel: 1,
            photometric: PHOTOMETRIC_MINISBLACK,
            planar_config: PLANARCONFIG_CONTIG,
            rows_per_strip: height,
            color_mode: NDColorMode::Mono,
        };

        Ok(match array.dims.as_slice() {
            [x] => mono(x.size, 1),
            [x, y] => mono(x.size, y.size),
            [c, x, y] if c.size == 3 && color_mode == NDColorMode::RGB1 => TiffLayout {
                width: x.size,
                height: y.size,
                samples_per_pixel: 3,
                photometric: PHOTOMETRIC_RGB,
                planar_config: PLANARCONFIG_CONTIG,
                rows_per_strip: y.size,
                color_mode: NDColorMode::RGB1,
            },
            [x, c, y] if c.size == 3 && color_mode == NDColorMode::RGB2 => TiffLayout {
                width: x.size,
                height: y.size,
                samples_per_pixel: 3,
                photometric: PHOTOMETRIC_RGB,
                planar_config: PLANARCONFIG_SEPARATE,
                rows_per_strip: 1,
                color_mode: NDColorMode::RGB2,
            },
            [x, y, c] if c.size == 3 && color_mode == NDColorMode::RGB3 => TiffLayout {
                width: x.size,
                height: y.size,
                samples_per_pixel: 3,
                photometric: PHOTOMETRIC_RGB,
                planar_config: PLANARCONFIG_SEPARATE,
                rows_per_strip: y.size,
                color_mode: NDColorMode::RGB3,
            },
            _ => {
                return Err(ADError::InvalidDimensions(
                    "unsupported array structure".into(),
                ));
            }
        })
    }

    /// C `sampleFormat`/`bitsPerSample` (NDFileTIFF.cpp:139-179).
    fn sample_format_and_bits(data_type: NDDataType) -> (u16, u16) {
        match data_type {
            NDDataType::Int8 => (SAMPLEFORMAT_INT, 8),
            NDDataType::UInt8 => (SAMPLEFORMAT_UINT, 8),
            NDDataType::Int16 => (SAMPLEFORMAT_INT, 16),
            NDDataType::UInt16 => (SAMPLEFORMAT_UINT, 16),
            NDDataType::Int32 => (SAMPLEFORMAT_INT, 32),
            NDDataType::UInt32 => (SAMPLEFORMAT_UINT, 32),
            NDDataType::Int64 => (SAMPLEFORMAT_INT, 64),
            NDDataType::UInt64 => (SAMPLEFORMAT_UINT, 64),
            NDDataType::Float32 => (SAMPLEFORMAT_IEEEFP, 32),
            NDDataType::Float64 => (SAMPLEFORMAT_IEEEFP, 64),
        }
    }

    /// The strips C's `writeFile` emits, as `(strip index, source byte range)`
    /// **in the order C writes them** (NDFileTIFF.cpp:383-406).
    ///
    /// For RGB2 that order is not the strip order: C walks rows and writes red
    /// row *s* as strip `s`, green as strip `sizeY + s`, blue as strip
    /// `2*sizeY + s`, so the file's byte stream stays row-interleaved while the
    /// StripOffsets table de-interleaves it into three planes. A reader walking
    /// strips in index order therefore sees plane R, plane G, plane B — which is
    /// exactly how C's own `readFile` recovers the image (:497-505, as RGB3).
    fn strip_writes(layout: &TiffLayout, bytes_per_sample: usize) -> Vec<(usize, Range<usize>)> {
        let TiffLayout { width, height, .. } = *layout;
        match layout.color_mode {
            NDColorMode::RGB2 => {
                let strip = width * bytes_per_sample; // one row of one plane
                let mut writes = Vec::with_capacity(3 * height);
                for row in 0..height {
                    let red = 3 * row * strip;
                    writes.push((row, red..red + strip));
                    writes.push((height + row, red + strip..red + 2 * strip));
                    writes.push((2 * height + row, red + 2 * strip..red + 3 * strip));
                }
                writes
            }
            NDColorMode::RGB3 => {
                let plane = width * height * bytes_per_sample;
                (0..3).map(|p| (p, p * plane..(p + 1) * plane)).collect()
            }
            // Mono and RGB1 are chunky: a single strip holding the whole image.
            _ => {
                let total =
                    width * height * usize::from(layout.samples_per_pixel) * bytes_per_sample;
                vec![(0, 0..total)]
            }
        }
    }

    /// Serialise a classic little-endian TIFF: header, strip data, then the IFD
    /// and its value area.
    ///
    /// Written by hand rather than through the `tiff` crate because that crate
    /// cannot express `PLANARCONFIG_SEPARATE` (its ImageEncoder assumes chunky
    /// strips of `width * samples` each) and unconditionally emits XResolution /
    /// YResolution / ResolutionUnit, which C never sets. The tag set below is
    /// exactly C's `TIFFSetField` calls plus the baseline structural tags libtiff
    /// itself writes (Compression, StripOffsets, StripByteCounts).
    fn encode(array: &NDArray, layout: &TiffLayout) -> ADResult<Vec<u8>> {
        let raw = array.data.as_u8_slice();
        let (sample_format, bits_per_sample) = Self::sample_format_and_bits(array.data.data_type());
        let bytes_per_sample = usize::from(bits_per_sample) / 8;

        let writes = Self::strip_writes(layout, bytes_per_sample);
        let expected: usize = writes.iter().map(|(_, r)| r.len()).sum();
        if raw.len() < expected {
            return Err(ADError::InvalidDimensions(format!(
                "TIFF: array holds {} bytes, the {:?} layout needs {}",
                raw.len(),
                layout.color_mode,
                expected
            )));
        }

        let mut out: Vec<u8> = Vec::with_capacity(expected + 1024);
        out.extend_from_slice(b"II"); // little-endian
        out.extend_from_slice(&42u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // IFD offset, patched below

        let strip_count = writes.len();
        let mut strip_offsets = vec![0u32; strip_count];
        let mut strip_byte_counts = vec![0u32; strip_count];
        for (index, range) in writes {
            strip_offsets[index] = out.len() as u32;
            strip_byte_counts[index] = range.len() as u32;
            out.extend_from_slice(&raw[range]);
        }
        if out.len() % 2 != 0 {
            out.push(0); // IFDs must begin on a word boundary
        }

        let samples = usize::from(layout.samples_per_pixel);
        let mut entries = vec![
            IfdEntry::short_or_long(TAG_IMAGE_WIDTH, layout.width)?,
            IfdEntry::short_or_long(TAG_IMAGE_LENGTH, layout.height)?,
            IfdEntry::short(TAG_BITS_PER_SAMPLE, &vec![bits_per_sample; samples]),
            IfdEntry::short(TAG_COMPRESSION, &[COMPRESSION_NONE]),
            IfdEntry::short(TAG_PHOTOMETRIC, &[layout.photometric]),
            IfdEntry::long(TAG_STRIP_OFFSETS, &strip_offsets),
            IfdEntry::short(TAG_SAMPLES_PER_PIXEL, &[layout.samples_per_pixel]),
            IfdEntry::short_or_long(TAG_ROWS_PER_STRIP, layout.rows_per_strip)?,
            IfdEntry::long(TAG_STRIP_BYTE_COUNTS, &strip_byte_counts),
            IfdEntry::short(TAG_PLANAR_CONFIG, &[layout.planar_config]),
            IfdEntry::short(TAG_SAMPLE_FORMAT, &vec![sample_format; samples]),
        ];

        // Standard tags derived from well-known attributes (NDFileTIFF.cpp:244-267).
        let model = array
            .attributes
            .get("Model")
            .map(|a| a.value.as_string())
            .unwrap_or_else(|| "Unknown".to_string());
        let make = array
            .attributes
            .get("Manufacturer")
            .map(|a| a.value.as_string())
            .unwrap_or_else(|| "Unknown".to_string());
        entries.push(IfdEntry::ascii(TAG_MODEL, &model));
        entries.push(IfdEntry::ascii(TAG_MAKE, &make));
        entries.push(IfdEntry::ascii(TAG_SOFTWARE, "EPICS areaDetector"));
        if let Some(desc) = array.attributes.get("TIFFImageDescription") {
            entries.push(IfdEntry::ascii(
                TAG_IMAGE_DESCRIPTION,
                &desc.value.as_string(),
            ));
        }

        // EPICS metadata tags 65000-65003 — types per C's tiffFieldInfo (:47-51).
        entries.push(IfdEntry::double(TIFFTAG_NDTIMESTAMP, array.time_stamp));
        entries.push(IfdEntry::long(TIFFTAG_UNIQUEID, &[array.unique_id as u32]));
        entries.push(IfdEntry::long(TIFFTAG_EPICSTSSEC, &[array.timestamp.sec]));
        entries.push(IfdEntry::long(TIFFTAG_EPICSTSNSEC, &[array.timestamp.nsec]));

        // NDArray attributes as ASCII tags from 65010 (:277-355).
        for (i, attr) in array.attributes.iter().enumerate() {
            let Some(tag) = TIFFTAG_FIRST_ATTRIBUTE.checked_add(i as u16) else {
                break; // C stops at TIFFTAG_LAST_ATTRIBUTE (65535).
            };
            entries.push(IfdEntry::ascii(tag, &attribute_tag_string(attr)));
        }

        entries.sort_by_key(|e| e.tag); // TIFF requires ascending tag order

        let ifd_offset = out.len();
        let ifd_size = 2 + 12 * entries.len() + 4;
        let mut values_offset = ifd_offset + ifd_size;
        let mut ifd: Vec<u8> = Vec::with_capacity(ifd_size);
        let mut values: Vec<u8> = Vec::new();

        ifd.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in &entries {
            ifd.extend_from_slice(&entry.tag.to_le_bytes());
            ifd.extend_from_slice(&entry.field_type.to_le_bytes());
            ifd.extend_from_slice(&entry.count.to_le_bytes());
            if entry.data.len() <= 4 {
                let mut inline = entry.data.clone();
                inline.resize(4, 0);
                ifd.extend_from_slice(&inline);
            } else {
                ifd.extend_from_slice(&(values_offset as u32).to_le_bytes());
                values.extend_from_slice(&entry.data);
                if values.len() % 2 != 0 {
                    values.push(0);
                }
                values_offset = ifd_offset + ifd_size + values.len();
            }
        }
        ifd.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

        out.extend_from_slice(&ifd);
        out.extend_from_slice(&values);
        out[4..8].copy_from_slice(&(ifd_offset as u32).to_le_bytes());
        Ok(out)
    }

    fn attach_color_mode(array: &mut NDArray, color_mode: NDColorMode) {
        array.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "Color mode",
            NDAttrSource::Driver,
            NDAttrValue::Int32(color_mode as i32),
        ));
    }
}

impl NDFileWriter for TiffWriter {
    fn open_file(&mut self, path: &Path, _mode: NDFileMode, _array: &NDArray) -> ADResult<()> {
        self.current_path = Some(path.to_path_buf());
        Ok(())
    }

    fn write_file(&mut self, array: &NDArray) -> ADResult<()> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;
        let layout = Self::layout(array)?;
        let bytes = Self::encode(array, &layout)?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;
        let bytes = std::fs::read(path)?;
        decode(&bytes)
    }

    fn close_file(&mut self) -> ADResult<()> {
        self.current_path = None;
        Ok(())
    }

    fn supports_multiple_arrays(&self) -> bool {
        false
    }
}

/// Read one classic TIFF, mirroring C `NDFileTIFF::readFile`
/// (NDFileTIFF.cpp:426-560).
///
/// C classifies the image from (photometric, planarConfig, samplesPerPixel):
/// MINISBLACK/CONTIG/1 → Mono `[x, y]`, RGB/CONTIG/3 → RGB1 `[3, x, y]`, and
/// RGB/SEPARATE/3 → RGB3 `[x, y, 3]` — a planar file always comes back as RGB3,
/// even when written from an RGB2 array, because the strips are read in index
/// order and that order is plane R, plane G, plane B. Anything else is an error.
/// Strips are concatenated in index order into the array buffer (:513-524).
fn decode(bytes: &[u8]) -> ADResult<NDArray> {
    let err = |m: &str| ADError::UnsupportedConversion(format!("TIFF decode error: {m}"));

    let read_u16 = |off: usize| -> ADResult<u16> {
        bytes
            .get(off..off + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .ok_or_else(|| err("truncated"))
    };
    let read_u32 = |off: usize| -> ADResult<u32> {
        bytes
            .get(off..off + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or_else(|| err("truncated"))
    };

    if bytes.len() < 8 || &bytes[0..2] != b"II" || read_u16(2)? != 42 {
        return Err(err("not a little-endian classic TIFF"));
    }

    // The reader only needs the integer tags; ASCII/DOUBLE entries are skipped.
    let ifd = read_u32(4)? as usize;
    let entry_count = read_u16(ifd)? as usize;
    let mut tags: BTreeMap<u16, Vec<u64>> = BTreeMap::new();
    for i in 0..entry_count {
        let entry = ifd + 2 + 12 * i;
        let tag = read_u16(entry)?;
        let size = match read_u16(entry + 2)? {
            TYPE_SHORT => 2usize,
            TYPE_LONG => 4,
            _ => continue,
        };
        let n = read_u32(entry + 4)? as usize;
        let base = if n * size <= 4 {
            entry + 8
        } else {
            read_u32(entry + 8)? as usize
        };
        let mut values = Vec::with_capacity(n);
        for k in 0..n {
            values.push(match size {
                2 => u64::from(read_u16(base + 2 * k)?),
                _ => u64::from(read_u32(base + 4 * k)?),
            });
        }
        tags.insert(tag, values);
    }

    let scalar = |tag: u16| -> Option<u64> { tags.get(&tag).and_then(|v| v.first().copied()) };
    let width = scalar(TAG_IMAGE_WIDTH).ok_or_else(|| err("no ImageWidth"))? as usize;
    let height = scalar(TAG_IMAGE_LENGTH).ok_or_else(|| err("no ImageLength"))? as usize;
    let bits = scalar(TAG_BITS_PER_SAMPLE).ok_or_else(|| err("no BitsPerSample"))?;
    let samples_per_pixel = scalar(TAG_SAMPLES_PER_PIXEL).unwrap_or(1);
    let photometric = scalar(TAG_PHOTOMETRIC).ok_or_else(|| err("no PhotometricInterpretation"))?;
    let planar_config = scalar(TAG_PLANAR_CONFIG).unwrap_or(u64::from(PLANARCONFIG_CONTIG));
    // C: "Sample format is not defined! Default UINT is used." (:457-463)
    let sample_format = match scalar(TAG_SAMPLE_FORMAT) {
        Some(0) | None => u64::from(SAMPLEFORMAT_UINT),
        Some(v) => v,
    };

    let data_type = match (bits, sample_format as u16) {
        (8, SAMPLEFORMAT_INT) => NDDataType::Int8,
        (8, SAMPLEFORMAT_UINT) => NDDataType::UInt8,
        (16, SAMPLEFORMAT_INT) => NDDataType::Int16,
        (16, SAMPLEFORMAT_UINT) => NDDataType::UInt16,
        (32, SAMPLEFORMAT_INT) => NDDataType::Int32,
        (32, SAMPLEFORMAT_UINT) => NDDataType::UInt32,
        (64, SAMPLEFORMAT_INT) => NDDataType::Int64,
        (64, SAMPLEFORMAT_UINT) => NDDataType::UInt64,
        (32, SAMPLEFORMAT_IEEEFP) => NDDataType::Float32,
        (64, SAMPLEFORMAT_IEEEFP) => NDDataType::Float64,
        _ => {
            return Err(err(&format!(
                "unsupported bitsPerSample={bits} sampleFormat={sample_format}"
            )));
        }
    };

    let (dims, color_mode) = match (photometric as u16, planar_config as u16, samples_per_pixel) {
        (PHOTOMETRIC_MINISBLACK, PLANARCONFIG_CONTIG, 1) => (
            vec![NDDimension::new(width), NDDimension::new(height)],
            NDColorMode::Mono,
        ),
        (PHOTOMETRIC_RGB, PLANARCONFIG_CONTIG, 3) => (
            vec![
                NDDimension::new(3),
                NDDimension::new(width),
                NDDimension::new(height),
            ],
            NDColorMode::RGB1,
        ),
        (PHOTOMETRIC_RGB, PLANARCONFIG_SEPARATE, 3) => (
            vec![
                NDDimension::new(width),
                NDDimension::new(height),
                NDDimension::new(3),
            ],
            NDColorMode::RGB3,
        ),
        _ => {
            return Err(err(&format!(
                "unsupported photoMetric={photometric}, planarConfig={planar_config}, \
                 samplesPerPixel={samples_per_pixel}"
            )));
        }
    };

    let offsets = tags
        .get(&TAG_STRIP_OFFSETS)
        .ok_or_else(|| err("no StripOffsets"))?;
    let counts = tags
        .get(&TAG_STRIP_BYTE_COUNTS)
        .ok_or_else(|| err("no StripByteCounts"))?;
    if offsets.len() != counts.len() {
        return Err(err("StripOffsets/StripByteCounts length mismatch"));
    }
    let mut raw: Vec<u8> = Vec::new();
    for (offset, byte_count) in offsets.iter().zip(counts) {
        let (start, len) = (*offset as usize, *byte_count as usize);
        let strip = bytes
            .get(start..start + len)
            .ok_or_else(|| err("strip extends past the end of the file"))?;
        raw.extend_from_slice(strip);
    }

    let mut array = NDArray::new(dims, data_type);
    array.data = crate::codec::buffer_from_bytes(&raw, data_type)
        .ok_or_else(|| err("strip data is not a whole number of samples"))?;
    TiffWriter::attach_color_mode(&mut array, color_mode);
    Ok(array)
}

/// TIFF file processor wrapping `FilePluginController<TiffWriter>`.
pub struct TiffFileProcessor {
    pub ctrl: Mutex<FilePluginController<TiffWriter>>,
}

impl TiffFileProcessor {
    pub fn new() -> Self {
        Self {
            ctrl: Mutex::new(FilePluginController::new(TiffWriter::new())),
        }
    }
}

impl Default for TiffFileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for TiffFileProcessor {
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        self.ctrl.lock().process_array(array)
    }

    fn plugin_type(&self) -> &str {
        "NDFileTIFF"
    }

    /// C `NDPluginFile.cpp:948` (base of every file writer) sets
    /// `NDArrayCallbacks = 0`: file plugins write to disk, not downstream.
    fn does_array_callbacks(&self) -> bool {
        false
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        self.ctrl.lock().register_params(base)
    }

    fn on_param_change(&self, reason: usize, params: &PluginParamSnapshot) -> ParamChangeResult {
        self.ctrl.lock().on_param_change(reason, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::NDDataBuffer;
    // The tests verify the bytes this module writes with an INDEPENDENT TIFF
    // decoder (the `tiff` crate), not with our own reader.
    use ad_core_rs::params::ndarray_driver::NDArrayDriverParams;
    use ad_core_rs::plugin::runtime::{ParamChangeValue, ParamUpdate, PluginParamSnapshot};
    use asyn_rs::port::{PortDriverBase, PortFlags};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tiff::decoder::Decoder;
    use tiff::tags::Tag;

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(prefix: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "adcore_test_{}_{}_{}.tif",
            std::process::id(),
            prefix,
            n
        ))
    }

    /// R8-75: a 3-D array whose ColorMode attribute is absent is an error in C.
    ///
    /// C defaults `colorMode` to Mono (NDFileTIFF.cpp:81) and overwrites it only
    /// from the attribute (:135-136). Each of the three 3-D branches requires the
    /// attribute to name the layout (:196, :204, :212), so a 3-D array without it
    /// matches none of them, falls to the else, and returns asynError (:220-224).
    /// The port inferred RGB1 from `dims[0] == 3` and wrote a file.
    #[test]
    fn test_r8_75_3d_without_colormode_attribute_is_an_error() {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        use ad_core_rs::color::NDColorMode;

        let rgb1_dims = || {
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(4),
            ]
        };

        // No ColorMode attribute → C's colorMode stays Mono → asynError.
        let arr = NDArray::new(rgb1_dims(), NDDataType::UInt8);
        let path = temp_path("tiff_3d_no_colormode");
        let mut writer = TiffWriter::new();
        writer
            .open_file(&path, NDFileMode::Single, &arr)
            .expect("open");
        let err = writer.write_file(&arr).unwrap_err();
        assert!(
            matches!(err, ADError::InvalidDimensions(_)),
            "3-D without ColorMode must be rejected, got {err:?}"
        );
        // ADP-95(b): C rejects in `openFile` (NDFileTIFF.cpp:220-224), before any write, so no file
        // exists. The port decides one call later but must leave the same
        // absence — a caller branching on the error must not find a file.
        assert!(
            !path.exists(),
            "a rejected array must leave no file: {}",
            path.display()
        );
        std::fs::remove_file(&path).ok();

        // Positive control: the same dims WITH ColorMode=RGB1 write normally, so
        // the rejection is keyed on the attribute, not on the shape.
        let mut arr = NDArray::new(rgb1_dims(), NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
            "ColorMode",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(NDColorMode::RGB1 as i32),
        ));
        let path = temp_path("tiff_3d_rgb1");
        let mut writer = TiffWriter::new();
        writer
            .open_file(&path, NDFileMode::Single, &arr)
            .expect("open");
        writer
            .write_file(&arr)
            .expect("3-D WITH ColorMode=RGB1 must still write");
        writer.close_file().ok();
        assert!(path.exists());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_u8_mono() {
        let path = temp_path("tiff_u8");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(v) = &mut arr.data {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let data = std::fs::read(&path).unwrap();
        assert!(data.len() > 16);
        assert!(
            &data[0..2] == &[0x49, 0x49] || &data[0..2] == &[0x4D, 0x4D],
            "Expected TIFF magic bytes"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_u16() {
        let path = temp_path("tiff_u16");
        let mut writer = TiffWriter::new();

        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let data = std::fs::read(&path).unwrap();
        assert!(data.len() > 32);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_roundtrip_u8() {
        let path = temp_path("tiff_rt_u8");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(v) = &mut arr.data {
            for i in 0..16 {
                v[i] = (i * 10) as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();

        let read_back = writer.read_file().unwrap();
        if let (NDDataBuffer::U8(orig), NDDataBuffer::U8(read)) = (&arr.data, &read_back.data) {
            assert_eq!(orig, read);
        } else {
            panic!("data type mismatch on roundtrip");
        }

        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_roundtrip_u16() {
        let path = temp_path("tiff_rt_u16");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(v) = &mut arr.data {
            for i in 0..16 {
                v[i] = (i * 1000) as u16;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();

        let read_back = writer.read_file().unwrap();
        if let (NDDataBuffer::U16(orig), NDDataBuffer::U16(read)) = (&arr.data, &read_back.data) {
            assert_eq!(orig, read);
        } else {
            panic!("data type mismatch on roundtrip");
        }

        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_on_param_change_read_file_emits_array_and_resets_busy() {
        let path = temp_path("tiff_read_param");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(3)],
            NDDataType::UInt8,
        );
        arr.unique_id = 77;
        if let NDDataBuffer::U8(v) = &mut arr.data {
            for (i, item) in v.iter_mut().enumerate() {
                *item = i as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut base = PortDriverBase::new("TIFFTEST", 1, PortFlags::default());
        let _nd_params = NDArrayDriverParams::create(&mut base).unwrap();

        let mut proc = TiffFileProcessor::new();
        proc.register_params(&mut base).unwrap();

        let reason_path = base.find_param("FILE_PATH").unwrap();
        let reason_name = base.find_param("FILE_NAME").unwrap();
        let reason_template = base.find_param("FILE_TEMPLATE").unwrap();
        let reason_read = base.find_param("READ_FILE").unwrap();

        let _ = proc.on_param_change(
            reason_path,
            &PluginParamSnapshot {
                enable_callbacks: true,
                reason: reason_path,
                addr: 0,
                value: ParamChangeValue::Octet(
                    path.parent().unwrap().to_str().unwrap().to_string(),
                ),
            },
        );
        let _ = proc.on_param_change(
            reason_name,
            &PluginParamSnapshot {
                enable_callbacks: true,
                reason: reason_name,
                addr: 0,
                value: ParamChangeValue::Octet(
                    path.file_name().unwrap().to_str().unwrap().to_string(),
                ),
            },
        );
        let _ = proc.on_param_change(
            reason_template,
            &PluginParamSnapshot {
                enable_callbacks: true,
                reason: reason_template,
                addr: 0,
                value: ParamChangeValue::Octet("%s%s".into()),
            },
        );

        let result = proc.on_param_change(
            reason_read,
            &PluginParamSnapshot {
                enable_callbacks: true,
                reason: reason_read,
                addr: 0,
                value: ParamChangeValue::Int32(1),
            },
        );

        assert_eq!(result.output_arrays.len(), 1);
        assert!(result.param_updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Int32 { reason, value: 0, .. } if *reason == reason_read
        )));
        match &result.output_arrays[0].data {
            NDDataBuffer::U8(v) => assert_eq!(v.len(), 12),
            other => panic!("unexpected data buffer: {other:?}"),
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_metadata_tags_match_cpp_numbers_and_types() {
        let path = temp_path("tiff_meta_tags");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.unique_id = 4242;
        arr.time_stamp = 1234.5;
        arr.timestamp.sec = 1_000_000;
        arr.timestamp.nsec = 500;

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut decoder = Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
        // 65000 = NDTimeStamp (TIFF_DOUBLE).
        assert_eq!(decoder.get_tag_f64(Tag::Unknown(65000)).unwrap(), 1234.5);
        // 65001 = NDUniqueId (TIFF_LONG).
        assert_eq!(decoder.get_tag_u32(Tag::Unknown(65001)).unwrap(), 4242);
        // 65002 = EPICSTSSec, 65003 = EPICSTSNsec.
        assert_eq!(decoder.get_tag_u32(Tag::Unknown(65002)).unwrap(), 1_000_000);
        assert_eq!(decoder.get_tag_u32(Tag::Unknown(65003)).unwrap(), 500);
        // Standard Software tag.
        assert_eq!(
            decoder
                .get_tag(Tag::Software)
                .unwrap()
                .into_string()
                .unwrap(),
            "EPICS areaDetector"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_standard_tags_from_attributes() {
        let path = temp_path("tiff_std_tags");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute::new_static(
            "Model",
            "",
            NDAttrSource::Driver,
            NDAttrValue::String("SimDetector".into()),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "Manufacturer",
            "",
            NDAttrSource::Driver,
            NDAttrValue::String("EPICS".into()),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "TIFFImageDescription",
            "",
            NDAttrSource::Driver,
            NDAttrValue::String("test frame".into()),
        ));

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut decoder = Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(
            decoder.get_tag(Tag::Model).unwrap().into_string().unwrap(),
            "SimDetector"
        );
        assert_eq!(
            decoder.get_tag(Tag::Make).unwrap().into_string().unwrap(),
            "EPICS"
        );
        assert_eq!(
            decoder
                .get_tag(Tag::ImageDescription)
                .unwrap()
                .into_string()
                .unwrap(),
            "test frame"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_attribute_tag_format_uses_colon_and_type() {
        // C++ uses "name:value" with typed numeric formatting.
        let mut a = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        a.attributes.add(NDAttribute::new_static(
            "Gain",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(-7),
        ));

        let path = temp_path("tiff_attr_fmt");
        let mut writer = TiffWriter::new();
        writer.open_file(&path, NDFileMode::Single, &a).unwrap();
        writer.write_file(&a).unwrap();
        writer.close_file().unwrap();

        let mut decoder = Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
        // First attribute tag is 65010.
        let s = decoder
            .get_tag(Tag::Unknown(65010))
            .unwrap()
            .into_string()
            .unwrap();
        assert_eq!(s, "Gain:-7");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_signed_rgb_writes_instead_of_erroring() {
        let path = temp_path("tiff_signed_rgb");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(2),
                NDDimension::new(2),
            ],
            NDDataType::Int16,
        );
        TiffWriter::attach_color_mode(&mut arr, NDColorMode::RGB1);
        if let NDDataBuffer::I16(v) = &mut arr.data {
            for (i, item) in v.iter_mut().enumerate() {
                *item = (i as i16) - 6;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        // Previously a hard error; must now succeed.
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut decoder = Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
        let sf = decoder.get_tag_u16_vec(Tag::SampleFormat).unwrap();
        // SampleFormat 2 = SAMPLEFORMAT_INT.
        assert!(sf.iter().all(|&s| s == 2), "expected signed sample format");

        std::fs::remove_file(&path).ok();
    }

    // ---- R8-67: PlanarConfiguration, separate RGB2/RGB3 planes, RowsPerStrip ----

    /// An RGB image in one of the three AD layouts; `pixel(x, y, c)` is
    /// deterministic so every layout holds the same pixels, ordered differently.
    fn rgb_array(mode: NDColorMode, w: usize, h: usize) -> NDArray {
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
        TiffWriter::attach_color_mode(&mut arr, mode);
        if let NDDataBuffer::U8(v) = &mut arr.data {
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

    fn pixel(x: usize, y: usize, c: usize) -> u8 {
        ((x * 7 + y * 13 + c * 61) % 256) as u8
    }

    fn mono_array(w: usize, h: usize) -> NDArray {
        let mut arr = NDArray::new(
            vec![NDDimension::new(w), NDDimension::new(h)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(v) = &mut arr.data {
            v.iter_mut().enumerate().for_each(|(i, x)| *x = i as u8);
        }
        arr
    }

    /// The image bytes as a reader gets them: every strip, in **strip index**
    /// order (not file order), concatenated — exactly what C's readFile does with
    /// TIFFReadEncodedStrip (NDFileTIFF.cpp:513-524). Offsets and counts come
    /// from an independent TIFF decoder.
    fn strips_in_index_order(path: &Path) -> Vec<u8> {
        let bytes = std::fs::read(path).unwrap();
        let mut decoder = Decoder::new(std::fs::File::open(path).unwrap()).unwrap();
        let offsets = decoder.get_tag_u32_vec(Tag::StripOffsets).unwrap();
        let counts = decoder.get_tag_u32_vec(Tag::StripByteCounts).unwrap();
        assert_eq!(offsets.len(), counts.len());
        let mut out = Vec::new();
        for (off, count) in offsets.iter().zip(&counts) {
            out.extend_from_slice(&bytes[*off as usize..(*off + *count) as usize]);
        }
        out
    }

    fn write_tiff(prefix: &str, array: &NDArray) -> PathBuf {
        let path = temp_path(prefix);
        let mut writer = TiffWriter::new();
        writer.open_file(&path, NDFileMode::Single, array).unwrap();
        writer.write_file(array).unwrap();
        writer.close_file().unwrap();
        path
    }

    #[test]
    fn test_r8_67_every_image_carries_planarconfiguration() {
        // C sets TIFFTAG_PLANARCONFIG on every image (NDFileTIFF.cpp:229):
        // CONTIG(1) for mono and RGB1, SEPARATE(2) for RGB2 and RGB3. The port
        // emitted tag 284 on no image at all.
        for (name, array, expected) in [
            ("planar_mono", mono_array(4, 3), PLANARCONFIG_CONTIG),
            (
                "planar_rgb1",
                rgb_array(NDColorMode::RGB1, 4, 3),
                PLANARCONFIG_CONTIG,
            ),
            (
                "planar_rgb2",
                rgb_array(NDColorMode::RGB2, 4, 3),
                PLANARCONFIG_SEPARATE,
            ),
            (
                "planar_rgb3",
                rgb_array(NDColorMode::RGB3, 4, 3),
                PLANARCONFIG_SEPARATE,
            ),
        ] {
            let path = write_tiff(name, &array);
            let mut decoder = Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
            assert_eq!(
                decoder.get_tag_u32(Tag::PlanarConfiguration).unwrap() as u16,
                expected,
                "{name}: PlanarConfiguration (tag 284) must be on disk with C's value"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn test_r8_67_rgb2_and_rgb3_are_written_as_three_separate_planes() {
        // C writes RGB2/RGB3 as three separate colour planes (:389-405): reading
        // the strips in index order yields plane R, then G, then B. The port
        // converted both to RGB1 and wrote chunky RGBRGB… bytes instead.
        let (w, h) = (4usize, 3usize);
        let mut expected_planes: Vec<u8> = Vec::new();
        for c in 0..3 {
            for y in 0..h {
                for x in 0..w {
                    expected_planes.push(pixel(x, y, c));
                }
            }
        }

        for (name, mode) in [
            ("sep_rgb2", NDColorMode::RGB2),
            ("sep_rgb3", NDColorMode::RGB3),
        ] {
            let path = write_tiff(name, &rgb_array(mode, w, h));
            assert_eq!(
                strips_in_index_order(&path),
                expected_planes,
                "{mode:?}: on-disk strips must be the R, G and B planes in order"
            );
            std::fs::remove_file(&path).ok();
        }

        // RGB1 stays chunky: the single strip holds pixel-interleaved RGB.
        let mut expected_chunky: Vec<u8> = Vec::new();
        for y in 0..h {
            for x in 0..w {
                for c in 0..3 {
                    expected_chunky.push(pixel(x, y, c));
                }
            }
        }
        let path = write_tiff("sep_rgb1", &rgb_array(NDColorMode::RGB1, w, h));
        assert_eq!(strips_in_index_order(&path), expected_chunky);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_r8_67_rows_per_strip_matches_c() {
        // C: rowsPerStrip = sizeY for mono/RGB1/RGB3, but 1 for RGB2 because its
        // rows interleave the planes (:190-219). The `tiff` crate instead chose a
        // ~1 MB strip height, so the tag value (and the strip count) diverged on
        // every image.
        let (w, h) = (4usize, 3usize);
        for (name, array, rows, strips) in [
            ("rps_mono", mono_array(w, h), h as u32, 1),
            ("rps_rgb1", rgb_array(NDColorMode::RGB1, w, h), h as u32, 1),
            ("rps_rgb2", rgb_array(NDColorMode::RGB2, w, h), 1, 3 * h),
            ("rps_rgb3", rgb_array(NDColorMode::RGB3, w, h), h as u32, 3),
        ] {
            let path = write_tiff(name, &array);
            let mut decoder = Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
            assert_eq!(
                decoder.get_tag_u32(Tag::RowsPerStrip).unwrap(),
                rows,
                "{name}: RowsPerStrip"
            );
            assert_eq!(
                decoder.get_tag_u32_vec(Tag::StripOffsets).unwrap().len(),
                strips,
                "{name}: strip count"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn test_r8_67_no_resolution_tags() {
        // C never calls TIFFSetField for XResolution/YResolution/ResolutionUnit,
        // so a C-written file carries none of them; the `tiff` crate emitted all
        // three (1/1, 1/1, unit None) on every image.
        let path = write_tiff("no_resolution", &rgb_array(NDColorMode::RGB1, 4, 3));
        let mut decoder = Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
        for tag in [Tag::XResolution, Tag::YResolution, Tag::ResolutionUnit] {
            assert!(
                decoder.get_tag(tag).is_err(),
                "{tag:?} must not be written — C sets no resolution tags"
            );
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_r8_67_planar_file_reads_back_as_rgb3() {
        // C's readFile maps RGB/SEPARATE/3 to RGB3 dims [x, y, 3] (:497-505), so
        // an RGB2 frame round-trips as RGB3 holding the same pixels. Our reader
        // must do the same — the `tiff` crate's decoder reads only the first band
        // of a planar image, which is why read_file is hand-written too.
        let (w, h) = (4usize, 3usize);
        let path = write_tiff("planar_read", &rgb_array(NDColorMode::RGB2, w, h));

        let mut writer = TiffWriter::new();
        writer
            .open_file(
                &path,
                NDFileMode::Single,
                &NDArray::new(vec![], NDDataType::UInt8),
            )
            .unwrap();
        let read_back = writer.read_file().unwrap();
        writer.close_file().unwrap();

        assert_eq!(
            read_back.dims.iter().map(|d| d.size).collect::<Vec<_>>(),
            vec![w, h, 3]
        );
        assert_eq!(
            read_back
                .attributes
                .get("ColorMode")
                .unwrap()
                .value
                .as_i64(),
            Some(NDColorMode::RGB3 as i64)
        );
        assert_eq!(
            read_back.data.as_u8_slice(),
            rgb_array(NDColorMode::RGB3, w, h).data.as_u8_slice(),
            "the RGB2 pixels must come back as the same image in RGB3 layout"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_single_mode_requires_auto_save_for_automatic_write() {
        let path = temp_path("tiff_autosave_single");
        let full_name = path.to_string_lossy().to_string();
        let file_path = path.parent().unwrap().to_str().unwrap().to_string();
        let file_name = path.file_name().unwrap().to_str().unwrap().to_string();

        let proc = TiffFileProcessor::new();
        proc.ctrl.lock().file_base.file_path = file_path.clone() + "/";
        proc.ctrl.lock().file_base.file_name = file_name;
        proc.ctrl.lock().file_base.file_template = "%s%s".into();
        proc.ctrl.lock().file_base.set_mode(NDFileMode::Single);

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(v) = &mut arr.data {
            for (i, item) in v.iter_mut().enumerate() {
                *item = i as u8;
            }
        }

        proc.ctrl.lock().auto_save = false;
        let _ = proc.process_array(&arr, &NDArrayPool::new(1024));
        assert!(!std::path::Path::new(&full_name).exists());

        proc.ctrl.lock().auto_save = true;
        let _ = proc.process_array(&arr, &NDArrayPool::new(1024));
        assert!(std::path::Path::new(&full_name).exists());

        std::fs::remove_file(&path).ok();
    }
}
