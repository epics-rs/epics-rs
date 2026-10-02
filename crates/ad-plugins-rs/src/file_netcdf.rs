use std::path::{Path, PathBuf};
use std::sync::Arc;

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue};
use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::finalize::Finalize;
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{FileAttributes, NDFileMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::FilePluginController;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamChangeResult, PluginParamSnapshot, ProcessResult,
};

use parking_lot::Mutex;
use rust_hdf5::format::messages::datatype::DatatypeMessage;
use rust_hdf5::{H5Dataset, H5File};

const VAR_NAME: &str = "array_data";
/// The frame axis. netCDF-3 named this dimension `numArrays` and made it
/// NC_UNLIMITED for a multi-array file; here it is the leading axis of every
/// dataset and the name of its dimension scale.
const DIM_UNLIMITED: &str = "numArrays";
/// File-format version written as the NDNetCDFFileVersion global attribute so
/// readers can gate on format changes (C NDFileNetCDF.h:19 `#define
/// NDNetCDFFileVersion 3.1`).
const ND_NETCDF_FILE_VERSION: f64 = 3.1;
/// Provenance, in the `version=2,<library>=<version>` form libnetcdf writes.
/// netCDF-4 is an HDF5 container, and this attribute is how a reader learns
/// which library produced the file.
const NC_PROPERTIES: &str = concat!(
    "version=2,ad-plugins-rs=",
    env!("CARGO_PKG_VERSION"),
    ",rust-hdf5=0.6"
);
/// The `NAME` a dimension scale carries when it is a dimension only and not a
/// coordinate variable. netcdf-c writes this exact text, the length
/// right-aligned in ten columns (nc4hdf5.c `dimscale_wo_var`), and reads it
/// back to tell a pure dimension from a coordinate variable.
const DIM_WITHOUT_VARIABLE: &str = "This is a netCDF dimension but not a netCDF variable.";
/// Fixed field width of a string-valued attribute dataset's element, C
/// `MAX_ATTRIBUTE_STRING_SIZE` (netCDF-3 spelled the same limit as the
/// `attrStringSize` dimension).
const ATTR_STRING_SIZE: usize = 256;
/// Records the exact `NDDataType` ordinal on `array_data`, so read-back does
/// not have to infer the type from the HDF5 element width. netCDF-4 has the
/// full signed/unsigned set, so this only disambiguates what the file already
/// says; the root-level `dataType` global attribute carries the same value for
/// netCDF readers, which have no per-variable place to look.
const DTYPE_ATTR: &str = "dataType";

/// A single captured NDAttribute, preserving its typed value and metadata.
struct AttrData {
    name: String,
    description: String,
    /// Source string (e.g. PV name), C++ `getSource()`.
    source: String,
    /// C++ `getSourceInfo()` source-type string.
    source_type: String,
    /// C++ `dataTypeString` (e.g. "Int32", "Float64", "String").
    data_type_string: String,
    value: NDAttrValue,
}

/// A single buffered frame: the NDArray itself (held, not copied, the way
/// C keeps a reserved `NDArray*`) plus the attribute list as it stood when
/// the frame arrived.
struct FrameData {
    frame: Arc<NDArray>,
    attrs: Vec<AttrData>,
}

/// A fixed-length, null-terminated ASCII string of `N` bytes — libhdf5's
/// `H5Tcopy(H5T_C_S1)` + `H5Tset_size(N)` + `H5Tset_strpad(H5T_STR_NULLTERM)`,
/// which is what `H5LT_set_attribute_string` builds (H5LT.c:3355-3363) and the
/// only string shape the dimension-scale API accepts.
///
/// Both users need exactly this and not a variable-length string: a
/// string-valued NDAttribute was a 2-D NC_CHAR variable `[numArrays,
/// attrStringSize]` in netCDF-3, whose netCDF-4 equivalent is a rank-1 dataset
/// of `H5T_C_S1` sized to `ATTR_STRING_SIZE`; and `H5DSis_scale` reads a
/// scale's `CLASS` with a fixed-size expectation (see `create_dim_scale`).
#[derive(Clone, Copy)]
#[repr(transparent)]
struct FixedStr<const N: usize>([u8; N]);

impl<const N: usize> FixedStr<N> {
    /// Truncate to the field width, keeping room for the terminator, as C's
    /// `strncpy` into a `MAX_ATTRIBUTE_STRING_SIZE` field does.
    fn new(s: &str) -> Self {
        let mut bytes = [0u8; N];
        let src = s.as_bytes();
        let n = src.len().min(N - 1);
        bytes[..n].copy_from_slice(&src[..n]);
        Self(bytes)
    }
}

impl<const N: usize> rust_hdf5::types::H5Type for FixedStr<N> {
    fn hdf5_type() -> DatatypeMessage {
        DatatypeMessage::fixed_string(N as u32)
    }

    fn element_size() -> usize {
        N
    }
}

/// Map an `NDAttrSource` to the C++ `sourceTypeString_` label
/// (NDAttribute.cpp:48-67), written by `getSourceInfo()`.
fn attr_source_type_string(src: &NDAttrSource) -> &'static str {
    match src {
        NDAttrSource::Driver => "NDAttrSourceDriver",
        NDAttrSource::EpicsPV(_) => "NDAttrSourceEPICSPV",
        NDAttrSource::Param { .. } => "NDAttrSourceParam",
        NDAttrSource::Function(_) => "NDAttrSourceFunct",
        NDAttrSource::Constant(_) => "NDAttrSourceConst",
        NDAttrSource::Undefined => "Undefined",
    }
}

/// C++ `dataTypeString` for an NDAttribute value (NDFileNetCDF.cpp:213-258).
fn attr_data_type_string(value: &NDAttrValue) -> &'static str {
    match value {
        NDAttrValue::Int8(_) => "Int8",
        NDAttrValue::UInt8(_) => "UInt8",
        NDAttrValue::Int16(_) => "Int16",
        NDAttrValue::UInt16(_) => "UInt16",
        NDAttrValue::Int32(_) => "Int32",
        NDAttrValue::UInt32(_) => "UInt32",
        NDAttrValue::Int64(_) => "Int64",
        NDAttrValue::UInt64(_) => "UInt64",
        NDAttrValue::Float32(_) => "Float32",
        NDAttrValue::Float64(_) => "Float64",
        NDAttrValue::String(_) => "String",
        NDAttrValue::Undefined => "Undefined",
    }
}

