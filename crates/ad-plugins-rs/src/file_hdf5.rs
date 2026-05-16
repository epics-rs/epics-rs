use std::path::{Path, PathBuf};

use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{NDFileMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::FilePluginController;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamChangeResult, ParamUpdate, PluginParamSnapshot, ProcessResult,
};

use rust_hdf5::H5File;
use rust_hdf5::format::messages::filter::{
    FILTER_BLOSC, FILTER_JPEG, FILTER_NBIT, FILTER_SZIP, Filter, FilterPipeline,
};
use rust_hdf5::swmr::SwmrFileWriter;

/// C ADCore compression type enum values (matching NDFileHDF5.h).
const COMPRESS_NONE: i32 = 0;
const COMPRESS_NBIT: i32 = 1;
const COMPRESS_SZIP: i32 = 2;
const COMPRESS_ZLIB: i32 = 3;
const COMPRESS_BLOSC: i32 = 4;
#[allow(dead_code)]
const COMPRESS_BSHUF: i32 = 5;
const COMPRESS_LZ4: i32 = 6;
const COMPRESS_JPEG: i32 = 7;

/// C ADCore BLOSC compressor sub-types.
const BLOSC_LZ: i32 = 0;
const BLOSC_LZ4: i32 = 1;
const BLOSC_LZ4HC: i32 = 2;
const BLOSC_SNAPPY: i32 = 3;
const BLOSC_ZLIB: i32 = 4;
const BLOSC_ZSTD: i32 = 5;

/// Internal handle: either a standard H5File or a SWMR streaming writer.
enum Hdf5Handle {
    Standard(H5File),
    Swmr {
        writer: SwmrFileWriter,
        ds_index: usize,
    },
}

/// HDF5 file writer using the hdf5 crate.
pub struct Hdf5Writer {
    current_path: Option<PathBuf>,
    handle: Option<Hdf5Handle>,
    frame_count: usize,
    dataset_name: String,
    // compression
    compression_type: i32,
    z_compress_level: u32,
    szip_num_pixels: u32,
    nbit_precision: u32,
    nbit_offset: u32,
    jpeg_quality: u32,
    blosc_shuffle_type: i32,
    blosc_compressor: i32,
    blosc_compress_level: u32,
    // SWMR
    swmr_mode: bool,
    flush_nth_frame: usize,
    pub swmr_cb_counter: u32,
    // options
    pub store_attributes: bool,
    pub store_performance: bool,
    pub total_runtime: f64,
    pub total_bytes: u64,
}

impl Hdf5Writer {
    pub fn new() -> Self {
        Self {
            current_path: None,
            handle: None,
            frame_count: 0,
            dataset_name: "data".to_string(),
            compression_type: 0,
            z_compress_level: 6,
            szip_num_pixels: 16,
            nbit_precision: 0,
            nbit_offset: 0,
            jpeg_quality: 90,
            blosc_shuffle_type: 0,
            blosc_compressor: 0,
            blosc_compress_level: 5,
            swmr_mode: false,
            flush_nth_frame: 0,
            swmr_cb_counter: 0,
            store_attributes: true,
            store_performance: false,
            total_runtime: 0.0,
            total_bytes: 0,
        }
    }

    pub fn set_dataset_name(&mut self, name: &str) {
        self.dataset_name = name.to_string();
    }

    pub fn set_compression_type(&mut self, v: i32) {
        self.compression_type = v;
    }

    pub fn set_z_compress_level(&mut self, v: u32) {
        self.z_compress_level = v;
    }

    pub fn set_szip_num_pixels(&mut self, v: u32) {
        self.szip_num_pixels = v;
    }

    pub fn set_blosc_shuffle_type(&mut self, v: i32) {
        self.blosc_shuffle_type = v;
    }

    pub fn set_blosc_compressor(&mut self, v: i32) {
        self.blosc_compressor = v;
    }

    pub fn set_blosc_compress_level(&mut self, v: u32) {
        self.blosc_compress_level = v;
    }

    pub fn set_nbit_precision(&mut self, v: u32) {
        self.nbit_precision = v;
    }

    pub fn set_nbit_offset(&mut self, v: u32) {
        self.nbit_offset = v;
    }

    pub fn set_jpeg_quality(&mut self, v: u32) {
        self.jpeg_quality = v;
    }

    pub fn set_store_attributes(&mut self, v: bool) {
        self.store_attributes = v;
    }

