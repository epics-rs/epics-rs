use std::path::{Path, PathBuf};

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

/// A single buffered frame captured from an NDArray.
struct FrameData {
    dims: Vec<usize>,
    data: NDDataBuffer,
    data_type: NDDataType,
    attrs: Vec<(String, String)>,
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
        NDDataType::Int64 | NDDataType::UInt64 => Err(ADError::UnsupportedConversion(
            "NetCDF-3 does not support 64-bit integer types".into(),
        )),
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
        _ => Err(ADError::UnsupportedConversion(
            "NetCDF-3 does not support 64-bit integer types".into(),
        )),
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
        _ => Err(ADError::UnsupportedConversion(
            "NetCDF-3 does not support 64-bit integer types".into(),
        )),
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
        let attrs: Vec<(String, String)> = array
            .attributes
            .iter()
            .map(|a| (a.name.clone(), a.value.as_string()))
            .collect();

        self.frames.push(FrameData {
            dims,
            data: array.data.clone(),
            data_type: array.data.data_type(),
            attrs,
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

        // Fixed dimensions: dim0, dim1, ...
        let mut dim_names: Vec<String> = Vec::new();
        for (i, &size) in first.dims.iter().enumerate() {
            let name = format!("dim{}", i);
            ds.add_fixed_dim(&name, size).map_err(map_def)?;
            dim_names.push(name);
        }

        // Variable dimensions list
        let var_dims: Vec<String> = if multi {
            // Unlimited dimension first for record variables
            ds.set_unlimited_dim(DIM_UNLIMITED, self.frames.len())
                .map_err(map_def)?;
            let mut v = vec![DIM_UNLIMITED.to_string()];
            v.extend(dim_names.iter().cloned());
            v
        } else {
            dim_names.clone()
        };

        let var_dim_refs: Vec<&str> = var_dims.iter().map(|s| s.as_str()).collect();
        ds.add_var(VAR_NAME, &var_dim_refs, nc_dt)
            .map_err(map_def)?;

        // Store NDArray attributes as variable attributes on array_data
        // Merge attributes from all frames (first frame wins on duplicates)
        let mut seen_attrs = std::collections::HashSet::new();
        for frame in &self.frames {
            for (name, value) in &frame.attrs {
                if seen_attrs.insert(name.clone()) {
                    let _ = ds.add_var_attr_string(VAR_NAME, name, value);
                }
            }
        }

        // Global attributes
        ds.add_global_attr_i32("uniqueId", vec![0])
            .map_err(map_def)?;
        ds.add_global_attr_i32("dataType", vec![first.data_type as i32])
            .map_err(map_def)?;
        ds.add_global_attr_i32("numArrays", vec![self.frames.len() as i32])
            .map_err(map_def)?;

        // Write
        let mut writer = FileWriter::open(&path).map_err(map_write)?;
        writer
            .set_def(&ds, Version::Classic, 0)
            .map_err(map_write)?;

        if multi {
            for (i, frame) in self.frames.iter().enumerate() {
                write_record_data(&mut writer, i, &frame.data)?;
            }
        } else {
            write_var_data(&mut writer, &self.frames[0].data)?;
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
                if d.is_unlimited() {
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
    fn test_attributes_stored() {
        let path = temp_path("nc_attrs");
        let mut writer = NetcdfWriter::new();

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute {
            name: "exposure".into(),
            description: "".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Float64(0.5),
        });
        arr.attributes.add(NDAttribute {
            name: "gain".into(),
            description: "".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Int32(42),
        });

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        // Verify attributes via FileReader
        let reader = FileReader::open(&path).unwrap();
        let ds = reader.data_set();

        let exposure = ds.get_var_attr_as_string(VAR_NAME, "exposure");
        assert_eq!(exposure, Some("0.5".to_string()));

        let gain = ds.get_var_attr_as_string(VAR_NAME, "gain");
        assert_eq!(gain, Some("42".to_string()));

        drop(reader);
        std::fs::remove_file(&path).ok();
    }
}