/// netCDF-4 file writer.
///
/// netCDF-4 is an HDF5 container, so the file is written through `rust-hdf5`
/// rather than a netCDF library: `array_data` and the per-array metadata are
/// HDF5 datasets, the netCDF global attributes are root-group attributes, and
/// each dimension gets an HDF5 dimension scale.
///
/// Frames are buffered and the file is written in `close_file`, as the
/// netCDF-3 writer had to do. HDF5 can extend a dataset per frame, so this is
/// no longer forced by the format; changing it would change what
/// `writes_incrementally` promises and is left alone here.
pub struct NetcdfWriter {
    current_path: Option<PathBuf>,
    frames: Vec<FrameData>,
    /// C's `openMode & NDFileModeMultiple` (NDFileNetCDF.cpp:118) — the sole
    /// input to the numArrays dimension. NDPluginFile passes the Multiple bit
    /// for Capture and Stream and withholds it for Single (NDPluginFile.cpp:245,
    /// :281, :335), so it is fixed when the file is opened and cannot be
    /// re-derived later from how many frames happened to arrive: a Capture file
    /// that captured exactly one frame still has an extensible frame axis.
    open_multiple: bool,
    /// C `pFileAttributes` (NDFileNetCDF.cpp:72, :362): the writer's own
    /// attribute list, sticky for the life of the file.
    file_attributes: FileAttributes,
}

impl NetcdfWriter {
    pub fn new() -> Self {
        Self {
            current_path: None,
            frames: Vec::new(),
            open_multiple: false,
            file_attributes: FileAttributes::default(),
        }
    }
}

fn map_h5(e: rust_hdf5::Hdf5Error) -> ADError {
    ADError::UnsupportedConversion(format!("netCDF-4 write error: {e}"))
}

/// Create one dimension scale: a rank-1 dataset of `len` whose values are
/// never written, which is what a netCDF dimension with no coordinate variable
/// is. `set_scale` writes the `CLASS`/`NAME` pair in the fixed-length form
/// `H5DSis_scale` requires, and the returned handle is what the data variables
/// attach to.
fn create_dim_scale(
    file: &H5File,
    name: &str,
    len: usize,
    extensible: bool,
) -> ADResult<H5Dataset> {
    let mut builder = file.new_dataset::<f32>().shape(&[len][..]);
    if extensible {
        builder = builder.chunk(&[len.max(1)]).max_shape(&[None]);
    }
    let ds = builder.create(name).map_err(map_h5)?;
    ds.set_scale(Some(&format!("{DIM_WITHOUT_VARIABLE}{len:10}")))
        .map_err(map_h5)?;
    Ok(ds)
}

/// Create a `[frames]` dataset and fill it with one value per frame.
fn write_per_frame<T: rust_hdf5::types::H5Type + Copy>(
    file: &H5File,
    name: &str,
    values: &[T],
    extensible: bool,
) -> ADResult<H5Dataset> {
    let mut builder = file.new_dataset::<T>().shape(&[values.len()][..]);
    // HDF5 only extends a chunked dataset, so an unlimited maximum extent
    // comes with a chunk shape.
    if extensible {
        builder = builder.chunk(&[values.len().max(1)]).max_shape(&[None]);
    }
    let ds = builder.create(name).map_err(map_h5)?;
    ds.write_raw(values).map_err(map_h5)?;
    Ok(ds)
}

/// Create `array_data` with the leading frame axis and write every frame into
/// it.
///
/// C `NDFileNetCDF` gives `array_data` the `numArrays` dimension even for a
/// single-array file (NDFileNetCDF.cpp:202-204), and reverses the NDArray
/// dimensions because a netCDF file's first dimension varies slowest
/// (:123-132). Both hold here: the HDF5 dataspace is
/// `[frames, dim[n-1], … dim[0]]`, chunked one frame deep so each frame is one
/// chunk write.
fn write_array_data(file: &H5File, frames: &[FrameData], extensible: bool) -> ADResult<H5Dataset> {
    let first = &frames[0];
    let dims = &first.frame.dims;
    let mut shape = vec![frames.len()];
    shape.extend(dims.iter().rev().map(|d| d.size));
    let mut chunk = vec![1usize];
    chunk.extend(dims.iter().rev().map(|d| d.size));
    let dtype_ordinal = first.frame.data.data_type() as i32;
    // Unlimited on the frame axis only; the frame's own dimensions are fixed.
    let mut max_shape: Vec<Option<usize>> = shape.iter().map(|&s| Some(s)).collect();
    max_shape[0] = None;

    macro_rules! write_typed {
        ($t:ty, $variant:ident) => {{
            let mut builder = file.new_dataset::<$t>().shape(&shape[..]).chunk(&chunk[..]);
            if extensible {
                builder = builder.max_shape(&max_shape[..]);
            }
            let ds = builder.create(VAR_NAME).map_err(map_h5)?;
            ds.new_attr::<i32>()
                .shape(())
                .create(DTYPE_ATTR)
                .and_then(|a| a.write_numeric(&dtype_ordinal))
                .map_err(map_h5)?;
            let mut starts = vec![0usize; shape.len()];
            let mut counts = shape.clone();
            counts[0] = 1;
            for (i, frame) in frames.iter().enumerate() {
                let NDDataBuffer::$variant(v) = &frame.frame.data else {
                    return Err(ADError::UnsupportedConversion(format!(
                        "frame {i} changed element type mid-file"
                    )));
                };
                starts[0] = i;
                ds.write_slice::<$t>(&starts, &counts, v).map_err(map_h5)?;
            }
            Ok(ds)
        }};
    }

    // netCDF-4 has the full signed/unsigned integer set, so every NDArray type
    // is stored as itself. netCDF-3 had neither unsigned nor 64-bit integers,
    // which is why C reinterprets UInt8 as NC_BYTE and casts Int64/UInt64 to
    // NC_DOUBLE (NDFileNetCDF.cpp:154-180); none of that is needed here, and
    // the 64-bit integer frames are no longer rounded through a double.
    match first.frame.data {
        NDDataBuffer::I8(_) => write_typed!(i8, I8),
        NDDataBuffer::U8(_) => write_typed!(u8, U8),
        NDDataBuffer::I16(_) => write_typed!(i16, I16),
        NDDataBuffer::U16(_) => write_typed!(u16, U16),
        NDDataBuffer::I32(_) => write_typed!(i32, I32),
        NDDataBuffer::U32(_) => write_typed!(u32, U32),
        NDDataBuffer::I64(_) => write_typed!(i64, I64),
        NDDataBuffer::U64(_) => write_typed!(u64, U64),
        NDDataBuffer::F32(_) => write_typed!(f32, F32),
        NDDataBuffer::F64(_) => write_typed!(f64, F64),
    }
}