    pub fn set_store_performance(&mut self, v: bool) {
        self.store_performance = v;
    }

    pub fn set_swmr_mode(&mut self, v: bool) {
        self.swmr_mode = v;
    }

    pub fn set_flush_nth_frame(&mut self, v: usize) {
        self.flush_nth_frame = v;
    }

    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Trigger a SWMR flush. No-op if not in SWMR mode.
    pub fn flush_swmr(&mut self) {
        if let Some(Hdf5Handle::Swmr { ref mut writer, .. }) = self.handle {
            if writer.flush().is_ok() {
                self.swmr_cb_counter += 1;
            }
        }
    }

    /// Returns true if SWMR is currently active.
    pub fn is_swmr_active(&self) -> bool {
        matches!(self.handle, Some(Hdf5Handle::Swmr { .. }))
    }

    /// Build a FilterPipeline from the current compression settings.
    fn build_pipeline(&self, element_size: usize) -> Option<FilterPipeline> {
        match self.compression_type {
            COMPRESS_NONE => None,
            COMPRESS_ZLIB => Some(FilterPipeline::deflate(self.z_compress_level)),
            COMPRESS_SZIP => Some(FilterPipeline {
                filters: vec![Filter {
                    id: FILTER_SZIP,
                    flags: 0,
                    cd_values: vec![4, self.szip_num_pixels],
                }],
            }),
            COMPRESS_LZ4 => Some(FilterPipeline::lz4()),
            COMPRESS_BLOSC => {
                let compressor_code = match self.blosc_compressor {
                    BLOSC_LZ => 0,
                    BLOSC_LZ4 => 1,
                    BLOSC_LZ4HC => 2,
                    BLOSC_SNAPPY => 3,
                    BLOSC_ZLIB => 4,
                    BLOSC_ZSTD => 5,
                    _ => 0,
                };
                Some(FilterPipeline {
                    filters: vec![Filter {
                        id: FILTER_BLOSC,
                        flags: 0,
                        cd_values: vec![
                            2,
                            2,
                            element_size as u32,
                            0,
                            self.blosc_shuffle_type as u32,
                            compressor_code,
                            self.blosc_compress_level,
                        ],
                    }],
                })
            }
            COMPRESS_NBIT => {
                if self.nbit_precision > 0 {
                    Some(FilterPipeline {
                        filters: vec![Filter {
                            id: FILTER_NBIT,
                            flags: 0,
                            cd_values: vec![self.nbit_precision, self.nbit_offset],
                        }],
                    })
                } else {
                    None
                }
            }
            COMPRESS_JPEG => Some(FilterPipeline {
                filters: vec![Filter {
                    id: FILTER_JPEG,
                    flags: 0,
                    cd_values: vec![self.jpeg_quality],
                }],
            }),
            _ => None,
        }
    }

    /// Open file in SWMR streaming mode.
    fn open_swmr(&mut self, path: &Path, array: &NDArray) -> ADResult<()> {
        let mut swmr = SwmrFileWriter::create(path)
            .map_err(|e| ADError::UnsupportedConversion(format!("SWMR create error: {}", e)))?;

        let frame_dims: Vec<u64> = array.dims.iter().rev().map(|d| d.size as u64).collect();

        macro_rules! create_ds {
            ($t:ty) => {
                swmr.create_streaming_dataset::<$t>(&self.dataset_name, &frame_dims)
                    .map_err(|e| {
                        ADError::UnsupportedConversion(format!("SWMR create dataset error: {}", e))
                    })
            };
        }

        let ds_index = match array.data.data_type() {
            NDDataType::Int8 => create_ds!(i8)?,
            NDDataType::UInt8 => create_ds!(u8)?,
            NDDataType::Int16 => create_ds!(i16)?,
            NDDataType::UInt16 => create_ds!(u16)?,
            NDDataType::Int32 => create_ds!(i32)?,
            NDDataType::UInt32 => create_ds!(u32)?,
            NDDataType::Int64 => create_ds!(i64)?,
            NDDataType::UInt64 => create_ds!(u64)?,
            NDDataType::Float32 => create_ds!(f32)?,
            NDDataType::Float64 => create_ds!(f64)?,
        };

        swmr.start_swmr()
            .map_err(|e| ADError::UnsupportedConversion(format!("SWMR start error: {}", e)))?;

        self.handle = Some(Hdf5Handle::Swmr {
            writer: swmr,
            ds_index,
        });
        Ok(())
    }

