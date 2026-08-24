use std::path::{Path, PathBuf};

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue};
use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::finalize::Finalize;
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{NDFileMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::FilePluginController;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamChangeResult, PluginParamSnapshot, ProcessResult,
};

use netcdf3::{DataSet, FileReader, FileWriter, Version};

const VAR_NAME: &str = "array_data";
const DIM_UNLIMITED: &str = "numArrays";
/// File-format version written as the NDNetCDFFileVersion global attribute so
/// readers can gate on format changes (C NDFileNetCDF.h:19 `#define
/// NDNetCDFFileVersion 3.1`).
const ND_NETCDF_FILE_VERSION: f64 = 3.1;

/// Dimension metadata captured from NDArray dimensions.
struct DimMeta {
    size: usize,
    offset: usize,
    binning: usize,
    reverse: bool,
}

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

/// A single buffered frame captured from an NDArray.
struct FrameData {
    dims: Vec<usize>,
    dim_meta: Vec<DimMeta>,
    data: NDDataBuffer,
    data_type: NDDataType,
    attrs: Vec<AttrData>,
    unique_id: i32,
    time_stamp: f64,
    epics_ts_sec: i32,
    epics_ts_nsec: i32,
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

/// NetCDF-3 file writer.
///
/// Because `netcdf3::FileWriter` is `!Send` (uses `Rc` internally), we cannot
/// store it as a field on a `Send + Sync` struct.  Instead we buffer frame data
/// in memory and materialise the `FileWriter` only inside `close_file()`, where
/// it is created, used, and dropped within a single method call.  The same
/// approach is used for `read_file()` with `FileReader`.
pub struct NetcdfWriter {
    current_path: Option<PathBuf>,
    frames: Vec<FrameData>,
    /// C's `openMode & NDFileModeMultiple` (NDFileNetCDF.cpp:118) — the sole
    /// input to the numArrays dimension. NDPluginFile passes the Multiple bit
    /// for Capture and Stream and withholds it for Single (NDPluginFile.cpp:245,
    /// :281, :335), so it is fixed when the file is opened and cannot be
    /// re-derived later from how many frames happened to arrive: a Capture file
    /// that captured exactly one frame is still NC_UNLIMITED.
    open_multiple: bool,
}

impl NetcdfWriter {
    pub fn new() -> Self {
        Self {
            current_path: None,
            frames: Vec::new(),
            open_multiple: false,
        }
    }
}

/// nc_type of the `array_data` variable, i.e. C's `switch (pArray->dataType)`
/// (NDFileNetCDF.cpp:152-178).
///
/// netCDF-3 has no unsigned types, so C maps *both* Int8 and UInt8 to NC_BYTE
/// and lets the `dataType` global attribute carry the sign back to the reader
/// (:88-92). It has no 64-bit integer either, so Int64/UInt64 are cast to
/// NC_DOUBLE (:169-172).
///
/// Beware the netcdf3 crate's spelling: `DataType::I8` is NC_BYTE (nc_type 1),
/// but `DataType::U8` is NC_CHAR (nc_type 2) — a *text* type. C never stores
/// image data as NC_CHAR, so `U8` must not appear here.
fn nc_data_type(dt: NDDataType) -> ADResult<netcdf3::DataType> {
    match dt {
        NDDataType::Int8 | NDDataType::UInt8 => Ok(netcdf3::DataType::I8),
        NDDataType::Int16 | NDDataType::UInt16 => Ok(netcdf3::DataType::I16),
        NDDataType::Int32 | NDDataType::UInt32 => Ok(netcdf3::DataType::I32),
        NDDataType::Float32 => Ok(netcdf3::DataType::F32),
        NDDataType::Float64 => Ok(netcdf3::DataType::F64),
        NDDataType::Int64 | NDDataType::UInt64 => Ok(netcdf3::DataType::F64),
    }
}

/// Write a single frame's data to a fixed-dimension variable.
fn write_var_data(writer: &mut FileWriter, data: &NDDataBuffer) -> ADResult<()> {
    let err = |e: netcdf3::error::WriteError| {
        ADError::UnsupportedConversion(format!("NetCDF write error: {:?}", e))
    };
    match data {
        NDDataBuffer::I8(v) => writer.write_var_i8(VAR_NAME, v).map_err(err),
        // NC_BYTE variable: C `nc_put_vara_uchar` into an NC_BYTE variable is a
        // straight bit-pattern copy (NDFileNetCDF.cpp:385-388), so values above
        // 127 land as negative bytes on disk.
        NDDataBuffer::U8(v) => {
            let reinterp: Vec<i8> = v.iter().map(|&x| x as i8).collect();
            writer.write_var_i8(VAR_NAME, &reinterp).map_err(err)
        }
        NDDataBuffer::I16(v) => writer.write_var_i16(VAR_NAME, v).map_err(err),
        NDDataBuffer::U16(v) => {
            let reinterp: Vec<i16> = v.iter().map(|&x| x as i16).collect();
            writer.write_var_i16(VAR_NAME, &reinterp).map_err(err)
        }
        NDDataBuffer::I32(v) => writer.write_var_i32(VAR_NAME, v).map_err(err),
        NDDataBuffer::U32(v) => {
            let reinterp: Vec<i32> = v.iter().map(|&x| x as i32).collect();
            writer.write_var_i32(VAR_NAME, &reinterp).map_err(err)
        }
        NDDataBuffer::F32(v) => writer.write_var_f32(VAR_NAME, v).map_err(err),
        NDDataBuffer::F64(v) => writer.write_var_f64(VAR_NAME, v).map_err(err),
        NDDataBuffer::I64(v) => {
            let reinterp: Vec<f64> = v.iter().map(|&x| x as f64).collect();
            writer.write_var_f64(VAR_NAME, &reinterp).map_err(err)
        }
        NDDataBuffer::U64(v) => {
            let reinterp: Vec<f64> = v.iter().map(|&x| x as f64).collect();
            writer.write_var_f64(VAR_NAME, &reinterp).map_err(err)
        }
    }
}

/// Write a single record (one frame) to a record variable.
fn write_record_data(
    writer: &mut FileWriter,
    record_index: usize,
    data: &NDDataBuffer,
) -> ADResult<()> {
    let err = |e: netcdf3::error::WriteError| {
        ADError::UnsupportedConversion(format!("NetCDF write error: {:?}", e))
    };
    match data {
        NDDataBuffer::I8(v) => writer
            .write_record_i8(VAR_NAME, record_index, v)
            .map_err(err),
        // NC_BYTE variable — see `write_var_data`.
        NDDataBuffer::U8(v) => {
            let reinterp: Vec<i8> = v.iter().map(|&x| x as i8).collect();
            writer
                .write_record_i8(VAR_NAME, record_index, &reinterp)
                .map_err(err)
        }
        NDDataBuffer::I16(v) => writer
            .write_record_i16(VAR_NAME, record_index, v)
            .map_err(err),
        NDDataBuffer::U16(v) => {
            let reinterp: Vec<i16> = v.iter().map(|&x| x as i16).collect();
            writer
                .write_record_i16(VAR_NAME, record_index, &reinterp)
                .map_err(err)
        }
        NDDataBuffer::I32(v) => writer
            .write_record_i32(VAR_NAME, record_index, v)
            .map_err(err),
        NDDataBuffer::U32(v) => {
            let reinterp: Vec<i32> = v.iter().map(|&x| x as i32).collect();
            writer
                .write_record_i32(VAR_NAME, record_index, &reinterp)
                .map_err(err)
        }
        NDDataBuffer::F32(v) => writer
            .write_record_f32(VAR_NAME, record_index, v)
            .map_err(err),
        NDDataBuffer::F64(v) => writer
            .write_record_f64(VAR_NAME, record_index, v)
            .map_err(err),
        NDDataBuffer::I64(v) => {
            let reinterp: Vec<f64> = v.iter().map(|&x| x as f64).collect();
            writer
                .write_record_f64(VAR_NAME, record_index, &reinterp)
                .map_err(err)
        }
        NDDataBuffer::U64(v) => {
            let reinterp: Vec<f64> = v.iter().map(|&x| x as f64).collect();
            writer
                .write_record_f64(VAR_NAME, record_index, &reinterp)
                .map_err(err)
        }
    }
}

const ATTR_STRING_DIM: &str = "attrStringSize";
const ATTR_STRING_SIZE: usize = 256;

/// nc_type of the `Attr_<name>` variable, i.e. C's second
/// `switch (attrDataType)` (NDFileNetCDF.cpp:283-310).
///
/// String attributes are NC_CHAR — `DataType::U8` in the netcdf3 crate's
/// spelling — and are the *only* NC_CHAR variables C writes. Everything else
/// follows the same signed/64-bit collapse as [`nc_data_type`], with
/// `Undefined` falling back to NC_BYTE (:305).
fn attr_nc_type(value: &NDAttrValue) -> netcdf3::DataType {
    match value {
        NDAttrValue::Int8(_) | NDAttrValue::UInt8(_) | NDAttrValue::Undefined => {
            netcdf3::DataType::I8
        }
        NDAttrValue::Int16(_) | NDAttrValue::UInt16(_) => netcdf3::DataType::I16,
        NDAttrValue::Int32(_) | NDAttrValue::UInt32(_) => netcdf3::DataType::I32,
        NDAttrValue::Float32(_) => netcdf3::DataType::F32,
        NDAttrValue::Float64(_) | NDAttrValue::Int64(_) | NDAttrValue::UInt64(_) => {
            netcdf3::DataType::F64
        }
        NDAttrValue::String(_) => netcdf3::DataType::U8,
    }
}

/// Write one frame's value into the `Attr_<name>` variable at `record_index`.
/// For single-frame files `record_index` is 0 and the variable is non-record.
fn write_attr_value(
    writer: &mut FileWriter,
    var_name: &str,
    record_index: usize,
    multi: bool,
    value: &NDAttrValue,
) -> ADResult<()> {
    let werr = |e: netcdf3::error::WriteError| {
        ADError::UnsupportedConversion(format!("NetCDF attr write error: {:?}", e))
    };
    // String values are stored as a fixed-width NC_CHAR row. C writes only
    // `strlen(attrString)` characters (NDFileNetCDF.cpp:462-465) and leaves the
    // tail at the NC_CHAR fill value, which is NUL — the same bytes this
    // NUL-padded full-width write produces.
    if let NDAttrValue::String(s) = value {
        let mut bytes: Vec<u8> = s.bytes().take(ATTR_STRING_SIZE).collect();
        bytes.resize(ATTR_STRING_SIZE, 0);
        return if multi {
            writer
                .write_record_u8(var_name, record_index, &bytes)
                .map_err(werr)
        } else {
            writer.write_var_u8(var_name, &bytes).map_err(werr)
        };
    }
    match attr_nc_type(value) {
        netcdf3::DataType::I8 => {
            let v = value.as_i64().unwrap_or(0) as i8;
            if multi {
                writer
                    .write_record_i8(var_name, record_index, &[v])
                    .map_err(werr)
            } else {
                writer.write_var_i8(var_name, &[v]).map_err(werr)
            }
        }
        netcdf3::DataType::I16 => {
            let v = value.as_i64().unwrap_or(0) as i16;
            if multi {
                writer
                    .write_record_i16(var_name, record_index, &[v])
                    .map_err(werr)
            } else {
                writer.write_var_i16(var_name, &[v]).map_err(werr)
            }
        }
        netcdf3::DataType::I32 => {
            let v = value.as_i64().unwrap_or(0) as i32;
            if multi {
                writer
                    .write_record_i32(var_name, record_index, &[v])
                    .map_err(werr)
            } else {
                writer.write_var_i32(var_name, &[v]).map_err(werr)
            }
        }
        netcdf3::DataType::F32 => {
            let v = value.as_f64().unwrap_or(0.0) as f32;
            if multi {
                writer
                    .write_record_f32(var_name, record_index, &[v])
                    .map_err(werr)
            } else {
                writer.write_var_f32(var_name, &[v]).map_err(werr)
            }
        }
        netcdf3::DataType::F64 => {
            let v = value.as_f64().unwrap_or(0.0);
            if multi {
                writer
                    .write_record_f64(var_name, record_index, &[v])
                    .map_err(werr)
            } else {
                writer.write_var_f64(var_name, &[v]).map_err(werr)
            }
        }
        // NC_CHAR is reached only for string attributes, handled above.
        netcdf3::DataType::U8 => unreachable!("attr_nc_type returns U8 only for strings"),
    }
}

/// Build the netCDF-3 header, mirroring C `NDFileNetCDF::openFile`
/// (NDFileNetCDF.cpp:83-333) statement for statement.
///
/// A netCDF-3 header stores its dimensions, its global attributes and its
/// variables as three ordered lists, and every reader that walks a file by
/// index — rather than by name — sees that order. So the order in which C
/// defines things *is* file format, and this function is the one place that
/// owns it: dimensions and definitions are emitted here in C's order, and
/// nowhere else, so no later edit can re-order the header by adding a
/// definition next to the code that happens to need it.
///
/// Returns the data set plus the `Attr_<name>` variable names in definition
/// order, so the write pass visits the attribute variables in the same order.
fn define_data_set(
    first: &FrameData,
    num_frames: usize,
    multi: bool,
) -> ADResult<(DataSet, Vec<String>)> {
    let map_def = |e: netcdf3::error::InvalidDataSet| {
        ADError::UnsupportedConversion(format!("NetCDF definition error: {:?}", e))
    };

    let mut ds = DataSet::new();
    let ndims = first.dims.len();

    // --- Global attributes, part 1 (C :88-101, :107-110, :137-151) ---------
    // C emits dataType and NDNetCDFFileVersion before it defines any
    // dimension, and the dim* metadata attributes right after; the gatt list
    // therefore starts with these seven, in this order.
    ds.add_global_attr_i32("dataType", vec![first.data_type as i32])
        .map_err(map_def)?;
    ds.add_global_attr_f64("NDNetCDFFileVersion", vec![ND_NETCDF_FILE_VERSION])
        .map_err(map_def)?;
    ds.add_global_attr_i32("numArrayDims", vec![ndims as i32])
        .map_err(map_def)?;
    // C reads dims[i] here — natural order, *not* the reversed order used for
    // the dimension definitions below (:125-131).
    let dim_size: Vec<i32> = first.dim_meta.iter().map(|d| d.size as i32).collect();
    ds.add_global_attr_i32("dimSize", dim_size)
        .map_err(map_def)?;
    let dim_offset: Vec<i32> = first.dim_meta.iter().map(|d| d.offset as i32).collect();
    ds.add_global_attr_i32("dimOffset", dim_offset)
        .map_err(map_def)?;
    let dim_binning: Vec<i32> = first.dim_meta.iter().map(|d| d.binning as i32).collect();
    ds.add_global_attr_i32("dimBinning", dim_binning)
        .map_err(map_def)?;
    let dim_reverse: Vec<i32> = first
        .dim_meta
        .iter()
        .map(|d| if d.reverse { 1 } else { 0 })
        .collect();
    ds.add_global_attr_i32("dimReverse", dim_reverse)
        .map_err(map_def)?;

    // --- Dimensions (C :113-136) ------------------------------------------
    // numArrays first: NC_UNLIMITED for a multi-array file, fixed size 1
    // otherwise (:118-120).
    if multi {
        ds.set_unlimited_dim(DIM_UNLIMITED, num_frames)
            .map_err(map_def)?;
    } else {
        ds.add_fixed_dim(DIM_UNLIMITED, 1).map_err(map_def)?;
    }
    // Then the array dimensions, reversed: netCDF's first dimension varies
    // slowest, the opposite of the NDArray convention (:122-127).
    let mut dim_names: Vec<String> = Vec::new();
    for i in 0..ndims {
        let name = format!("dim{}", i);
        ds.add_fixed_dim(&name, first.dims[ndims - 1 - i])
            .map_err(map_def)?;
        dim_names.push(name);
    }
    // attrStringSize last — defined unconditionally (:134-136), even when no
    // string attribute uses it. It is part of the header C always writes.
    ds.add_fixed_dim(ATTR_STRING_DIM, ATTR_STRING_SIZE)
        .map_err(map_def)?;

    // --- Variables (C :181-206) -------------------------------------------
    // The four per-array metadata variables come first, array_data fifth.
    ds.add_var("uniqueId", &[DIM_UNLIMITED], netcdf3::DataType::I32)
        .map_err(map_def)?;
    ds.add_var("timeStamp", &[DIM_UNLIMITED], netcdf3::DataType::F64)
        .map_err(map_def)?;
    ds.add_var("epicsTSSec", &[DIM_UNLIMITED], netcdf3::DataType::I32)
        .map_err(map_def)?;
    ds.add_var("epicsTSNsec", &[DIM_UNLIMITED], netcdf3::DataType::I32)
        .map_err(map_def)?;

    // array_data always carries the leading numArrays dimension, so a
    // single-array file is still rank ndims+1 (:203-205).
    let mut var_dims: Vec<&str> = vec![DIM_UNLIMITED];
    var_dims.extend(dim_names.iter().map(|s| s.as_str()));
    ds.add_var(VAR_NAME, &var_dims, nc_data_type(first.data_type)?)
        .map_err(map_def)?;

    // --- Per-attribute variables and their text attributes (C :208-330) ----
    // One pass over the attribute list, exactly as C does: the four
    // Attr_<name>_* global text attributes, then the Attr_<name> variable.
    // The attribute set is the first frame's — C snapshots the list at
    // openFile time and requires it not to change (:417).
    let mut attr_var_names: Vec<String> = Vec::new();
    for attr in &first.attrs {
        ds.add_global_attr_string(
            &format!("Attr_{}_DataType", attr.name),
            &attr.data_type_string,
        )
        .map_err(map_def)?;
        ds.add_global_attr_string(
            &format!("Attr_{}_Description", attr.name),
            &attr.description,
        )
        .map_err(map_def)?;
        ds.add_global_attr_string(&format!("Attr_{}_Source", attr.name), &attr.source)
            .map_err(map_def)?;
        ds.add_global_attr_string(&format!("Attr_{}_SourceType", attr.name), &attr.source_type)
            .map_err(map_def)?;

        let var_name = format!("Attr_{}", attr.name);
        // A string attribute is a 2-D NC_CHAR variable [numArrays,
        // attrStringSize]; everything else is 1-D over numArrays (:312-321).
        if matches!(attr.value, NDAttrValue::String(_)) {
            ds.add_var(
                &var_name,
                &[DIM_UNLIMITED, ATTR_STRING_DIM],
                attr_nc_type(&attr.value),
            )
            .map_err(map_def)?;
        } else {
            ds.add_var(&var_name, &[DIM_UNLIMITED], attr_nc_type(&attr.value))
                .map_err(map_def)?;
        }
        attr_var_names.push(var_name);
    }

    Ok((ds, attr_var_names))
}

impl NDFileWriter for NetcdfWriter {
    fn open_file(&mut self, path: &Path, mode: NDFileMode, _array: &NDArray) -> ADResult<()> {
        self.current_path = Some(path.to_path_buf());
        self.frames.clear();
        // C: NDPluginFile opens Single with `NDFileModeWrite` and Capture/Stream
        // with `NDFileModeWrite | NDFileModeMultiple` (NDPluginFile.cpp:245, :281,
        // :335) — this writer reports supportsMultipleArrays, so those two modes
        // always carry the Multiple bit.
        self.open_multiple = mode != NDFileMode::Single;
        Ok(())
    }

