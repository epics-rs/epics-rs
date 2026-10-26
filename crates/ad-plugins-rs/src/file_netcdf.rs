use asyn_rs::port::InterfaceMask;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttributeList};
use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::{NDArray, NDDataType, NDDataView};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{FileAttributes, NDFileOpenMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::{FileControl, FilePluginController};
use ad_core_rs::plugin::runtime::{NDPluginProcess, PluginAttributes, PluginPool, ProcessResult};

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
///
/// Both versions are derived — ours from cargo, the HDF5 half from the
/// manifest pin by `build.rs` — because a literal here drifts silently: it
/// still said `0.6` once the pin moved to 0.7.2, so every file named a
/// library that had not written it.
const NC_PROPERTIES: &str = concat!(
    "version=2,ad-plugins-rs=",
    env!("CARGO_PKG_VERSION"),
    ",rust-hdf5=",
    env!("RUST_HDF5_PIN")
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
/// C's global attribute holding the `NDDataType` ordinal (NDFileNetCDF.cpp:92).
const DTYPE_ATTR: &str = "dataType";

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
/// The file follows C's define/write split: `open_file` creates the file, its
/// global attributes, dimensions and variables (NDFileNetCDF.cpp:58-327), and
/// each `write_file` appends one record along `numArrays` (:335-487), so a
/// frame is on disk when `write_file` returns and no frame is held.
pub struct NetcdfWriter {
    current_path: Option<PathBuf>,
    open: Option<OpenFile>,
    /// C `pFileAttributes` (NDFileNetCDF.cpp:72, :362): the writer's own
    /// attribute list, sticky for the life of the file.
    file_attributes: FileAttributes,
    /// The plugin's own set, C `getAttributes` in `openFile`/`writeFile`.
    plugin_attributes: NDAttributeList,
}

/// The variables C defines in `openFile`, held from open to close.
struct OpenFile {
    file: H5File,
    frame_scale: H5Dataset,
    array_data: H5Dataset,
    /// The element type and array dimensions `array_data` was defined with;
    /// a frame that differs cannot be a record of this variable.
    data_type: NDDataType,
    frame_dims: Vec<usize>,
    /// uniqueId, timeStamp, epicsTSSec, epicsTSNsec (C :183-199).
    unique_id: H5Dataset,
    time_stamp: H5Dataset,
    epics_ts_sec: H5Dataset,
    epics_ts_nsec: H5Dataset,
    /// One `Attr_<name>` variable per entry of `pFileAttributes` at open, in
    /// list order, typed by the value it had then. Record `n` is written from
    /// the first `attrs.len()` entries of the sticky list, which only grows
    /// and never re-orders (C's comment at :418 requires the set not change).
    attrs: Vec<(H5Dataset, NDAttrValue)>,
    records: usize,
}

impl NetcdfWriter {
    pub fn new() -> Self {
        Self {
            current_path: None,
            open: None,
            file_attributes: FileAttributes::default(),
            plugin_attributes: NDAttributeList::new(),
        }
    }
}

fn map_h5(e: rust_hdf5::Hdf5Error) -> ADError {
    ADError::UnsupportedConversion(format!("netCDF-4 write error: {e}"))
}

/// The `NAME` netcdf-c gives the scale of a dimension of length `len`.
fn dim_scale_name(len: usize) -> String {
    format!("{DIM_WITHOUT_VARIABLE}{len:10}")
}

/// Create one fixed dimension scale: a rank-1 dataset of `len` whose values
/// are never written, which is what a netCDF dimension with no coordinate
/// variable is. `set_scale` writes the `CLASS`/`NAME` pair in the fixed-length
/// form `H5DSis_scale` requires, and the returned handle is what the data
/// variables attach to.
fn create_dim_scale(file: &H5File, name: &str, len: usize) -> ADResult<H5Dataset> {
    let ds = file
        .new_dataset::<f32>()
        .shape(&[len][..])
        .create(name)
        .map_err(map_h5)?;
    ds.set_scale(Some(&dim_scale_name(len))).map_err(map_h5)?;
    Ok(ds)
}

/// Create a variable whose leading axis is `numArrays`, empty until the first
/// record. That axis is chunked one record deep so every append writes its
/// record at once instead of holding a partial chunk; it is unlimited for a
/// multi-array file and fixed at 1 otherwise (C :117-119), the netCDF-4
/// spelling of NC_UNLIMITED being an HDF5 axis with no maximum.
fn create_record_var<T: rust_hdf5::types::H5Type>(
    file: &H5File,
    name: &str,
    record_dims: &[usize],
    multiple: bool,
) -> ADResult<H5Dataset> {
    let mut shape = vec![0usize];
    shape.extend_from_slice(record_dims);
    let mut chunk = vec![1usize];
    chunk.extend(record_dims.iter().map(|&d| d.max(1)));
    let mut max_shape = vec![if multiple { None } else { Some(1) }];
    max_shape.extend(record_dims.iter().map(|&d| Some(d)));
    file.new_dataset::<T>()
        .shape(&shape[..])
        .chunk(&chunk[..])
        .max_shape(&max_shape[..])
        .create(name)
        .map_err(map_h5)
}

/// Create the `Attr_<name>` variable for an attribute, typed as its value is
/// (C NDFileNetCDF.cpp:312-321).
fn create_attr_var(
    file: &H5File,
    name: &str,
    value: &NDAttrValue,
    multiple: bool,
) -> ADResult<H5Dataset> {
    let create = match value {
        NDAttrValue::Int8(_) => create_record_var::<i8>,
        NDAttrValue::UInt8(_) => create_record_var::<u8>,
        NDAttrValue::Int16(_) => create_record_var::<i16>,
        NDAttrValue::UInt16(_) => create_record_var::<u16>,
        NDAttrValue::Int32(_) => create_record_var::<i32>,
        NDAttrValue::UInt32(_) => create_record_var::<u32>,
        NDAttrValue::Int64(_) => create_record_var::<i64>,
        NDAttrValue::UInt64(_) => create_record_var::<u64>,
        NDAttrValue::Float32(_) => create_record_var::<f32>,
        NDAttrValue::Float64(_) => create_record_var::<f64>,
        NDAttrValue::String(_) => create_record_var::<FixedStr<ATTR_STRING_SIZE>>,
        // C defines an undefined attribute as NC_BYTE (:305-306).
        NDAttrValue::Undefined => create_record_var::<i8>,
    };
    create(file, name, &[], multiple)
}

/// Append `value` as one record of an `Attr_<name>` variable defined with
/// `defined`'s type.
fn append_attr(ds: &H5Dataset, defined: &NDAttrValue, value: &NDAttrValue) -> ADResult<()> {
    let i = value.as_i64().unwrap_or(0);
    let f = value.as_f64().unwrap_or(0.0);
    match defined {
        NDAttrValue::Int8(_) => ds.append(&[i as i8]),
        NDAttrValue::UInt8(_) => ds.append(&[i as u8]),
        NDAttrValue::Int16(_) => ds.append(&[i as i16]),
        NDAttrValue::UInt16(_) => ds.append(&[i as u16]),
        NDAttrValue::Int32(_) => ds.append(&[i as i32]),
        NDAttrValue::UInt32(_) => ds.append(&[i as u32]),
        NDAttrValue::Int64(_) => ds.append(&[i]),
        NDAttrValue::UInt64(_) => ds.append(&[i as u64]),
        NDAttrValue::Float32(_) => ds.append(&[f as f32]),
        NDAttrValue::Float64(_) => ds.append(&[f]),
        NDAttrValue::String(_) => {
            ds.append(&[FixedStr::<ATTR_STRING_SIZE>::new(&value.as_string())])
        }
        // netCDF cannot store NaN; C writes a 0 byte (:470-473).
        NDAttrValue::Undefined => ds.append(&[0i8]),
    }
    .map_err(map_h5)
}

impl OpenFile {
    /// C `openFile` (NDFileNetCDF.cpp:58-327): everything but the records.
    fn define(
        path: &Path,
        multiple: bool,
        array: &NDArray,
        attrs: &FileAttributes,
    ) -> ADResult<Self> {
        let dims = &array.dims;
        let ndims = dims.len();
        let data_type = array.data().data_type();
        let file = H5File::create(path).map_err(map_h5)?;

        // --- Global attributes (C :92-101, :108-110, :140-151) --------------
        // The same set and values C writes, as root-group attributes: that is
        // where a netCDF-4 file keeps its global attributes. `_NCProperties`
        // is the one addition the container asks for. HDF5 stores attributes
        // in a name-indexed header, so unlike a netCDF-3 header this order is
        // not file format.
        file.set_attr_string("_NCProperties", NC_PROPERTIES)
            .map_err(map_h5)?;
        file.set_attr_numeric(DTYPE_ATTR, &(data_type as i32))
            .map_err(map_h5)?;
        file.set_attr_numeric("NDNetCDFFileVersion", &ND_NETCDF_FILE_VERSION)
            .map_err(map_h5)?;
        file.set_attr_numeric("numArrayDims", &(ndims as i32))
            .map_err(map_h5)?;
        // C reads dims[i] here — natural order, *not* the reversed order the
        // dimensions themselves are declared in (:125-131).
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
        for attr in attrs.iter() {
            for (suffix, value) in [
                ("DataType", attr_data_type_string(&attr.value)),
                ("Description", attr.description.as_str()),
                // C writes `NDAttribute::getSource()` verbatim; never
                // synthesize it.
                ("Source", attr.source.source_string()),
                ("SourceType", attr_source_type_string(&attr.source)),
            ] {
                file.set_attr_string(&format!("Attr_{}_{suffix}", attr.name), value)
                    .map_err(map_h5)?;
            }
        }

        // --- Dimensions (C :117-137) ----------------------------------------
        // numArrays first, then the array dimensions reversed: netCDF's first
        // dimension varies slowest, the opposite of the NDArray convention
        // (:123-132).
        let record_dims: Vec<usize> = dims.iter().rev().map(|d| d.size).collect();
        let frame_scale = create_record_var::<f32>(&file, DIM_UNLIMITED, &[], multiple)?;
        frame_scale
            .set_scale(Some(&dim_scale_name(0)))
            .map_err(map_h5)?;
        let mut dim_scales = Vec::with_capacity(ndims);
        for (i, &len) in record_dims.iter().enumerate() {
            dim_scales.push(create_dim_scale(&file, &format!("dim{i}"), len)?);
        }

        // --- Variables (C :183-204, :312-321) -------------------------------
        // Every variable's leading axis is the frame dimension, and
        // `array_data` carries the array dimensions after it: a netCDF
        // variable's dimensions are the dimension scales attached to its axes,
        // and an unattached axis is read as an anonymous `phony_dim_N` instead
        // of as `numArrays`.
        let unique_id = create_record_var::<i32>(&file, "uniqueId", &[], multiple)?;
        let time_stamp = create_record_var::<f64>(&file, "timeStamp", &[], multiple)?;
        let epics_ts_sec = create_record_var::<i32>(&file, "epicsTSSec", &[], multiple)?;
        let epics_ts_nsec = create_record_var::<i32>(&file, "epicsTSNsec", &[], multiple)?;
        for ds in [&unique_id, &time_stamp, &epics_ts_sec, &epics_ts_nsec] {
            ds.attach_scale(0, &frame_scale).map_err(map_h5)?;
        }

        // netCDF-4 has the full signed/unsigned integer set, so every NDArray
        // type is stored as itself. netCDF-3 had neither unsigned nor 64-bit
        // integers, which is why C reinterprets UInt8 as NC_BYTE and casts
        // Int64/UInt64 to NC_DOUBLE (NDFileNetCDF.cpp:154-180); none of that
        // is needed here, and the 64-bit integer frames are no longer rounded
        // through a double.
        let create = match data_type {
            NDDataType::Int8 => create_record_var::<i8>,
            NDDataType::UInt8 => create_record_var::<u8>,
            NDDataType::Int16 => create_record_var::<i16>,
            NDDataType::UInt16 => create_record_var::<u16>,
            NDDataType::Int32 => create_record_var::<i32>,
            NDDataType::UInt32 => create_record_var::<u32>,
            NDDataType::Int64 => create_record_var::<i64>,
            NDDataType::UInt64 => create_record_var::<u64>,
            NDDataType::Float32 => create_record_var::<f32>,
            NDDataType::Float64 => create_record_var::<f64>,
        };
        let array_data = create(&file, VAR_NAME, &record_dims, multiple)?;
        array_data.attach_scale(0, &frame_scale).map_err(map_h5)?;
        for (axis, scale) in dim_scales.iter().enumerate() {
            array_data.attach_scale(axis + 1, scale).map_err(map_h5)?;
        }

        let mut attr_vars = Vec::with_capacity(attrs.len());
        for attr in attrs.iter() {
            let ds = create_attr_var(&file, &format!("Attr_{}", attr.name), &attr.value, multiple)?;
            ds.attach_scale(0, &frame_scale).map_err(map_h5)?;
            attr_vars.push((ds, attr.value.clone()));
        }

        Ok(Self {
            file,
            frame_scale,
            array_data,
            data_type,
            frame_dims: dims.iter().map(|d| d.size).collect(),
            unique_id,
            time_stamp,
            epics_ts_sec,
            epics_ts_nsec,
            attrs: attr_vars,
            records: 0,
        })
    }

    /// C `writeFile` (NDFileNetCDF.cpp:335-487): one record per frame.
    fn append(&mut self, array: &NDArray, attrs: &FileAttributes) -> ADResult<()> {
        if array.data().data_type() != self.data_type
            || !array
                .dims
                .iter()
                .map(|d| d.size)
                .eq(self.frame_dims.iter().copied())
        {
            return Err(ADError::UnsupportedConversion(format!(
                "frame {} does not match the array_data variable defined at open",
                array.unique_id
            )));
        }
        self.unique_id.append(&[array.unique_id]).map_err(map_h5)?;
        self.time_stamp
            .append(&[array.time_stamp])
            .map_err(map_h5)?;
        self.epics_ts_sec
            .append(&[array.timestamp.sec as i32])
            .map_err(map_h5)?;
        self.epics_ts_nsec
            .append(&[array.timestamp.nsec as i32])
            .map_err(map_h5)?;
        let ds = &self.array_data;
        match array.data().view() {
            NDDataView::I8(v) => ds.append(v),
            NDDataView::U8(v) => ds.append(v),
            NDDataView::I16(v) => ds.append(v),
            NDDataView::U16(v) => ds.append(v),
            NDDataView::I32(v) => ds.append(v),
            NDDataView::U32(v) => ds.append(v),
            NDDataView::I64(v) => ds.append(v),
            NDDataView::U64(v) => ds.append(v),
            NDDataView::F32(v) => ds.append(v),
            NDDataView::F64(v) => ds.append(v),
        }
        .map_err(map_h5)?;
        // C writes every attribute variable out of the sticky list
        // (:419-483), so an attribute that drops out of a later frame goes on
        // record at its most recent value.
        for ((ds, defined), attr) in self.attrs.iter().zip(attrs.iter()) {
            append_attr(ds, defined, &attr.value)?;
        }
        self.records += 1;
        self.frame_scale.extend(&[self.records]).map_err(map_h5)
    }
}

impl NDFileWriter for NetcdfWriter {
    fn open_file(&mut self, path: &Path, mode: NDFileOpenMode, array: &NDArray) -> ADResult<()> {
        // NDFileNetCDF.cpp:64-67
        mode.refuse_read_or_append("NDFileNetCDF")?;
        self.open = None;
        self.current_path = Some(path.to_path_buf());
        // C `openFile` clears `pFileAttributes`, gets the plugin's set and
        // copies this frame's list on top (NDFileNetCDF.cpp:72-77).
        self.file_attributes.open(&self.plugin_attributes, array);
        // The Multiple bit alone makes numArrays unlimited
        // (NDFileNetCDF.cpp:117-119) however many frames the file ends with.
        let multiple = mode.contains(NDFileOpenMode::MULTIPLE);
        self.open = Some(OpenFile::define(
            path,
            multiple,
            array,
            &self.file_attributes,
        )?);
        Ok(())
    }

    fn write_file(&mut self, array: &Arc<NDArray>) -> ADResult<()> {
        let open = self
            .open
            .as_mut()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;
        // C `writeFile` merges the plugin's set and this frame into
        // `pFileAttributes` first (NDFileNetCDF.cpp:359-362).
        self.file_attributes.frame(&self.plugin_attributes, array);
        open.append(array, &self.file_attributes)
    }

    /// The open file is taken before anything fallible, so a failed close
    /// still releases it.
    fn close_file(&mut self) -> ADResult<()> {
        self.current_path = None;
        let Some(open) = self.open.take() else {
            return Ok(());
        };
        // netcdf-c records a dimension's length in its scale's `NAME`.
        open.frame_scale
            .set_scale(Some(&dim_scale_name(open.records)))
            .map_err(map_h5)?;
        // Not left to `Drop`: closing writes the superblock and the root
        // header, and that failure has to be reported.
        open.file.close().map_err(map_h5)
    }

    /// C `readFile` returns asynError (NDFileNetCDF.cpp:491-496).
    fn read_file(&mut self, _pool: &NDArrayPool) -> ADResult<NDArray> {
        Err(ADError::UnsupportedConversion(
            "NDFileNetCDF cannot read".into(),
        ))
    }

    fn set_plugin_attributes(&mut self, attributes: &NDAttributeList) {
        self.plugin_attributes.clone_from(attributes);
    }

    fn supports_multiple_arrays(&self) -> bool {
        true
    }
}

/// NetCDF file processor wrapping NDPluginFileBase + NetcdfWriter.
pub struct NetcdfFileProcessor {
    ctrl: Mutex<FilePluginController<NetcdfWriter>>,
    attributes: Option<PluginAttributes>,
}

impl NetcdfFileProcessor {
    pub fn new() -> Self {
        Self {
            ctrl: Mutex::new(FilePluginController::new(NetcdfWriter::new())),
            attributes: None,
        }
    }
}

impl Default for NetcdfFileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for NetcdfFileProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_CANBLOCK;
    // NDFileNetCDF.cpp:526, the same mask for both.
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
        "NDFileNetCDF"
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
    use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
    use ad_core_rs::ndarray::{NDDataBuffer, NDDataViewMut, NDDimension};
    use ad_core_rs::plugin::file_base::{NDFileMode, NDPluginFileBase};
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

    /// C `openFile` refuses NDFileModeRead (NDFileNetCDF.cpp:64), so ReadFile
    /// fails at the open.
    #[test]
    fn read_file_is_refused_at_the_open() {
        let path = temp_path("nc_read_refused");
        let mut writer = NetcdfWriter::new();
        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        writer
            .open_file(&path, NDFileOpenMode::WRITE, &arr)
            .unwrap();
        writer.write_file(&Arc::new(arr)).unwrap();
        writer.close_file().unwrap();
        let mut proc = NetcdfFileProcessor::new();
        let (out, _base) = crate::test_util::read_file(&mut proc, &path);
        assert_eq!(
            out.trace,
            crate::test_util::read_refused_lines(&path, &_base)
        );
        assert_eq!(out.publish, ad_core_rs::plugin::runtime::Published::Nothing);
        std::fs::remove_file(&path).ok();
    }

    fn make_u8(dims: &[usize], fill: impl Fn(usize) -> u8) -> NDArray {
        let n: usize = dims.iter().product();
        let mut arr = NDArray::new(
            dims.iter().map(|&d| NDDimension::new(d)).collect(),
            NDDataType::UInt8,
        );
        if let NDDataViewMut::U8(v) = arr.data_mut().view_mut() {
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
        writer
            .open_file(&path, crate::test_util::opened(mode), &frames[0])
            .unwrap();
        for f in frames {
            writer.write_file(&Arc::new(f.clone())).unwrap();
        }
        writer.close_file().unwrap();
        path
    }

    /// Record 0 of `array_data` (`[frames, dim[n-1], … dim[0]]`), in NDArray
    /// dimension order.
    fn read_back(path: &Path) -> NDArray {
        let file = H5File::open(path).unwrap();
        let ds = file.dataset(VAR_NAME).unwrap();
        let dims = ds.shape()[1..]
            .iter()
            .rev()
            .map(|&s| NDDimension::new(s))
            .collect();
        let data = crate::test_util::read_native(&ds, Some(1));
        NDArray::with_data(dims, data)
    }

    /// C creates the file in `openFile` (NDFileNetCDF.cpp:84-85), so a path
    /// that cannot be created fails the open, and no file is left open for a
    /// record to land in.
    #[test]
    fn an_open_that_cannot_create_the_file_fails_and_leaves_none_open() {
        // A parent that does NOT exist, under a root this test owns.
        let root = tempfile::tempdir().expect("fixture root");
        let path = root.path().join("no-such-dir").join("frames.nc");
        let mut writer = NetcdfWriter::new();

        let arr = make_u8(&[4, 4], |_| 0);
        assert!(
            writer
                .open_file(&path, NDFileOpenMode::WRITE, &arr)
                .is_err()
        );
        assert!(writer.open.is_none());
        assert!(writer.write_file(&Arc::new(arr)).is_err());
    }

    /// Each record is written by `write_file`, so the writer keeps no
    /// reference to the frame.
    #[test]
    fn write_file_does_not_hold_the_frame() {
        let path = temp_path("nc_arc");
        let mut writer = NetcdfWriter::new();
        let arr = Arc::new(make_u8(&[4, 4], |_| 0));
        writer
            .open_file(
                &path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                &arr,
            )
            .unwrap();
        writer.write_file(&arr).unwrap();
        assert_eq!(Arc::strong_count(&arr), 1);
        assert_eq!(writer.open.as_ref().unwrap().records, 1);
        writer.close_file().unwrap();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_u8_mono() {
        let arr = make_u8(&[4, 4], |i| (i * 10) as u8);
        let path = write_frames("nc_u8", NDFileMode::Single, std::slice::from_ref(&arr));
        let back = read_back(&path);
        let (NDDataView::U8(want), NDDataView::U8(got)) = (arr.data().view(), back.data().view())
        else {
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
        if let NDDataViewMut::U16(v) = arr.data_mut().view_mut() {
            for i in 0..16 {
                v[i] = (i * 1000) as u16;
            }
        }
        let path = write_frames("nc_u16", NDFileMode::Single, std::slice::from_ref(&arr));
        let back = read_back(&path);
        let (NDDataView::U16(want), NDDataView::U16(got)) = (arr.data().view(), back.data().view())
        else {
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
                let arr = NDArray::with_data(
                    vec![NDDimension::new(values.len())],
                    NDDataBuffer::from_slice(&values),
                );
                let path = write_frames($prefix, NDFileMode::Single, &[arr]);
                let back = read_back(&path);
                let NDDataView::$variant(got) = back.data().view() else {
                    panic!("{} came back as {:?}", $prefix, back.data().data_type());
                };
                assert_eq!(got, &values, "{}", $prefix);
                assert_eq!(back.data().data_type(), $dt);
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
        let NDDataView::U8(v) = back.data().view() else {
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
            let NDDataView::U8(want) = frame.data().view() else {
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

    /// C evaluates a flushed frame's attributes on the flushing thread
    /// (NDPluginFile.cpp:301), so its hand-on never waits on the port: a
    /// Single-mode WriteFile arriving mid-flush waits for the capture file
    /// to close, then writes its own.
    #[test]
    fn a_single_write_file_during_a_capture_flush_returns() {
        use crate::test_util::{frame, gated, publish, wait_saved, write_file_returns};
        use std::time::Duration;
        let path = temp_path("flush_write_file");
        let (keep, handle, gate, entered) = gated("NC_FLUSH_WF", NetcdfFileProcessor::new(), &path);
        let up = handle.port_runtime().port_handle();
        let p = &handle.ndarray_params;
        up.submit_blocking(
            asyn_rs::request::RequestOp::OctetWrite {
                data: b"%s%s_%d".to_vec(),
            },
            asyn_rs::user::AsynUser::new(p.file_template),
        )
        .unwrap();
        up.write_int32_blocking(p.auto_increment, 0, 1).unwrap();
        publish(&handle, &frame(0));
        wait_saved(&handle, 1);
        assert_eq!(entered.recv_timeout(Duration::from_secs(10)), Ok(0));
        up.write_int32_blocking(p.file_write_mode, 0, NDFileMode::Capture as i32)
            .unwrap();
        up.write_int32_blocking(p.num_capture, 0, 2).unwrap();
        up.write_int32_blocking(p.capture, 0, 1).unwrap();
        assert!(handle.wait_params_applied(Duration::from_secs(10)));

        let held = gate.lock();
        publish(&handle, &frame(1));
        publish(&handle, &frame(2));
        assert_eq!(entered.recv_timeout(Duration::from_secs(10)), Ok(1));
        up.write_int32_blocking(p.file_write_mode, 0, NDFileMode::Single as i32)
            .unwrap();
        let returned = write_file_returns(&handle, || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        if !returned {
            // The port is deadlocked; its threads cannot be joined.
            std::mem::forget((keep, handle));
            panic!("WriteFile during a capture flush never returned");
        }
        let ids = |n: i32| -> Vec<i32> {
            let name = format!("{}_{n}", path.display());
            let ids = H5File::open(&name)
                .unwrap()
                .dataset("uniqueId")
                .unwrap()
                .read_raw()
                .unwrap();
            std::fs::remove_file(&name).ok();
            ids
        };
        assert_eq!(ids(0), vec![0]);
        assert_eq!(ids(1), vec![1, 2], "the capture file keeps both frames");
        assert_eq!(ids(2), vec![2], "WriteFile wrote after the flush closed");
    }

    /// C `openFile`/`writeFile` fill `pFileAttributes` from `getAttributes`
    /// with the frame's list copied on top (NDFileNetCDF.cpp:72-77,
    /// :359-362), so the plugin's own set gets `Attr_` variables.
    #[test]
    fn the_plugin_attributes_get_variables_under_the_frame() {
        use crate::test_util::{auto_save_to, publish, wait_saved, with_attributes_file};
        let xml = r#"<Attributes>
            <Attribute name="plugin_only" type="PARAM" source="AUTO_SAVE" datatype="INT"/>
            <Attribute name="gain" type="CONST" source="99"/>
        </Attributes>"#;
        let (handle, _rx, _jh) =
            with_attributes_file("NETCDF_ATTRS", NetcdfFileProcessor::new(), xml);
        let path = temp_path("plugin_attrs");
        auto_save_to(&handle, &path);
        let mut arr = make_u8(&[4], |_| 0);
        arr.attributes.add(NDAttribute::new_static(
            "gain",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(42),
        ));
        publish(&handle, &arr);
        wait_saved(&handle, 1);
        let file = H5File::open(&path).unwrap();
        let values = |name: &str| -> Vec<i32> { file.dataset(name).unwrap().read_raw().unwrap() };
        assert_eq!(values("Attr_plugin_only"), vec![1]);
        assert_eq!(values("Attr_gain"), vec![42]);
        drop(file);
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

    /// C puts `dataType` on the file only (NDFileNetCDF.cpp:92), not on
    /// `array_data`.
    #[test]
    fn array_data_carries_no_data_type_attribute() {
        let path = write_frames(
            "nc_no_dtype",
            NDFileMode::Single,
            &[make_u8(&[4, 3], |_| 0)],
        );
        let file = H5File::open(&path).unwrap();
        let names = file.dataset(VAR_NAME).unwrap().attr_names().unwrap();
        assert!(!names.iter().any(|n| n == "dataType"), "{names:?}");
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
            ("Undef", NDAttrValue::Undefined),
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
            // C defines an undefined attribute as NC_BYTE and writes 0
            // (NDFileNetCDF.cpp:305-306, :470-473).
            ("Attr_Undef", 1),
        ] {
            let ds = file.dataset(name).unwrap();
            assert_eq!(ds.element_size(), width, "{name} element width");
            assert_eq!(ds.shape(), vec![1], "{name} shape");
        }
        assert_eq!(
            file.dataset("Attr_I64").unwrap().read_raw::<i64>().unwrap(),
            vec![-3]
        );
        assert_eq!(
            file.dataset("Attr_Undef")
                .unwrap()
                .read_raw::<i8>()
                .unwrap(),
            vec![0]
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
    /// reaches a close. C appends one record per `writeFile`
    /// (NDFileNetCDF.cpp:335-487), so every frame `NumCaptured_RBV` counts is
    /// already a record of the open file, and none is held in RAM.
    #[test]
    fn stream_mode_appends_one_record_per_frame() {
        let root = tempfile::tempdir().expect("fixture root");
        let mut fb = NDPluginFileBase::new();
        fb.file_path = format!("{}/", root.path().display());
        fb.file_name = "nc_stream_".into();
        fb.file_template = "%s%s%4.4d".into();
        fb.set_mode(NDFileMode::Stream);
        fb.set_num_capture(0);

        let mut writer = NetcdfWriter::new();
        let frames: Vec<Arc<NDArray>> = (0u8..3)
            .map(|k| Arc::new(make_u8(&[4], move |i| i as u8 + k * 10)))
            .collect();
        for (n, frame) in frames.iter().enumerate() {
            fb.process_array(Arc::clone(frame), &mut writer).unwrap();
            assert!(fb.is_open());
            assert_eq!(fb.num_captured(), n + 1);
            assert_eq!(writer.open.as_ref().unwrap().records, n + 1);
        }
        assert!(
            frames.iter().all(|f| Arc::strong_count(f) == 1),
            "no frame may be held"
        );

        let path = PathBuf::from(fb.full_file_name());
        fb.close_stream(&mut writer).unwrap();
        let file = H5File::open(&path).unwrap();
        let ds = file.dataset(VAR_NAME).unwrap();
        assert_eq!(ds.max_shape().unwrap()[0], None);
        for (k, frame) in frames.iter().enumerate() {
            let NDDataView::U8(want) = frame.data().view() else {
                unreachable!()
            };
            assert_eq!(&ds.read_slice::<u8>(&[k, 0], &[1, 4]).unwrap(), want);
        }
        assert_eq!(
            file.dataset(DIM_UNLIMITED)
                .unwrap()
                .attr("NAME")
                .unwrap()
                .read_string()
                .unwrap(),
            dim_scale_name(3)
        );
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
        let sent = mk(0);
        let (NDDataView::U8(want), NDDataView::U8(got)) = (sent.data().view(), back.data().view())
        else {
            panic!("expected UInt8 on both sides");
        };
        assert_eq!(want, got);
        std::fs::remove_file(&path).ok();

        let path = write_frames("nc_big_capture", NDFileMode::Capture, &[mk(0), mk(77)]);
        let file = H5File::open(&path).unwrap();
        let ds = file.dataset(VAR_NAME).unwrap();
        for (record, seed) in [(0usize, 0u8), (1, 77)] {
            let got = ds.read_slice::<u8>(&[record, 0], &[1, n]).unwrap();
            let sent = mk(seed);
            let NDDataView::U8(want) = sent.data().view() else {
                unreachable!()
            };
            assert_eq!(&got, want, "record {record}");
        }
        drop(file);
        std::fs::remove_file(&path).ok();
    }
}