    /// Write a frame in standard (non-SWMR) mode.
    fn write_standard(&mut self, array: &NDArray) -> ADResult<()> {
        let h5file = match self.handle {
            Some(Hdf5Handle::Standard(ref f)) => f,
            _ => return Err(ADError::UnsupportedConversion("no HDF5 file open".into())),
        };

        let dataset_name = if self.frame_count == 0 {
            self.dataset_name.clone()
        } else {
            format!("{}_{}", self.dataset_name, self.frame_count)
        };

        let shape = array.dims.iter().rev().map(|d| d.size).collect::<Vec<_>>();
        let element_size = array.data.data_type().element_size();
        let pipeline = self.build_pipeline(element_size);

        macro_rules! write_typed {
            ($t:ty, $v:expr) => {{
                let ds = if let Some(ref pl) = pipeline {
                    h5file
                        .new_dataset::<$t>()
                        .shape(&shape[..])
                        .chunk(&shape[..])
                        .filter_pipeline(pl.clone())
                        .create(dataset_name.as_str())
                        .map_err(|e| {
                            ADError::UnsupportedConversion(format!("HDF5 dataset error: {}", e))
                        })?
                } else {
                    h5file
                        .new_dataset::<$t>()
                        .shape(&shape[..])
                        .create(dataset_name.as_str())
                        .map_err(|e| {
                            ADError::UnsupportedConversion(format!("HDF5 dataset error: {}", e))
                        })?
                };
                if pipeline.is_some() {
                    ds.write_chunk(0, array.data.as_u8_slice()).map_err(|e| {
                        ADError::UnsupportedConversion(format!("HDF5 write error: {}", e))
                    })?;
                } else {
                    ds.write_raw($v).map_err(|e| {
                        ADError::UnsupportedConversion(format!("HDF5 write error: {}", e))
                    })?;
                }
                if self.store_attributes {
                    for attr in array.attributes.iter() {
                        let val_str = attr.value.as_string();
                        let _ = ds
                            .new_attr::<rust_hdf5::types::VarLenUnicode>()
                            .shape(())
                            .create(attr.name.as_str())
                            .and_then(|a| {
                                let s: rust_hdf5::types::VarLenUnicode =
                                    val_str.parse().unwrap_or_default();
                                a.write_scalar(&s)
                            });
                    }
                }
            }};
        }

        match &array.data {
            NDDataBuffer::I8(v) => write_typed!(i8, v),
            NDDataBuffer::U8(v) => write_typed!(u8, v),
            NDDataBuffer::I16(v) => write_typed!(i16, v),
            NDDataBuffer::U16(v) => write_typed!(u16, v),
            NDDataBuffer::I32(v) => write_typed!(i32, v),
            NDDataBuffer::U32(v) => write_typed!(u32, v),
            NDDataBuffer::I64(v) => write_typed!(i64, v),
            NDDataBuffer::U64(v) => write_typed!(u64, v),
            NDDataBuffer::F32(v) => write_typed!(f32, v),
            NDDataBuffer::F64(v) => write_typed!(f64, v),
        }
        Ok(())
    }

    /// Write a frame in SWMR mode.
    fn write_swmr(&mut self, array: &NDArray) -> ADResult<()> {
        let (writer, ds_index) = match self.handle {
            Some(Hdf5Handle::Swmr {
                ref mut writer,
                ds_index,
            }) => (writer, ds_index),
            _ => return Err(ADError::UnsupportedConversion("no SWMR writer open".into())),
        };

        writer
            .append_frame(ds_index, array.data.as_u8_slice())
            .map_err(|e| ADError::UnsupportedConversion(format!("SWMR append error: {}", e)))?;

        // Periodic flush
        let count = self.frame_count + 1; // will be incremented after return
        if self.flush_nth_frame > 0 && count % self.flush_nth_frame == 0 {
            writer
                .flush()
                .map_err(|e| ADError::UnsupportedConversion(format!("SWMR flush error: {}", e)))?;
        }

        Ok(())
    }
}

