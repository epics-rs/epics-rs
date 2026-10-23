use asyn_rs::port::InterfaceMask;
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute, NDAttributeList};
use ad_core_rs::color::NDColorMode;
use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::{NDArray, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{NDFileOpenMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::{FileControl, FilePluginController};
use ad_core_rs::plugin::runtime::{NDPluginProcess, PluginAttributes, PluginPool, ProcessResult};
use epics_libcom_rs::cfmt::CFloat;
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
const TYPE_BYTE: u16 = 1;
const TYPE_ASCII: u16 = 2;
const TYPE_SHORT: u16 = 3;
const TYPE_LONG: u16 = 4;
const TYPE_SBYTE: u16 = 6;
const TYPE_SSHORT: u16 = 8;
const TYPE_SLONG: u16 = 9;
const TYPE_DOUBLE: u16 = 12;
const TYPE_LONG8: u16 = 16;
const TYPE_SLONG8: u16 = 17;

// libtiff constants, as passed by C.
const COMPRESSION_NONE: u16 = 1;
const PHOTOMETRIC_MINISBLACK: u16 = 1;
const PHOTOMETRIC_RGB: u16 = 2;
const PLANARCONFIG_CONTIG: u16 = 1;
const PLANARCONFIG_SEPARATE: u16 = 2;
const SAMPLEFORMAT_UINT: u16 = 1;
const SAMPLEFORMAT_INT: u16 = 2;
const SAMPLEFORMAT_IEEEFP: u16 = 3;
const SAMPLEFORMAT_COMPLEXIEEEFP: u16 = 6;

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
        NDAttrValue::Float32(v) => format!("{:.6}", CFloat(f64::from(*v))),
        NDAttrValue::Float64(v) => format!("{:.6}", CFloat(*v)),
        NDAttrValue::String(s) => s.clone(),
        NDAttrValue::Undefined => String::new(),
    };
    format!("{}:{}", attr.name, value)
}

/// TIFF file writer using the `tiff` crate for proper encoding/decoding.
pub struct TiffWriter {
    current_path: Option<PathBuf>,
    /// The plugin's own set, C `getAttributes` in `openFile` and `readFile`.
    plugin_attributes: NDAttributeList,
}

