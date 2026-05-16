use std::path::{Path, PathBuf};

use ad_core_rs::attributes::{NDAttrDataType, NDAttrValue};
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
    FILTER_BLOSC, FILTER_BSHUF, FILTER_JPEG, FILTER_NBIT, FILTER_SZIP, Filter, FilterPipeline,
};
use rust_hdf5::swmr::SwmrFileWriter;

use crate::hdf5_layout::Hdf5Layout;

/// C ADCore compression type enum values (matching NDFileHDF5.h).
const COMPRESS_NONE: i32 = 0;
const COMPRESS_NBIT: i32 = 1;
const COMPRESS_SZIP: i32 = 2;
const COMPRESS_ZLIB: i32 = 3;
const COMPRESS_BLOSC: i32 = 4;
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

/// Maximum number of extra dimensions (C `MAXEXTRADIMS`).
const MAX_EXTRA_DIMS: usize = 10;

/// Name of the HDF5 attribute that records the NDArray data type ordinal
/// (matches C `NDDataType_t`). `read_file` uses it to recover the exact type.
const DTYPE_ATTR: &str = "NDArrayDataType";

/// User-controlled chunk geometry (C `HDF5_*Chunks` params).
#[derive(Clone)]
struct ChunkConfig {
    /// `HDF5_chunkSizeAuto` — when true, ignore the explicit row/col/frame
    /// values and let the writer pick (full-frame spatial, one frame deep).
    auto: bool,
    n_row_chunks: usize,
    n_col_chunks: usize,
    n_frames_chunks: usize,
    /// `HDF5_NDAttributeChunk` — chunk depth for NDAttribute datasets.
    ndattr_chunk: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            auto: true,
            n_row_chunks: 0,
            n_col_chunks: 0,
            n_frames_chunks: 1,
            ndattr_chunk: 16,
        }
    }
}

/// One extra-dimension entry (C `HDF5_extraDimSizeN` / `HDF5_extraDimNameN`).
#[derive(Clone, Default)]
struct ExtraDim {
    size: usize,
    name: String,
}

/// State for a single open attribute time-series dataset (one per NDAttribute).
/// Mirrors C++ `NDFileHDF5AttributeDataset`: a 1-D extensible dataset holding
/// one numeric (or string) value per frame.
struct AttributeDataset {
    name: String,
    data_type: NDAttrDataType,
    /// Raw little-endian bytes accumulated, one element per frame.
    buffer: Vec<u8>,
    frames: usize,
}

impl AttributeDataset {
    fn new(name: String, data_type: NDAttrDataType) -> Self {
        Self {
            name,
            data_type,
            buffer: Vec::new(),
            frames: 0,
        }
    }

    /// Element byte width for this attribute's numeric type. Strings are
    /// stored as a fixed-width field (matching C++ `MAX_ATTRIBUTE_STRING_SIZE`).
    fn element_size(&self) -> usize {
        match self.data_type {
            NDAttrDataType::Int8 | NDAttrDataType::UInt8 => 1,
            NDAttrDataType::Int16 | NDAttrDataType::UInt16 => 2,
            NDAttrDataType::Int32 | NDAttrDataType::UInt32 | NDAttrDataType::Float32 => 4,
            NDAttrDataType::Int64 | NDAttrDataType::UInt64 | NDAttrDataType::Float64 => 8,
            NDAttrDataType::String => MAX_ATTRIBUTE_STRING_SIZE,
        }
    }

    /// Append one frame's value, encoding it to the dataset's native type.
    fn push(&mut self, value: &NDAttrValue) {
        let es = self.element_size();
        let mut bytes = vec![0u8; es];
        match self.data_type {
            NDAttrDataType::Int8 => bytes[0] = value.as_i64().unwrap_or(0) as i8 as u8,
            NDAttrDataType::UInt8 => bytes[0] = value.as_i64().unwrap_or(0) as u8,
            NDAttrDataType::Int16 => {
                bytes.copy_from_slice(&(value.as_i64().unwrap_or(0) as i16).to_le_bytes())
            }
            NDAttrDataType::UInt16 => {
                bytes.copy_from_slice(&(value.as_i64().unwrap_or(0) as u16).to_le_bytes())
            }
            NDAttrDataType::Int32 => {
                bytes.copy_from_slice(&(value.as_i64().unwrap_or(0) as i32).to_le_bytes())
            }
            NDAttrDataType::UInt32 => {
                bytes.copy_from_slice(&(value.as_i64().unwrap_or(0) as u32).to_le_bytes())
            }
            NDAttrDataType::Int64 => {
                bytes.copy_from_slice(&(value.as_i64().unwrap_or(0)).to_le_bytes())
            }
            NDAttrDataType::UInt64 => {
                bytes.copy_from_slice(&(value.as_i64().unwrap_or(0) as u64).to_le_bytes())
            }
            NDAttrDataType::Float32 => {
                bytes.copy_from_slice(&(value.as_f64().unwrap_or(0.0) as f32).to_le_bytes())
            }
            NDAttrDataType::Float64 => {
                bytes.copy_from_slice(&(value.as_f64().unwrap_or(0.0)).to_le_bytes())
            }
            NDAttrDataType::String => {
                let s = value.as_string();
                let src = s.as_bytes();
                let n = src.len().min(es - 1);
                bytes[..n].copy_from_slice(&src[..n]);
            }
        }
        self.buffer.extend_from_slice(&bytes);
        self.frames += 1;
    }
}

/// Fixed string field width for string-typed attribute datasets
/// (C++ `MAX_ATTRIBUTE_STRING_SIZE`).
const MAX_ATTRIBUTE_STRING_SIZE: usize = 256;

/// Internal handle: either a standard H5File or a SWMR streaming writer.
enum Hdf5Handle {
    Standard {
        file: H5File,
        /// Primary image dataset handle, created lazily on the first frame.
        /// Retained across frames so the leading dimension can be extended
        /// (`H5File::dataset` cannot re-open a dataset in write mode).
        primary: Option<rust_hdf5::H5Dataset>,
    },
    Swmr {
        writer: SwmrFileWriter,
        ds_index: usize,
        /// SWMR compression is requested but the public `rust-hdf5` 0.2.0
        /// SWMR API exposes no filtered streaming dataset constructor.
        compression_dropped: bool,
    },
}