    fn write_file(&mut self, array: &NDArray) -> ADResult<()> {
        // Validate data type early
        nc_data_type(array.data.data_type())?;

        let dims: Vec<usize> = array.dims.iter().map(|d| d.size).collect();
        let dim_meta: Vec<DimMeta> = array
            .dims
            .iter()
            .map(|d| DimMeta {
                size: d.size,
                offset: d.offset,
                binning: d.binning,
                reverse: d.reverse,
            })
            .collect();
        let attrs: Vec<AttrData> = array
            .attributes
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
            dims,
            dim_meta,
            data: array.data.clone(),
            data_type: array.data.data_type(),
            attrs,
            unique_id: array.unique_id,
            time_stamp: array.time_stamp,
            epics_ts_sec: array.timestamp.sec as i32,
            epics_ts_nsec: array.timestamp.nsec as i32,
        });
        Ok(())
    }

    /// Close the open file — for netCDF this is where the whole file is
    /// written, from the frames `write_file` buffered.
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

            let map_write = |e: netcdf3::error::WriteError| {
                ADError::UnsupportedConversion(format!("NetCDF write error: {:?}", e))
            };

            let first = &w.frames[0];
            // C keys the numArrays dimension on the *open mode*, never on how many
            // frames the file ended up holding (NDFileNetCDF.cpp:117-119).
            let multi = w.open_multiple;
            let (ds, attr_var_names) = define_data_set(first, w.frames.len(), multi)?;

            // Write
            let mut writer = FileWriter::open(&path).map_err(map_write)?;
            writer
                .set_def(&ds, Version::Classic, 0)
                .map_err(map_write)?;

            if multi {
                for (i, frame) in w.frames.iter().enumerate() {
                    write_record_data(&mut writer, i, &frame.data)?;
                    writer
                        .write_record_i32("uniqueId", i, &[frame.unique_id])
                        .map_err(map_write)?;
                    writer
                        .write_record_f64("timeStamp", i, &[frame.time_stamp])
                        .map_err(map_write)?;
                    writer
                        .write_record_i32("epicsTSSec", i, &[frame.epics_ts_sec])
                        .map_err(map_write)?;
                    writer
                        .write_record_i32("epicsTSNsec", i, &[frame.epics_ts_nsec])
                        .map_err(map_write)?;
                    // Per-attribute values: align to the first frame's attribute
                    // order; missing attributes in later frames are skipped.
                    for (attr, var_name) in first.attrs.iter().zip(&attr_var_names) {
                        let value = frame
                            .attrs
                            .iter()
                            .find(|a| a.name == attr.name)
                            .map(|a| &a.value)
                            .unwrap_or(&attr.value);
                        write_attr_value(&mut writer, var_name, i, true, value)?;
                    }
                }
            } else {
                write_var_data(&mut writer, &w.frames[0].data)?;
                writer
                    .write_var_i32("uniqueId", &[first.unique_id])
                    .map_err(map_write)?;
                writer
                    .write_var_f64("timeStamp", &[first.time_stamp])
                    .map_err(map_write)?;
                writer
                    .write_var_i32("epicsTSSec", &[first.epics_ts_sec])
                    .map_err(map_write)?;
                writer
                    .write_var_i32("epicsTSNsec", &[first.epics_ts_nsec])
                    .map_err(map_write)?;
                for (attr, var_name) in first.attrs.iter().zip(&attr_var_names) {
                    write_attr_value(&mut writer, var_name, 0, false, &attr.value)?;
                }
            }

            writer.close().map_err(map_write)?;
            Ok(())
        })
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let map_read = |e: netcdf3::error::ReadError| {
            ADError::UnsupportedConversion(format!("NetCDF read error: {:?}", e))
        };

        let mut reader = FileReader::open(path).map_err(map_read)?;

        // Extract metadata from data_set() before any mutable read calls
        let (is_record, dims, original_type_ordinal) = {
            let ds = reader.data_set();
            let var = ds.get_var(VAR_NAME).ok_or_else(|| {
                ADError::UnsupportedConversion(format!(
                    "variable '{}' not found in NetCDF file",
                    VAR_NAME
                ))
            })?;

            let is_record = ds.is_record_var(VAR_NAME).unwrap_or(false);

            let var_dims_rc = var.get_dims();
            let mut dims: Vec<NDDimension> = Vec::new();
            for d in &var_dims_rc {
                // Skip the leading numArrays dimension. It is unlimited for
                // multi-frame files and a fixed dim of size 1 for single-frame
                // files, so match it by name as well as the unlimited flag.
                if d.is_unlimited() || d.name() == DIM_UNLIMITED {
                    continue;
                }
                dims.push(NDDimension::new(d.size()));
            }

            let original_type_ordinal = ds
                .get_global_attr_i32("dataType")
                .and_then(|slice| slice.first().copied());

            (is_record, dims, original_type_ordinal)
        };

        // Read first frame (record 0 if record variable, else full var)
        let data_vec = if is_record {
            reader.read_record(VAR_NAME, 0).map_err(map_read)?
        } else {
            reader.read_var(VAR_NAME).map_err(map_read)?
        };

        let (nd_type, buf) = match data_vec {
            netcdf3::DataVector::I8(v) => (NDDataType::Int8, NDDataBuffer::I8(v)),
            netcdf3::DataVector::U8(v) => (NDDataType::UInt8, NDDataBuffer::U8(v)),
            netcdf3::DataVector::I16(v) => (NDDataType::Int16, NDDataBuffer::I16(v)),
            netcdf3::DataVector::I32(v) => (NDDataType::Int32, NDDataBuffer::I32(v)),
            netcdf3::DataVector::F32(v) => (NDDataType::Float32, NDDataBuffer::F32(v)),
            netcdf3::DataVector::F64(v) => (NDDataType::Float64, NDDataBuffer::F64(v)),
        };

        // Check global attr "dataType" to recover original NDDataType
        let actual_type = original_type_ordinal
            .and_then(|v| NDDataType::from_ordinal(v as u8))
            .unwrap_or(nd_type);

        // Re-interpret if the original type was unsigned and stored as signed.
        // netCDF-3 has no unsigned types, so `dataType` is the only record of
        // the sign; UInt8 comes back from an NC_BYTE variable as i8.
        let buf = match (actual_type, buf) {
            (NDDataType::UInt8, NDDataBuffer::I8(v)) => {
                NDDataBuffer::U8(v.into_iter().map(|x| x as u8).collect())
            }
            (NDDataType::UInt16, NDDataBuffer::I16(v)) => {
                NDDataBuffer::U16(v.into_iter().map(|x| x as u16).collect())
            }
            (NDDataType::UInt32, NDDataBuffer::I32(v)) => {
                NDDataBuffer::U32(v.into_iter().map(|x| x as u32).collect())
            }
            (_, buf) => buf,
        };

        let mut arr = NDArray::new(dims, actual_type);
        arr.data = buf;
        Ok(arr)
    }

    fn supports_multiple_arrays(&self) -> bool {
        true
    }

    /// netCDF-3 keeps the record count in the header, and `netcdf3` 0.6 fixes
    /// it when `FileWriter::set_def` writes that header: `set_def` may be
    /// called once, `write_record_*` rejects any index past the count it
    /// declared, and `FileWriter::open` truncates rather than appends. The
    /// whole file therefore has to be written from `close_file`, so a frame is
    /// not on disk when `write_file` returns.
    fn writes_incrementally(&self) -> bool {
        false
    }
}