impl TiffWriter {
    pub fn new() -> Self {
        Self {
            current_path: None,
            plugin_attributes: NDAttributeList::new(),
        }
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
    /// `attributes` is C's `pFileAttributes`, every tag below reads it.
    fn encode(
        array: &NDArray,
        attributes: &NDAttributeList,
        layout: &TiffLayout,
    ) -> ADResult<Vec<u8>> {
        let raw = array.data().as_u8_slice();
        let (sample_format, bits_per_sample) =
            Self::sample_format_and_bits(array.data().data_type());
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
        let model = attributes
            .get("Model")
            .map(|a| a.value.as_string())
            .unwrap_or_else(|| "Unknown".to_string());
        let make = attributes
            .get("Manufacturer")
            .map(|a| a.value.as_string())
            .unwrap_or_else(|| "Unknown".to_string());
        entries.push(IfdEntry::ascii(TAG_MODEL, &model));
        entries.push(IfdEntry::ascii(TAG_MAKE, &make));
        entries.push(IfdEntry::ascii(TAG_SOFTWARE, "EPICS areaDetector"));
        if let Some(desc) = attributes.get("TIFFImageDescription") {
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
        for (i, attr) in attributes.iter().enumerate() {
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
    fn open_file(&mut self, path: &Path, mode: NDFileOpenMode, _array: &NDArray) -> ADResult<()> {
        // C reads but does not append (NDFileTIFF.cpp:102).
        if mode.contains(NDFileOpenMode::APPEND) {
            return Err(ADError::UnsupportedConversion(
                "NDFileTIFF cannot append".into(),
            ));
        }
        self.current_path = Some(path.to_path_buf());
        Ok(())
    }

    fn write_file(&mut self, array: &Arc<NDArray>) -> ADResult<()> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;
        let layout = Self::layout(array)?;
        // C `openFile`: the plugin's set, then the frame's on top
        // (NDFileTIFF.cpp:240-242).
        let mut attributes = self.plugin_attributes.clone();
        attributes.copy_from(&array.attributes);
        let bytes = Self::encode(array, &attributes, &layout)?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    fn read_file(&mut self, pool: &NDArrayPool) -> ADResult<NDArray> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;
        let bytes = std::fs::read(path)?;
        decode(&bytes, self.plugin_attributes.clone(), pool)
    }

    fn close_file(&mut self) -> ADResult<()> {
        self.current_path = None;
        Ok(())
    }

    fn supports_multiple_arrays(&self) -> bool {
        false
    }

    fn set_plugin_attributes(&mut self, attributes: NDAttributeList) {
        self.plugin_attributes = attributes;
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
/// The image is allocated from `pool` and its strips are read into it in
/// index order (:515-533); C dereferences a refused alloc, here the read
/// fails. The image starts from `attributes`, the plugin's set C `readFile`
/// gets ahead of ColorMode (NDFileTIFF.cpp:540-543).
fn decode(bytes: &[u8], attributes: NDAttributeList, pool: &NDArrayPool) -> ADResult<NDArray> {
    let err = |m: &str| ADError::UnsupportedConversion(format!("TIFF decode error: {m}"));

    // libtiff reads either byte order: the header and IFD in the file's,
    // the samples swabbed to the host's (tif_postdecode).
    let big_endian = match bytes.get(0..2) {
        Some(b"II") => false,
        Some(b"MM") => true,
        _ => return Err(err("not a classic TIFF")),
    };
    let read_u16 = |off: usize| -> ADResult<u16> {
        bytes
            .get(off..off + 2)
            .map(|b| {
                let b = [b[0], b[1]];
                if big_endian {
                    u16::from_be_bytes(b)
                } else {
                    u16::from_le_bytes(b)
                }
            })
            .ok_or_else(|| err("truncated"))
    };
    let read_u32 = |off: usize| -> ADResult<u32> {
        bytes
            .get(off..off + 4)
            .map(|b| {
                let b = [b[0], b[1], b[2], b[3]];
                if big_endian {
                    u32::from_be_bytes(b)
                } else {
                    u32::from_le_bytes(b)
                }
            })
            .ok_or_else(|| err("truncated"))
    };

    if bytes.len() < 8 || read_u16(2)? != 42 {
        return Err(err("not a classic TIFF"));
    }

    // The entry of each tag; a repeated tag keeps its first, as
    // TIFFReadDirectory ignores the later ones.
    let ifd = read_u32(4)? as usize;
    let entry_count = read_u16(ifd)? as usize;
    let mut entries: BTreeMap<u16, usize> = BTreeMap::new();
    for i in 0..entry_count {
        let entry = ifd + 2 + 12 * i;
        entries.entry(read_u16(entry)?).or_insert(entry);
    }
    // TIFFReadDirEntry*: every integer type reads, as long as no value is
    // negative; any other type fails the open.
    let values = |tag: u16| -> ADResult<Option<Vec<u64>>> {
        let Some(&entry) = entries.get(&tag) else {
            return Ok(None);
        };
        let kind = read_u16(entry + 2)?;
        let (size, signed) = match kind {
            TYPE_BYTE => (1, false),
            TYPE_SBYTE => (1, true),
            TYPE_SHORT => (2, false),
            TYPE_SSHORT => (2, true),
            TYPE_LONG => (4, false),
            TYPE_SLONG => (4, true),
            TYPE_LONG8 => (8, false),
            TYPE_SLONG8 => (8, true),
            _ => return Err(err(&format!("tag {tag} has type {kind}"))),
        };
        let len = (read_u32(entry + 4)? as usize)
            .checked_mul(size)
            .ok_or_else(|| err("truncated"))?;
        let at = if len <= 4 {
            entry + 8
        } else {
            read_u32(entry + 8)? as usize
        };
        let raw = bytes
            .get(at..)
            .and_then(|b| b.get(..len))
            .ok_or_else(|| err("truncated"))?;
        raw.chunks_exact(size)
            .map(|v| {
                let mut b = [0; 8];
                let value = if big_endian {
                    b[8 - size..].copy_from_slice(v);
                    u64::from_be_bytes(b)
                } else {
                    b[..size].copy_from_slice(v);
                    u64::from_le_bytes(b)
                };
                if signed && value >> (8 * size - 1) != 0 {
                    return Err(err(&format!("tag {tag} has a negative value")));
                }
                Ok(value)
            })
            .collect::<ADResult<Vec<_>>>()
            .map(Some)
    };
    // TIFFReadDirEntryShort/Long: one value, no greater than `max`.
    let one = |tag: u16, max: u64| -> ADResult<Option<u64>> {
        match values(tag)?.as_deref() {
            None => Ok(None),
            Some(&[v]) if v <= max => Ok(Some(v)),
            Some(_) => Err(err(&format!("tag {tag} is not one value up to {max}"))),
        }
    };
    let short = u64::from(u16::MAX);
    let long = u64::from(u32::MAX);

    let width = one(TAG_IMAGE_WIDTH, long)?.ok_or_else(|| err("no ImageWidth"))? as usize;
    let height = one(TAG_IMAGE_LENGTH, long)?.ok_or_else(|| err("no ImageLength"))? as usize;
    let samples_per_pixel = match one(TAG_SAMPLES_PER_PIXEL, short)? {
        Some(0) => return Err(err("bad value 0 for SamplesPerPixel")),
        Some(n) => n,
        None => 1,
    };
    // One value, or one per sample all alike (TIFFReadDirEntryPersampleShort).
    let per_sample = |tag: u16| -> ADResult<Option<u64>> {
        let Some(v) = values(tag)? else {
            return Ok(None);
        };
        let samples = samples_per_pixel as usize;
        match v[..] {
            [one] if one <= short => Ok(Some(one)),
            _ if v.len() >= samples
                && v.iter().all(|&s| s <= short)
                && v[..samples].iter().all(|&s| s == v[0]) =>
            {
                Ok(Some(v[0]))
            }
            _ => Err(err(&format!("tag {tag} is not one value per sample"))),
        }
    };
    // C decodes through libtiff's codecs; with none here, a compressed strip
    // would come back as its raw bytes.
    let compression = per_sample(TAG_COMPRESSION)?.unwrap_or(u64::from(COMPRESSION_NONE));
    if compression != u64::from(COMPRESSION_NONE) {
        return Err(err(&format!("unsupported Compression={compression}")));
    }
    let bits = per_sample(TAG_BITS_PER_SAMPLE)?.ok_or_else(|| err("no BitsPerSample"))?;
    let photometric =
        one(TAG_PHOTOMETRIC, short)?.ok_or_else(|| err("no PhotometricInterpretation"))?;
    let planar_config = one(TAG_PLANAR_CONFIG, short)?.unwrap_or(u64::from(PLANARCONFIG_CONTIG));
    // _TIFFVSetField fails the open on a SampleFormat outside 1..=6. Without
    // the tag, C's sampleFormat is left uninitialized; UINT here, as its 0
    // branch (:457-463) intends.
    let sample_format = match per_sample(TAG_SAMPLE_FORMAT)? {
        Some(v) if !(SAMPLEFORMAT_UINT..=SAMPLEFORMAT_COMPLEXIEEEFP).contains(&(v as u16)) => {
            return Err(err(&format!("bad value {v} for SampleFormat")));
        }
        Some(v) => v,
        None => u64::from(SAMPLEFORMAT_UINT),
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

    // libtiff refuses these at TIFFOpen (tif_dirread.c:3561, :3621, :4053).
    if width == 0 || height == 0 {
        return Err(err("no image"));
    }
    let rows_per_strip = match one(TAG_ROWS_PER_STRIP, long)? {
        Some(0) => return Err(err("bad value 0 for RowsPerStrip")),
        Some(rows) => (rows as usize).min(height),
        None => height,
    };
    let separate = planar_config == u64::from(PLANARCONFIG_SEPARATE);
    let planes = if separate {
        samples_per_pixel as usize
    } else {
        1
    };
    let strips_per_plane = height.div_ceil(rows_per_strip);
    let num_strips = strips_per_plane * planes;
    // One entry per strip of the geometry, cut or zero-filled, as
    // TIFFFetchStripThing leaves the tables.
    let table = |tag: u16| -> ADResult<Option<Vec<u64>>> {
        Ok(values(tag)?.map(|v| {
            (0..num_strips)
                .map(|i| v.get(i).copied().unwrap_or(0))
                .collect()
        }))
    };
    let offsets = table(TAG_STRIP_OFFSETS)?.ok_or_else(|| err("no StripOffsets"))?;
    let mut array = pool.alloc(dims, data_type)?;
    let image = array.data_mut().as_u8_slice_mut();
    let size = image.len();
    // TIFFScanlineSize: one row of one plane.
    let scanline = size / height / planes;
    let file_size = bytes.len() as u64;
    let estimate = || vec![(scanline * (height / strips_per_plane)) as u64; num_strips];
    // TIFFReadDirectory estimates the counts from the geometry when they are
    // missing, when a single one is 0, past the file or short of the image,
    // or when the first two of more than two contiguous strips differ
    // (tif_dirread.c:3901-3976).
    let counts = match table(TAG_STRIP_BYTE_COUNTS)? {
        None if num_strips != planes => return Err(err("no StripByteCounts")),
        None => estimate(),
        Some(c)
            if num_strips == 1
                && offsets[0] != 0
                && (c[0] == 0
                    || c[0] > file_size.wrapping_sub(offsets[0])
                    || c[0] < (scanline * height) as u64) =>
        {
            estimate()
        }
        Some(c) if !separate && num_strips > 2 && c[0] != c[1] && c[0] != 0 && c[1] != 0 => {
            estimate()
        }
        Some(c) => c,
    };
    let mut total = 0;
    for strip in 0..num_strips {
        // TIFFReadEncodedStrip: the strip's rows from the geometry, capped at
        // what the image has left; the count only has to cover them.
        let rows = (height - strip % strips_per_plane * rows_per_strip).min(rows_per_strip);
        let len = (rows * scanline).min(size - total);
        let (offset, count) = (offsets[strip], counts[strip]);
        if count > file_size || offset > file_size - count {
            return Err(err(&format!("strip {strip} runs past the end of the file")));
        }
        if count < len as u64 {
            return Err(err(&format!(
                "strip {strip} has {count} bytes, its rows {len}"
            )));
        }
        let start = offset as usize;
        image[total..total + len].copy_from_slice(&bytes[start..start + len]);
        total += len;
    }
    if big_endian != cfg!(target_endian = "big") {
        image
            .chunks_exact_mut(data_type.element_size())
            .for_each(<[u8]>::reverse);
    }
    array.attributes = attributes;
    TiffWriter::attach_color_mode(&mut array, color_mode);
    Ok(array)
}

/// TIFF file processor wrapping `FilePluginController<TiffWriter>`.
pub struct TiffFileProcessor {
    pub ctrl: Mutex<FilePluginController<TiffWriter>>,
    attributes: Option<PluginAttributes>,
}

impl TiffFileProcessor {
    pub fn new() -> Self {
        Self {
            ctrl: Mutex::new(FilePluginController::new(TiffWriter::new())),
            attributes: None,
        }
    }
}

impl Default for TiffFileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for TiffFileProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_CANBLOCK;
    // NDFileTIFF.cpp:614, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::GENERIC_POINTER;
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;

    fn posts_before_write(&self, reason: usize, value: i32) -> bool {
        self.ctrl.lock().posts_before_write(reason, value)
    }

    fn processing_required(&self, array: &NDArray) -> bool {
        self.ctrl.lock().processing_required(array)
    }

    /// C NDPluginFile::writeInt32 (NDPluginFile.cpp:799-870).
    fn validate_int32(
        &self,
        reason: usize,
        _addr: i32,
        value: i32,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<i32>,
        params: &mut ad_core_rs::plugin::runtime::WritePort<'_>,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<i32> {
        let mut ctrl = self.ctrl.lock();
        ctrl.write_int32(reason, value, verdict, params)
    }

    /// C NDPluginFile::writeOctet: the setting is stored inside the write.
    fn validate_octet(
        &self,
        reason: usize,
        _addr: i32,
        _value: &str,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<String>,
        _params: &asyn_rs::port::PortDriverBase,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<String> {
        self.ctrl.lock().write_octet(reason, verdict)
    }
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        // Made before the controller lock: its reads wait on the actor,
        // which takes that lock in its writes.
        let mut post = ad_core_rs::plugin::runtime::PortPost::of(self.attributes.as_ref());
        let mut ctrl = self.ctrl.lock();
        ctrl.process_array(array, &mut post)
    }

    fn bind_attributes(&mut self, attributes: PluginAttributes) {
        self.attributes = Some(attributes);
    }

    fn bind_pool(&mut self, pool: PluginPool) {
        self.ctrl.get_mut().bind_pool(pool);
    }

    fn plugin_type(&self) -> &str {
        "NDFileTIFF"
    }

    /// C `NDPluginFile.cpp:948` (base of every file writer) sets
    /// `NDArrayCallbacks = 0`: file plugins write to disk, not downstream.
    fn array_callbacks_seed(&self) -> Option<bool> {
        Some(false)
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        self.ctrl.lock().register_params(base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataView, NDDataViewMut};
    use ad_core_rs::plugin::file_base::NDFileMode;
    // The tests verify the bytes this module writes with an INDEPENDENT TIFF
    // decoder (the `tiff` crate), not with our own reader.
    use ad_core_rs::params::ndarray_driver::NDArrayDriverParams;
    use ad_core_rs::plugin::runtime::WriteVerdict;
    use asyn_rs::port::InterfaceMask;
    use asyn_rs::port::{PortDriverBase, PortFlags};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tiff::decoder::Decoder;

    /// C++'s `%f` spells a NaN `nan`, and `-nan` when its sign bit is set.
    #[test]
    fn a_float_attribute_tag_prints_a_nan_as_c_does() {
        let tag = |value| {
            attribute_tag_string(&NDAttribute::new_static(
                "A",
                "",
                NDAttrSource::Driver,
                value,
            ))
        };
        assert_eq!(tag(NDAttrValue::Float64(f64::NAN)), "A:nan");
        assert_eq!(tag(NDAttrValue::Float64(-f64::NAN)), "A:-nan");
        assert_eq!(tag(NDAttrValue::Float32(f32::NAN)), "A:nan");
        assert_eq!(tag(NDAttrValue::Float32(-f32::NAN)), "A:-nan");
    }
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

    /// C `openFile` reads Model, Manufacturer and the 65010 tags from
    /// `pFileAttributes`: the plugin's own set with the frame's on top
    /// (NDFileTIFF.cpp:240-242).
    /// A frame for another plugin returns before beginProcessCallbacks
    /// (NDPluginFile.cpp:724-725), so none of its fields is set.
    #[test]
    fn a_frame_for_another_plugin_sets_no_begin_field() {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        let (handle, _rx, _jh) = crate::test_util::running("TIFF_DEST", TiffFileProcessor::new());
        let port = handle.port_runtime().port_handle();
        let p = &handle.ndarray_params;
        let before = [p.unique_id, p.n_dimensions, p.array_counter]
            .map(|r| port.read_int32_blocking(r, 0).ok());
        let mut array = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        array.unique_id = 7;
        array.attributes.add(NDAttribute::new_static(
            "FilePluginDestination",
            "",
            NDAttrSource::Driver,
            NDAttrValue::String("OTHER".into()),
        ));
        crate::test_util::publish(&handle, &array);
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        let after = [p.unique_id, p.n_dimensions, p.array_counter]
            .map(|r| port.read_int32_blocking(r, 0).ok());
        assert_eq!(after, before);
    }

    /// A frame addressed to this plugin's own port name is processed
    /// (NDPluginFile.cpp:646), whatever the case.
    #[test]
    fn a_frame_for_this_port_is_processed() {
        use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        let (handle, _rx, _jh) = crate::test_util::running("TIFF_MINE", TiffFileProcessor::new());
        let mut array = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        array.unique_id = 7;
        array.attributes.add(NDAttribute::new_static(
            "FilePluginDestination",
            "",
            NDAttrSource::Driver,
            NDAttrValue::String("tiff_mine".into()),
        ));
        crate::test_util::publish(&handle, &array);
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        let port = handle.port_runtime().port_handle();
        assert_eq!(
            port.read_int32_blocking(handle.ndarray_params.unique_id, 0)
                .ok(),
            Some(7)
        );
    }

    #[test]
    fn the_file_carries_the_plugin_attributes_under_the_frame() {
        use crate::test_util::{auto_save_to, publish, wait_saved, with_attributes_file};
        let xml = r#"<Attributes>
            <Attribute name="Model" type="CONST" source="PluginModel"/>
            <Attribute name="Manufacturer" type="CONST" source="PluginMake"/>
            <Attribute name="ColorMode" type="CONST" source="5" datatype="INT"/>
        </Attributes>"#;
        let (handle, mut rx, _jh) =
            with_attributes_file("TIFF_ATTRS", TiffFileProcessor::new(), xml);
        let path = temp_path("plugin_attrs");
        auto_save_to(&handle, &path);
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.attributes.add(NDAttribute::new_static(
            "Manufacturer",
            "",
            NDAttrSource::Driver,
            NDAttrValue::String("FrameMake".into()),
        ));
        publish(&handle, &arr);
        wait_saved(&handle, 1);
        let mut decoder = Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
        let tag = |d: &mut Decoder<std::fs::File>, t| d.get_tag(t).unwrap().into_string().unwrap();
        assert_eq!(tag(&mut decoder, Tag::Model), "PluginModel");
        assert_eq!(tag(&mut decoder, Tag::Make), "FrameMake");

        // C `readFile` gets the set onto the image, then adds ColorMode
        // (NDFileTIFF.cpp:540-543); `readFileBase`'s `endProcessCallbacks`
        // gets the set again, over it (NDPluginFile.cpp:193).
        let port = handle.port_runtime().port_handle();
        port.write_int32_blocking(handle.ndarray_params.array_callbacks, 0, 1)
            .unwrap();
        let reason = port
            .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("READ_FILE", 0))
            .unwrap()
            .reason;
        port.write_int32_blocking(reason, 0, 1).unwrap();
        let image = rx.blocking_recv().unwrap();
        let value = |name: &str| image.attributes.get(name).map(|a| a.value.clone());
        assert_eq!(
            value("Model"),
            Some(NDAttrValue::String("PluginModel".into()))
        );
        assert_eq!(value("ColorMode"), Some(NDAttrValue::String("5".into())));
        std::fs::remove_file(&path).ok();
    }

    /// C hands a saved frame on and counts it (NDPluginFile.cpp:252, :749);
    /// WriteFile hands the same frame on again without counting it, since
    /// writeFileBase runs no beginProcessCallbacks (:801-820).
    #[test]
    fn a_written_frame_goes_downstream() {
        use crate::test_util::{auto_save_to, publish, running, wait_saved};
        let (handle, mut rx, _jh) = running("TIFF_ON", TiffFileProcessor::new());
        let port = handle.port_runtime().port_handle();
        let p = &handle.ndarray_params;
        port.write_int32_blocking(p.array_callbacks, 0, 1).unwrap();
        let path = temp_path("handed_on");
        auto_save_to(&handle, &path);
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        arr.unique_id = 3;
        publish(&handle, &arr);
        wait_saved(&handle, 1);
        assert_eq!(rx.blocking_recv().unwrap().unique_id, 3);
        port.write_int32_blocking(p.write_file, 0, 1).unwrap();
        assert_eq!(rx.blocking_recv().unwrap().unique_id, 3);
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        assert_eq!(port.read_int32_blocking(p.array_counter, 0).unwrap(), 1);
        std::fs::remove_file(&path).ok();
    }

    /// What a monitor on NumCaptured and a blocking downstream plugin see,
    /// in the one order they see it.
    #[derive(Debug, PartialEq)]
    enum Seen {
        NumCaptured(i32),
        Frame(i32),
    }

    /// Logs each frame it is handed, standing in for a downstream plugin.
    struct Downstream(Arc<parking_lot::Mutex<Vec<Seen>>>);

    impl NDPluginProcess for Downstream {
        const ASYN_FLAGS: PortFlags = ad_core_rs::plugin::runtime::ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            self.0.lock().push(Seen::Frame(array.unique_id));
            ProcessResult::sink(vec![])
        }
        fn plugin_type(&self) -> &str {
            "Downstream"
        }
    }

    /// A 3-frame capture into a TIFF plugin whose output feeds a
    /// BlockingCallbacks downstream, flushed by AutoSave or by WriteFile:
    /// what a NumCaptured monitor and the downstream saw, in one sequence.
    /// `hold` keeps the port's actor in the NumCaptured=3 callback until the
    /// downstream has frame 1, as a loaded box can.
    fn capture_flush_log(port: &str, write_file: bool, hold: bool) -> Vec<Seen> {
        use crate::test_util::{auto_save_to, publish, registered, running, wait_saved};
        use std::time::Duration;
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let down_name = format!("{port}_DS");
        let (down, _down_rx, _down_jh) = running(&down_name, Downstream(log.clone()));
        let down_port = down.port_runtime().port_handle();
        down_port
            .write_int32_blocking(down.plugin_params.blocking_callbacks, 0, 1)
            .unwrap();
        assert!(down.wait_params_applied(Duration::from_secs(10)));

        let (handle, _rx, _jh) = registered(port, TiffFileProcessor::new());
        ad_core_rs::plugin::runtime::wire_downstream(&handle, down.array_sender().clone());
        let up = handle.port_runtime().port_handle();
        let p = &handle.ndarray_params;
        up.write_int32_blocking(p.array_callbacks, 0, 1).unwrap();
        let path = temp_path(port);
        auto_save_to(&handle, &path);
        // doCapture needs an array's dimensions first (NDPluginFile.cpp:464).
        publish(
            &handle,
            &NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8),
        );
        wait_saved(&handle, 1);
        up.write_int32_blocking(p.auto_save, 0, i32::from(!write_file))
            .unwrap();
        up.write_int32_blocking(p.file_write_mode, 0, NDFileMode::Capture as i32)
            .unwrap();
        up.write_int32_blocking(p.num_capture, 0, 3).unwrap();
        up.write_int32_blocking(p.capture, 0, 1).unwrap();
        assert!(handle.wait_params_applied(Duration::from_secs(10)));

        let posts = log.clone();
        let _monitor = up
            .interrupts()
            .register_sync_callback(
                asyn_rs::interrupt::InterruptFilter {
                    reason: Some(p.num_captured),
                    ..Default::default()
                },
                move |iv| {
                    if let asyn_rs::param::ParamValue::Int32(v) = iv.value {
                        let deadline = std::time::Instant::now() + Duration::from_secs(10);
                        while hold
                            && v == 3
                            && !posts.lock().contains(&Seen::Frame(1))
                            && std::time::Instant::now() < deadline
                        {
                            std::thread::yield_now();
                        }
                        posts.lock().push(Seen::NumCaptured(v));
                    }
                },
            )
            .unwrap();
        for id in 1..=3 {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.unique_id = id;
            publish(&handle, &arr);
            wait_saved(&handle, id + 1);
        }
        if write_file {
            up.write_int32_blocking(p.write_file, 0, 1).unwrap();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !log.lock().ends_with(&[Seen::NumCaptured(0)]) {
            assert!(std::time::Instant::now() < deadline, "{:?}", log.lock());
            std::thread::yield_now();
        }
        std::fs::remove_file(&path).ok();
        std::mem::take(&mut *log.lock())
    }

    /// Whether the flush after the last buffered frame handed frames 1-3 on
    /// in order, each before the NumCaptured posted for it. The count's order
    /// against the next frame is not asserted: in C a record only gets a
    /// scanIoRequest from the callback (devAsynInt32.c:575), so a PV has none.
    /// That includes the 3 the last buffered frame posts, so it only marks
    /// where the flush's counts start, not where its frames do.
    fn hands_on_before_each_post(log: &[Seen]) -> bool {
        let Some(start) = log.iter().position(|s| *s == Seen::NumCaptured(3)) else {
            return false;
        };
        let flush = &log[start + 1..];
        let at = |seen: Seen| log.iter().position(|s| *s == seen);
        let posted = |n: i32| {
            flush
                .iter()
                .position(|s| *s == Seen::NumCaptured(n))
                .map(|i| start + 1 + i)
        };
        let frames: Vec<_> = log
            .iter()
            .filter(|s| matches!(s, Seen::Frame(k) if *k > 0))
            .collect();
        let counts: Vec<_> = flush
            .iter()
            .filter(|s| matches!(s, Seen::NumCaptured(_)))
            .collect();
        frames == [&Seen::Frame(1), &Seen::Frame(2), &Seen::Frame(3)]
            && counts
                == [
                    &Seen::NumCaptured(2),
                    &Seen::NumCaptured(1),
                    &Seen::NumCaptured(0),
                ]
            && (1..=3).all(|k| at(Seen::Frame(k)) < posted(3 - k))
    }

    /// C's capture loop writes frame N, hands it to endProcessCallbacks,
    /// then posts NumCaptured for it (NDPluginFile.cpp:296-321): a blocking
    /// downstream has frame N before a monitor sees N's count.
    #[test]
    fn an_autosave_flush_hands_each_frame_on_before_its_post() {
        let log = capture_flush_log("TIFF_FLUSH_ORDER", false, false);
        assert!(hands_on_before_each_post(&log), "{log:?}");
    }

    /// The flush's frame 1 can reach the downstream before the monitor sees
    /// the 3 posted ahead of it: the frame callback's posts queue on the
    /// port's actor while the frames are handed on from the callback's thread.
    #[test]
    fn a_late_count_before_the_flush_keeps_the_order() {
        let log = capture_flush_log("TIFF_FLUSH_LATE", false, true);
        let three = log.iter().position(|s| *s == Seen::NumCaptured(3));
        let one = log.iter().position(|s| *s == Seen::Frame(1));
        assert!(one < three, "not held: {log:?}");
        assert!(hands_on_before_each_post(&log), "{log:?}");
    }

    /// WriteFile runs the same loop on the port thread, so a blocking
    /// downstream makes the write wait for it as C's port thread does, and
    /// the write still returns (NDPluginFile.cpp:296-321, :801-820).
    #[test]
    fn a_write_file_flush_waits_on_a_blocking_downstream_in_order() {
        let log = capture_flush_log("TIFF_WRITE_ORDER", true, false);
        assert!(hands_on_before_each_post(&log), "{log:?}");
    }

    /// C evaluates an AutoSave frame's attributes on the frame's thread
    /// (NDPluginFile.cpp:252), so a WriteFile arriving while the frame is
    /// written waits for it and returns. `path` is a FIFO, which holds each
    /// write until it is read.
    // The hold is a POSIX FIFO; Windows has no blocking open to stand in for
    // it, and the locking under test does not depend on the OS.
    #[cfg(unix)]
    #[test]
    fn a_write_file_during_a_single_mode_write_returns() {
        use crate::test_util::{frame, gated, publish, wait_saved, write_file_returns};
        use std::io::Read;
        use std::time::Duration;
        let path = temp_path("single_write_file");
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        let (keep, handle, _gate, entered) =
            gated("TIFF_SINGLE_WF", TiffFileProcessor::new(), &path);
        let (permit, permits) = std::sync::mpsc::channel::<()>();
        let (read, reads) = std::sync::mpsc::channel();
        let fifo = path.clone();
        std::thread::spawn(move || {
            for () in permits {
                let mut bytes = Vec::new();
                let mut file = std::fs::File::open(&fifo).unwrap();
                file.read_to_end(&mut bytes).unwrap();
                let _ = read.send(bytes.len());
            }
        });
        permit.send(()).unwrap();
        publish(&handle, &frame(1));
        wait_saved(&handle, 1);
        assert_eq!(entered.recv_timeout(Duration::from_secs(10)), Ok(1));
        assert!(reads.recv_timeout(Duration::from_secs(10)).is_ok());

        // Frame 2's write waits on the FIFO with the writer held.
        publish(&handle, &frame(2));
        std::thread::sleep(Duration::from_millis(300));
        let returned = write_file_returns(&handle, || {
            std::thread::sleep(Duration::from_millis(300));
            permit.send(()).unwrap();
            permit.send(()).unwrap();
        });
        if !returned {
            // The port is deadlocked; its threads cannot be joined.
            std::mem::forget((keep, handle, permit));
            std::fs::remove_file(&path).ok();
            panic!("WriteFile during a Single-mode write never returned");
        }
        wait_saved(&handle, 2);
        assert_eq!(entered.recv_timeout(Duration::from_secs(10)), Ok(2));
        assert_eq!(entered.recv_timeout(Duration::from_secs(10)), Ok(2));
        drop(permit);
        std::fs::remove_file(&path).ok();
    }

    /// C's flush counts ArrayCounter up after each frame it hands on
    /// (NDPluginFile.cpp:301-319), and the next frame's `getAttributes`
    /// reads the count posted before it.
    #[test]
    fn a_flushed_frame_reads_the_counter_posted_before_it() {
        use crate::test_util::{auto_save_to, frame, publish, wait_saved, with_attributes_file};
        use ad_core_rs::attributes::NDAttrValue;
        let xml = r#"<Attributes>
            <Attribute name="Counter" type="PARAM" source="ARRAY_COUNTER" datatype="INT"/>
        </Attributes>"#;
        let (handle, mut rx, _jh) =
            with_attributes_file("TIFF_FLUSH_COUNTER", TiffFileProcessor::new(), xml);
        let path = temp_path("flush_counter");
        auto_save_to(&handle, &path);
        let port = handle.port_runtime().port_handle();
        let p = &handle.ndarray_params;
        port.write_int32_blocking(p.array_callbacks, 0, 1).unwrap();
        publish(&handle, &frame(0));
        wait_saved(&handle, 1);
        port.write_int32_blocking(p.file_write_mode, 0, NDFileMode::Capture as i32)
            .unwrap();
        port.write_int32_blocking(p.num_capture, 0, 2).unwrap();
        port.write_int32_blocking(p.capture, 0, 1).unwrap();
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        publish(&handle, &frame(1));
        publish(&handle, &frame(2));
        let counters: Vec<_> = (0..3)
            .map(|_| {
                let a = rx.blocking_recv().unwrap();
                let counter = a.attributes.get("Counter").map(|c| c.value.clone());
                (a.unique_id, counter)
            })
            .collect();
        std::fs::remove_file(&path).ok();
        assert_eq!(
            counters,
            vec![
                (0, Some(NDAttrValue::Int32(1))),
                (1, Some(NDAttrValue::Int32(3))),
                (2, Some(NDAttrValue::Int32(4))),
            ]
        );
    }

    /// C `openFile`'s `getAttributes` runs after `createFileName` set
    /// FullFileName and moved FileNumber past it (NDPluginFile.cpp:43-52).
    #[test]
    fn the_file_attributes_read_the_name_being_written() {
        use crate::test_util::{auto_save_to, publish, wait_saved, with_attributes_file};
        let xml = r#"<Attributes>
            <Attribute name="Model" type="PARAM" source="FULL_FILE_NAME" datatype="STRING"/>
            <Attribute name="Number" type="PARAM" source="FILE_NUMBER" datatype="INT"/>
        </Attributes>"#;
        let (handle, _rx, _jh) = with_attributes_file("TIFF_NAME", TiffFileProcessor::new(), xml);
        let path = temp_path("name_being_written");
        auto_save_to(&handle, &path);
        let port = handle.port_runtime().port_handle();
        let p = &handle.ndarray_params;
        port.submit_blocking(
            asyn_rs::request::RequestOp::OctetWrite {
                data: b"%s%s_%d".to_vec(),
            },
            asyn_rs::user::AsynUser::new(p.file_template),
        )
        .unwrap();
        port.write_int32_blocking(p.file_number, 0, 3).unwrap();
        port.write_int32_blocking(p.auto_increment, 0, 1).unwrap();
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        publish(
            &handle,
            &NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8),
        );
        wait_saved(&handle, 1);
        let written = format!("{}_3", path.display());
        let mut decoder = Decoder::new(std::fs::File::open(&written).unwrap()).unwrap();
        let model = decoder.get_tag(Tag::Model).unwrap().into_string().unwrap();
        let number = (65010..65020)
            .filter_map(|t| decoder.get_tag(Tag::Unknown(t)).ok())
            .filter_map(|v| v.into_string().ok())
            .find(|v| v.starts_with("Number:"));
        std::fs::remove_file(&written).ok();
        assert_eq!(model, written);
        assert_eq!(number.as_deref(), Some("Number:4"));
    }

    /// C hands the written frame on at NDPluginFile.cpp:252, after
    /// openFileBase set FullFileName (:52) and createFileName moved
    /// FileNumber, so its getAttributes (NDPluginDriver.cpp:290) reads them.
    #[test]
    fn a_handed_on_frame_reads_the_name_just_written() {
        use crate::test_util::{auto_save_to, publish, wait_saved, with_attributes_file};
        use ad_core_rs::attributes::NDAttrValue;
        let xml = r#"<Attributes>
            <Attribute name="Name" type="PARAM" source="FULL_FILE_NAME" datatype="STRING"/>
            <Attribute name="Number" type="PARAM" source="FILE_NUMBER" datatype="INT"/>
        </Attributes>"#;
        let (handle, mut rx, _jh) =
            with_attributes_file("TIFF_HANDED_NAME", TiffFileProcessor::new(), xml);
        let path = temp_path("handed_name");
        auto_save_to(&handle, &path);
        let port = handle.port_runtime().port_handle();
        let p = &handle.ndarray_params;
        port.submit_blocking(
            asyn_rs::request::RequestOp::OctetWrite {
                data: b"%s%s_%d".to_vec(),
            },
            asyn_rs::user::AsynUser::new(p.file_template),
        )
        .unwrap();
        port.write_int32_blocking(p.file_number, 0, 3).unwrap();
        port.write_int32_blocking(p.auto_increment, 0, 1).unwrap();
        port.write_int32_blocking(p.array_callbacks, 0, 1).unwrap();
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        publish(
            &handle,
            &NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8),
        );
        wait_saved(&handle, 1);
        let a = rx.blocking_recv().unwrap();
        let written = format!("{}_3", path.display());
        std::fs::remove_file(&written).ok();
        let name = a.attributes.get("Name").map(|c| c.value.clone());
        let number = a.attributes.get("Number").map(|c| c.value.clone());
        assert_eq!(
            (name, number),
            (
                Some(NDAttrValue::String(written)),
                Some(NDAttrValue::Int32(4))
            )
        );
    }

    /// The capture flush opens each TIFF (NDPluginFile.cpp:292),
    /// setting FullFileName, before handing it on (:301).
    #[test]
    fn a_flushed_frame_reads_its_own_file_name() {
        use crate::test_util::{auto_save_to, frame, publish, with_attributes_file};
        use ad_core_rs::attributes::NDAttrValue;
        let xml = r#"<Attributes>
            <Attribute name="Name" type="PARAM" source="FULL_FILE_NAME" datatype="STRING"/>
            <Attribute name="Number" type="PARAM" source="FILE_NUMBER" datatype="INT"/>
        </Attributes>"#;
        let (handle, mut rx, _jh) =
            with_attributes_file("TIFF_FLUSHED_NAME", TiffFileProcessor::new(), xml);
        let path = temp_path("flushed_name");
        auto_save_to(&handle, &path);
        let port = handle.port_runtime().port_handle();
        let p = &handle.ndarray_params;
        port.submit_blocking(
            asyn_rs::request::RequestOp::OctetWrite {
                data: b"%s%s_%d".to_vec(),
            },
            asyn_rs::user::AsynUser::new(p.file_template),
        )
        .unwrap();
        port.write_int32_blocking(p.file_number, 0, 3).unwrap();
        port.write_int32_blocking(p.auto_increment, 0, 1).unwrap();
        port.write_int32_blocking(p.array_callbacks, 0, 1).unwrap();
        port.write_int32_blocking(p.auto_save, 0, 0).unwrap();
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        // The frame capture registers; it must be done before Capture is set.
        publish(&handle, &frame(7));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while port.read_int32_blocking(p.unique_id, 0).ok() != Some(7) {
            assert!(
                std::time::Instant::now() < deadline,
                "frame 7 not processed"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        port.write_int32_blocking(p.auto_save, 0, 1).unwrap();
        port.write_int32_blocking(p.file_write_mode, 0, NDFileMode::Capture as i32)
            .unwrap();
        port.write_int32_blocking(p.num_capture, 0, 2).unwrap();
        port.write_int32_blocking(p.capture, 0, 1).unwrap();
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        publish(&handle, &frame(1));
        publish(&handle, &frame(2));
        let got: Vec<_> = (0..2)
            .map(|_| {
                let a = rx.blocking_recv().unwrap();
                (
                    a.unique_id,
                    a.attributes.get("Name").map(|c| c.value.clone()),
                    a.attributes.get("Number").map(|c| c.value.clone()),
                )
            })
            .collect();
        for n in 3..5 {
            std::fs::remove_file(format!("{}_{n}", path.display())).ok();
        }
        let name = |n| Some(NDAttrValue::String(format!("{}_{n}", path.display())));
        assert_eq!(
            got,
            vec![
                (1, name(3), Some(NDAttrValue::Int32(4))),
                (2, name(4), Some(NDAttrValue::Int32(5))),
            ]
        );
    }

    /// C sets NumCaptured to 1 before a Single open (NDPluginFile.cpp:244),
    /// so the file's and the handed-on frame's attributes read it, and the
    /// write posts it (:265).
    #[test]
    fn a_single_write_reads_and_posts_one_captured() {
        use crate::test_util::{auto_save_to, publish, wait_saved, with_attributes_file};
        use ad_core_rs::attributes::NDAttrValue;
        let xml = r#"<Attributes>
            <Attribute name="Captured" type="PARAM" source="NUM_CAPTURED" datatype="INT"/>
        </Attributes>"#;
        let (handle, mut rx, _jh) =
            with_attributes_file("TIFF_SINGLE_CAPTURED", TiffFileProcessor::new(), xml);
        let path = temp_path("single_captured");
        auto_save_to(&handle, &path);
        let port = handle.port_runtime().port_handle();
        let p = &handle.ndarray_params;
        port.submit_blocking(
            asyn_rs::request::RequestOp::OctetWrite {
                data: b"%s%s_%d".to_vec(),
            },
            asyn_rs::user::AsynUser::new(p.file_template),
        )
        .unwrap();
        port.write_int32_blocking(p.file_number, 0, 3).unwrap();
        port.write_int32_blocking(p.array_callbacks, 0, 1).unwrap();
        assert!(handle.wait_params_applied(std::time::Duration::from_secs(10)));
        publish(
            &handle,
            &NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8),
        );
        wait_saved(&handle, 1);
        let handed_on = rx.blocking_recv().unwrap();
        let written = format!("{}_3", path.display());
        let mut decoder = Decoder::new(std::fs::File::open(&written).unwrap()).unwrap();
        let tag = (65010..65020)
            .filter_map(|t| decoder.get_tag(Tag::Unknown(t)).ok())
            .filter_map(|v| v.into_string().ok())
            .find(|v| v.starts_with("Captured:"));
        std::fs::remove_file(&written).ok();
        assert_eq!(
            (
                tag.as_deref(),
                handed_on
                    .attributes
                    .get("Captured")
                    .map(|c| c.value.clone()),
                port.read_int32_blocking(p.num_captured, 0).ok(),
            ),
            (Some("Captured:1"), Some(NDAttrValue::Int32(1)), Some(1))
        );
    }

    /// C `readFileBase` counts the image it read and gets the set onto it
    /// after that (NDPluginFile.cpp:187-193).
    #[test]
    fn a_read_image_gets_the_set_after_it_is_counted() {
        use crate::test_util::{auto_save_to, publish, wait_saved, with_attributes_file};
        let xml = r#"<Attributes>
            <Attribute name="Counter" type="PARAM" source="ARRAY_COUNTER" datatype="INT"/>
        </Attributes>"#;
        let (handle, mut rx, _jh) =
            with_attributes_file("TIFF_READ_COUNT", TiffFileProcessor::new(), xml);
        let path = temp_path("read_count");
        auto_save_to(&handle, &path);
        publish(
            &handle,
            &NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8),
        );
        wait_saved(&handle, 1);
        let port = handle.port_runtime().port_handle();
        port.write_int32_blocking(handle.ndarray_params.array_callbacks, 0, 1)
            .unwrap();
        let reason = port
            .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("READ_FILE", 0))
            .unwrap()
            .reason;
        port.write_int32_blocking(reason, 0, 1).unwrap();
        let image = rx.blocking_recv().unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(
            image.attributes.get("Counter").map(|a| a.value.clone()),
            Some(NDAttrValue::Int32(2))
        );
    }

    /// A setting write while a file write holds the writer returns at once:
    /// staging the plugin's attribute set must not take the writer lock.
    #[test]
    fn a_setting_write_during_a_busy_write_returns_at_once() {
        let mut base = PortDriverBase::new(
            "TIFFBUSY",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let _nd_params = NDArrayDriverParams::create(&mut base).unwrap();
        let mut proc = TiffFileProcessor::new();
        proc.register_params(&mut base).unwrap();
        let auto_increment = base.find_param("AUTO_INCREMENT").unwrap();
        let writer = Arc::clone(&proc.ctrl.lock().writer);
        assert!(
            crate::test_util::returns_while_busy(&writer, || {
                proc.validate_int32(
                    auto_increment,
                    0,
                    1,
                    WriteVerdict::Accept(1),
                    &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
                );
            }),
            "the setting write waited for the file write"
        );
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
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .expect("open");
        let err = writer.write_file(&Arc::new(arr)).unwrap_err();
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
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .expect("open");
        writer
            .write_file(&Arc::new(arr))
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
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
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

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
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
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
            for i in 0..16 {
                v[i] = (i * 10) as u8;
            }
        }

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap();

        let read_back = writer.read_file(&NDArrayPool::new(0)).unwrap();
        if let (NDDataView::U8(orig), NDDataView::U8(read)) =
            (arr.data().view(), read_back.data().view())
        {
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
        if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
            for i in 0..16 {
                v[i] = (i * 1000) as u16;
            }
        }

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr.clone())).unwrap();