/// Create one `Attr_<name>` dataset holding that attribute's value for every
/// frame, typed as the attribute is (C NDFileNetCDF.cpp:312-321).
fn write_attr_dataset(
    file: &H5File,
    name: &str,
    values: &[NDAttrValue],
    extensible: bool,
) -> ADResult<Option<H5Dataset>> {
    macro_rules! numeric {
        ($t:ty, $get:expr) => {{
            let column: Vec<$t> = values.iter().map($get).collect();
            write_per_frame(file, name, &column, extensible).map(Some)
        }};
    }

    let as_i = |v: &NDAttrValue| v.as_i64().unwrap_or(0);
    let as_f = |v: &NDAttrValue| v.as_f64().unwrap_or(0.0);
    match &values[0] {
        NDAttrValue::Int8(_) => numeric!(i8, |v| as_i(v) as i8),
        NDAttrValue::UInt8(_) => numeric!(u8, |v| as_i(v) as u8),
        NDAttrValue::Int16(_) => numeric!(i16, |v| as_i(v) as i16),
        NDAttrValue::UInt16(_) => numeric!(u16, |v| as_i(v) as u16),
        NDAttrValue::Int32(_) => numeric!(i32, |v| as_i(v) as i32),
        NDAttrValue::UInt32(_) => numeric!(u32, |v| as_i(v) as u32),
        NDAttrValue::Int64(_) => numeric!(i64, as_i),
        NDAttrValue::UInt64(_) => numeric!(u64, |v| as_i(v) as u64),
        NDAttrValue::Float32(_) => numeric!(f32, |v| as_f(v) as f32),
        NDAttrValue::Float64(_) => numeric!(f64, as_f),
        NDAttrValue::String(_) => {
            let column: Vec<FixedStr<ATTR_STRING_SIZE>> = values
                .iter()
                .map(|v| FixedStr::new(&v.as_string()))
                .collect();
            write_per_frame(file, name, &column, extensible).map(Some)
        }
        // C skips an undefined attribute rather than defining a variable for
        // it (NDFileNetCDF.cpp:254-258 leaves `dataTypeString` "Undefined"
        // and :312 defines nothing).
        NDAttrValue::Undefined => Ok(None),
    }
}

impl NDFileWriter for NetcdfWriter {
    fn open_file(&mut self, path: &Path, mode: NDFileMode, array: &NDArray) -> ADResult<()> {
        self.current_path = Some(path.to_path_buf());
        self.frames.clear();
        // C `openFile` clears `pFileAttributes` and copies this frame's list
        // into it (NDFileNetCDF.cpp:72-77).
        self.file_attributes.open(array);
        // C: NDPluginFile opens Single with `NDFileModeWrite` and Capture/Stream
        // with `NDFileModeWrite | NDFileModeMultiple` (NDPluginFile.cpp:245, :281,
        // :335) — this writer reports supportsMultipleArrays, so those two modes
        // always carry the Multiple bit.
        self.open_multiple = mode != NDFileMode::Single;
        Ok(())
    }

    fn write_file(&mut self, array: &Arc<NDArray>) -> ADResult<()> {
        // C `writeFile` merges this frame into `pFileAttributes` and then
        // writes every attribute variable out of that list
        // (NDFileNetCDF.cpp:359-362, :419-483), so an attribute that drops out
        // of a later frame goes on record at its most recent value rather than
        // at the first frame's.
        self.file_attributes.frame(array);
        let attrs: Vec<AttrData> = self
            .file_attributes
            .iter()
            .map(|a| AttrData {
                name: a.name.clone(),
                description: a.description.clone(),
                // C `NDFileNetCDF` writes `NDAttribute::getSource()` verbatim
                // (NDFileNetCDF.cpp getAttributesFromFile); never synthesize it.
                source: a.source.source_string().to_string(),
                source_type: attr_source_type_string(&a.source).to_string(),
                data_type_string: attr_data_type_string(&a.value).to_string(),
                value: a.value.clone(),
            })
            .collect();

        self.frames.push(FrameData {
            frame: Arc::clone(array),
            attrs,
        });
        Ok(())
    }