/// HDF5 file writer using the rust-hdf5 crate.
pub struct Hdf5Writer {
    current_path: Option<PathBuf>,
    handle: Option<Hdf5Handle>,
    frame_count: usize,
    dataset_name: String,
    /// Cached data type of the open primary dataset.
    open_data_type: Option<NDDataType>,
    /// Cached spatial (per-frame) dimensions, fastest-varying last.
    open_frame_dims: Option<Vec<usize>>,
    /// `Some(total)` when the open dataset has a fixed extra-dim leading
    /// layout (created at full size, no per-frame extend); `None` when the
    /// leading frame axis is extended per write.
    open_extra_extent: Option<usize>,
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
    // chunking & layout
    chunk: ChunkConfig,
    n_extra_dims: usize,
    extra_dims: [ExtraDim; MAX_EXTRA_DIMS],
    fill_value: f64,
    dim_att_datasets: bool,
    // SWMR
    swmr_mode: bool,
    flush_nth_frame: usize,
    pub swmr_cb_counter: u32,
    // options
    pub store_attributes: bool,
    pub store_performance: bool,
    pub total_runtime: f64,
    pub total_bytes: u64,
    /// Per-frame I/O timing rows for the `timestamp` performance dataset.
    /// Each row is the 5 doubles C++ `writePerformanceDataset` records.
    perf_rows: Vec<[f64; 5]>,
    perf_prev: Option<std::time::Instant>,
    perf_first: Option<std::time::Instant>,
    /// Open NDAttribute time-series datasets, keyed by attribute name.
    attr_datasets: Vec<AttributeDataset>,
    /// Layout XML state.
    layout_filename: Option<PathBuf>,
    layout: Option<Hdf5Layout>,
    pub layout_valid: bool,
    pub layout_error: String,
}