        let read_back = writer.read_file(&NDArrayPool::new(0)).unwrap();
        if let (NDDataView::U16(orig), NDDataView::U16(read)) =
            (arr.data().view(), read_back.data().view())
        {
            assert_eq!(orig, read);
        } else {
            panic!("data type mismatch on roundtrip");
        }

        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_read_file_write_publishes_the_array_and_resets_busy() {
        let path = temp_path("tiff_read_param");
        let mut writer = TiffWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(3)],
            NDDataType::UInt8,
        );
        arr.unique_id = 77;
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
            for (i, item) in v.iter_mut().enumerate() {
                *item = i as u8;
            }
        }

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();

        let mut base = PortDriverBase::new(
            "TIFFTEST",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let _nd_params = NDArrayDriverParams::create(&mut base).unwrap();

        let mut proc = TiffFileProcessor::new();
        proc.register_params(&mut base).unwrap();

        let reason_path = base.find_param("FILE_PATH").unwrap();
        let reason_name = base.find_param("FILE_NAME").unwrap();
        let reason_template = base.find_param("FILE_TEMPLATE").unwrap();
        let reason_read = base.find_param("READ_FILE").unwrap();

        let octet = |reason, v: String| {
            proc.validate_octet(reason, 0, &v, WriteVerdict::Accept(v.clone()), &base);
        };
        // As the port's FilePath write leaves it: with checkPath's separator.
        octet(
            reason_path,
            format!("{}/", path.parent().unwrap().to_str().unwrap()),
        );
        octet(
            reason_name,
            path.file_name().unwrap().to_str().unwrap().to_string(),
        );
        octet(reason_template, "%s%s".into());

        let out = proc.validate_int32(
            reason_read,
            0,
            1,
            WriteVerdict::Accept(1),
            &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
        );

        assert_eq!(out.verdict, WriteVerdict::Accept(0));
        let ad_core_rs::plugin::runtime::Published::Read(read) = &out.publish else {
            panic!("ReadFile publishes its array: {:?}", out.publish);
        };
        match read.data().view() {
            NDDataView::U8(v) => assert_eq!(v.len(), 12),
            other => panic!("unexpected data buffer: {other:?}"),
        }

        std::fs::remove_file(&path).ok();
    }

    /// C readFile takes the image from the plugin's `pNDArrayPool`
    /// (NDFileTIFF.cpp:515), so the read frame is charged to that pool.
    #[test]
    fn a_read_file_allocates_the_image_from_the_plugin_pool() {
        let path = write_tiff("tiff_read_pool", &mono_array(4, 3));
        let pool = ad_core_rs::ndarray_pool::DriverPool::standalone(0);
        let mut proc = TiffFileProcessor::new();
        proc.bind_pool(ad_core_rs::plugin::runtime::PluginPool::new(pool.clone()));

        let (out, _base) = crate::test_util::read_file(&mut proc, &path);

        let ad_core_rs::plugin::runtime::Published::Read(read) = &out.publish else {
            panic!("ReadFile publishes its array: {:?}", out.publish);
        };
        assert_eq!(read.pool_id(), pool.id());
        assert_eq!(pool.allocated_bytes(), 12);
        assert_eq!(
            read.data().as_u8_slice(),
            mono_array(4, 3).data().as_u8_slice()
        );
        std::fs::remove_file(&path).ok();
    }

    /// A pool over its maxMemory refuses the image. C goes on to dereference
    /// the NULL (NDFileTIFF.cpp:517); here readFile fails and readFileBase
    /// reports it, with nothing published.
    #[test]
    fn a_read_file_the_pool_refuses_publishes_nothing() {
        let path = write_tiff("tiff_read_refused", &mono_array(4, 3));
        let pool = ad_core_rs::ndarray_pool::DriverPool::standalone(11);
        let mut proc = TiffFileProcessor::new();
        proc.bind_pool(ad_core_rs::plugin::runtime::PluginPool::new(pool.clone()));

        let (out, _base) = crate::test_util::read_file(&mut proc, &path);

        assert!(
            matches!(out.publish, ad_core_rs::plugin::runtime::Published::Nothing),
            "a refused read publishes nothing: {:?}",
            out.publish
        );
        let line = format!("NDPluginFile::readFileBase Error  file {}", path.display());
        assert!(
            out.trace.iter().any(|(_, l)| l.starts_with(&line)),
            "{:?}",
            out.trace
        );
        assert_eq!(pool.allocated_bytes(), 0);
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

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
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

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
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
        writer.open_file(&path, NDFileOpenMode::WRITE, &a).unwrap();
        writer.write_file(&Arc::new(a)).unwrap();
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
        if let NDDataViewMut::I16(v) = arr.data_mut().view_mut() {
            for (i, item) in v.iter_mut().enumerate() {
                *item = (i as i16) - 6;
            }
        }

        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        // Previously a hard error; must now succeed.
        writer.write_file(&Arc::new(arr)).unwrap();
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
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
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
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
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
        writer
            .open_file(&path, NDFileOpenMode::WRITE, array)
            .unwrap();
        writer.write_file(&Arc::new(array.clone())).unwrap();
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
                NDFileOpenMode::WRITE,
                &NDArray::new(vec![], NDDataType::UInt8),
            )
            .unwrap();
        let read_back = writer.read_file(&NDArrayPool::new(0)).unwrap();
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
            read_back.data().as_u8_slice(),
            rgb_array(NDColorMode::RGB3, w, h).data().as_u8_slice(),
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
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
            for (i, item) in v.iter_mut().enumerate() {
                *item = i as u8;
            }
        }

        proc.ctrl.lock().auto_save = false;
        let arr = Arc::new(arr);
        let _ = proc.process_array(&arr, &NDArrayPool::new(1024));
        assert!(!std::path::Path::new(&full_name).exists());

        proc.ctrl.lock().auto_save = true;
        let _ = proc.process_array(&arr, &NDArrayPool::new(1024));
        assert!(std::path::Path::new(&full_name).exists());

        std::fs::remove_file(&path).ok();
    }

    /// How a [`StripFile`] writes StripByteCounts.
    #[derive(Clone, Copy)]
    enum Counts {
        /// Each strip's geometry length.
        Geometry,
        Given(&'static [u32]),
        /// No StripByteCounts tag.
        Missing,
    }

    /// A file of the C comparison set, laid out as reviewer6's generator
    /// lays it out so that C's reads of those files pin this reader: the
    /// strips from offset 8, `pad` 0xee bytes, the out-of-line tag arrays,
    /// then the IFD. Sample `i` is `i*7+1`, `i*257+3` or `i*16843009+5`.
    #[derive(Clone, Copy)]
    struct StripFile {
        width: u32,
        height: u32,
        rows: u32,
        counts: Counts,
        pad: usize,
        samples: u16,
        separate: bool,
        bits: u16,
        big_endian: bool,
        /// Cuts StripOffsets to this length, or extends it.
        offsets_len: Option<usize>,
        offsets: Option<&'static [u32]>,
        /// Replaces a tag's written value.
        overrides: &'static [(u16, u32)],
        omit: &'static [u16],
    }

    impl Default for StripFile {
        fn default() -> Self {
            Self {
                width: 4,
                height: 3,
                rows: 3,
                counts: Counts::Geometry,
                pad: 16,
                samples: 1,
                separate: false,
                bits: 8,
                big_endian: false,
                offsets_len: None,
                offsets: None,
                overrides: &[],
                omit: &[],
            }
        }
    }

    impl StripFile {
        fn short(&self, value: u16) -> [u8; 2] {
            if self.big_endian {
                value.to_be_bytes()
            } else {
                value.to_le_bytes()
            }
        }

        fn long(&self, value: u32) -> [u8; 4] {
            if self.big_endian {
                value.to_be_bytes()
            } else {
                value.to_le_bytes()
            }
        }

        fn file(&self) -> Vec<u8> {
            let rows = match self.rows {
                0 => self.height,
                rows => rows.min(self.height).max(1),
            };
            let samples = u32::from(self.samples);
            let (planes, row_samples) = if self.separate {
                (samples, self.width)
            } else {
                (1, self.width * samples)
            };
            let mut data = Vec::new();
            let (mut offsets, mut geometry) = (Vec::new(), Vec::new());
            for plane in 0..planes {
                for first in (0..self.height).step_by(rows as usize) {
                    offsets.push(8 + data.len() as u32);
                    let start = data.len();
                    for row in first..(first + rows).min(self.height) {
                        for x in 0..row_samples {
                            let i = if self.separate {
                                (row * self.width + x) * samples + plane
                            } else {
                                row * row_samples + x
                            };
                            match self.bits {
                                8 => data.push((i * 7 + 1) as u8),
                                16 => data.extend(self.short((i * 257 + 3) as u16)),
                                _ => data.extend(self.long(i * 16843009 + 5)),
                            }
                        }
                    }
                    geometry.push((data.len() - start) as u32);
                }
            }
            data.extend(std::iter::repeat_n(0xee, self.pad));
            let mut offsets = self.offsets.map_or(offsets, <[u32]>::to_vec);
            if let Some(len) = self.offsets_len {
                let next = offsets[offsets.len() - 1] + 4;
                offsets.resize(len, next);
            }
            let counts = match self.counts {
                Counts::Geometry => Some(geometry),
                Counts::Given(counts) => Some(counts.to_vec()),
                Counts::Missing => None,
            };
            let arrays_at = 8 + data.len() as u32;
            let mut arrays = Vec::new();
            let mut put = |values: &[u32], kind: u16| match values {
                [one] => *one,
                _ => {
                    let at = arrays_at + arrays.len() as u32;
                    for &value in values {
                        if kind == TYPE_SHORT {
                            arrays.extend(self.short(value as u16));
                        } else {
                            arrays.extend(self.long(value));
                        }
                    }
                    at
                }
            };
            let offsets_at = put(&offsets, TYPE_LONG);
            let counts_at = counts.as_deref().map(|c| put(c, TYPE_LONG));
            let bits = put(
                &vec![u32::from(self.bits); usize::from(self.samples)],
                TYPE_SHORT,
            );
            if arrays.len() % 2 == 1 {
                arrays.push(0);
            }
            let ifd = arrays_at + arrays.len() as u32;
            let photometric = if self.samples == 3 {
                PHOTOMETRIC_RGB
            } else {
                PHOTOMETRIC_MINISBLACK
            };
            let planar = if self.separate {
                PLANARCONFIG_SEPARATE
            } else {
                PLANARCONFIG_CONTIG
            };
            let mut entries = vec![
                (TAG_IMAGE_WIDTH, TYPE_LONG, 1, self.width),
                (TAG_IMAGE_LENGTH, TYPE_LONG, 1, self.height),
                (TAG_BITS_PER_SAMPLE, TYPE_SHORT, samples, bits),
                (TAG_COMPRESSION, TYPE_SHORT, 1, 1),
                (TAG_PHOTOMETRIC, TYPE_SHORT, 1, u32::from(photometric)),
                (
                    TAG_STRIP_OFFSETS,
                    TYPE_LONG,
                    offsets.len() as u32,
                    offsets_at,
                ),
                (TAG_SAMPLES_PER_PIXEL, TYPE_SHORT, 1, samples),
                (TAG_ROWS_PER_STRIP, TYPE_LONG, 1, self.rows),
            ];
            if let (Some(counts), Some(at)) = (&counts, counts_at) {
                entries.push((TAG_STRIP_BYTE_COUNTS, TYPE_LONG, counts.len() as u32, at));
            }
            entries.push((TAG_PLANAR_CONFIG, TYPE_SHORT, 1, u32::from(planar)));
            entries.push((TAG_SAMPLE_FORMAT, TYPE_SHORT, 1, 1));
            entries.retain(|(tag, ..)| !self.omit.contains(tag));
            let mut file = if self.big_endian { b"MM" } else { b"II" }.to_vec();
            file.extend(self.short(42));
            file.extend(self.long(ifd));
            file.extend(data);
            file.extend(arrays);
            file.extend(self.short(entries.len() as u16));
            for (tag, kind, n, value) in entries {
                let value = self
                    .overrides
                    .iter()
                    .find(|(t, _)| *t == tag)
                    .map_or(value, |(_, v)| *v);
                file.extend(self.short(tag));
                file.extend(self.short(kind));
                file.extend(self.long(n));
                // A lone SHORT sits in the first half of the value field.
                if kind == TYPE_SHORT && n == 1 {
                    file.extend(self.short(value as u16));
                    file.extend([0, 0]);
                } else {
                    file.extend(self.long(value));
                }
            }
            file.extend(self.long(0));
            file
        }
    }

    /// The buffer as C's comparison reader prints it: hex, each sample
    /// little-endian.
    fn sample_hex(array: &NDArray) -> String {
        let bytes: Vec<u8> = match array.data().view() {
            NDDataView::U8(v) => v.to_vec(),
            NDDataView::U16(v) => v.iter().flat_map(|s| s.to_le_bytes()).collect(),
            NDDataView::U32(v) => v.iter().flat_map(|s| s.to_le_bytes()).collect(),
            other => panic!("{other:?}"),
        };
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// C reads a compressed file through libtiff's codec; this reader has
    /// none, so it refuses one even when its strips fill the image.
    #[test]
    fn a_compressed_file_is_refused() {
        let none = StripFile::default();
        let lzw = StripFile {
            overrides: &[(TAG_COMPRESSION, 5)],
            ..none
        };
        let read = decode(&lzw.file(), NDAttributeList::new(), &NDArrayPool::new(0));
        assert!(read.is_err(), "{:?}", read.map(|a| sample_hex(&a)));
        assert!(decode(&none.file(), NDAttributeList::new(), &NDArrayPool::new(0)).is_ok());
    }

    /// Each file of the C comparison set reads as C `readFile` reads it with
    /// ADSupport's libtiff 4.0.6: `None` where C fails the read or the open,
    /// else C's buffer. libtiff 4.5.1 reads the same but for `huge_last_pad`,
    /// whose count of over 1 MiB it trims to what the strip needs.
    #[test]
    fn a_strip_table_reads_as_libtiff_reads_it() {
        use Counts::{Given, Missing};
        const GRAY: &str = "01080f161d242b323940474e";
        const GRAY_H5: &str = "01080f161d242b323940474e555c636a71787f86";
        const GRAY_H4: &str = "01080f161d242b323940474e555c636a";
        const GRAY16: &str = "03000401050206030704080509060a070b080c090d0a0e0b";
        const GRAY32: &str = "05000000060101010702020208030303090404040a0505050b0606060c0707070d0808080e0909090f0a0a0a100b0b0b";
        const RGB1: &str =
            "01080f161d242b323940474e555c636a71787f868d949ba2a9b0b7bec5ccd3dae1e8eff6";
        const RGB3: &str =
            "01162b40556a7f94a9bed3e8081d32475c71869bb0c5daef0f24394e63788da2b7cce1f6";
        const RGB1_16: &str = "03000401050206030704080509060a070b080c090d0a0e0b0f0c100d110e120f13101411151216131714181519161a171b181c191d1a1e1b1f1c201d211e221f2320242125222623";
        const RGB3_16: &str = "0300060309060c090f0c120f151218151b181e1b211e2421040107040a070d0a100d1310161319161c191f1c221f2522050208050b080e0b110e141117141a171d1a201d23202623";
        let s = StripFile::default();
        let one = StripFile { rows: 1, ..s };
        let h5 = StripFile {
            height: 5,
            rows: 2,
            ..s
        };
        let h4 = StripFile { height: 4, ..s };
        let rgb = StripFile { samples: 3, ..s };
        let be = StripFile {
            big_endian: true,
            ..s
        };
        let planes = StripFile {
            separate: true,
            ..rgb
        };
        #[rustfmt::skip]
        let cases = [
            ("ok1", StripFile { counts: Given(&[12]), ..s }, Some(GRAY)),
            ("ok3", StripFile { counts: Given(&[4, 4, 4]), ..one }, Some(GRAY)),
            ("over1", StripFile { counts: Given(&[16]), ..s }, Some(GRAY)),
            ("over3_last", StripFile { counts: Given(&[4, 4, 6]), ..one }, Some(GRAY)),
            ("short3_last", StripFile { counts: Given(&[4, 4, 2]), ..one }, None),
            ("rps2", StripFile { rows: 2, counts: Given(&[8, 4]), ..s }, Some(GRAY)),
            ("rps2_short", StripFile { rows: 2, counts: Given(&[8, 2]), ..s }, None),
            ("rps10", StripFile { rows: 10, counts: Given(&[12]), ..s }, Some(GRAY)),
            ("rps_absent", StripFile { omit: &[TAG_ROWS_PER_STRIP], ..s }, Some(GRAY)),
            ("h4r3_ok", h4, Some(GRAY_H4)),
            ("h4r3_last_short", StripFile { counts: Given(&[12, 3]), ..h4 }, None),
            ("h5r2_ok", h5, Some(GRAY_H5)),
            ("h5r2_same_last_over", StripFile { counts: Given(&[8, 8, 9]), ..h5 }, Some(GRAY_H5)),
            ("h5r2_same_last_short", StripFile { counts: Given(&[8, 8, 3]), ..h5 }, None),
            // "Bogus": one strip whose count is 0, short of the image or
            // past the file is estimated from the geometry.
            ("short1", StripFile { counts: Given(&[8]), ..s }, Some(GRAY)),
            ("short1_nobytes", StripFile { counts: Given(&[8]), pad: 0, ..s }, Some(GRAY)),
            ("over1_pasteof", StripFile { counts: Given(&[1000]), ..s }, Some(GRAY)),
            ("c0_single", StripFile { counts: Given(&[0]), ..s }, Some(GRAY)),
            ("single_off_past", StripFile { offsets: Some(&[100000]), ..s }, None),
            // "Wrong": more than two contiguous strips, the first two
            // counts differ and neither is 0. The estimate is the image's
            // rows over the strip count, rounded down, for every strip.
            ("short3_mid", StripFile { counts: Given(&[4, 2, 4]), ..one }, Some(GRAY)),
            ("over3_first_short_mid", StripFile { counts: Given(&[6, 2, 4]), ..one }, Some(GRAY)),
            ("h5r2_wrong", StripFile { counts: Given(&[8, 4, 4]), ..h5 }, None),
            ("c0_first_multi", StripFile { counts: Given(&[0, 4, 4]), ..one }, None),
            // The strip tables are cut or zero-filled to the strip count; a
            // zero offset reads the header.
            ("offs_short", StripFile { offsets_len: Some(2), ..one }, Some("01080f161d242b3249492a00")),
            ("offs_long", StripFile { offsets_len: Some(5), ..one }, Some(GRAY)),
            ("counts_short", StripFile { counts: Given(&[4, 4]), ..one }, None),
            ("counts_long", StripFile { counts: Given(&[4, 4, 4, 99]), ..one }, Some(GRAY)),
            ("counts_short_wrong", StripFile { counts: Given(&[6, 4]), ..one }, Some(GRAY)),
            // The end-of-file check takes the count, not the rows' length.
            ("huge_last_pad", StripFile { counts: Given(&[4, 4, 2000000]), pad: 5000, ..one }, None),
            ("huge_last_nopad", StripFile { counts: Given(&[4, 4, 2000000]), ..one }, None),
            // No StripByteCounts: estimated for one strip per plane, else
            // the open fails.
            ("nocounts1", StripFile { counts: Missing, ..s }, Some(GRAY)),
            ("nocounts3", StripFile { counts: Missing, ..one }, None),
            ("h5r2_nocounts", StripFile { counts: Missing, ..h5 }, None),
            ("no_offsets", StripFile { omit: &[TAG_STRIP_OFFSETS], ..s }, None),
            ("rps0", StripFile { rows: 0, counts: Given(&[12]), ..s }, None),
            ("w0", StripFile { overrides: &[(TAG_IMAGE_WIDTH, 0)], ..s }, None),
            ("h0", StripFile { overrides: &[(TAG_IMAGE_LENGTH, 0)], ..s }, None),
            ("u16_ok", StripFile { bits: 16, ..one }, Some(GRAY16)),
            ("u16_wrong", StripFile { bits: 16, counts: Given(&[8, 4, 8]), ..one }, Some(GRAY16)),
            ("u16_short1", StripFile { bits: 16, counts: Given(&[20]), ..s }, Some(GRAY16)),
            ("le_u32", StripFile { bits: 32, ..one }, Some(GRAY32)),
            // A big-endian file's samples come back in the host's order.
            ("be_ok", be, Some(GRAY)),
            ("be_u16", StripFile { rows: 1, bits: 16, ..be }, Some(GRAY16)),
            ("be_u16_short1", StripFile { bits: 16, counts: Given(&[20]), ..be }, Some(GRAY16)),
            ("be_u32", StripFile { rows: 1, bits: 32, ..be }, Some(GRAY32)),
            ("be_rgb1_u16", StripFile { samples: 3, rows: 2, bits: 16, ..be }, Some(RGB1_16)),
            ("be_rgb3_u16", StripFile { samples: 3, separate: true, bits: 16, ..be }, Some(RGB3_16)),
            ("rgb1_ok", StripFile { rows: 1, ..rgb }, Some(RGB1)),
            ("rgb1_wrong", StripFile { rows: 1, counts: Given(&[12, 6, 12]), ..rgb }, Some(RGB1)),
            ("rgb1_short1", StripFile { counts: Given(&[30]), ..rgb }, Some(RGB1)),
            ("rgb3_ok", planes, Some(RGB3)),
            // Separate planes: one strip per plane is estimated with the
            // plane's rows, and there is no "Wrong" re-estimate.
            ("rgb3_nocounts", StripFile { counts: Missing, ..planes }, Some(RGB3)),
            ("rgb3_mid_short", StripFile { counts: Given(&[12, 8, 12]), ..planes }, None),
            ("rgb3_mid_over", StripFile { counts: Given(&[16, 8, 12]), ..planes }, None),
            ("rgb3_r1_nocounts", StripFile { rows: 1, counts: Missing, ..planes }, None),
            (
                "rgb3_r1_diff",
                StripFile { rows: 1, counts: Given(&[4, 2, 4, 4, 4, 4, 4, 4, 4]), ..planes },
                None,
            ),
        ];
        let wrong: Vec<_> = cases
            .iter()
            .filter_map(|(name, file, c)| {
                let read = decode(&file.file(), NDAttributeList::new(), &NDArrayPool::new(0));
                let rs = read.as_ref().ok().map(sample_hex);
                (rs.as_deref() != *c).then(|| format!("{name}: {:?}", read.map(|a| sample_hex(&a))))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    const TYPE_RATIONAL: u16 = 5;
    const TYPE_FLOAT: u16 = 11;

    /// A file of reviewer6's tag-type set, written as their generator
    /// writes it so that C's reads of those files pin this reader: a 4-wide
    /// image whose sample bytes run `(j+1)*7` from offset 8, 16 0xee bytes,
    /// the out-of-line tag values, then the IFD. `types` rewrites a tag's
    /// entry type and `values` its values; `extra` entries follow the rest.
    #[derive(Clone, Copy)]
    struct TagFile {
        height: u32,
        rows: u32,
        samples: u16,
        separate: bool,
        bits: u16,
        format: u16,
        types: &'static [(u16, u16)],
        values: &'static [(u16, &'static [i64])],
        extra: &'static [(u16, u16, &'static [i64])],
    }

    impl Default for TagFile {
        fn default() -> Self {
            Self {
                height: 3,
                rows: 1,
                samples: 1,
                separate: false,
                bits: 8,
                format: 1,
                types: &[],
                values: &[],
                extra: &[],
            }
        }
    }

    impl TagFile {
        fn file(&self) -> Vec<u8> {
            let width = 4;
            let samples = u32::from(self.samples);
            let bytes = u32::from(self.bits / 8);
            let rows = self.rows.min(self.height).max(1);
            let (planes, row_samples) = if self.separate {
                (samples, width)
            } else {
                (1, width * samples)
            };
            let mut data = Vec::new();
            let (mut offsets, mut counts) = (Vec::new(), Vec::new());
            for plane in 0..planes {
                for first in (0..self.height).step_by(rows as usize) {
                    offsets.push(8 + data.len() as i64);
                    let start = data.len();
                    for row in first..(first + rows).min(self.height) {
                        for x in 0..row_samples {
                            let i = if self.separate {
                                (row * width + x) * samples + plane
                            } else {
                                row * row_samples + x
                            };
                            data.extend((0..bytes).map(|k| ((i * bytes + k + 1) * 7) as u8));
                        }
                    }
                    counts.push((data.len() - start) as i64);
                }
            }
            data.extend([0xee; 16]);
            let photometric = if self.samples == 3 {
                PHOTOMETRIC_RGB
            } else {
                PHOTOMETRIC_MINISBLACK
            };
            let planar = if self.separate {
                PLANARCONFIG_SEPARATE
            } else {
                PLANARCONFIG_CONTIG
            };
            let bits = vec![i64::from(self.bits); usize::from(self.samples)];
            let mut tags = vec![
                (TAG_IMAGE_WIDTH, TYPE_LONG, vec![i64::from(width)]),
                (TAG_IMAGE_LENGTH, TYPE_LONG, vec![i64::from(self.height)]),
                (TAG_BITS_PER_SAMPLE, TYPE_SHORT, bits),
                (TAG_COMPRESSION, TYPE_SHORT, vec![1]),
                (TAG_PHOTOMETRIC, TYPE_SHORT, vec![i64::from(photometric)]),
                (TAG_STRIP_OFFSETS, TYPE_LONG, offsets),
                (
                    TAG_SAMPLES_PER_PIXEL,
                    TYPE_SHORT,
                    vec![i64::from(self.samples)],
                ),
                (TAG_ROWS_PER_STRIP, TYPE_LONG, vec![i64::from(self.rows)]),
                (TAG_STRIP_BYTE_COUNTS, TYPE_LONG, counts),
                (TAG_PLANAR_CONFIG, TYPE_SHORT, vec![i64::from(planar)]),
                (TAG_SAMPLE_FORMAT, TYPE_SHORT, vec![i64::from(self.format)]),
            ];
            for (tag, kind, values) in &mut tags {
                if let Some(&(_, k)) = self.types.iter().find(|(t, _)| t == tag) {
                    *kind = k;
                }
                if let Some(&(_, v)) = self.values.iter().find(|(t, _)| t == tag) {
                    *values = v.to_vec();
                }
            }
            tags.extend(self.extra.iter().map(|&(t, k, v)| (t, k, v.to_vec())));
            let arrays_at = 8 + data.len();
            let mut arrays = Vec::new();
            let mut ifd = (tags.len() as u16).to_le_bytes().to_vec();
            for (tag, kind, values) in tags {
                let size = match kind {
                    TYPE_BYTE | TYPE_SBYTE | TYPE_ASCII => 1,
                    TYPE_SHORT | TYPE_SSHORT => 2,
                    TYPE_LONG | TYPE_SLONG | TYPE_FLOAT => 4,
                    _ => 8,
                };
                let raw: Vec<u8> = values
                    .iter()
                    .flat_map(|v| v.to_le_bytes()[..size].to_vec())
                    .collect();
                ifd.extend(tag.to_le_bytes());
                ifd.extend(kind.to_le_bytes());
                ifd.extend((values.len() as u32).to_le_bytes());
                if raw.len() <= 4 {
                    ifd.extend(&raw);
                    ifd.extend(std::iter::repeat_n(0, 4 - raw.len()));
                } else {
                    ifd.extend(((arrays_at + arrays.len()) as u32).to_le_bytes());
                    arrays.extend(raw);
                    if arrays.len() % 2 == 1 {
                        arrays.push(0);
                    }
                }
            }
            ifd.extend(0u32.to_le_bytes());
            let mut file = b"II".to_vec();
            file.extend(42u16.to_le_bytes());
            file.extend(((arrays_at + arrays.len()) as u32).to_le_bytes());
            file.extend(data);
            file.extend(arrays);
            file.extend(ifd);
            file
        }
    }

    /// A read as C's comparison reader prints it: the type, the dims, then
    /// each sample's bytes little-endian.
    fn c_read(array: &NDArray) -> String {
        let bytes: Vec<u8> = match array.data().view() {
            NDDataView::U8(v) => v.to_vec(),
            NDDataView::U16(v) => v.iter().flat_map(|s| s.to_le_bytes()).collect(),
            NDDataView::I16(v) => v.iter().flat_map(|s| s.to_le_bytes()).collect(),
            other => panic!("{other:?}"),
        };
        let dims: Vec<_> = array.dims.iter().map(|d| d.size.to_string()).collect();
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        format!("{:?} {} {hex}", array.data().data_type(), dims.join(","))
    }

    /// Each file reads as C `readFile` reads it with ADSupport's libtiff
    /// 4.0.6: `None` where the open or a strip read fails, else C's type, dims
    /// and buffer. The `le_tt_*` files are reviewer6's; the rest cover the
    /// readers' other types, ranges and counts.
    #[test]
    fn tag_entries_read_as_libtiff_reads_them() {
        const GRAY: &str = "UInt8 4,3 070e151c232a31383f464d54";
        const GRAY_H4: &str = "UInt8 4,4 070e151c232a31383f464d545b626970";
        const GRAY16_H4: &str =
            "UInt16 4,4 070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9e0";
        const GRAY16: &str = "UInt16 4,3 070e151c232a31383f464d545b626970777e858c939aa1a8";
        const RGB1_16: &str = "UInt16 3,4,3 070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc030a11181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1f8";
        const RGB1: &str =
            "UInt8 3,4,3 070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9e0e7eef5fc";
        const RGB3: &str =
            "UInt8 4,3,3 071c31465b70859aafc4d9ee0e23384d62778ca1b6cbe0f5152a3f54697e93a8bdd2e7fc";
        const INT16: &str = "Int16 4,3 070e151c232a31383f464d545b626970777e858c939aa1a8";
        let s = TagFile::default();
        let rgb = TagFile { samples: 3, ..s };
        #[rustfmt::skip]
        let cases = [
            ("le_tt_w_short", TagFile { types: &[(TAG_IMAGE_WIDTH, TYPE_SHORT)], ..s }, Some(GRAY)),
            ("le_tt_h_short", TagFile { types: &[(TAG_IMAGE_LENGTH, TYPE_SHORT)], ..s }, Some(GRAY)),
            ("le_tt_rps_short", TagFile { types: &[(TAG_ROWS_PER_STRIP, TYPE_SHORT)], ..s }, Some(GRAY)),
            ("le_tt_offs_short3", TagFile { types: &[(TAG_STRIP_OFFSETS, TYPE_SHORT)], ..s }, Some(GRAY)),
            ("le_tt_counts_short3", TagFile { types: &[(TAG_STRIP_BYTE_COUNTS, TYPE_SHORT)], ..s }, Some(GRAY)),
            ("le_tt_both_short2", TagFile { height: 4, rows: 2, types: &[(TAG_STRIP_OFFSETS, TYPE_SHORT), (TAG_STRIP_BYTE_COUNTS, TYPE_SHORT)], ..s }, Some(GRAY_H4)),
            ("le_tt_both_short2_u16", TagFile { height: 4, rows: 2, bits: 16, types: &[(TAG_STRIP_OFFSETS, TYPE_SHORT), (TAG_STRIP_BYTE_COUNTS, TYPE_SHORT)], ..s }, Some(GRAY16_H4)),
            ("le_tt_counts_short1", TagFile { rows: 3, types: &[(TAG_STRIP_BYTE_COUNTS, TYPE_SHORT)], ..s }, Some(GRAY)),
            ("le_tt_offs_short1", TagFile { rows: 3, types: &[(TAG_STRIP_OFFSETS, TYPE_SHORT)], ..s }, Some(GRAY)),
            ("le_tt_bps_long", TagFile { bits: 16, types: &[(TAG_BITS_PER_SAMPLE, TYPE_LONG)], ..s }, Some(GRAY16)),
            ("le_tt_bps_long_rgb", TagFile { bits: 16, rows: 2, types: &[(TAG_BITS_PER_SAMPLE, TYPE_LONG)], ..rgb }, Some(RGB1_16)),
            ("le_tt_spp_long", TagFile { types: &[(TAG_SAMPLES_PER_PIXEL, TYPE_LONG)], ..rgb }, Some(RGB1)),
            ("le_tt_pm_long", TagFile { types: &[(TAG_PHOTOMETRIC, TYPE_LONG)], ..s }, Some(GRAY)),
            ("le_tt_pc_long", TagFile { separate: true, types: &[(TAG_PLANAR_CONFIG, TYPE_LONG)], ..rgb }, Some(RGB3)),
            ("le_tt_sf_long", TagFile { bits: 16, format: 2, types: &[(TAG_SAMPLE_FORMAT, TYPE_LONG)], ..s }, Some(INT16)),
            ("le_tt_comp_long", TagFile { types: &[(TAG_COMPRESSION, TYPE_LONG)], ..s }, Some(GRAY)),
            ("le_tt_pm_byte", TagFile { types: &[(TAG_PHOTOMETRIC, TYPE_BYTE)], ..s }, Some(GRAY)),
            ("le_tt_sf_sshort", TagFile { bits: 16, format: 2, types: &[(TAG_SAMPLE_FORMAT, TYPE_SSHORT)], ..s }, Some(INT16)),
            ("le_tt_w_slong", TagFile { types: &[(TAG_IMAGE_WIDTH, TYPE_SLONG)], ..s }, Some(GRAY)),
            ("le_tt_rps_byte", TagFile { types: &[(TAG_ROWS_PER_STRIP, TYPE_BYTE)], ..s }, Some(GRAY)),
            ("le_tt_bps_byte", TagFile { bits: 16, types: &[(TAG_BITS_PER_SAMPLE, TYPE_BYTE)], ..s }, Some(GRAY16)),
            ("w_sbyte", TagFile { types: &[(TAG_IMAGE_WIDTH, TYPE_SBYTE)], ..s }, Some(GRAY)),
            ("w_slong8", TagFile { types: &[(TAG_IMAGE_WIDTH, TYPE_SLONG8)], ..s }, Some(GRAY)),
            ("h_sshort", TagFile { types: &[(TAG_IMAGE_LENGTH, TYPE_SSHORT)], ..s }, Some(GRAY)),
            ("h_long8", TagFile { types: &[(TAG_IMAGE_LENGTH, TYPE_LONG8)], ..s }, Some(GRAY)),
            ("spp_sshort", TagFile { types: &[(TAG_SAMPLES_PER_PIXEL, TYPE_SSHORT)], ..rgb }, Some(RGB1)),
            ("pc_byte", TagFile { separate: true, types: &[(TAG_PLANAR_CONFIG, TYPE_BYTE)], ..rgb }, Some(RGB3)),
            ("offs_byte", TagFile { types: &[(TAG_STRIP_OFFSETS, TYPE_BYTE)], ..s }, Some(GRAY)),
            ("offs_long8", TagFile { types: &[(TAG_STRIP_OFFSETS, TYPE_LONG8)], ..s }, Some(GRAY)),
            ("counts_sbyte", TagFile { types: &[(TAG_STRIP_BYTE_COUNTS, TYPE_SBYTE)], ..s }, Some(GRAY)),
            ("counts_slong8", TagFile { types: &[(TAG_STRIP_BYTE_COUNTS, TYPE_SLONG8)], ..s }, Some(GRAY)),
            ("bps_sbyte_rgb", TagFile { types: &[(TAG_BITS_PER_SAMPLE, TYPE_SBYTE)], ..rgb }, Some(RGB1)),
            ("w_ascii", TagFile { types: &[(TAG_IMAGE_WIDTH, TYPE_ASCII)], ..s }, None),
            ("h_rational", TagFile { types: &[(TAG_IMAGE_LENGTH, TYPE_RATIONAL)], ..s }, None),
            ("bps_double", TagFile { types: &[(TAG_BITS_PER_SAMPLE, TYPE_DOUBLE)], ..s }, None),
            ("sf_float", TagFile { bits: 16, format: 2, types: &[(TAG_SAMPLE_FORMAT, TYPE_FLOAT)], ..s }, None),
            ("w_negative", TagFile { types: &[(TAG_IMAGE_WIDTH, TYPE_SLONG)], values: &[(TAG_IMAGE_WIDTH, &[-4])], ..s }, None),
            ("sf_negative", TagFile { bits: 16, types: &[(TAG_SAMPLE_FORMAT, TYPE_SSHORT)], values: &[(TAG_SAMPLE_FORMAT, &[-2])], ..s }, None),
            ("offs_negative", TagFile { types: &[(TAG_STRIP_OFFSETS, TYPE_SLONG)], values: &[(TAG_STRIP_OFFSETS, &[8, -12, 16])], ..s }, None),
            ("bps_over_short", TagFile { types: &[(TAG_BITS_PER_SAMPLE, TYPE_LONG)], values: &[(TAG_BITS_PER_SAMPLE, &[65544])], ..s }, None),
            ("pc_over_short", TagFile { types: &[(TAG_PLANAR_CONFIG, TYPE_LONG)], values: &[(TAG_PLANAR_CONFIG, &[65537])], ..s }, None),
            ("w_two", TagFile { values: &[(TAG_IMAGE_WIDTH, &[4, 4])], ..s }, None),
            ("spp_two", TagFile { values: &[(TAG_SAMPLES_PER_PIXEL, &[1, 1])], ..s }, None),
            ("spp0", TagFile { values: &[(TAG_SAMPLES_PER_PIXEL, &[0])], ..s }, None),
            ("bps_one_rgb", TagFile { values: &[(TAG_BITS_PER_SAMPLE, &[8])], ..rgb }, Some(RGB1)),
            ("bps_two_rgb", TagFile { values: &[(TAG_BITS_PER_SAMPLE, &[8, 8])], ..rgb }, None),
            ("bps_four_rgb", TagFile { values: &[(TAG_BITS_PER_SAMPLE, &[8, 8, 8, 16])], ..rgb }, Some(RGB1)),
            ("bps_mixed_rgb", TagFile { values: &[(TAG_BITS_PER_SAMPLE, &[8, 8, 16])], ..rgb }, None),
            ("sf_mixed_rgb", TagFile { values: &[(TAG_SAMPLE_FORMAT, &[1, 1, 2])], ..rgb }, None),
            ("sf_three_rgb", TagFile { values: &[(TAG_SAMPLE_FORMAT, &[1, 1, 1])], ..rgb }, Some(RGB1)),
            ("dup_pm", TagFile { extra: &[(TAG_PHOTOMETRIC, TYPE_SHORT, &[2])], ..s }, Some(GRAY)),
            ("dup_w", TagFile { extra: &[(TAG_IMAGE_WIDTH, TYPE_LONG, &[5])], ..s }, Some(GRAY)),
            ("dup_sf", TagFile { bits: 16, extra: &[(TAG_SAMPLE_FORMAT, TYPE_SHORT, &[2])], ..s }, Some(GRAY16)),
            ("comp_lzw", TagFile { values: &[(TAG_COMPRESSION, &[5])], ..s }, None),
            ("comp_packbits", TagFile { values: &[(TAG_COMPRESSION, &[32773])], ..s }, None),
            ("comp_unknown", TagFile { values: &[(TAG_COMPRESSION, &[12345])], ..s }, None),
            ("comp_byte", TagFile { types: &[(TAG_COMPRESSION, TYPE_BYTE)], ..s }, Some(GRAY)),
            ("comp_ascii", TagFile { types: &[(TAG_COMPRESSION, TYPE_ASCII)], ..s }, None),
            ("comp_negative", TagFile { types: &[(TAG_COMPRESSION, TYPE_SSHORT)], values: &[(TAG_COMPRESSION, &[-1])], ..s }, None),
            ("comp_two", TagFile { values: &[(TAG_COMPRESSION, &[1, 1])], ..s }, Some(GRAY)),
            ("comp_three_rgb", TagFile { values: &[(TAG_COMPRESSION, &[1, 1, 1])], ..rgb }, Some(RGB1)),
            ("comp_two_rgb", TagFile { values: &[(TAG_COMPRESSION, &[1, 1])], ..rgb }, None),
            ("comp_mixed_rgb", TagFile { values: &[(TAG_COMPRESSION, &[1, 1, 5])], ..rgb }, None),
            ("comp_lzw_rgb", TagFile { values: &[(TAG_COMPRESSION, &[5, 5, 5])], ..rgb }, None),
            ("sf0", TagFile { format: 0, ..s }, None),
            ("sf0_rgb", TagFile { values: &[(TAG_SAMPLE_FORMAT, &[0, 0, 0])], ..rgb }, None),
            ("sf4", TagFile { format: 4, ..s }, None),
            ("sf6", TagFile { format: 6, ..s }, None),
            ("sf7", TagFile { format: 7, ..s }, None),
            ("pc0", TagFile { values: &[(TAG_PLANAR_CONFIG, &[0])], ..s }, None),
            ("pc3", TagFile { values: &[(TAG_PLANAR_CONFIG, &[3])], ..s }, None),
            ("pc3_rgb", TagFile { values: &[(TAG_PLANAR_CONFIG, &[3])], ..rgb }, None),
        ];
        let wrong: Vec<_> = cases
            .iter()
            .filter_map(|(name, file, c): &(_, _, Option<&str>)| {
                let read = decode(&file.file(), NDAttributeList::new(), &NDArrayPool::new(0))
                    .map(|a| c_read(&a));
                (read.as_deref().ok() != *c).then(|| format!("{name}: {read:?}"))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}