/// NetCDF file processor wrapping NDPluginFileBase + NetcdfWriter.
pub struct NetcdfFileProcessor {
    ctrl: FilePluginController<NetcdfWriter>,
}

impl NetcdfFileProcessor {
    pub fn new() -> Self {
        Self {
            ctrl: FilePluginController::new(NetcdfWriter::new()),
        }
    }
}

impl Default for NetcdfFileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for NetcdfFileProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        self.ctrl.process_array(array)
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
        self.ctrl.register_params(base)
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        params: &PluginParamSnapshot,
    ) -> ParamChangeResult {
        self.ctrl.on_param_change(reason, params)
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
        std::env::temp_dir().join(format!("adcore_test_{}_{}.nc", prefix, n))
    }

    /// D2 sibling: `NetcdfWriter::close_file` is where the buffered frames are
    /// written, and `frames.clear()` used to sit after all ten fallible writes.
    /// A failed flush then held the frames resident until some later
    /// `open_file` happened to clear them.
    #[test]
    fn close_file_clears_the_frame_buffer_when_the_write_fails() {
        // A directory that does not exist, so `FileWriter::open` fails.
        let path = std::env::temp_dir()
            .join("adcore-no-such-dir-for-netcdf-close")
            .join("frames.nc");
        let mut writer = NetcdfWriter::new();

        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
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
    fn test_write_u8_mono() {
        let path = temp_path("nc_u8");
        let mut writer = NetcdfWriter::new();

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

        // Verify file exists and has NetCDF magic bytes: "CDF\x01" or "CDF\x02"
        let data = std::fs::read(&path).unwrap();
        assert!(data.len() > 16);
        assert_eq!(&data[0..3], b"CDF", "Expected NetCDF magic bytes");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_u16() {
        let path = temp_path("nc_u16");
        let mut writer = NetcdfWriter::new();

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
        writer.close_file().unwrap();

        let data = std::fs::read(&path).unwrap();
        assert!(data.len() > 32);
        assert_eq!(&data[0..3], b"CDF");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_roundtrip_u8() {
        let path = temp_path("nc_rt_u8");
        let mut writer = NetcdfWriter::new();

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
        writer.close_file().unwrap();

        writer.current_path = Some(path.clone());
        let read_back = writer.read_file().unwrap();
        if let (NDDataBuffer::U8(orig), NDDataBuffer::U8(read)) = (&arr.data, &read_back.data) {
            assert_eq!(orig, read);
        } else {
            panic!("data type mismatch on roundtrip");
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_roundtrip_i16() {
        let path = temp_path("nc_rt_i16");
        let mut writer = NetcdfWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::Int16,
        );
        if let NDDataBuffer::I16(v) = &mut arr.data {
            for i in 0..16 {
                v[i] = (i as i16) * 100 - 500;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        writer.current_path = Some(path.clone());
        let read_back = writer.read_file().unwrap();
        if let (NDDataBuffer::I16(orig), NDDataBuffer::I16(read)) = (&arr.data, &read_back.data) {
            assert_eq!(orig, read);
        } else {
            panic!("data type mismatch on roundtrip");
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_roundtrip_f32() {
        let path = temp_path("nc_rt_f32");
        let mut writer = NetcdfWriter::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::Float32,
        );
        if let NDDataBuffer::F32(v) = &mut arr.data {
            for i in 0..16 {
                v[i] = i as f32 * 0.5;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        writer.current_path = Some(path.clone());
        let read_back = writer.read_file().unwrap();
        if let (NDDataBuffer::F32(orig), NDDataBuffer::F32(read)) = (&arr.data, &read_back.data) {
            assert_eq!(orig, read);
        } else {
            panic!("data type mismatch on roundtrip");
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_multiple_frames() {
        let path = temp_path("nc_multi");
        let mut writer = NetcdfWriter::new();

        let mut arr1 = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(v) = &mut arr1.data {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }

        let mut arr2 = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(v) = &mut arr2.data {
            for i in 0..16 {
                v[i] = (i as u8).wrapping_add(100);
            }
        }

        let mut arr3 = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(v) = &mut arr3.data {
            for i in 0..16 {
                v[i] = (i as u8).wrapping_add(200);
            }
        }

        writer.open_file(&path, NDFileMode::Stream, &arr1).unwrap();
        writer.write_file(&arr1).unwrap();
        writer.write_file(&arr2).unwrap();
        writer.write_file(&arr3).unwrap();
        writer.close_file().unwrap();

        // Read back first frame
        writer.current_path = Some(path.clone());
        let read_back = writer.read_file().unwrap();
        if let NDDataBuffer::U8(v) = &read_back.data {
            assert_eq!(v.len(), 16);
            for i in 0..16 {
                assert_eq!(v[i], i as u8, "mismatch at index {}", i);
            }
        } else {
            panic!("expected U8 data");
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_num_arrays_dim_keyed_on_open_mode_not_frame_count() {
        // R8-73. C picks the numArrays dimension from the open mode alone —
        // `if (openMode & NDFileModeMultiple) dim0 = NC_UNLIMITED`
        // (NDFileNetCDF.cpp:117-119) — and NDPluginFile passes that bit for
        // Capture and Stream but not Single (NDPluginFile.cpp:245, :281, :335).
        // A Capture/Stream file that ends up holding exactly ONE frame is
        // therefore still NC_UNLIMITED; deriving it from `frames.len() > 1` made
        // it a fixed dim of 1, a header divergence.
        let frame = || NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        let num_arrays_is_unlimited = |path: &PathBuf| -> bool {
            let reader = FileReader::open(path).unwrap();
            reader
                .data_set()
                .get_dim(DIM_UNLIMITED)
                .expect("numArrays dimension")
                .is_unlimited()
        };

        // One frame, Capture mode → NC_UNLIMITED (this is the R8-73 case).
        let path = temp_path("nc_mode_capture_one");
        let mut writer = NetcdfWriter::new();
        writer
            .open_file(&path, NDFileMode::Capture, &frame())
            .unwrap();
        writer.write_file(&frame()).unwrap();
        writer.close_file().unwrap();
        assert!(
            num_arrays_is_unlimited(&path),
            "Capture with 1 frame must still be NC_UNLIMITED"
        );
        std::fs::remove_file(&path).ok();

        // One frame, Stream mode → NC_UNLIMITED.
        let path = temp_path("nc_mode_stream_one");
        let mut writer = NetcdfWriter::new();
        writer
            .open_file(&path, NDFileMode::Stream, &frame())
            .unwrap();
        writer.write_file(&frame()).unwrap();
        writer.close_file().unwrap();
        assert!(
            num_arrays_is_unlimited(&path),
            "Stream with 1 frame must still be NC_UNLIMITED"
        );
        std::fs::remove_file(&path).ok();

        // One frame, Single mode → fixed dim of 1 (C's `dim0 = 1`).
        let path = temp_path("nc_mode_single_one");
        let mut writer = NetcdfWriter::new();
        writer
            .open_file(&path, NDFileMode::Single, &frame())
            .unwrap();
        writer.write_file(&frame()).unwrap();
        writer.close_file().unwrap();
        assert!(
            !num_arrays_is_unlimited(&path),
            "Single must be a fixed numArrays dimension"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_attributes_stored_as_per_frame_variables() {
        let path = temp_path("nc_attrs");
        let mut writer = NetcdfWriter::new();

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
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

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut reader = FileReader::open(&path).unwrap();
        {
            let ds = reader.data_set();
            // Per-attribute Attr_<name> variables exist with the leading dim.
            assert!(ds.get_var("Attr_exposure").is_some());
            assert!(ds.get_var("Attr_gain").is_some());
            // Four descriptive global text attributes per NDAttribute.
            assert_eq!(
                ds.get_global_attr_as_string("Attr_exposure_DataType"),
                Some("Float64".to_string())
            );
            assert_eq!(
                ds.get_global_attr_as_string("Attr_gain_DataType"),
                Some("Int32".to_string())
            );
            assert_eq!(
                ds.get_global_attr_as_string("Attr_exposure_Description"),
                Some("Exposure time".to_string())
            );
            assert_eq!(
                ds.get_global_attr_as_string("Attr_gain_SourceType"),
                Some("NDAttrSourceDriver".to_string())
            );
        }
        // The per-frame value is recoverable from the variable.
        if let netcdf3::DataVector::F64(v) = reader.read_var("Attr_exposure").unwrap() {
            assert_eq!(v, vec![0.5]);
        } else {
            panic!("Attr_exposure should be F64");
        }
        if let netcdf3::DataVector::I32(v) = reader.read_var("Attr_gain").unwrap() {
            assert_eq!(v, vec![42]);
        } else {
            panic!("Attr_gain should be I32");
        }

        drop(reader);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_single_frame_array_data_has_leading_numarrays_dim() {
        let path = temp_path("nc_rank");
        let mut writer = NetcdfWriter::new();

        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(3)],
            NDDataType::UInt8,
        );
        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let reader = FileReader::open(&path).unwrap();
        let ds = reader.data_set();
        let var = ds.get_var("array_data").unwrap();
        // C++ always defines array_data with rank ndims+1; a 2-D NDArray
        // single-frame file must therefore have a 3-D array_data variable.
        assert_eq!(var.get_dims().len(), 3);
        assert_eq!(var.get_dims()[0].name(), "numArrays");
        assert_eq!(var.get_dims()[0].size(), 1);

        drop(reader);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_global_attrs_match_c_set() {
        // C (NDFileNetCDF.cpp:92-101) writes dataType then the
        // NDNetCDFFileVersion=3.1 double as global attributes. uniqueId is a
        // per-frame variable (:183) and numArrays is the unlimited dimension
        // (:119) — neither must appear as a global attribute.
        let path = temp_path("nc_globals");
        let mut writer = NetcdfWriter::new();

        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(3)],
            NDDataType::UInt8,
        );
        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let reader = FileReader::open(&path).unwrap();
        let ds = reader.data_set();

        assert_eq!(
            ds.get_global_attr_f64("NDNetCDFFileVersion"),
            Some([3.1f64].as_slice()),
            "NDNetCDFFileVersion global must be the 3.1 double"
        );
        assert!(ds.has_global_attr("dataType"));
        assert!(
            !ds.has_global_attr("uniqueId"),
            "uniqueId is a variable in C, not a global attribute"
        );
        assert!(
            !ds.has_global_attr("numArrays"),
            "numArrays is a dimension in C, not a global attribute"
        );
        // uniqueId must still be present as a variable.
        assert!(ds.get_var("uniqueId").is_some());

        drop(reader);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_all_four_metadata_variables_written_single_frame() {
        let path = temp_path("nc_meta");
        let mut writer = NetcdfWriter::new();

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = 99;
        arr.time_stamp = 12.5;
        arr.timestamp.sec = 555;
        arr.timestamp.nsec = 777;

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut reader = FileReader::open(&path).unwrap();
        for name in ["uniqueId", "timeStamp", "epicsTSSec", "epicsTSNsec"] {
            assert!(
                reader.data_set().get_var(name).is_some(),
                "{name} variable missing"
            );
        }
        match reader.read_var("uniqueId").unwrap() {
            netcdf3::DataVector::I32(v) => assert_eq!(v, vec![99]),
            other => panic!("uniqueId wrong type: {other:?}"),
        }
        match reader.read_var("epicsTSSec").unwrap() {
            netcdf3::DataVector::I32(v) => assert_eq!(v, vec![555]),
            other => panic!("epicsTSSec wrong type: {other:?}"),
        }
        match reader.read_var("epicsTSNsec").unwrap() {
            netcdf3::DataVector::I32(v) => assert_eq!(v, vec![777]),
            other => panic!("epicsTSNsec wrong type: {other:?}"),
        }

        drop(reader);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_nddatatype_ordinals_match_c() {
        // The `dataType` global attribute stores `NDDataType as i32`, which the
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

    // ---------------------------------------------------------------------
    // R8-68: the on-disk header. These tests parse the raw CDF-1 bytes rather
    // than going through the netcdf3 crate, because the defect they pin was a
    // wrong nc_type *code* in the file — something an API-level round-trip
    // through the same crate cannot see.
    // ---------------------------------------------------------------------

    /// netCDF-3 nc_type codes (classic format spec).
    const NC_BYTE: u32 = 1;
    const NC_CHAR: u32 = 2;
    const NC_SHORT: u32 = 3;
    const NC_INT: u32 = 4;
    const NC_DOUBLE: u32 = 6;

    struct RawVar {
        name: String,
        nc_type: u32,
        dim_ids: Vec<u32>,
        begin: usize,
    }

    struct RawHeader {
        dims: Vec<(String, u32)>,
        gatt_names: Vec<String>,
        vars: Vec<RawVar>,
    }

    impl RawHeader {
        fn var(&self, name: &str) -> &RawVar {
            self.vars
                .iter()
                .find(|v| v.name == name)
                .unwrap_or_else(|| panic!("no variable {} in file", name))
        }
        fn var_names(&self) -> Vec<&str> {
            self.vars.iter().map(|v| v.name.as_str()).collect()
        }
        fn dim_names(&self) -> Vec<&str> {
            self.dims.iter().map(|(n, _)| n.as_str()).collect()
        }
    }

    /// Minimal reader for the CDF-1 header: magic, numrecs, dim_list,
    /// gatt_list, var_list. Independent of the netcdf3 crate on purpose.
    struct Cursor<'a> {
        b: &'a [u8],
        p: usize,
    }

    impl<'a> Cursor<'a> {
        fn u32(&mut self) -> u32 {
            let v = u32::from_be_bytes(self.b[self.p..self.p + 4].try_into().unwrap());
            self.p += 4;
            v
        }
        /// name = nelems + chars, zero-padded to a 4-byte boundary.
        fn name(&mut self) -> String {
            let n = self.u32() as usize;
            let s = String::from_utf8(self.b[self.p..self.p + n].to_vec()).unwrap();
            self.p += n.div_ceil(4) * 4;
            s
        }
        fn skip_att_list(&mut self) {
            let tag = self.u32();
            let n = self.u32() as usize;
            assert!(
                tag == 0x0C || (tag == 0 && n == 0),
                "bad att_list tag {tag}"
            );
            for _ in 0..n {
                let _name = self.name();
                let nc_type = self.u32();
                let nelems = self.u32() as usize;
                let size = match nc_type {
                    1 | 2 => 1,
                    3 => 2,
                    4 | 5 => 4,
                    6 => 8,
                    t => panic!("bad nc_type {t}"),
                };
                self.p += (nelems * size).div_ceil(4) * 4;
            }
        }
    }

    fn parse_header(bytes: &[u8]) -> RawHeader {
        assert_eq!(&bytes[0..4], b"CDF\x01", "not a CDF-1 file");
        let mut c = Cursor { b: bytes, p: 4 };
        let _numrecs = c.u32();

        // dim_list
        let tag = c.u32();
        let ndims = c.u32() as usize;
        assert!(tag == 0x0A || (tag == 0 && ndims == 0));
        let mut dims = Vec::new();
        for _ in 0..ndims {
            let name = c.name();
            let len = c.u32();
            dims.push((name, len));
        }

        // gatt_list — capture the names in file order, skip the values.
        let tag = c.u32();
        let natts = c.u32() as usize;
        assert!(tag == 0x0C || (tag == 0 && natts == 0));
        let mut gatt_names = Vec::new();
        for _ in 0..natts {
            let name = c.name();
            let nc_type = c.u32();
            let nelems = c.u32() as usize;
            let size = match nc_type {
                1 | 2 => 1,
                3 => 2,
                4 | 5 => 4,
                6 => 8,
                t => panic!("bad nc_type {t}"),
            };
            c.p += (nelems * size).div_ceil(4) * 4;
            gatt_names.push(name);
        }

        // var_list
        let tag = c.u32();
        let nvars = c.u32() as usize;
        assert!(tag == 0x0B || (tag == 0 && nvars == 0));
        let mut vars = Vec::new();
        for _ in 0..nvars {
            let name = c.name();
            let rank = c.u32() as usize;
            let dim_ids: Vec<u32> = (0..rank).map(|_| c.u32()).collect();
            c.skip_att_list();
            let nc_type = c.u32();
            let _vsize = c.u32();
            let begin = c.u32() as usize;
            vars.push(RawVar {
                name,
                nc_type,
                dim_ids,
                begin,
            });
        }
        RawHeader {
            dims,
            gatt_names,
            vars,
        }
    }

    fn write_one(path: &PathBuf, arr: &NDArray) -> Vec<u8> {
        let mut writer = NetcdfWriter::new();
        writer.open_file(path, NDFileMode::Single, arr).unwrap();
        writer.write_file(arr).unwrap();
        writer.close_file().unwrap();
        let bytes = std::fs::read(path).unwrap();
        std::fs::remove_file(path).ok();
        bytes
    }

    /// C maps NDUInt8 to NC_BYTE (NDFileNetCDF.cpp:155-158). The port wrote
    /// NC_CHAR, which is a text type: readers get characters, not numbers.
    #[test]
    fn test_r8_68_uint8_array_data_is_nc_byte() {
        let path = temp_path("nc_r8_68_byte");
        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(v) = &mut arr.data {
            v.copy_from_slice(&[0, 1, 200, 255]);
        }
        let bytes = write_one(&path, &arr);
        let hdr = parse_header(&bytes);

        let var = hdr.var("array_data");
        assert_eq!(
            var.nc_type, NC_BYTE,
            "array_data must be NC_BYTE, not NC_CHAR"
        );

        // C's nc_put_vara_uchar into an NC_BYTE variable copies the bit
        // pattern, so 200 and 255 land as 0xC8 and 0xFF on disk.
        assert_eq!(&bytes[var.begin..var.begin + 4], &[0x00, 0x01, 0xC8, 0xFF]);
    }

    /// Int8 keeps NC_BYTE too — both signednesses collapse onto it in C, and
    /// the `dataType` global attribute carries the sign.
    #[test]
    fn test_r8_68_int8_array_data_is_nc_byte() {
        let path = temp_path("nc_r8_68_i8");
        let arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::Int8,
        );
        let hdr = parse_header(&write_one(&path, &arr));
        assert_eq!(hdr.var("array_data").nc_type, NC_BYTE);
    }

    /// C defines a string attribute's variable as NC_CHAR
    /// (NDFileNetCDF.cpp:302-304); the port used NC_BYTE. Non-string
    /// attributes keep C's numeric types.
    #[test]
    fn test_r8_68_attr_variable_nc_types_match_c() {
        let path = temp_path("nc_r8_68_attrs");
        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::UInt16,
        );
        arr.attributes.add(NDAttribute::new_static(
            "Str",
            "a string",
            NDAttrSource::Driver,
            NDAttrValue::String("hello".into()),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "I8",
            "a byte",
            NDAttrSource::Driver,
            NDAttrValue::Int8(-3),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "I16",
            "a short",
            NDAttrSource::Driver,
            NDAttrValue::Int16(-3),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "I32",
            "an int",
            NDAttrSource::Driver,
            NDAttrValue::Int32(-3),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "I64",
            "a long",
            NDAttrSource::Driver,
            NDAttrValue::Int64(-3),
        ));
        let bytes = write_one(&path, &arr);
        let hdr = parse_header(&bytes);

        assert_eq!(
            hdr.var("Attr_Str").nc_type,
            NC_CHAR,
            "string attr must be NC_CHAR"
        );
        assert_eq!(hdr.var("Attr_I8").nc_type, NC_BYTE);
        assert_eq!(hdr.var("Attr_I16").nc_type, NC_SHORT);
        assert_eq!(hdr.var("Attr_I32").nc_type, NC_INT);
        // netCDF-3 has no 64-bit integer: C casts to double (:299-301).
        assert_eq!(hdr.var("Attr_I64").nc_type, NC_DOUBLE);

        // The string variable is 2-D: [numArrays, attrStringSize] (:313-316).
        let str_var = hdr.var("Attr_Str");
        assert_eq!(str_var.dim_ids.len(), 2);
        let attr_dim = str_var.dim_ids[1] as usize;
        assert_eq!(hdr.dims[attr_dim], ("attrStringSize".to_string(), 256));
        // Text on disk, NUL-padded to the fixed width.
        assert_eq!(&bytes[str_var.begin..str_var.begin + 6], b"hello\0");
    }

    /// C defines attrStringSize unconditionally (NDFileNetCDF.cpp:134-136).
    /// The port defined it only when a string attribute existed, so files with
    /// no string attribute had a dimension list C never writes.
    #[test]
    fn test_r8_68_attr_string_dim_defined_without_string_attrs() {
        let path = temp_path("nc_r8_68_nodim");
        let arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::UInt16,
        );
        let hdr = parse_header(&write_one(&path, &arr));
        assert_eq!(
            hdr.dim_names(),
            vec!["numArrays", "dim0", "dim1", "attrStringSize"]
        );
        assert_eq!(hdr.dims[3].1, 256);
    }

    /// The header's three lists are ordered, and C's order is the format.
    /// Dimensions: numArrays, the reversed array dims, attrStringSize.
    /// Variables: the four metadata variables, array_data, then the
    /// attributes. Global attributes: the seven fixed ones, then four per
    /// attribute (NDFileNetCDF.cpp:88-330).
    #[test]
    fn test_r8_68_definition_order_matches_c() {
        let path = temp_path("nc_r8_68_order");
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(2)],
            NDDataType::UInt16,
        );
        arr.attributes.add(NDAttribute::new_static(
            "Gain",
            "detector gain",
            NDAttrSource::Driver,
            NDAttrValue::Float64(2.5),
        ));
        let hdr = parse_header(&write_one(&path, &arr));

        assert_eq!(
            hdr.dim_names(),
            vec!["numArrays", "dim0", "dim1", "attrStringSize"]
        );
        // Reversed: netCDF's first dimension varies slowest (:122-127).
        assert_eq!(hdr.dims[1].1, 2);
        assert_eq!(hdr.dims[2].1, 4);

        assert_eq!(
            hdr.var_names(),
            vec![
                "uniqueId",
                "timeStamp",
                "epicsTSSec",
                "epicsTSNsec",
                "array_data",
                "Attr_Gain",
            ]
        );

        assert_eq!(
            hdr.gatt_names,
            vec![
                "dataType",
                "NDNetCDFFileVersion",
                "numArrayDims",
                "dimSize",
                "dimOffset",
                "dimBinning",
                "dimReverse",
                "Attr_Gain_DataType",
                "Attr_Gain_Description",
                "Attr_Gain_Source",
                "Attr_Gain_SourceType",
            ]
        );
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
}