    /// Close the open file — this is where the whole file is written, from the
    /// frames `write_file` buffered.
    ///
    /// The buffer belongs to the finalizer for the same reason `current_path`
    /// is taken up front: every write below is fallible, and frames of a file
    /// that will never exist must not stay resident until some later
    /// `open_file` happens to clear them.
    fn close_file(&mut self) -> ADResult<()> {
        let mut closing = Finalize::new(self, |w: &mut Self| w.frames.clear());
        closing.run(|w| {
            let path = match w.current_path.take() {
                Some(p) => p,
                None => return Ok(()),
            };

            if w.frames.is_empty() {
                return Ok(());
            }

            let frames = &w.frames;
            let first = &frames[0];
            let dims = &first.frame.dims;
            let ndims = dims.len();
            // C keys the extensible frame axis on the *open mode*, never on how
            // many frames the file ended up holding (NDFileNetCDF.cpp:117-119).
            // netCDF-3 spelled that NC_UNLIMITED; the netCDF-4 spelling of an
            // unlimited dimension is an HDF5 dataspace whose maximum extent on
            // that axis is unlimited.
            let extensible = w.open_multiple;

            let file = H5File::create(&path).map_err(map_h5)?;

            // --- Global attributes (C :92-101, :108-110, :140-151) ----------
            // The same set and values C writes, as root-group attributes:
            // that is where a netCDF-4 file keeps its global attributes.
            // `_NCProperties` is the one addition the container asks for.
            // HDF5 stores attributes in a name-indexed header, so unlike a
            // netCDF-3 header this order is not file format.
            file.set_attr_string("_NCProperties", NC_PROPERTIES)
                .map_err(map_h5)?;
            file.set_attr_numeric(DTYPE_ATTR, &(first.frame.data.data_type() as i32))
                .map_err(map_h5)?;
            file.set_attr_numeric("NDNetCDFFileVersion", &ND_NETCDF_FILE_VERSION)
                .map_err(map_h5)?;
            file.set_attr_numeric("numArrayDims", &(ndims as i32))
                .map_err(map_h5)?;
            // C reads dims[i] here — natural order, *not* the reversed order
            // the dimensions themselves are declared in (:125-131).
            let dim_size: Vec<i32> = dims.iter().map(|d| d.size as i32).collect();
            let dim_offset: Vec<i32> = dims.iter().map(|d| d.offset as i32).collect();
            let dim_binning: Vec<i32> = dims.iter().map(|d| d.binning as i32).collect();
            let dim_reverse: Vec<i32> = dims.iter().map(|d| i32::from(d.reverse)).collect();
            for (name, values) in [
                ("dimSize", &dim_size),
                ("dimOffset", &dim_offset),
                ("dimBinning", &dim_binning),
                ("dimReverse", &dim_reverse),
            ] {
                file.set_attr_array_numeric(name, &values[..])
                    .map_err(map_h5)?;
            }
            // The four text attributes C writes per NDAttribute (:208-310).
            for attr in &first.attrs {
                for (suffix, value) in [
                    ("DataType", &attr.data_type_string),
                    ("Description", &attr.description),
                    ("Source", &attr.source),
                    ("SourceType", &attr.source_type),
                ] {
                    file.set_attr_string(&format!("Attr_{}_{suffix}", attr.name), value)
                        .map_err(map_h5)?;
                }
            }

            // --- Dimensions (C :117-137) -----------------------------------
            // numArrays first, then the array dimensions reversed: netCDF's
            // first dimension varies slowest, the opposite of the NDArray
            // convention (:123-132).
            let frame_scale = create_dim_scale(&file, DIM_UNLIMITED, frames.len(), extensible)?;
            let mut dim_scales = Vec::with_capacity(ndims);
            for i in 0..ndims {
                dim_scales.push(create_dim_scale(
                    &file,
                    &format!("dim{i}"),
                    dims[ndims - 1 - i].size,
                    false,
                )?);
            }

            // --- Per-array metadata and the data (C :183-204) ---------------
            let unique_ids: Vec<i32> = frames.iter().map(|f| f.frame.unique_id).collect();
            let time_stamps: Vec<f64> = frames.iter().map(|f| f.frame.time_stamp).collect();
            let secs: Vec<i32> = frames
                .iter()
                .map(|f| f.frame.timestamp.sec as i32)
                .collect();
            let nsecs: Vec<i32> = frames
                .iter()
                .map(|f| f.frame.timestamp.nsec as i32)
                .collect();
            // Every variable's leading axis is the frame dimension, and
            // `array_data` carries the array dimensions after it: a netCDF
            // variable's dimensions are the dimension scales attached to its
            // axes, and an unattached axis is read as an anonymous
            // `phony_dim_N` instead of as `numArrays`.
            for ds in [
                write_per_frame(&file, "uniqueId", &unique_ids, extensible)?,
                write_per_frame(&file, "timeStamp", &time_stamps, extensible)?,
                write_per_frame(&file, "epicsTSSec", &secs, extensible)?,
                write_per_frame(&file, "epicsTSNsec", &nsecs, extensible)?,
            ] {
                ds.attach_scale(0, &frame_scale).map_err(map_h5)?;
            }
            let data = write_array_data(&file, frames, extensible)?;
            data.attach_scale(0, &frame_scale).map_err(map_h5)?;
            for (axis, scale) in dim_scales.iter().enumerate() {
                data.attach_scale(axis + 1, scale).map_err(map_h5)?;
            }

            // --- Per-attribute datasets (C :312-321) ------------------------
            // Every frame's `attrs` is a snapshot of the same sticky list,
            // which only ever grows and never re-orders, so column `i` is the
            // same attribute in every frame and there is no lookup left to
            // miss. The set is the first frame's: C snapshots the list at
            // openFile time and requires it not to change (C's comment
            // at :418).
            for (i, attr) in first.attrs.iter().enumerate() {
                let column: Vec<NDAttrValue> = frames
                    .iter()
                    .map(|f| {
                        f.attrs
                            .get(i)
                            .map_or(NDAttrValue::Undefined, |a| a.value.clone())
                    })
                    .collect();
                if let Some(ds) =
                    write_attr_dataset(&file, &format!("Attr_{}", attr.name), &column, extensible)?
                {
                    ds.attach_scale(0, &frame_scale).map_err(map_h5)?;
                }
            }

            // Not left to `Drop`: closing writes the superblock and the root
            // header, and that failure has to be reported.
            file.close().map_err(map_h5)
        })
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let file = H5File::open(path)
            .map_err(|e| ADError::UnsupportedConversion(format!("netCDF-4 open error: {e}")))?;
        let ds = file.dataset(VAR_NAME).map_err(|e| {
            ADError::UnsupportedConversion(format!("dataset '{VAR_NAME}' not found: {e}"))
        })?;

        // `array_data` is `[frames, dim[n-1], … dim[0]]`: drop the frame axis
        // and undo the reversal to get NDArray dimension order back.
        let shape = ds.shape();
        if shape.is_empty() {
            return Err(ADError::UnsupportedConversion(format!(
                "'{VAR_NAME}' has no frame axis"
            )));
        }
        let dims: Vec<NDDimension> = shape[1..]
            .iter()
            .rev()
            .map(|&s| NDDimension::new(s))
            .collect();

        let data_type = ds
            .attr(DTYPE_ATTR)
            .ok()
            .and_then(|a| a.read_numeric::<i32>().ok())
            .and_then(|v| NDDataType::from_ordinal(v as u8))
            .ok_or_else(|| {
                ADError::UnsupportedConversion(format!(
                    "'{VAR_NAME}' carries no {DTYPE_ATTR} attribute"
                ))
            })?;

        // The first frame only, as the netCDF-3 reader returned record 0.
        let mut starts = vec![0usize; shape.len()];
        starts[0] = 0;
        let mut counts = shape.clone();
        counts[0] = 1;

        macro_rules! read_typed {
            ($t:ty, $variant:ident) => {{
                let data = ds.read_slice::<$t>(&starts, &counts).map_err(|e| {
                    ADError::UnsupportedConversion(format!("netCDF-4 read error: {e}"))
                })?;
                let mut arr = NDArray::new(dims, data_type);
                arr.data = NDDataBuffer::$variant(data);
                Ok(arr)
            }};
        }

        match data_type {
            NDDataType::Int8 => read_typed!(i8, I8),
            NDDataType::UInt8 => read_typed!(u8, U8),
            NDDataType::Int16 => read_typed!(i16, I16),
            NDDataType::UInt16 => read_typed!(u16, U16),
            NDDataType::Int32 => read_typed!(i32, I32),
            NDDataType::UInt32 => read_typed!(u32, U32),
            NDDataType::Int64 => read_typed!(i64, I64),
            NDDataType::UInt64 => read_typed!(u64, U64),
            NDDataType::Float32 => read_typed!(f32, F32),
            NDDataType::Float64 => read_typed!(f64, F64),
        }
    }