impl Default for Hdf5Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl NDFileWriter for Hdf5Writer {
    fn open_file(&mut self, path: &Path, mode: NDFileMode, array: &NDArray) -> ADResult<()> {
        self.current_path = Some(path.to_path_buf());
        self.frame_count = 0;
        self.total_runtime = 0.0;
        self.total_bytes = 0;
        self.swmr_cb_counter = 0;

        if self.swmr_mode && mode == NDFileMode::Stream {
            self.open_swmr(path, array)
        } else {
            let h5file = H5File::create(path)
                .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 create error: {}", e)))?;
            self.handle = Some(Hdf5Handle::Standard(h5file));
            Ok(())
        }
    }

    fn write_file(&mut self, array: &NDArray) -> ADResult<()> {
        let start = if self.store_performance {
            Some(std::time::Instant::now())
        } else {
            None
        };

        let is_swmr = matches!(self.handle, Some(Hdf5Handle::Swmr { .. }));
        if is_swmr {
            self.write_swmr(array)?;
        } else {
            self.write_standard(array)?;
        }
        self.frame_count += 1;

        if let Some(start) = start {
            self.total_runtime += start.elapsed().as_secs_f64();
            self.total_bytes += array.data.as_u8_slice().len() as u64;
        }
        Ok(())
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        let path = self
            .current_path
            .as_ref()
            .ok_or_else(|| ADError::UnsupportedConversion("no file open".into()))?;

        let h5file = H5File::open(path)
            .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 open error: {}", e)))?;

        let ds = h5file
            .dataset(&self.dataset_name)
            .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 dataset error: {}", e)))?;

        let shape = ds.shape();
        let dims: Vec<NDDimension> = shape.iter().rev().map(|&s| NDDimension::new(s)).collect();

        if let Ok(data) = ds.read_raw::<u8>() {
            let mut arr = NDArray::new(dims, NDDataType::UInt8);
            arr.data = NDDataBuffer::U8(data);
            return Ok(arr);
        }
        if let Ok(data) = ds.read_raw::<u16>() {
            let mut arr = NDArray::new(dims, NDDataType::UInt16);
            arr.data = NDDataBuffer::U16(data);
            return Ok(arr);
        }
        if let Ok(data) = ds.read_raw::<f64>() {
            let mut arr = NDArray::new(dims, NDDataType::Float64);
            arr.data = NDDataBuffer::F64(data);
            return Ok(arr);
        }

        Err(ADError::UnsupportedConversion(
            "unsupported HDF5 data type".into(),
        ))
    }

    fn close_file(&mut self) -> ADResult<()> {
        if let Some(Hdf5Handle::Swmr { writer, .. }) = self.handle.take() {
            writer
                .close()
                .map_err(|e| ADError::UnsupportedConversion(format!("SWMR close error: {}", e)))?;
        }
        self.current_path = None;
        Ok(())
    }

    fn supports_multiple_arrays(&self) -> bool {
        true
    }
}

// ============================================================
// Processor
// ============================================================

/// Param indices for HDF5-specific params.
#[derive(Default)]
struct Hdf5ParamIndices {
    compression_type: Option<usize>,
    z_compress_level: Option<usize>,
    szip_num_pixels: Option<usize>,
    nbit_precision: Option<usize>,
    nbit_offset: Option<usize>,
    jpeg_quality: Option<usize>,
    blosc_shuffle_type: Option<usize>,
    blosc_compressor: Option<usize>,
    blosc_compress_level: Option<usize>,
    store_attributes: Option<usize>,
    store_performance: Option<usize>,
    total_runtime: Option<usize>,
    total_io_speed: Option<usize>,
    swmr_mode: Option<usize>,
    swmr_flush_now: Option<usize>,
    swmr_running: Option<usize>,
    swmr_cb_counter: Option<usize>,
    swmr_supported: Option<usize>,
    flush_nth_frame: Option<usize>,
}

/// HDF5 file processor wrapping FilePluginController<Hdf5Writer>.
pub struct Hdf5FileProcessor {
    ctrl: FilePluginController<Hdf5Writer>,
    hdf5_params: Hdf5ParamIndices,
}

impl Hdf5FileProcessor {
    pub fn new() -> Self {
        Self {
            ctrl: FilePluginController::new(Hdf5Writer::new()),
            hdf5_params: Hdf5ParamIndices::default(),
        }
    }

    pub fn set_dataset_name(&mut self, name: &str) {
        self.ctrl.writer.set_dataset_name(name);
    }
}

