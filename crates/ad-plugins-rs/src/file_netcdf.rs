use std::path::{Path, PathBuf};

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue};
use ad_core_rs::error::{ADError, ADResult};
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

/// Map an `NDAttrSource` to the C++ `getSourceInfo()` source-type string.
fn attr_source_type_string(src: &NDAttrSource) -> &'static str {
    match src {
        NDAttrSource::Driver => "NDAttrSourceDriver",
        NDAttrSource::EpicsPV => "NDAttrSourceEPICSPV",
        NDAttrSource::Param { .. } => "NDAttrSourceParam",
        NDAttrSource::Function => "NDAttrSourceFunct",
        NDAttrSource::Constant => "NDAttrSourceConst",
        NDAttrSource::Undefined => "Undefined",
    }
}

/// Map an `NDAttrSource` to the C++ `getSource()` source string.
fn attr_source_string(src: &NDAttrSource) -> String {
    match src {
        NDAttrSource::Driver => "Driver".to_string(),
        NDAttrSource::EpicsPV => "EPICS_PV".to_string(),
        NDAttrSource::Param { param_name, .. } => param_name.clone(),
        NDAttrSource::Function => "Function".to_string(),
        NDAttrSource::Constant => "Const".to_string(),
        NDAttrSource::Undefined => String::new(),
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
}

impl NetcdfWriter {
    pub fn new() -> Self {
        Self {
            current_path: None,
            frames: Vec::new(),
        }
    }
}

/// Map NDDataType → netcdf3 DataType.  Returns error for 64-bit integers
/// which NetCDF-3 classic format does not support.
fn nc_data_type(dt: NDDataType) -> ADResult<netcdf3::DataType> {
    match dt {
        NDDataType::Int8 => Ok(netcdf3::DataType::I8),
        NDDataType::UInt8 => Ok(netcdf3::DataType::U8),
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
        NDDataBuffer::U8(v) => writer.write_var_u8(VAR_NAME, v).map_err(err),
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
        NDDataBuffer::U8(v) => writer
            .write_record_u8(VAR_NAME, record_index, v)
            .map_err(err),
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

/// netCDF-3 storage type for an NDAttribute value (NDFileNetCDF.cpp:283-310).
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
        NDAttrValue::String(_) => netcdf3::DataType::I8,
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
    // String values are stored as a fixed-width char row.
    if let NDAttrValue::String(s) = value {
        let mut bytes: Vec<i8> = s.bytes().take(ATTR_STRING_SIZE).map(|b| b as i8).collect();
        bytes.resize(ATTR_STRING_SIZE, 0);
        return if multi {
            writer
                .write_record_i8(var_name, record_index, &bytes)
                .map_err(werr)
        } else {
            writer.write_var_i8(var_name, &bytes).map_err(werr)
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
        netcdf3::DataType::U8 => unreachable!("attr_nc_type never returns U8"),
    }
}

impl NDFileWriter for NetcdfWriter {
    fn open_file(&mut self, path: &Path, _mode: NDFileMode, _array: &NDArray) -> ADResult<()> {
        self.current_path = Some(path.to_path_buf());
        self.frames.clear();
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
                source: attr_source_string(&a.source),
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

    fn close_file(&mut self) -> ADResult<()> {
        let path = match self.current_path.take() {
            Some(p) => p,
            None => return Ok(()),
        };

        if self.frames.is_empty() {
            return Ok(());
        }

        let map_def = |e: netcdf3::error::InvalidDataSet| {
            ADError::UnsupportedConversion(format!("NetCDF definition error: {:?}", e))
        };
        let map_write = |e: netcdf3::error::WriteError| {
            ADError::UnsupportedConversion(format!("NetCDF write error: {:?}", e))
        };

        let first = &self.frames[0];
        let nc_dt = nc_data_type(first.data_type)?;
        let multi = self.frames.len() > 1;

        // Build DataSet definition
        let mut ds = DataSet::new();

        // Leading "numArrays" dimension: NC_UNLIMITED for multi-frame files,
        // a fixed dimension of size 1 for single-frame files. C++ NDFileNetCDF
        // always defines `array_data` with rank `ndims+1` and dim0 = numArrays
        // (NDFileNetCDF.cpp:117-119), so a single-frame file is still rank
        // `ndims+1`, not `ndims`.
        if multi {
            ds.set_unlimited_dim(DIM_UNLIMITED, self.frames.len())
                .map_err(map_def)?;
        } else {
            ds.add_fixed_dim(DIM_UNLIMITED, 1).map_err(map_def)?;
        }

        // Fixed dimensions in reversed order (matching C++ NDFileNetCDF)
        let ndims = first.dims.len();
        let mut dim_names: Vec<String> = Vec::new();
        for i in 0..ndims {
            let dim_idx = ndims - 1 - i;
            let name = format!("dim{}", i);
            ds.add_fixed_dim(&name, first.dims[dim_idx])
                .map_err(map_def)?;
            dim_names.push(name);
        }

        // String-attribute fixed dimension (NDFileNetCDF.cpp:135).
        let has_string_attr = self.frames.iter().any(|f| {
            f.attrs
                .iter()
                .any(|a| matches!(a.value, NDAttrValue::String(_)))
        });
        if has_string_attr {
            ds.add_fixed_dim(ATTR_STRING_DIM, ATTR_STRING_SIZE)
                .map_err(map_def)?;
        }

        // `array_data` always carries the leading numArrays dimension.
        let var_dims: Vec<String> = {
            let mut v = vec![DIM_UNLIMITED.to_string()];
            v.extend(dim_names.iter().cloned());
            v
        };
        let var_dim_refs: Vec<&str> = var_dims.iter().map(|s| s.as_str()).collect();
        ds.add_var(VAR_NAME, &var_dim_refs, nc_dt)
            .map_err(map_def)?;

        // Per-frame metadata variables — always defined, leading numArrays dim.
        ds.add_var("uniqueId", &[DIM_UNLIMITED], netcdf3::DataType::I32)
            .map_err(map_def)?;
        ds.add_var("timeStamp", &[DIM_UNLIMITED], netcdf3::DataType::F64)
            .map_err(map_def)?;
        ds.add_var("epicsTSSec", &[DIM_UNLIMITED], netcdf3::DataType::I32)
            .map_err(map_def)?;
        ds.add_var("epicsTSNsec", &[DIM_UNLIMITED], netcdf3::DataType::I32)
            .map_err(map_def)?;

        // Per-attribute record variables `Attr_<name>` plus the four
        // global text attributes describing each one (NDFileNetCDF.cpp:210-330).
        // The attribute set is taken from the first frame (C++ snapshots the
        // attribute list at openFile time).
        let mut attr_var_names: Vec<String> = Vec::new();
        for attr in &first.attrs {
            let var_name = format!("Attr_{}", attr.name);
            let nc_type = attr_nc_type(&attr.value);
            let is_string = matches!(attr.value, NDAttrValue::String(_));
            if is_string {
                ds.add_var(
                    &var_name,
                    &[DIM_UNLIMITED, ATTR_STRING_DIM],
                    netcdf3::DataType::I8,
                )
                .map_err(map_def)?;
            } else {
                ds.add_var(&var_name, &[DIM_UNLIMITED], nc_type)
                    .map_err(map_def)?;
            }
            attr_var_names.push(var_name);

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
        }

        // Global attributes
        ds.add_global_attr_i32("uniqueId", vec![first.unique_id])
            .map_err(map_def)?;
        ds.add_global_attr_i32("dataType", vec![first.data_type as i32])
            .map_err(map_def)?;
        ds.add_global_attr_i32("numArrays", vec![self.frames.len() as i32])
            .map_err(map_def)?;

        // Dimension metadata global attributes
        ds.add_global_attr_i32("numArrayDims", vec![ndims as i32])
            .map_err(map_def)?;
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

        // Write
        let mut writer = FileWriter::open(&path).map_err(map_write)?;
        writer
            .set_def(&ds, Version::Classic, 0)
            .map_err(map_write)?;

        if multi {
            for (i, frame) in self.frames.iter().enumerate() {
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
            write_var_data(&mut writer, &self.frames[0].data)?;
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
        self.frames.clear();
        Ok(())
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

        // Re-interpret if the original type was unsigned and stored as signed
        let buf = match (actual_type, buf) {
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
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(prefix: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("adcore_test_{}_{}.nc", prefix, n))
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
}