    fn supports_multiple_arrays(&self) -> bool {
        true
    }

    /// The frames are written in `close_file`, so a frame is not on disk when
    /// `write_file` returns. HDF5 could extend the datasets per frame; the
    /// writer keeps the netCDF-3 writer's buffer-then-write shape, and Stream
    /// mode stays refused rather than silently accumulating frames in RAM.
    fn writes_incrementally(&self) -> bool {
        false
    }
}

/// NetCDF file processor wrapping NDPluginFileBase + NetcdfWriter.
pub struct NetcdfFileProcessor {
    ctrl: Mutex<FilePluginController<NetcdfWriter>>,
}

impl NetcdfFileProcessor {
    pub fn new() -> Self {
        Self {
            ctrl: Mutex::new(FilePluginController::new(NetcdfWriter::new())),
        }
    }
}

impl Default for NetcdfFileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for NetcdfFileProcessor {
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        self.ctrl.lock().process_array(array)
    }

    fn plugin_type(&self) -> &str {
        "NDFileNetCDF"
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
    use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
    use ad_core_rs::plugin::file_base::NDPluginFileBase;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(prefix: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "adcore_test_{}_{}_{}.nc",
            std::process::id(),
            prefix,
            n
        ))
    }

    fn make_u8(dims: &[usize], fill: impl Fn(usize) -> u8) -> NDArray {
        let n: usize = dims.iter().product();
        let mut arr = NDArray::new(
            dims.iter().map(|&d| NDDimension::new(d)).collect(),
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(v) = &mut arr.data {
            for i in 0..n {
                v[i] = fill(i);
            }
        }
        arr
    }

    /// Write one file in the given mode and return its path.
    fn write_frames(prefix: &str, mode: NDFileMode, frames: &[NDArray]) -> PathBuf {
        let path = temp_path(prefix);
        let mut writer = NetcdfWriter::new();
        writer.open_file(&path, mode, &frames[0]).unwrap();
        for f in frames {
            writer.write_file(&Arc::new(f.clone())).unwrap();
        }
        writer.close_file().unwrap();
        path
    }

    fn read_back(path: &Path) -> NDArray {
        let mut writer = NetcdfWriter::new();
        writer.current_path = Some(path.to_path_buf());
        writer.read_file().unwrap()
    }

    /// D2 sibling: `NetcdfWriter::close_file` is where the buffered frames are
    /// written, and `frames.clear()` used to sit after all ten fallible writes.
    /// A failed flush then held the frames resident until some later
    /// `open_file` happened to clear them.
    #[test]
    fn close_file_clears_the_frame_buffer_when_the_write_fails() {
        // The precondition is a parent that does NOT exist, so creating the
        // file fails. Spelling it as an implausible name under the shared temp
        // dir made that a hope about every process on the host; an exclusive
        // root we own and then leave unpopulated makes it a fact about this
        // test.
        let root = tempfile::tempdir().expect("fixture root");
        let path = root.path().join("no-such-dir").join("frames.nc");
        let mut writer = NetcdfWriter::new();

        let arr = make_u8(&[4, 4], |_| 0);
        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        assert_eq!(writer.frames.len(), 1);

        assert!(
            writer.close_file().is_err(),
            "the write into a missing directory must fail the close"
        );
        assert!(
            writer.frames.is_empty(),
            "a failed close must still drop the frames of the file that was never written"
        );
    }

    #[test]
    fn the_buffered_frame_is_the_input_arc_itself() {
        let path = temp_path("nc_arc");
        let mut writer = NetcdfWriter::new();
        let arr = Arc::new(make_u8(&[4, 4], |_| 0));
        writer.open_file(&path, NDFileMode::Capture, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        assert!(Arc::ptr_eq(&writer.frames[0].frame, &arr));
        assert_eq!(Arc::strong_count(&arr), 2);
        writer.close_file().unwrap();
        assert_eq!(Arc::strong_count(&arr), 1, "close releases the frame");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_u8_mono() {
        let arr = make_u8(&[4, 4], |i| (i * 10) as u8);
        let path = write_frames("nc_u8", NDFileMode::Single, std::slice::from_ref(&arr));
        let back = read_back(&path);
        let (NDDataBuffer::U8(want), NDDataBuffer::U8(got)) = (&arr.data, &back.data) else {
            panic!("expected UInt8 on both sides");
        };
        assert_eq!(want, got);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_u16() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(v) = &mut arr.data {
            for i in 0..16 {
                v[i] = (i * 1000) as u16;
            }
        }
        let path = write_frames("nc_u16", NDFileMode::Single, std::slice::from_ref(&arr));
        let back = read_back(&path);
        let (NDDataBuffer::U16(want), NDDataBuffer::U16(got)) = (&arr.data, &back.data) else {
            panic!("expected UInt16 on both sides");
        };
        assert_eq!(want, got);
        std::fs::remove_file(&path).ok();
    }

    /// One case per element type: the frame that goes in is the frame that
    /// comes back, with its type intact. netCDF-3 could store neither the
    /// unsigned nor the 64-bit integer types, so C reinterpreted UInt8/16/32 as
    /// signed and rounded Int64/UInt64 through a double
    /// (NDFileNetCDF.cpp:154-180); netCDF-4 has all ten, and the two 64-bit
    /// cases here are values no double can hold exactly.
    #[test]
    fn every_element_type_round_trips_losslessly() {
        macro_rules! case {
            ($prefix:literal, $t:ty, $variant:ident, $dt:expr, $values:expr) => {{
                let values: Vec<$t> = $values;
                let mut arr = NDArray::new(vec![NDDimension::new(values.len())], $dt);
                arr.data = NDDataBuffer::$variant(values.clone());
                let path = write_frames($prefix, NDFileMode::Single, &[arr]);
                let back = read_back(&path);
                let NDDataBuffer::$variant(got) = &back.data else {
                    panic!("{} came back as {:?}", $prefix, back.data.data_type());
                };
                assert_eq!(got, &values, "{}", $prefix);
                assert_eq!(back.data.data_type(), $dt);
                std::fs::remove_file(&path).ok();
            }};
        }

        case!("nc_t_i8", i8, I8, NDDataType::Int8, vec![-128, -1, 0, 127]);
        case!("nc_t_u8", u8, U8, NDDataType::UInt8, vec![0, 1, 200, 255]);
        case!(
            "nc_t_i16",
            i16,
            I16,
            NDDataType::Int16,
            vec![-32768, 0, 32767]
        );
        case!(
            "nc_t_u16",
            u16,
            U16,
            NDDataType::UInt16,
            vec![0, 1000, 65535]
        );
        case!(
            "nc_t_i32",
            i32,
            I32,
            NDDataType::Int32,
            vec![i32::MIN, 0, 7]
        );
        case!("nc_t_u32", u32, U32, NDDataType::UInt32, vec![0, u32::MAX]);
        // 2^53+1 and 2^64-1: not representable in an f64.
        case!(
            "nc_t_i64",
            i64,
            I64,
            NDDataType::Int64,
            vec![9_007_199_254_740_993, i64::MIN]
        );
        case!("nc_t_u64", u64, U64, NDDataType::UInt64, vec![u64::MAX, 0]);
        case!("nc_t_f32", f32, F32, NDDataType::Float32, vec![1.5, -0.25]);
        case!("nc_t_f64", f64, F64, NDDataType::Float64, vec![1.5, -0.25]);
    }

    #[test]
    fn test_multiple_frames() {
        let frames: Vec<NDArray> = (0u8..3)
            .map(|k| make_u8(&[4, 4], move |i| (i as u8).wrapping_add(k * 100)))
            .collect();
        let path = write_frames("nc_multi", NDFileMode::Stream, &frames);

        // read_file returns the first frame, as the netCDF-3 reader returned
        // record 0.
        let back = read_back(&path);
        let NDDataBuffer::U8(v) = &back.data else {
            panic!("expected U8 data");
        };
        assert_eq!(v.len(), 16);
        for i in 0..16 {
            assert_eq!(v[i], i as u8, "mismatch at index {i}");
        }

        // Every frame is in the file, each on its own row of the frame axis.
        let file = H5File::open(&path).unwrap();
        let ds = file.dataset(VAR_NAME).unwrap();
        assert_eq!(ds.shape(), vec![3, 4, 4]);
        for (k, frame) in frames.iter().enumerate() {
            let got = ds.read_slice::<u8>(&[k, 0, 0], &[1, 4, 4]).unwrap();
            let NDDataBuffer::U8(want) = &frame.data else {
                unreachable!()
            };
            assert_eq!(&got, want, "frame {k}");
        }
        drop(file);
        std::fs::remove_file(&path).ok();
    }

    /// R8-73. C picks the numArrays dimension from the open mode alone —
    /// `if (openMode & NDFileModeMultiple) dim0 = NC_UNLIMITED`
    /// (NDFileNetCDF.cpp:117-119) — and NDPluginFile passes that bit for
    /// Capture and Stream but not Single (NDPluginFile.cpp:245, :281, :335).
    /// A Capture/Stream file that ends up holding exactly ONE frame therefore
    /// still has an unlimited frame axis; deriving it from `frames.len() > 1`
    /// made it a fixed axis of 1. In netCDF-4 an unlimited dimension is an
    /// HDF5 dataspace with no maximum on that axis.
    #[test]
    fn the_frame_axis_is_extensible_for_capture_and_stream_not_single() {
        let frame_axis_max = |path: &Path| -> Option<usize> {
            let file = H5File::open(path).unwrap();
            let ds = file.dataset(VAR_NAME).unwrap();
            ds.max_shape().unwrap()[0]
        };

        for (prefix, mode) in [
            ("nc_mode_capture_one", NDFileMode::Capture),
            ("nc_mode_stream_one", NDFileMode::Stream),
        ] {
            let path = write_frames(prefix, mode, &[make_u8(&[4], |_| 0)]);
            assert_eq!(
                frame_axis_max(&path),
                None,
                "{prefix} with 1 frame must still be unlimited"
            );
            std::fs::remove_file(&path).ok();
        }

        let path = write_frames(
            "nc_mode_single_one",
            NDFileMode::Single,
            &[make_u8(&[4], |_| 0)],
        );
        assert_eq!(
            frame_axis_max(&path),
            Some(1),
            "Single must be a fixed frame axis"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn attributes_are_per_frame_datasets_with_their_text_metadata() {
        let mut arr = make_u8(&[4], |_| 0);
        arr.attributes.add(NDAttribute::new_static(
            "exposure",
            "Exposure time",
            NDAttrSource::Driver,
            NDAttrValue::Float64(0.5),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "gain",
            "Detector gain",
            NDAttrSource::Driver,
            NDAttrValue::Int32(42),
        ));
        let path = write_frames("nc_attrs", NDFileMode::Single, &[arr]);

        let file = H5File::open(&path).unwrap();
        // The per-frame value is recoverable from the dataset, typed as the
        // attribute is.
        assert_eq!(
            file.dataset("Attr_exposure")
                .unwrap()
                .read_raw::<f64>()
                .unwrap(),
            vec![0.5]
        );
        assert_eq!(
            file.dataset("Attr_gain")
                .unwrap()
                .read_raw::<i32>()
                .unwrap(),
            vec![42]
        );
        // Four descriptive text attributes per NDAttribute, on the root group
        // where a netCDF-4 file keeps its global attributes.
        for (name, want) in [
            ("Attr_exposure_DataType", "Float64"),
            ("Attr_gain_DataType", "Int32"),
            ("Attr_exposure_Description", "Exposure time"),
            ("Attr_gain_SourceType", "NDAttrSourceDriver"),
        ] {
            assert_eq!(file.attr_string(name).unwrap(), want, "{name}");
        }
        drop(file);
        std::fs::remove_file(&path).ok();
    }

    /// ADP-102. C merges each frame into the sticky `pFileAttributes`
    /// (NDFileNetCDF.cpp:362) and writes every attribute variable out of that
    /// list (`:419-483`), so a dropped attribute records its most recent value.
    /// The port read each frame's own list and fell back to the *first*
    /// frame's value, which is the same only until the attribute changes.
    #[test]
    fn attribute_that_drops_out_records_its_last_value() {
        let mk = |exposure: Option<f64>| {
            let mut arr = make_u8(&[4], |_| 0);
            if let Some(v) = exposure {
                arr.attributes.add(NDAttribute::new_static(
                    "exposure",
                    "",
                    NDAttrSource::Driver,
                    NDAttrValue::Float64(v),
                ));
            }
            arr
        };
        let path = write_frames(
            "nc_attr_sticky",
            NDFileMode::Stream,
            &[mk(Some(0.5)), mk(Some(0.75)), mk(None)],
        );

        let file = H5File::open(&path).unwrap();
        assert_eq!(
            file.dataset("Attr_exposure")
                .unwrap()
                .read_raw::<f64>()
                .unwrap(),
            vec![0.5, 0.75, 0.75]
        );
        drop(file);
        std::fs::remove_file(&path).ok();
    }

    /// C++ always defines array_data with rank ndims+1 (NDFileNetCDF.cpp:202-204),
    /// reversing the NDArray dimensions because the first netCDF dimension
    /// varies slowest (:123-132). A 2-D single-frame file is therefore a 3-D
    /// dataset, and the reader has to undo both to get the NDArray back.
    #[test]
    fn single_frame_array_data_keeps_the_leading_frame_axis() {
        let path = write_frames("nc_rank", NDFileMode::Single, &[make_u8(&[4, 3], |_| 0)]);

        let file = H5File::open(&path).unwrap();
        assert_eq!(file.dataset(VAR_NAME).unwrap().shape(), vec![1, 3, 4]);
        drop(file);

        let back = read_back(&path);
        assert_eq!(
            back.dims.iter().map(|d| d.size).collect::<Vec<_>>(),
            vec![4, 3],
            "read_file must undo the reversal"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn global_attrs_match_the_c_set_plus_the_container_provenance() {
        // C (NDFileNetCDF.cpp:92-151) writes dataType, the
        // NDNetCDFFileVersion=3.1 double and the five dim* attributes as
        // global attributes. uniqueId is a per-frame variable (:183) and
        // numArrays is a dimension (:119) — neither may appear as a global
        // attribute. `_NCProperties` is the netCDF-4 container's own
        // provenance.
        let path = write_frames("nc_globals", NDFileMode::Single, &[make_u8(&[4, 3], |_| 0)]);

        let file = H5File::open(&path).unwrap();
        let mut names = file.attr_names().unwrap();
        names.sort();
        let mut want = vec![
            "_NCProperties",
            "dataType",
            "NDNetCDFFileVersion",
            "numArrayDims",
            "dimSize",
            "dimOffset",
            "dimBinning",
            "dimReverse",
        ];
        want.sort();
        assert_eq!(names, want);
        assert!(
            file.attr_string("_NCProperties")
                .unwrap()
                .starts_with("version=2,")
        );
        // uniqueId is a dataset, not a global attribute.
        assert!(file.dataset("uniqueId").is_ok());
        drop(file);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_four_metadata_datasets_carry_the_frame_identity() {
        let mut arr = make_u8(&[4], |_| 0);
        arr.unique_id = 99;
        arr.time_stamp = 12.5;
        arr.timestamp.sec = 555;
        arr.timestamp.nsec = 777;
        let path = write_frames("nc_meta", NDFileMode::Single, &[arr]);

        let file = H5File::open(&path).unwrap();
        assert_eq!(
            file.dataset("uniqueId").unwrap().read_raw::<i32>().unwrap(),
            vec![99]
        );
        assert_eq!(
            file.dataset("timeStamp")
                .unwrap()
                .read_raw::<f64>()
                .unwrap(),
            vec![12.5]
        );
        assert_eq!(
            file.dataset("epicsTSSec")
                .unwrap()
                .read_raw::<i32>()
                .unwrap(),
            vec![555]
        );
        assert_eq!(
            file.dataset("epicsTSNsec")
                .unwrap()
                .read_raw::<i32>()
                .unwrap(),
            vec![777]
        );
        drop(file);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_nddatatype_ordinals_match_c() {
        // The `dataType` attribute stores `NDDataType as i32`, which the
        // reader uses to recover the original type. The discriminants must
        // match the C `NDDataType_t` enum (NDInt8=0 .. NDFloat64=9).
        assert_eq!(NDDataType::Int8 as i32, 0);
        assert_eq!(NDDataType::UInt8 as i32, 1);
        assert_eq!(NDDataType::Int16 as i32, 2);
        assert_eq!(NDDataType::UInt16 as i32, 3);
        assert_eq!(NDDataType::Int32 as i32, 4);
        assert_eq!(NDDataType::UInt32 as i32, 5);
        assert_eq!(NDDataType::Int64 as i32, 6);
        assert_eq!(NDDataType::UInt64 as i32, 7);
        assert_eq!(NDDataType::Float32 as i32, 8);
        assert_eq!(NDDataType::Float64 as i32, 9);
    }

    /// An attribute dataset's element is the attribute's own type, and a
    /// string attribute's is the fixed 256-byte field C writes
    /// (NDFileNetCDF.cpp:302-316 spelled it `[numArrays, attrStringSize]`).
    #[test]
    fn attr_datasets_are_typed_like_the_attribute() {
        let mut arr = make_u8(&[2, 2], |_| 0);
        for (name, value) in [
            ("Str", NDAttrValue::String("hello".into())),
            ("I8", NDAttrValue::Int8(-3)),
            ("I16", NDAttrValue::Int16(-3)),
            ("I32", NDAttrValue::Int32(-3)),
            ("I64", NDAttrValue::Int64(-3)),
            ("F32", NDAttrValue::Float32(-3.0)),
        ] {
            arr.attributes.add(NDAttribute::new_static(
                name,
                "",
                NDAttrSource::Driver,
                value,
            ));
        }
        let path = write_frames("nc_attr_types", NDFileMode::Single, &[arr]);

        let file = H5File::open(&path).unwrap();
        for (name, width) in [
            ("Attr_I8", 1),
            ("Attr_I16", 2),
            ("Attr_I32", 4),
            // netCDF-3 had to cast a 64-bit integer to a double (:299-301);
            // netCDF-4 stores it as itself.
            ("Attr_I64", 8),
            ("Attr_F32", 4),
            ("Attr_Str", ATTR_STRING_SIZE),
        ] {
            let ds = file.dataset(name).unwrap();
            assert_eq!(ds.element_size(), width, "{name} element width");
            assert_eq!(ds.shape(), vec![1], "{name} shape");
        }
        assert_eq!(
            file.dataset("Attr_I64").unwrap().read_raw::<i64>().unwrap(),
            vec![-3]
        );
        let text = file.dataset("Attr_Str").unwrap().read_raw_bytes().unwrap();
        assert_eq!(text.len(), ATTR_STRING_SIZE);
        assert_eq!(&text[..6], b"hello\0", "NUL-terminated in a fixed field");
        drop(file);
        std::fs::remove_file(&path).ok();
    }

    /// Every netCDF dimension is an HDF5 dimension scale: `numArrays` and one
    /// per array dimension, reversed as the dataspace is. The scales are
    /// dimensions without coordinate variables, which is what the `NAME` text
    /// records.
    ///
    /// `CLASS` and `NAME` are fixed-length null-terminated strings, and the
    /// widths are not free: `H5DSis_scale` reports "not a dimension scale" for
    /// a `CLASS` that is variable-length or not exactly 16 bytes on every
    /// libhdf5 before 2.2.0 (hl/src/H5DS.c:2285-2300).
    #[test]
    fn every_dimension_gets_a_named_scale() {
        let path = write_frames("nc_dims", NDFileMode::Single, &[make_u8(&[4, 2], |_| 0)]);

        let file = H5File::open(&path).unwrap();
        let mut names = file.dataset_names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "array_data",
                "dim0",
                "dim1",
                "epicsTSNsec",
                "epicsTSSec",
                "numArrays",
                "timeStamp",
                "uniqueId",
            ]
        );
        // Every variable names its dimensions through DIMENSION_LIST: without
        // the attachment a netCDF reader invents an anonymous `phony_dim_N` for
        // the axis instead of reading `numArrays`.
        for name in [
            "array_data",
            "uniqueId",
            "timeStamp",
            "epicsTSSec",
            "epicsTSNsec",
        ] {
            assert!(
                file.dataset(name)
                    .unwrap()
                    .attr_names()
                    .unwrap()
                    .contains(&"DIMENSION_LIST".to_string()),
                "{name} DIMENSION_LIST"
            );
        }
        for (name, len) in [("numArrays", 1), ("dim0", 2), ("dim1", 4)] {
            let ds = file.dataset(name).unwrap();
            assert_eq!(ds.shape(), vec![len], "{name} length");
            let class = ds.attr("CLASS").unwrap();
            assert_eq!(
                class.read_string().unwrap(),
                "DIMENSION_SCALE",
                "{name} CLASS"
            );
            assert_eq!(
                class.datatype().unwrap(),
                DatatypeMessage::fixed_string(16),
                "{name} CLASS datatype"
            );
            let label = ds.attr("NAME").unwrap();
            let text = format!("{DIM_WITHOUT_VARIABLE}{len:10}");
            assert_eq!(label.read_string().unwrap(), text, "{name} NAME");
            assert_eq!(
                label.datatype().unwrap(),
                DatatypeMessage::fixed_string(text.len() as u32 + 1),
                "{name} NAME datatype"
            );
            // The scale is on the receiving end of an attachment, so it carries
            // the reciprocal REFERENCE_LIST.
            assert!(
                ds.attr_names()
                    .unwrap()
                    .contains(&"REFERENCE_LIST".to_string()),
                "{name} REFERENCE_LIST"
            );
        }
        drop(file);
        std::fs::remove_file(&path).ok();
    }

    /// F4: with `FileWriteMode=Stream` and `NumCapture=0` the controller never
    /// reaches a close, so every frame stayed in `frames` with nothing on disk
    /// while `NumCaptured_RBV` counted it as captured. The stream open is
    /// refused now that the writer reports it is not incremental.
    #[test]
    fn stream_mode_is_refused_rather_than_buffered_in_ram() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/tmp/".into();
        fb.file_name = "nc_stream_".into();
        fb.set_mode(NDFileMode::Stream);
        fb.set_num_capture(0);

        let mut writer = NetcdfWriter::new();
        let array = std::sync::Arc::new(NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8));
        assert!(fb.process_array(array, &mut writer).is_err());
        assert!(writer.frames.is_empty(), "no frame may be buffered");
        assert!(!fb.is_open());
        assert_eq!(fb.num_captured(), 0);
    }

    /// `array_data` is chunked one frame deep, so a frame larger than a chunk
    /// cache or an HDF5 I/O block is the case that crosses whatever buffering
    /// sits under the write. Both modes: a single frame, and two frames whose
    /// rows have to land in the right chunk.
    #[test]
    fn frames_larger_than_one_megabyte_round_trip() {
        let n = (1 << 20) + 12_345;
        let mk = |seed: u8| make_u8(&[n], move |i| (i as u8).wrapping_add(seed));

        let path = write_frames("nc_big_single", NDFileMode::Single, &[mk(0)]);
        let back = read_back(&path);
        let (NDDataBuffer::U8(want), NDDataBuffer::U8(got)) = (&mk(0).data, &back.data) else {
            panic!("expected UInt8 on both sides");
        };
        assert_eq!(want, got);
        std::fs::remove_file(&path).ok();

        let path = write_frames("nc_big_capture", NDFileMode::Capture, &[mk(0), mk(77)]);
        let file = H5File::open(&path).unwrap();
        let ds = file.dataset(VAR_NAME).unwrap();
        for (record, seed) in [(0usize, 0u8), (1, 77)] {
            let got = ds.read_slice::<u8>(&[record, 0], &[1, n]).unwrap();
            let NDDataBuffer::U8(want) = &mk(seed).data else {
                unreachable!()
            };
            assert_eq!(&got, want, "record {record}");
        }
        drop(file);
        std::fs::remove_file(&path).ok();
    }
}