/// Register all HDF5-specific params.
fn register_hdf5_params(
    base: &mut asyn_rs::port::PortDriverBase,
) -> asyn_rs::error::AsynResult<()> {
    use asyn_rs::param::ParamType;
    base.create_param("HDF5_SWMRFlushNow", ParamType::Int32)?;
    base.create_param("HDF5_chunkSizeAuto", ParamType::Int32)?;
    base.create_param("HDF5_nRowChunks", ParamType::Int32)?;
    base.create_param("HDF5_nColChunks", ParamType::Int32)?;
    base.create_param("HDF5_chunkSize2", ParamType::Int32)?;
    base.create_param("HDF5_chunkSize3", ParamType::Int32)?;
    base.create_param("HDF5_chunkSize4", ParamType::Int32)?;
    base.create_param("HDF5_chunkSize5", ParamType::Int32)?;
    base.create_param("HDF5_chunkSize6", ParamType::Int32)?;
    base.create_param("HDF5_chunkSize7", ParamType::Int32)?;
    base.create_param("HDF5_chunkSize8", ParamType::Int32)?;
    base.create_param("HDF5_chunkSize9", ParamType::Int32)?;
    base.create_param("HDF5_nFramesChunks", ParamType::Int32)?;
    base.create_param("HDF5_NDAttributeChunk", ParamType::Int32)?;
    base.create_param("HDF5_chunkBoundaryAlign", ParamType::Int32)?;
    base.create_param("HDF5_chunkBoundaryThreshold", ParamType::Int32)?;
    base.create_param("HDF5_nExtraDims", ParamType::Int32)?;
    base.create_param("HDF5_extraDimSizeN", ParamType::Int32)?;
    base.create_param("HDF5_extraDimNameN", ParamType::Octet)?;
    base.create_param("HDF5_extraDimSizeX", ParamType::Int32)?;
    base.create_param("HDF5_extraDimNameX", ParamType::Octet)?;
    base.create_param("HDF5_extraDimSizeY", ParamType::Int32)?;
    base.create_param("HDF5_extraDimNameY", ParamType::Octet)?;
    base.create_param("HDF5_extraDimSize3", ParamType::Int32)?;
    base.create_param("HDF5_extraDimName3", ParamType::Octet)?;
    base.create_param("HDF5_extraDimSize4", ParamType::Int32)?;
    base.create_param("HDF5_extraDimName4", ParamType::Octet)?;
    base.create_param("HDF5_extraDimSize5", ParamType::Int32)?;
    base.create_param("HDF5_extraDimName5", ParamType::Octet)?;
    base.create_param("HDF5_extraDimSize6", ParamType::Int32)?;
    base.create_param("HDF5_extraDimName6", ParamType::Octet)?;
    base.create_param("HDF5_extraDimSize7", ParamType::Int32)?;
    base.create_param("HDF5_extraDimName7", ParamType::Octet)?;
    base.create_param("HDF5_extraDimSize8", ParamType::Int32)?;
    base.create_param("HDF5_extraDimName8", ParamType::Octet)?;
    base.create_param("HDF5_extraDimSize9", ParamType::Int32)?;
    base.create_param("HDF5_extraDimName9", ParamType::Octet)?;
    base.create_param("HDF5_storeAttributes", ParamType::Int32)?;
    base.create_param("HDF5_storePerformance", ParamType::Int32)?;
    base.create_param("HDF5_totalRuntime", ParamType::Float64)?;
    base.create_param("HDF5_totalIoSpeed", ParamType::Float64)?;
    base.create_param("HDF5_flushNthFrame", ParamType::Int32)?;
    base.create_param("HDF5_compressionType", ParamType::Int32)?;
    base.create_param("HDF5_nbitsPrecision", ParamType::Int32)?;
    base.create_param("HDF5_nbitsOffset", ParamType::Int32)?;
    base.create_param("HDF5_szipNumPixels", ParamType::Int32)?;
    base.create_param("HDF5_zCompressLevel", ParamType::Int32)?;
    base.create_param("HDF5_bloscShuffleType", ParamType::Int32)?;
    base.create_param("HDF5_bloscCompressor", ParamType::Int32)?;
    base.create_param("HDF5_bloscCompressLevel", ParamType::Int32)?;
    base.create_param("HDF5_jpegQuality", ParamType::Int32)?;
    base.create_param("HDF5_dimAttDatasets", ParamType::Int32)?;
    base.create_param("HDF5_layoutErrorMsg", ParamType::Octet)?;
    base.create_param("HDF5_layoutValid", ParamType::Int32)?;
    base.create_param("HDF5_layoutFilename", ParamType::Octet)?;
    base.create_param("HDF5_SWMRSupported", ParamType::Int32)?;
    base.create_param("HDF5_SWMRMode", ParamType::Int32)?;
    base.create_param("HDF5_SWMRRunning", ParamType::Int32)?;
    base.create_param("HDF5_SWMRCbCounter", ParamType::Int32)?;
    base.create_param("HDF5_posRunning", ParamType::Int32)?;
    base.create_param("HDF5_posNameDimN", ParamType::Octet)?;
    base.create_param("HDF5_posNameDimX", ParamType::Octet)?;
    base.create_param("HDF5_posNameDimY", ParamType::Octet)?;
    base.create_param("HDF5_posNameDim3", ParamType::Octet)?;
    base.create_param("HDF5_posNameDim4", ParamType::Octet)?;
    base.create_param("HDF5_posNameDim5", ParamType::Octet)?;
    base.create_param("HDF5_posNameDim6", ParamType::Octet)?;
    base.create_param("HDF5_posNameDim7", ParamType::Octet)?;
    base.create_param("HDF5_posNameDim8", ParamType::Octet)?;
    base.create_param("HDF5_posNameDim9", ParamType::Octet)?;
    base.create_param("HDF5_posIndexDimN", ParamType::Octet)?;
    base.create_param("HDF5_posIndexDimX", ParamType::Octet)?;
    base.create_param("HDF5_posIndexDimY", ParamType::Octet)?;
    base.create_param("HDF5_posIndexDim3", ParamType::Octet)?;
    base.create_param("HDF5_posIndexDim4", ParamType::Octet)?;
    base.create_param("HDF5_posIndexDim5", ParamType::Octet)?;
    base.create_param("HDF5_posIndexDim6", ParamType::Octet)?;
    base.create_param("HDF5_posIndexDim7", ParamType::Octet)?;
    base.create_param("HDF5_posIndexDim8", ParamType::Octet)?;
    base.create_param("HDF5_posIndexDim9", ParamType::Octet)?;
    base.create_param("HDF5_fillValue", ParamType::Float64)?;
    base.create_param("HDF5_extraDimChunkX", ParamType::Int32)?;
    base.create_param("HDF5_extraDimChunkY", ParamType::Int32)?;
    base.create_param("HDF5_extraDimChunk3", ParamType::Int32)?;
    base.create_param("HDF5_extraDimChunk4", ParamType::Int32)?;
    base.create_param("HDF5_extraDimChunk5", ParamType::Int32)?;
    base.create_param("HDF5_extraDimChunk6", ParamType::Int32)?;
    base.create_param("HDF5_extraDimChunk7", ParamType::Int32)?;
    base.create_param("HDF5_extraDimChunk8", ParamType::Int32)?;
    base.create_param("HDF5_extraDimChunk9", ParamType::Int32)?;
    Ok(())
}