impl Hdf5Writer {
    pub fn new() -> Self {
        Self {
            current_path: None,
            handle: None,
            frame_count: 0,
            dataset_name: "data".to_string(),
            open_data_type: None,
            open_frame_dims: None,
            open_extra_extent: None,
            compression_type: 0,
            z_compress_level: 6,
            szip_num_pixels: 16,
            nbit_precision: 0,
            nbit_offset: 0,
            jpeg_quality: 90,
            blosc_shuffle_type: 0,
            blosc_compressor: 0,
            blosc_compress_level: 5,
            chunk: ChunkConfig::default(),
            n_extra_dims: 0,
            extra_dims: Default::default(),
            fill_value: 0.0,
            dim_att_datasets: false,
            swmr_mode: false,
            flush_nth_frame: 0,
            swmr_cb_counter: 0,
            store_attributes: true,
            store_performance: false,
            total_runtime: 0.0,
            total_bytes: 0,
            perf_rows: Vec::new(),
            perf_prev: None,
            perf_first: None,
            attr_datasets: Vec::new(),
            layout_filename: None,
            layout: None,
            layout_valid: false,
            layout_error: String::new(),
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

    pub fn set_chunk_size_auto(&mut self, v: bool) {
        self.chunk.auto = v;
    }

    pub fn set_n_row_chunks(&mut self, v: usize) {
        self.chunk.n_row_chunks = v;
    }

    pub fn set_n_col_chunks(&mut self, v: usize) {
        self.chunk.n_col_chunks = v;
    }

    pub fn set_n_frames_chunks(&mut self, v: usize) {
        self.chunk.n_frames_chunks = v;
    }

    pub fn set_ndattr_chunk(&mut self, v: usize) {
        self.chunk.ndattr_chunk = v.max(1);
    }

    pub fn set_n_extra_dims(&mut self, v: usize) {
        self.n_extra_dims = v.min(MAX_EXTRA_DIMS);
    }

    pub fn set_extra_dim_size(&mut self, idx: usize, size: usize) {
        if idx < MAX_EXTRA_DIMS {
            self.extra_dims[idx].size = size;
        }
    }

    pub fn set_extra_dim_name(&mut self, idx: usize, name: &str) {
        if idx < MAX_EXTRA_DIMS {
            self.extra_dims[idx].name = name.to_string();
        }
    }

    pub fn set_fill_value(&mut self, v: f64) {
        self.fill_value = v;
    }

    pub fn set_dim_att_datasets(&mut self, v: bool) {
        self.dim_att_datasets = v;
    }

    /// Set the layout XML filename and (re)parse it. Returns whether parsing
    /// succeeded; `layout_error` carries any message (C `HDF5_layoutErrorMsg`).
    pub fn set_layout_filename(&mut self, path: &str) -> bool {
        if path.trim().is_empty() {
            self.layout_filename = None;
            self.layout = None;
            self.layout_valid = false;
            self.layout_error.clear();
            return true;
        }
        let p = PathBuf::from(path);
        match Hdf5Layout::from_file(&p) {
            Ok(layout) => {
                self.layout_filename = Some(p);
                self.layout = Some(layout);
                self.layout_valid = true;
                self.layout_error.clear();
                true
            }
            Err(e) => {
                self.layout_filename = Some(p);
                self.layout = None;
                self.layout_valid = false;
                self.layout_error = e.0;
                false
            }
        }
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

    /// Whether SWMR compression had to be silently dropped (crate limitation).
    pub fn swmr_compression_dropped(&self) -> bool {
        matches!(
            self.handle,
            Some(Hdf5Handle::Swmr {
                compression_dropped: true,
                ..
            })
        )
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
            COMPRESS_BSHUF => Some(FilterPipeline {
                // Bitshuffle (HDF5 filter 32008): cd_values are
                // [major_ver, minor_ver, elem_size, block_size, comp_type].
                // comp_type 2 == LZ4, matching ADCore's default bitshuffle.
                filters: vec![Filter {
                    id: FILTER_BSHUF,
                    flags: 0,
                    cd_values: vec![0, 0, element_size as u32, 0, 2],
                }],
            }),
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
                            self.blosc_compress_level,
                            self.blosc_shuffle_type as u32,
                            compressor_code,
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

    /// Compute the dataset shape and chunk geometry for the primary image
    /// dataset.
    ///
    /// Layout, fastest-varying last: `[frame, Y, X]`. The leading frame axis
    /// is extensible. When `HDF5_nExtraDims = N` is set, the leading axis is
    /// fixed at `product(extraDimSizeN..)` and the dataset is created at full
    /// size up front; the extra-dimension sizes and names are recorded as
    /// HDF5 attributes (`HDF5_nExtraDims`, `HDF5_extraDimSize0..`,
    /// `HDF5_extraDimName0..`) so the N-dimensional layout is recoverable.
    ///
    /// A true N-D leading layout (`[extra_0, .., extra_{N-1}, Y, X]`) cannot
    /// be used: `rust-hdf5` 0.2.0's chunked read path mis-places chunks
    /// whenever more than one axis has a chunk grid larger than 1 (verified
    /// empirically). A single flat leading axis keeps exactly one multi-chunk
    /// axis, which the crate reconstructs correctly.
    ///
    /// The chunk shape is `[1, Y, X]` — one full frame per chunk, giving a
    /// proper per-frame hyperslab offset.
    ///
    /// Returns `(shape, chunk, extra_dim_extent)` where `extra_dim_extent` is
    /// `Some(total_frames)` when extra dims fix the dataset size up front, or
    /// `None` when the leading frame axis is extended per write.
    fn primary_layout(&self, frame_dims: &[usize]) -> (Vec<usize>, Vec<usize>, Option<usize>) {
        let extra_extent = if self.n_extra_dims > 0 {
            Some(
                (0..self.n_extra_dims)
                    .map(|i| self.extra_dims[i].size.max(1))
                    .product::<usize>(),
            )
        } else {
            None
        };

        let mut shape: Vec<usize> = Vec::new();
        // Leading frame axis: full extra-dim product, or 1 (extensible).
        shape.push(extra_extent.unwrap_or(1));
        shape.extend_from_slice(frame_dims);

        let ndims = shape.len();
        let mut chunk = vec![1usize; ndims];
        // Spatial chunk == full per-frame extent (one chunk per frame).
        for (i, &d) in frame_dims.iter().enumerate() {
            chunk[1 + i] = d.max(1);
        }
        (shape, chunk, extra_extent)
    }

    /// Write one frame's bytes into the primary dataset at frame index
    /// `frame_idx`. With one chunk per frame the linear chunk index is the
    /// frame index — a proper per-frame hyperslab offset.
    fn write_frame_chunk(
        ds: &rust_hdf5::H5Dataset,
        frame_idx: usize,
        frame_bytes: &[u8],
    ) -> ADResult<()> {
        ds.write_chunk(frame_idx, frame_bytes).map_err(|e| {
            ADError::UnsupportedConversion(format!("HDF5 write_chunk error: {}", e))
        })
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

        // The public rust-hdf5 0.2.0 SWMR API (`SwmrFileWriter`) has no
        // filtered streaming-dataset constructor, so compression cannot be
        // applied here. Record the fact and warn loudly — never drop it
        // silently.
        let compression_dropped = self.compression_type != COMPRESS_NONE;
        if compression_dropped {
            eprintln!(
                "NDFileHDF5: WARNING — SWMR mode requested with compression \
                 type {} but rust-hdf5 0.2.0 exposes no filtered SWMR \
                 dataset API; the SWMR file will be written UNCOMPRESSED.",
                self.compression_type
            );
        }

        self.handle = Some(Hdf5Handle::Swmr {
            writer: swmr,
            ds_index,
            compression_dropped,
        });
        self.open_data_type = Some(array.data.data_type());
        self.open_frame_dims =
            Some(array.dims.iter().rev().map(|d| d.size).collect::<Vec<_>>());
        Ok(())
    }

    /// Create the primary image dataset on first frame in standard mode.
    /// The dataset is a single extensible `[nframes, .., Y, X]` array; later
    /// frames extend the leading dimension (C++ `NDFileHDF5Dataset`).
    fn create_primary_dataset(&mut self, array: &NDArray) -> ADResult<()> {
        let frame_dims: Vec<usize> = array.dims.iter().rev().map(|d| d.size).collect();
        let (shape, chunk, extra_extent) = self.primary_layout(&frame_dims);
        let element_size = array.data.data_type().element_size();
        let pipeline = self.build_pipeline(element_size);
        // Max shape: with extra dims the dataset is created at full size, so
        // every axis is fixed (`Some`). Without extra dims the leading frame
        // axis is extensible (`None`); spatial axes are fixed.
        let max_shape: Vec<Option<usize>> = shape
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                if i == 0 && extra_extent.is_none() {
                    None
                } else {
                    Some(s)
                }
            })
            .collect();

        let h5file = match self.handle {
            Some(Hdf5Handle::Standard { ref file, .. }) => file,
            _ => return Err(ADError::UnsupportedConversion("no HDF5 file open".into())),
        };

        let dtype_ordinal = array.data.data_type() as i32;
        let fill = self.fill_value;
        let ds_name = self.dataset_name.clone();
        let row_chunks = self.chunk.n_row_chunks as i32;
        let col_chunks = self.chunk.n_col_chunks as i32;
        let frame_chunks = self.chunk.n_frames_chunks as i32;
        let n_extra = self.n_extra_dims as i32;
        let extra_meta: Vec<(usize, i32, String)> = (0..self.n_extra_dims)
            .map(|i| {
                (
                    i,
                    self.extra_dims[i].size.max(1) as i32,
                    self.extra_dims[i].name.clone(),
                )
            })
            .collect();

        macro_rules! create_ds {
            ($t:ty) => {{
                let mut builder = h5file
                    .new_dataset::<$t>()
                    .shape(&shape[..])
                    .chunk(&chunk[..])
                    .max_shape(&max_shape[..]);
                if let Some(ref pl) = pipeline {
                    builder = builder.filter_pipeline(pl.clone());
                }
                let ds = builder.create(ds_name.as_str()).map_err(|e| {
                    ADError::UnsupportedConversion(format!("HDF5 dataset error: {}", e))
                })?;
                // Record the exact NDArray data type for lossless read-back.
                let _ = ds
                    .new_attr::<i32>()
                    .shape(())
                    .create(DTYPE_ATTR)
                    .and_then(|a| a.write_numeric(&dtype_ordinal));
                // Record the configured fill value.
                //
                // CRATE LIMITATION: C++ NDFileHDF5 sets HDF5_fillValue on the
                // dataset creation property list (H5Pset_fill_value). rust-hdf5
                // 0.2.12 hard-codes the HDF5 Fill Value message to zeros for
                // chunked datasets (writer.rs `fill_value: None`) and exposes
                // no DatasetBuilder / create_dataset API to override it, so the
                // value cannot reach the DCPL. The closest correct alternative
                // is to record it as a dataset attribute named HDF5_fillValue;
                // downstream tooling can read it, but unwritten chunks still
                // read back as zero, not `fill`. A true fix needs a rust-hdf5
                // release with a fill-value API.
                let _ = ds
                    .new_attr::<f64>()
                    .shape(())
                    .create("HDF5_fillValue")
                    .and_then(|a| a.write_numeric(&fill));
                // Record the requested chunk geometry. The on-disk chunk is
                // one frame per chunk (crate limitation); these attributes
                // preserve the user's intent for downstream tooling.
                for (name, val) in [
                    ("HDF5_nRowChunks", row_chunks),
                    ("HDF5_nColChunks", col_chunks),
                    ("HDF5_nFramesChunks", frame_chunks),
                    ("HDF5_nExtraDims", n_extra),
                ] {
                    let _ = ds
                        .new_attr::<i32>()
                        .shape(())
                        .create(name)
                        .and_then(|a| a.write_numeric(&val));
                }
                // Record extra-dimension sizes and names so the flat leading
                // axis can be reshaped into the intended N-D layout.
                for (i, size, name) in &extra_meta {
                    let _ = ds
                        .new_attr::<i32>()
                        .shape(())
                        .create(&format!("HDF5_extraDimSize{}", i))
                        .and_then(|a| a.write_numeric(size));
                    if !name.is_empty() {
                        let s = rust_hdf5::types::VarLenUnicode(name.clone());
                        let _ = ds
                            .new_attr::<rust_hdf5::types::VarLenUnicode>()
                            .shape(())
                            .create(&format!("HDF5_extraDimName{}", i))
                            .and_then(|a| a.write_scalar(&s));
                    }
                }
                ds
            }};
        }

        let ds = match array.data {
            NDDataBuffer::I8(_) => create_ds!(i8),
            NDDataBuffer::U8(_) => create_ds!(u8),
            NDDataBuffer::I16(_) => create_ds!(i16),
            NDDataBuffer::U16(_) => create_ds!(u16),
            NDDataBuffer::I32(_) => create_ds!(i32),
            NDDataBuffer::U32(_) => create_ds!(u32),
            NDDataBuffer::I64(_) => create_ds!(i64),
            NDDataBuffer::U64(_) => create_ds!(u64),
            NDDataBuffer::F32(_) => create_ds!(f32),
            NDDataBuffer::F64(_) => create_ds!(f64),
        };

        if let Some(Hdf5Handle::Standard { primary, .. }) = self.handle.as_mut() {
            *primary = Some(ds);
        }
        self.open_data_type = Some(array.data.data_type());
        self.open_frame_dims = Some(frame_dims);
        self.open_extra_extent = extra_extent;
        Ok(())
    }

    /// Write a frame in standard (non-SWMR) mode into the single extensible
    /// dataset, extending its leading dimension.
    fn write_standard(&mut self, array: &NDArray) -> ADResult<()> {
        if self.frame_count == 0 {
            self.create_primary_dataset(array)?;
            self.create_attribute_datasets(array);
        }

        let frame_dims = self
            .open_frame_dims
            .clone()
            .ok_or_else(|| ADError::UnsupportedConversion("dataset not initialised".into()))?;
        let cur_dims: Vec<usize> = array.dims.iter().rev().map(|d| d.size).collect();
        if cur_dims != frame_dims {
            return Err(ADError::UnsupportedConversion(format!(
                "HDF5 frame shape changed mid-stream: {:?} != {:?}",
                cur_dims, frame_dims
            )));
        }

        let (shape, _chunk, _extra) = self.primary_layout(&frame_dims);
        let frame_idx = self.frame_count;
        let extra_extent = self.open_extra_extent;

        // With a fixed extra-dim layout, the frame counter must not exceed
        // the product of the extra-dim sizes.
        if let Some(total) = extra_extent {
            if frame_idx >= total {
                return Err(ADError::UnsupportedConversion(format!(
                    "HDF5 extra-dimension capacity exceeded: frame {} >= {}",
                    frame_idx, total
                )));
            }
        }

        let ds = match self.handle {
            Some(Hdf5Handle::Standard {
                primary: Some(ref ds),
                ..
            }) => ds,
            _ => {
                return Err(ADError::UnsupportedConversion(
                    "HDF5 primary dataset not initialised".into(),
                ))
            }
        };

        if extra_extent.is_none() {
            // No extra dims: extend the leading (frame) dimension per write.
            let mut new_shape = shape.clone();
            new_shape[0] = frame_idx + 1;
            ds.extend(&new_shape)
                .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 extend error: {}", e)))?;
        }
        // With extra dims the dataset is created at full size; frame `frame_idx`
        // maps row-major onto the extra-dim grid, so the linear chunk index is
        // the frame counter itself.

        Self::write_frame_chunk(ds, frame_idx, array.data.as_u8_slice())?;

        // Append NDAttribute values for this frame.
        if self.store_attributes {
            for ad in self.attr_datasets.iter_mut() {
                let value = array
                    .attributes
                    .get(&ad.name)
                    .map(|a| a.value.clone())
                    .unwrap_or(NDAttrValue::Undefined);
                ad.push(&value);
            }
        }
        Ok(())
    }

    /// Create one attribute time-series dataset per NDAttribute, preserving
    /// the NDAttrValue numeric type. Mirrors C++ `createAttributeDataset`.
    fn create_attribute_datasets(&mut self, array: &NDArray) {
        self.attr_datasets.clear();
        if !self.store_attributes {
            return;
        }
        for attr in array.attributes.iter() {
            let dt = attr.value.data_type();
            self.attr_datasets
                .push(AttributeDataset::new(attr.name.clone(), dt));
        }
    }

    /// Flush accumulated NDAttribute datasets into the open standard file.
    /// Each becomes a chunked, extensible 1-D dataset under `NDAttributes/`.
    fn flush_attribute_datasets(&mut self) -> ADResult<()> {
        if self.attr_datasets.is_empty() {
            return Ok(());
        }
        let chunk_depth = self.chunk.ndattr_chunk.max(1);
        let h5file = match self.handle {
            Some(Hdf5Handle::Standard { ref file, .. }) => file,
            _ => return Ok(()),
        };
        let group = h5file
            .create_group("NDAttributes")
            .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 group error: {}", e)))?;

        for ad in self.attr_datasets.iter() {
            if ad.frames == 0 {
                continue;
            }
            let n = ad.frames;
            let chunk = chunk_depth.min(n).max(1);

            macro_rules! write_attr_ds {
                ($t:ty) => {{
                    let es = std::mem::size_of::<$t>();
                    let ds = group
                        .new_dataset::<$t>()
                        .shape(&[n])
                        .chunk(&[chunk])
                        .max_shape(&[None])
                        .create(&ad.name)
                        .map_err(|e| {
                            ADError::UnsupportedConversion(format!(
                                "HDF5 attribute dataset error: {}",
                                e
                            ))
                        })?;
                    // One chunk holds `chunk` consecutive frames; write each
                    // chunk's whole byte span (zero-padded for the trailing
                    // partial chunk, as rust-hdf5 requires full-chunk writes).
                    write_chunked_buffer(&ds, &ad.buffer, chunk * es)?;
                }};
            }

            match ad.data_type {
                NDAttrDataType::Int8 => write_attr_ds!(i8),
                NDAttrDataType::UInt8 => write_attr_ds!(u8),
                NDAttrDataType::Int16 => write_attr_ds!(i16),
                NDAttrDataType::UInt16 => write_attr_ds!(u16),
                NDAttrDataType::Int32 => write_attr_ds!(i32),
                NDAttrDataType::UInt32 => write_attr_ds!(u32),
                NDAttrDataType::Int64 => write_attr_ds!(i64),
                NDAttrDataType::UInt64 => write_attr_ds!(u64),
                NDAttrDataType::Float32 => write_attr_ds!(f32),
                NDAttrDataType::Float64 => write_attr_ds!(f64),
                NDAttrDataType::String => {
                    // Fixed-width u8 field per frame.
                    let es = MAX_ATTRIBUTE_STRING_SIZE;
                    let ds = group
                        .new_dataset::<u8>()
                        .shape([n, es])
                        .chunk(&[chunk, es])
                        .max_shape(&[None, Some(es)])
                        .create(&ad.name)
                        .map_err(|e| {
                            ADError::UnsupportedConversion(format!(
                                "HDF5 attribute dataset error: {}",
                                e
                            ))
                        })?;
                    write_chunked_buffer(&ds, &ad.buffer, chunk * es)?;
                }
            }
        }
        Ok(())
    }

    /// Write the `timestamp` performance dataset (`[nframes, 5]` doubles)
    /// into the open standard file. Mirrors C++ `writePerformanceDataset`.
    fn flush_performance_dataset(&mut self) -> ADResult<()> {
        if !self.store_performance || self.perf_rows.is_empty() {
            return Ok(());
        }
        let n = self.perf_rows.len();
        let mut flat: Vec<f64> = Vec::with_capacity(n * 5);
        for row in &self.perf_rows {
            flat.extend_from_slice(row);
        }
        let raw: Vec<u8> = flat.iter().flat_map(|v| v.to_le_bytes()).collect();

        let h5file = match self.handle {
            Some(Hdf5Handle::Standard { ref file, .. }) => file,
            _ => return Ok(()),
        };
        let group = h5file
            .create_group("performance")
            .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 group error: {}", e)))?;
        let ds = group
            .new_dataset::<f64>()
            .shape([n, 5])
            .chunk(&[1, 5])
            .max_shape(&[None, Some(5)])
            .create("timestamp")
            .map_err(|e| {
                ADError::UnsupportedConversion(format!("HDF5 performance dataset error: {}", e))
            })?;
        for f in 0..n {
            let start = f * 5 * 8;
            let end = start + 5 * 8;
            ds.write_chunk(f, &raw[start..end]).map_err(|e| {
                ADError::UnsupportedConversion(format!("HDF5 performance write error: {}", e))
            })?;
        }
        Ok(())
    }

    /// Write a frame in SWMR mode.
    fn write_swmr(&mut self, array: &NDArray) -> ADResult<()> {
        let (writer, ds_index) = match self.handle {
            Some(Hdf5Handle::Swmr {
                ref mut writer,
                ds_index,
                ..
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

    /// Record one frame's I/O timing into the performance buffer.
    fn record_performance(&mut self, write_duration: f64, frame_bytes: usize) {
        let now = std::time::Instant::now();
        let first = *self.perf_first.get_or_insert(now);
        let runtime = now.duration_since(first).as_secs_f64();
        let period = match self.perf_prev {
            Some(prev) => now.duration_since(prev).as_secs_f64(),
            None => write_duration,
        };
        self.perf_prev = Some(now);
        let fb = frame_bytes as f64;
        let inst_speed = if period > 0.0 { fb / period } else { 0.0 };
        let avg_speed = if runtime > 0.0 {
            (self.perf_rows.len() as f64 + 1.0) * fb / runtime
        } else {
            0.0
        };
        self.perf_rows
            .push([write_duration, period, runtime, inst_speed, avg_speed]);
    }
}

/// Write `buffer` into a chunked dataset, one `chunk_bytes`-sized chunk at a
/// time at consecutive linear indices. The trailing partial chunk is
/// zero-padded to a full chunk, which `rust-hdf5`'s `write_chunk` requires.
fn write_chunked_buffer(
    ds: &rust_hdf5::H5Dataset,
    buffer: &[u8],
    chunk_bytes: usize,
) -> ADResult<()> {
    let n_chunks = buffer.len().div_ceil(chunk_bytes.max(1));
    for c in 0..n_chunks {
        let start = c * chunk_bytes;
        let end = ((c + 1) * chunk_bytes).min(buffer.len());
        let slice = &buffer[start..end];
        if slice.len() == chunk_bytes {
            ds.write_chunk(c, slice)
        } else {
            let mut padded = vec![0u8; chunk_bytes];
            padded[..slice.len()].copy_from_slice(slice);
            ds.write_chunk(c, &padded)
        }
        .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 chunk write error: {}", e)))?;
    }
    Ok(())
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
        self.open_data_type = None;
        self.open_frame_dims = None;
        self.open_extra_extent = None;
        self.perf_rows.clear();
        self.perf_prev = None;
        self.perf_first = None;
        self.attr_datasets.clear();

        if self.swmr_mode && mode == NDFileMode::Stream {
            self.open_swmr(path, array)
        } else {
            let h5file = H5File::create(path)
                .map_err(|e| ADError::UnsupportedConversion(format!("HDF5 create error: {}", e)))?;
            self.handle = Some(Hdf5Handle::Standard { file: h5file, primary: None });
            Ok(())
        }
    }

    fn write_file(&mut self, array: &NDArray) -> ADResult<()> {
        let start = std::time::Instant::now();

        let is_swmr = matches!(self.handle, Some(Hdf5Handle::Swmr { .. }));
        if is_swmr {
            self.write_swmr(array)?;
        } else {
            self.write_standard(array)?;
        }
        self.frame_count += 1;

        let elapsed = start.elapsed().as_secs_f64();
        let frame_bytes = array.data.as_u8_slice().len();
        if self.store_performance {
            self.total_runtime += elapsed;
            self.total_bytes += frame_bytes as u64;
            self.record_performance(elapsed, frame_bytes);
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
        let element_size = ds.element_size();

        // Prefer the exact data type recorded at write time.
        let recorded: Option<NDDataType> = ds
            .attr(DTYPE_ATTR)
            .ok()
            .and_then(|a| a.read_numeric::<i32>().ok())
            .and_then(|v| NDDataType::from_ordinal(v as u8));

        let data_type = recorded.unwrap_or(match element_size {
            1 => NDDataType::UInt8,
            2 => NDDataType::UInt16,
            4 => NDDataType::Float32,
            8 => NDDataType::Float64,
            other => {
                return Err(ADError::UnsupportedConversion(format!(
                    "unsupported HDF5 element size {}",
                    other
                )))
            }
        });

        macro_rules! read_typed {
            ($t:ty, $variant:ident) => {{
                let data = ds.read_raw::<$t>().map_err(|e| {
                    ADError::UnsupportedConversion(format!("HDF5 read error: {}", e))
                })?;
                let mut arr = NDArray::new(dims, data_type);
                arr.data = NDDataBuffer::$variant(data);
                return Ok(arr);
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

    fn close_file(&mut self) -> ADResult<()> {
        match self.handle {
            Some(Hdf5Handle::Standard { .. }) => {
                // Emit the accumulated attribute and performance datasets
                // before the file is finalised.
                self.flush_attribute_datasets()?;
                self.flush_performance_dataset()?;
                self.handle = None;
            }
            Some(Hdf5Handle::Swmr { .. }) => {
                if let Some(Hdf5Handle::Swmr { writer, .. }) = self.handle.take() {
                    writer.close().map_err(|e| {
                        ADError::UnsupportedConversion(format!("SWMR close error: {}", e))
                    })?;
                }
            }
            None => {}
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
    chunk_size_auto: Option<usize>,
    n_row_chunks: Option<usize>,
    n_col_chunks: Option<usize>,
    n_frames_chunks: Option<usize>,
    ndattr_chunk: Option<usize>,
    n_extra_dims: Option<usize>,
    extra_dim_size: [Option<usize>; MAX_EXTRA_DIMS],
    extra_dim_name: [Option<usize>; MAX_EXTRA_DIMS],
    fill_value: Option<usize>,
    dim_att_datasets: Option<usize>,
    layout_filename: Option<usize>,
    layout_valid: Option<usize>,
    layout_error_msg: Option<usize>,
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

/// Names of the `HDF5_extraDimSizeN..9` params in slot order.
const EXTRA_DIM_SIZE_PARAMS: [&str; MAX_EXTRA_DIMS] = [
    "HDF5_extraDimSizeN",
    "HDF5_extraDimSizeX",
    "HDF5_extraDimSizeY",
    "HDF5_extraDimSize3",
    "HDF5_extraDimSize4",
    "HDF5_extraDimSize5",
    "HDF5_extraDimSize6",
    "HDF5_extraDimSize7",
    "HDF5_extraDimSize8",
    "HDF5_extraDimSize9",
];

/// Names of the `HDF5_extraDimNameN..9` params in slot order.
const EXTRA_DIM_NAME_PARAMS: [&str; MAX_EXTRA_DIMS] = [
    "HDF5_extraDimNameN",
    "HDF5_extraDimNameX",
    "HDF5_extraDimNameY",
    "HDF5_extraDimName3",
    "HDF5_extraDimName4",
    "HDF5_extraDimName5",
    "HDF5_extraDimName6",
    "HDF5_extraDimName7",
    "HDF5_extraDimName8",
    "HDF5_extraDimName9",
];

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
        self.hdf5_params.chunk_size_auto = base.find_param("HDF5_chunkSizeAuto");
        self.hdf5_params.n_row_chunks = base.find_param("HDF5_nRowChunks");
        self.hdf5_params.n_col_chunks = base.find_param("HDF5_nColChunks");
        self.hdf5_params.n_frames_chunks = base.find_param("HDF5_nFramesChunks");
        self.hdf5_params.ndattr_chunk = base.find_param("HDF5_NDAttributeChunk");
        self.hdf5_params.n_extra_dims = base.find_param("HDF5_nExtraDims");
        for i in 0..MAX_EXTRA_DIMS {
            self.hdf5_params.extra_dim_size[i] = base.find_param(EXTRA_DIM_SIZE_PARAMS[i]);
            self.hdf5_params.extra_dim_name[i] = base.find_param(EXTRA_DIM_NAME_PARAMS[i]);
        }
        self.hdf5_params.fill_value = base.find_param("HDF5_fillValue");
        self.hdf5_params.dim_att_datasets = base.find_param("HDF5_dimAttDatasets");
        self.hdf5_params.layout_filename = base.find_param("HDF5_layoutFilename");
        self.hdf5_params.layout_valid = base.find_param("HDF5_layoutValid");
        self.hdf5_params.layout_error_msg = base.find_param("HDF5_layoutErrorMsg");

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
        // -- chunking params --
        if Some(reason) == self.hdf5_params.chunk_size_auto {
            self.ctrl
                .writer
                .set_chunk_size_auto(params.value.as_i32() != 0);
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.n_row_chunks {
            self.ctrl
                .writer
                .set_n_row_chunks(params.value.as_i32().max(0) as usize);
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.n_col_chunks {
            self.ctrl
                .writer
                .set_n_col_chunks(params.value.as_i32().max(0) as usize);
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.n_frames_chunks {
            self.ctrl
                .writer
                .set_n_frames_chunks(params.value.as_i32().max(0) as usize);
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.ndattr_chunk {
            self.ctrl
                .writer
                .set_ndattr_chunk(params.value.as_i32().max(1) as usize);
            return ParamChangeResult::updates(vec![]);
        }
        // -- extra dimensions --
        if Some(reason) == self.hdf5_params.n_extra_dims {
            self.ctrl
                .writer
                .set_n_extra_dims(params.value.as_i32().max(0) as usize);
            return ParamChangeResult::updates(vec![]);
        }
        for i in 0..MAX_EXTRA_DIMS {
            if Some(reason) == self.hdf5_params.extra_dim_size[i] {
                self.ctrl
                    .writer
                    .set_extra_dim_size(i, params.value.as_i32().max(1) as usize);
                return ParamChangeResult::updates(vec![]);
            }
            if Some(reason) == self.hdf5_params.extra_dim_name[i] {
                self.ctrl
                    .writer
                    .set_extra_dim_name(i, params.value.as_string().unwrap_or(""));
                return ParamChangeResult::updates(vec![]);
            }
        }
        if Some(reason) == self.hdf5_params.fill_value {
            self.ctrl.writer.set_fill_value(params.value.as_f64());
            return ParamChangeResult::updates(vec![]);
        }
        if Some(reason) == self.hdf5_params.dim_att_datasets {
            self.ctrl
                .writer
                .set_dim_att_datasets(params.value.as_i32() != 0);
            return ParamChangeResult::updates(vec![]);
        }
        // -- layout XML --
        if Some(reason) == self.hdf5_params.layout_filename {
            let path = params.value.as_string().unwrap_or("").to_string();
            self.ctrl.writer.set_layout_filename(&path);
            let mut updates = vec![];
            if let Some(idx) = self.hdf5_params.layout_valid {
                updates.push(ParamUpdate::int32(
                    idx,
                    if self.ctrl.writer.layout_valid { 1 } else { 0 },
                ));
            }
            if let Some(idx) = self.hdf5_params.layout_error_msg {
                updates.push(ParamUpdate::Octet {
                    reason: idx,
                    addr: 0,
                    value: self.ctrl.writer.layout_error.clone(),
                });
            }
            return ParamChangeResult::updates(updates);
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

        // Single-frame standard mode: dataset is [1, 4, 4].
        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("data").unwrap();
        assert_eq!(ds.shape(), vec![1, 4, 4]);
        let data: Vec<u8> = ds.read_raw().unwrap();
        assert_eq!(data[0], 0);
        assert_eq!(data[15], 15);
        drop(h5);

        let mut reader = Hdf5Writer::new();
        reader.current_path = Some(path.clone());
        let read_arr = reader.read_file().unwrap();
        assert_eq!(read_arr.dims.len(), 3);
        assert_eq!(read_arr.dims[2].size, 1); // leading frame dim

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_multiple_frames() {
        let path = temp_path("hdf5_multi");
        let mut writer = Hdf5Writer::new();

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        // Mark each frame distinctly so we can verify per-frame placement.
        for f in 0..3u8 {
            if let NDDataBuffer::U8(ref mut v) = arr.data {
                for x in v.iter_mut() {
                    *x = f;
                }
            }
            if f == 0 {
                writer.open_file(&path, NDFileMode::Stream, &arr).unwrap();
            }
            writer.write_file(&arr).unwrap();
        }
        writer.close_file().unwrap();

        assert!(writer.supports_multiple_arrays());
        assert_eq!(writer.frame_count(), 3);

        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[0..8], b"\x89HDF\r\n\x1a\n");

        // Single extensible dataset [3, 4, 4] — NOT one dataset per frame.
        let h5 = H5File::open(&path).unwrap();
        let names = h5.dataset_names();
        assert!(names.contains(&"data".to_string()));
        assert!(
            !names.contains(&"data_1".to_string()),
            "must not write per-frame datasets"
        );
        let ds = h5.dataset("data").unwrap();
        assert_eq!(ds.shape(), vec![3, 4, 4], "rank/shape must be [nframes,Y,X]");
        let raw: Vec<u8> = ds.read_raw().unwrap();
        assert_eq!(raw.len(), 3 * 4 * 4);
        // Frame 0 all zeros, frame 1 all ones, frame 2 all twos.
        assert_eq!(raw[0], 0);
        assert_eq!(raw[16], 1);
        assert_eq!(raw[32], 2);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_attribute_datasets() {
        let path = temp_path("hdf5_attr_ds");
        let mut writer = Hdf5Writer::new();

        let mk = |exposure: f64, count: i32| {
            let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            arr.attributes.add(NDAttribute::new_static(
                "exposure",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Float64(exposure),
            ));
            arr.attributes.add(NDAttribute::new_static(
                "count",
                "",
                NDAttrSource::Driver,
                NDAttrValue::Int32(count),
            ));
            arr
        };

        let a0 = mk(0.5, 10);
        writer.open_file(&path, NDFileMode::Stream, &a0).unwrap();
        writer.write_file(&a0).unwrap();
        writer.write_file(&mk(0.75, 20)).unwrap();
        writer.write_file(&mk(1.25, 30)).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        // One HDF5 dataset per NDAttribute, under NDAttributes/, [nframes].
        let exp = h5.dataset("NDAttributes/exposure").unwrap();
        assert_eq!(exp.shape(), vec![3]);
        let exp_vals: Vec<f64> = exp.read_raw().unwrap();
        assert_eq!(exp_vals, vec![0.5, 0.75, 1.25]);

        let cnt = h5.dataset("NDAttributes/count").unwrap();
        assert_eq!(cnt.shape(), vec![3]);
        // Numeric type preserved: i32, not stringified.
        let cnt_vals: Vec<i32> = cnt.read_raw().unwrap();
        assert_eq!(cnt_vals, vec![10, 20, 30]);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_fill_value_recorded_on_dataset() {
        // rust-hdf5 0.2.12 cannot set the DCPL fill value, so the configured
        // HDF5_fillValue is recorded as a dataset attribute. Verify it round-trips.
        let path = temp_path("hdf5_fill");
        let mut writer = Hdf5Writer::new();
        writer.set_fill_value(7.5);

        let arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("data").unwrap();
        let fv: f64 = ds.attr("HDF5_fillValue").unwrap().read_numeric().unwrap();
        assert_eq!(fv, 7.5);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_performance_dataset() {
        let path = temp_path("hdf5_perf");
        let mut writer = Hdf5Writer::new();
        writer.set_store_performance(true);

        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        writer.open_file(&path, NDFileMode::Stream, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ts = h5.dataset("performance/timestamp").unwrap();
        assert_eq!(ts.shape(), vec![2, 5]);
        let vals: Vec<f64> = ts.read_raw().unwrap();
        assert_eq!(vals.len(), 10);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_roundtrip_all_types() {
        macro_rules! roundtrip {
            ($name:expr, $dt:expr, $variant:ident, $ty:ty, $vals:expr) => {{
                let path = temp_path($name);
                let mut writer = Hdf5Writer::new();
                let mut arr = NDArray::new(vec![NDDimension::new(4)], $dt);
                if let NDDataBuffer::$variant(ref mut v) = arr.data {
                    let src: Vec<$ty> = $vals;
                    v.copy_from_slice(&src);
                }
                writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
                writer.write_file(&arr).unwrap();
                writer.close_file().unwrap();

                let mut reader = Hdf5Writer::new();
                reader.current_path = Some(path.clone());
                let r = reader.read_file().unwrap();
                assert_eq!(r.data.data_type(), $dt, "type for {}", $name);
                if let NDDataBuffer::$variant(ref v) = r.data {
                    let src: Vec<$ty> = $vals;
                    assert_eq!(v, &src, "values for {}", $name);
                } else {
                    panic!("wrong buffer variant for {}", $name);
                }
                std::fs::remove_file(&path).ok();
            }};
        }

        roundtrip!("rt_i8", NDDataType::Int8, I8, i8, vec![-1, 0, 1, 127]);
        roundtrip!("rt_u8", NDDataType::UInt8, U8, u8, vec![0, 1, 200, 255]);
        roundtrip!(
            "rt_i16",
            NDDataType::Int16,
            I16,
            i16,
            vec![-32768, -1, 1, 32767]
        );
        roundtrip!(
            "rt_u16",
            NDDataType::UInt16,
            U16,
            u16,
            vec![0, 1, 40000, 65535]
        );
        roundtrip!(
            "rt_i32",
            NDDataType::Int32,
            I32,
            i32,
            vec![i32::MIN, -1, 1, i32::MAX]
        );
        roundtrip!(
            "rt_u32",
            NDDataType::UInt32,
            U32,
            u32,
            vec![0, 1, 3_000_000_000, u32::MAX]
        );
        roundtrip!(
            "rt_i64",
            NDDataType::Int64,
            I64,
            i64,
            vec![i64::MIN, -1, 1, i64::MAX]
        );
        roundtrip!(
            "rt_u64",
            NDDataType::UInt64,
            U64,
            u64,
            vec![0, 1, 9_000_000_000, u64::MAX]
        );
        roundtrip!(
            "rt_f32",
            NDDataType::Float32,
            F32,
            f32,
            vec![-1.5, 0.0, 2.25, 3.75]
        );
        roundtrip!(
            "rt_f64",
            NDDataType::Float64,
            F64,
            f64,
            vec![-1.5, 0.0, 2.25, 3.75]
        );
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
    fn test_bitshuffle_compressed_write() {
        let path = temp_path("hdf5_bshuf");
        let mut writer = Hdf5Writer::new();
        writer.set_compression_type(COMPRESS_BSHUF);

        let mut arr = NDArray::new(
            vec![NDDimension::new(64), NDDimension::new(64)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = (i % 8) as u16;
            }
        }

        writer.open_file(&path, NDFileMode::Single, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let h5file = H5File::open(&path).unwrap();
        let ds = h5file.dataset("data").unwrap();
        let data: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 64 * 64);
        assert_eq!(data[0], 0);
        assert_eq!(data[9], 1);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_chunk_geometry_recorded() {
        // Requested row/col chunk geometry is recorded as dataset attributes
        // (the on-disk chunk is one frame per chunk — crate limitation).
        let path = temp_path("hdf5_chunkgeom");
        let mut writer = Hdf5Writer::new();
        writer.set_chunk_size_auto(false);
        writer.set_n_row_chunks(4);
        writer.set_n_col_chunks(2);
        writer.set_n_frames_chunks(3);

        let mut arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            for i in 0..v.len() {
                v[i] = i as u16;
            }
        }

        writer.open_file(&path, NDFileMode::Stream, &arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("data").unwrap();
        assert_eq!(ds.shape(), vec![2, 8, 8]);
        // Data still round-trips correctly through the per-frame chunks.
        let data: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 2 * 64);
        for i in 0..64usize {
            assert_eq!(data[i], i as u16, "frame0 element {}", i);
            assert_eq!(data[64 + i], i as u16, "frame1 element {}", i);
        }
        // Requested geometry preserved as attributes.
        assert_eq!(ds.attr("HDF5_nRowChunks").unwrap().read_numeric::<i32>().unwrap(), 4);
        assert_eq!(ds.attr("HDF5_nColChunks").unwrap().read_numeric::<i32>().unwrap(), 2);
        assert_eq!(
            ds.attr("HDF5_nFramesChunks")
                .unwrap()
                .read_numeric::<i32>()
                .unwrap(),
            3
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_extra_dimensions_layout() {
        // HDF5_nExtraDims=2 with sizes [2,3] => 6-frame [6,Y,X] dataset with
        // the extra-dim sizes/names recorded as attributes (the flat leading
        // axis reshapes to the intended [2,3,Y,X]).
        let path = temp_path("hdf5_extradims");
        let mut writer = Hdf5Writer::new();
        writer.set_n_extra_dims(2);
        writer.set_extra_dim_size(0, 2);
        writer.set_extra_dim_size(1, 3);
        writer.set_extra_dim_name(0, "scanY");
        writer.set_extra_dim_name(1, "scanX");

        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt16,
        );
        for f in 0..6u16 {
            if let NDDataBuffer::U16(ref mut v) = arr.data {
                for x in v.iter_mut() {
                    *x = f;
                }
            }
            if f == 0 {
                writer.open_file(&path, NDFileMode::Stream, &arr).unwrap();
            }
            writer.write_file(&arr).unwrap();
        }
        writer.close_file().unwrap();

        let h5 = H5File::open(&path).unwrap();
        let ds = h5.dataset("data").unwrap();
        // Flat leading axis = product(2,3) = 6.
        assert_eq!(ds.shape(), vec![6, 4, 4]);
        let data: Vec<u16> = ds.read_raw().unwrap();
        assert_eq!(data.len(), 6 * 16);
        for f in 0..6usize {
            for i in 0..16usize {
                assert_eq!(data[f * 16 + i], f as u16, "frame {} elem {}", f, i);
            }
        }
        // Extra-dim layout recoverable from attributes.
        assert_eq!(
            ds.attr("HDF5_nExtraDims")
                .unwrap()
                .read_numeric::<i32>()
                .unwrap(),
            2
        );
        assert_eq!(
            ds.attr("HDF5_extraDimSize0")
                .unwrap()
                .read_numeric::<i32>()
                .unwrap(),
            2
        );
        assert_eq!(
            ds.attr("HDF5_extraDimSize1")
                .unwrap()
                .read_numeric::<i32>()
                .unwrap(),
            3
        );
        assert_eq!(
            ds.attr("HDF5_extraDimName0").unwrap().read_string().unwrap(),
            "scanY"
        );

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

    #[test]
    fn test_swmr_compression_dropped_is_flagged() {
        // rust-hdf5 0.2.0 exposes no filtered SWMR dataset API. When SWMR is
        // combined with compression the writer must flag the drop, never
        // silently produce an uncompressed file as if it were compressed.
        let path = temp_path("hdf5_swmr_comp");
        let mut writer = Hdf5Writer::new();
        writer.set_swmr_mode(true);
        writer.set_compression_type(COMPRESS_ZLIB);

        let arr = NDArray::new(
            vec![NDDimension::new(8), NDDimension::new(8)],
            NDDataType::UInt16,
        );
        writer.open_file(&path, NDFileMode::Stream, &arr).unwrap();
        assert!(
            writer.swmr_compression_dropped(),
            "SWMR+compression must be flagged as dropped"
        );
        writer.write_file(&arr).unwrap();
        writer.close_file().unwrap();

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_layout_xml_param() {
        // Valid and invalid layout XML drive layout_valid / layout_error.
        let mut writer = Hdf5Writer::new();
        let dir = std::env::temp_dir();
        let good = dir.join("adcore_layout_good.xml");
        std::fs::write(
            &good,
            r#"<hdf5_layout><group name="entry"><dataset name="data" source="detector" det_default="true"/></group></hdf5_layout>"#,
        )
        .unwrap();
        assert!(writer.set_layout_filename(good.to_str().unwrap()));
        assert!(writer.layout_valid);
        assert!(writer.layout_error.is_empty());

        let bad = dir.join("adcore_layout_bad.xml");
        std::fs::write(&bad, r#"<not_a_layout/>"#).unwrap();
        assert!(!writer.set_layout_filename(bad.to_str().unwrap()));
        assert!(!writer.layout_valid);
        assert!(!writer.layout_error.is_empty());

        std::fs::remove_file(&good).ok();
        std::fs::remove_file(&bad).ok();
    }
}