impl Default for Hdf5FileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for Hdf5FileProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let was_swmr = self.ctrl.writer.is_swmr_active();
        let mut result = self.ctrl.process_array(array);
        let is_swmr = self.ctrl.writer.is_swmr_active();

        // SWMR running status changed
        if was_swmr != is_swmr {
            if let Some(idx) = self.hdf5_params.swmr_running {
                result
                    .param_updates
                    .push(ParamUpdate::int32(idx, if is_swmr { 1 } else { 0 }));
            }
        }

        // SWMR callback counter
        if is_swmr {
            if let Some(idx) = self.hdf5_params.swmr_cb_counter {
                result.param_updates.push(ParamUpdate::int32(
                    idx,
                    self.ctrl.writer.swmr_cb_counter as i32,
                ));
            }
        }

        // Performance stats
        if self.ctrl.writer.store_performance {
            if let Some(idx) = self.hdf5_params.total_runtime {
                result
                    .param_updates
                    .push(ParamUpdate::float64(idx, self.ctrl.writer.total_runtime));
            }
            if let Some(idx) = self.hdf5_params.total_io_speed {
                let speed = if self.ctrl.writer.total_runtime > 0.0 {
                    self.ctrl.writer.total_bytes as f64
                        / self.ctrl.writer.total_runtime
                        / 1_000_000.0
                } else {
                    0.0
                };
                result.param_updates.push(ParamUpdate::float64(idx, speed));
            }
        }

        result
    }

    fn plugin_type(&self) -> &str {
        "NDFileHDF5"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        self.ctrl.register_params(base)?;
        register_hdf5_params(base)?;
        self.hdf5_params.compression_type = base.find_param("HDF5_compressionType");
        self.hdf5_params.z_compress_level = base.find_param("HDF5_zCompressLevel");
        self.hdf5_params.szip_num_pixels = base.find_param("HDF5_szipNumPixels");
        self.hdf5_params.nbit_precision = base.find_param("HDF5_nbitsPrecision");
        self.hdf5_params.nbit_offset = base.find_param("HDF5_nbitsOffset");
        self.hdf5_params.jpeg_quality = base.find_param("HDF5_jpegQuality");
        self.hdf5_params.blosc_shuffle_type = base.find_param("HDF5_bloscShuffleType");
        self.hdf5_params.blosc_compressor = base.find_param("HDF5_bloscCompressor");
        self.hdf5_params.blosc_compress_level = base.find_param("HDF5_bloscCompressLevel");
        self.hdf5_params.store_attributes = base.find_param("HDF5_storeAttributes");
        self.hdf5_params.store_performance = base.find_param("HDF5_storePerformance");
        self.hdf5_params.total_runtime = base.find_param("HDF5_totalRuntime");
        self.hdf5_params.total_io_speed = base.find_param("HDF5_totalIoSpeed");
        self.hdf5_params.swmr_mode = base.find_param("HDF5_SWMRMode");
        self.hdf5_params.swmr_flush_now = base.find_param("HDF5_SWMRFlushNow");
        self.hdf5_params.swmr_running = base.find_param("HDF5_SWMRRunning");
        self.hdf5_params.swmr_cb_counter = base.find_param("HDF5_SWMRCbCounter");
        self.hdf5_params.swmr_supported = base.find_param("HDF5_SWMRSupported");
        self.hdf5_params.flush_nth_frame = base.find_param("HDF5_flushNthFrame");

        // Report SWMR as always supported
        if let Some(idx) = self.hdf5_params.swmr_supported {
            base.set_int32_param(idx, 0, 1)?;
        }
        Ok(())
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        params: &PluginParamSnapshot,
    ) -> ParamChangeResult {
        // -- compression params --
        if Some(reason) == self.hdf5_params.compression_type {
            self.ctrl.writer.set_compression_type(params.value.as_i32());
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.z_compress_level {
            self.ctrl
                .writer
                .set_z_compress_level(params.value.as_i32() as u32);
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.szip_num_pixels {
            self.ctrl
                .writer
                .set_szip_num_pixels(params.value.as_i32() as u32);
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.blosc_shuffle_type {
            self.ctrl
                .writer
                .set_blosc_shuffle_type(params.value.as_i32());
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.blosc_compressor {
            self.ctrl.writer.set_blosc_compressor(params.value.as_i32());
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.blosc_compress_level {
            self.ctrl
                .writer
                .set_blosc_compress_level(params.value.as_i32() as u32);
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.nbit_precision {
            self.ctrl
                .writer
                .set_nbit_precision(params.value.as_i32() as u32);
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.nbit_offset {
            self.ctrl
                .writer
                .set_nbit_offset(params.value.as_i32() as u32);
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.jpeg_quality {
            self.ctrl
                .writer
                .set_jpeg_quality(params.value.as_i32() as u32);
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.store_attributes {
            self.ctrl
                .writer
                .set_store_attributes(params.value.as_i32() != 0);
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.store_performance {
            self.ctrl
                .writer
                .set_store_performance(params.value.as_i32() != 0);
            return ParamChangeResult::updates(vec![]);
        }
        // -- SWMR params --
        if Some(reason) == self.hdf5_params.swmr_mode {
            self.ctrl.writer.set_swmr_mode(params.value.as_i32() != 0);
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.swmr_flush_now {
            if params.value.as_i32() != 0 {
                self.ctrl.writer.flush_swmr();
                let mut updates = vec![];
                if let Some(idx) = self.hdf5_params.swmr_cb_counter {
                    updates.push(ParamUpdate::int32(
                        idx,
                        self.ctrl.writer.swmr_cb_counter as i32,
                    ));
                }
                return ParamChangeResult::updates(updates);
            }
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.flush_nth_frame {
            self.ctrl
                .writer
                .set_flush_nth_frame(params.value.as_i32().max(0) as usize);
            return ParamChangeResult::updates(vec![]);
        }
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
        std::env::temp_dir().join(format!("adcore_test_{}_{}.h5", prefix, n))
    }

    #[test]
    fn test_write_single_frame() {
        let path = temp_path("hdf5_single");
        let mut writer = Hdf5Writer::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let mut reader = Hdf5Writer::new();
        reader.current_path = Some(path.clone());
        let read_arr = reader.read_file().unwrap();
        assert_eq!(read_arr.dims.len(), 2);
        assert_eq!(read_arr.dims[0].size, 4);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_multiple_frames() {
        let path = temp_path("hdf5_multi");
        let mut writer = Hdf5Writer::new();

        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );

        writer.open_file(&path, NDFileMode::Stream, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        assert!(writer.supports_multiple_arrays());
        assert_eq!(writer.frame_count(), 3);

        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[0..8], b"\x89HDF\r\n\x1a\n");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_attributes_stored() {
        let path = temp_path("hdf5_attrs");
        let mut writer = Hdf5Writer::new();

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.attributes.add(NDAttribute::new_static(
                "exposure",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Float64(0.5),
            ));

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("data").unwrap();
        let attr = ds.attr("exposure").unwrap();
        let val = attr.read_string().unwrap();
        assert_eq!(val, "0.5");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_roundtrip_u16() {
        let path = temp_path("hdf5_u16");
        let mut writer = Hdf5Writer::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for i in 0..16 {
                v[i] = (i * 100) as u16;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("data").unwrap();
        let data: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(data[0], 0);
        assert_eq!(data[1], 100);
        assert_eq!(data[15], 1500);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_roundtrip_f64() {
        let path = temp_path("hdf5_f64");
        let mut writer = Hdf5Writer::new();

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::Float64);
        if let NDDataBuffer::F64(ref mut v) = arr.data {
            v[0] = 1.5;
            v[1] = 2.5;
            v[2] = 3.5;
            v[3] = 4.5;
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("data").unwrap();
        let data: Vec<f64> = ds.read_raw().unwrap();
        assert!((data[0] - 1.5).abs() < 1e-10);
        assert!((data[3] - 4.5).abs() < 1e-10);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_deflate_compressed_write() {
        let path = temp_path("hdf5_deflate");
        let mut writer = Hdf5Writer::new();
        writer.set_compression_type(COMPRESS_ZLIB);
        writer.set_z_compress_level(6);

        let mut arr = NDArray::new(
            vec![NDDimension::new(64), NDDimension::new(64)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = (i % 256) as u16;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let file_size = std::fs::metadata(&path).unwrap().len();
        assert!(
            file_size < 8192,
            "compressed file should be smaller than raw data"
        );

        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("data").unwrap();
        let data: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 64 * 64);
        assert_eq!(data[0], 0);
        assert_eq!(data[255], 255);
        assert_eq!(data[256], 0);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_lz4_compressed_write() {
        let path = temp_path("hdf5_lz4");
        let mut writer = Hdf5Writer::new();
        writer.set_compression_type(COMPRESS_LZ4);

        let mut arr = NDArray::new(
            vec![NDDimension::new(32), NDDimension::new(32)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = (i % 4) as u8;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("data").unwrap();
        let data: Vec<u8> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 32 * 32);
        assert_eq!(data[0], 0);
        assert_eq!(data[3], 3);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_swmr_streaming() {
        let path = temp_path("hdf5_swmr");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_flush_nth_frame(2);

        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::Float32,
        );

        writer.open_file(&path, NDFileMode::Stream, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.write_file(&arr).unwrap(); // should trigger flush
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        assert_eq!(writer.frame_count(), 3);

        // Read back via SwmrFileReader
        let mut reader = rust_hdf5::swmr::SwmrFileReader::open(&path).unwrap();
        let shape = reader.dataset_shape("data").unwrap();
        assert_eq!(shape[0], 3); // 3 frames
        assert_eq!(shape[1], 8);
        assert_eq!(shape[2], 8);

        let data: Vec<f32> = reader.read_dataset("data").unwrap();
        assert_eq!(data.len(), 3 * 8 * 8);

        std::fs::remove_file(&path).ok();
    }
}
